# Permission audit vs Claude Code 2.1.270 — auto mode, 2026-09-13

Trigger: on desktop, **Auto mode prompted on essentially every command**, and
`AskUserQuestion` / "create task" raised a permission request too.

Oracle: `~/.local/share/claude/versions/2.1.270` (released 2026-09-13), split on
`// @bun @bytecode` into 1697 chunks. Every claim below cites a chunk offset.

## Landing status

Everything below has landed. §1 and §2b waited on another session's
uncommitted classifier substrate (`permission/src/loop_llm.rs`,
`permission/src/bundled/`, `orchestrator/src/loop_permission_classifier.rs`,
and the async rewrite of `policy_gate::auto_mode_classifier_result`); that
substrate and this change went in together as `af24c7042`, because
`policy_gate.rs` interleaves the two at line level.

| § | change | status |
|---|---|---|
| 3 | `AskUserQuestion` stops stacking a prompt | landed |
| 4 | `defaults_per_tool` — 11 rows | landed |
| 2a | `Sft` / `ojo` auto-mode safe allowlist (the SET) | landed |
| 5 | auto-gate provider family `XD` | landed |
| 6a | plan-mode bypass needs an interactive launch | landed |
| 6b | the dead `provider` auto-denial reason, deleted | landed |
| 6c | Shift+Tab can reach Auto | landed |
| 2a | wiring that set into the Auto path | landed (`af24c7042`) |
| 2b | the acceptEdits simulation (its policy-side guard IS landed) | landed (`af24c7042`) |
| 1 | the classifier judges every tool | landed (`af24c7042`) |

A second pass the same day went back over "Remaining known divergences" below:
it fixed one of them, found that another was recorded backwards (and fixed the
real defect it had been hiding), and turned up a third that this report missed
entirely. All three are in §7.

---

## 0. The shape of the auto-mode decision upstream

`dKo` (`src_169588164.js` @2460174, 16,277 bytes) is the whole Auto-mode
decision. When the base result is `ask` and the effective mode is auto
(`fd(U)`), it walks, in order:

1. safety-check / ask-rule / org-ceiling / plan-floor → fall back to ask
2. `requiresUserInteraction()` → fall back to ask
3. workflow usage consent → ask
4. outside-read first prompt → ask
5. **acceptEdits simulation** → allow (`"would be allowed in acceptEdits mode"`)
6. **`Sft` safe allowlist** → allow (`"tool is on the safe allowlist"`)
7. **the two-stage LLM classifier** decides everything else
8. classifier unavailable → deny, fail-closed

Steps 5–7 are the whole mechanism. The port had **none of them reachable** for a
general tool.

---

## 1. 🚨 The classifier was wired to three tool names

`permission/src/policy_gate.rs`:

```rust
if matches!(name, "CronCreate" | "ScheduleWakeup" | "Monitor") {
    if let Some(classifier) = self.loop_classifier.get() { … }
}
```

This port **owns the entire 2.1.270 classifier** — `permission/src/loop_llm.rs`
(two-stage fast/thinking protocol, `parse_block`, retry/timeout budgets), the
bundled 129 KB system prompt + template in `permission/src/bundled/`, and
`orchestrator::loop_permission_classifier::SessionLoopClassifier` (transcript
renderer, cache boundaries, CLAUDE.md weighing). It was reachable for exactly
the three `/loop` tools, a leftover of the batch that introduced it. Everything
else fell through to the offline `classifier::classify_tool_call` table, whose
`Pass` arm is a **prompt**.

Measured before the fix, over 50 ordinary development shell commands with no
rules configured:

| mode | allowed | prompted |
|---|---|---|
| Auto | 27 | **23** |
| Default | 17 | 33 |

`local_shell_allow` refuses anything containing `|`, `>`, `<`, `$(`, a backtick,
`&&` or `;`, and its base-command list is ~20 entries — so `pnpm i`, `make test`,
`tsc --noEmit`, `node scripts/gen.js`, `cp`, `mv`, `chmod`, `./gradlew …`,
`awk`, `open .` all prompted.

**Fixed.** The classifier now judges every tool in Auto mode. The offline table
is kept AHEAD of it as a local fast path: an `Allow` verdict answers without a
round-trip; a `Pass` or a local `Deny` is the classifier's call — matching `dKo`,
which has no local deny list. With no classifier bound (a bare gate in a test, a
host that never filled `loop_classifier_handle`) the local verdict still stands,
so those hosts keep today's behavior rather than silently losing the deny side.

## 2. The two missing fast paths

### 2a. `Sft` / `ojo` — the auto-mode safe allowlist

`Sft(e,n)` @2150529 over the `ojo` set @2149246. Ported as
`mode_policy::is_auto_mode_safe_tool` (26 rows). 🚨 It is **not** the same list
as `PLAN_SAFE_TOOLS`, though the port originally reused one set for both: by
2.1.270 `ojo` no longer carries `SendMessage` or `AskUserQuestion`, and it has
grown `ReadMcpResourceDirTool`, `RefreshMcpTools`, `WaitForMcpServers`,
`ReportFindings`, `GetTask`, `ConnectGitHub`, `ShowOnboardingRolePicker`,
`SearchMcpRegistry`, `SuggestConnectors`, `ListConnectors`. The Plan backstop is
this port's own construct with no upstream twin (upstream enforces plan mode by
not ADVERTISING mutating tools), and removing `AskUserQuestion` from it would
break the one tool plan mode is documented to use — so the two sets are now
separate.

Documented omissions: `PLUGIN_SKILL_SAFE_TOOL_NAMES` (six plugin-skill tools)
and `Sft`'s `sjo`/`BPn`/`UPn`/`HPn` arms, which are per-input predicates over
Chrome/browser MCP tools this port does not advertise.

### 2b. The acceptEdits simulation

`dKo` re-runs the tool's own `checkPermissions` with `mode:"acceptEdits"` and
the dangerous-classifier allow rules filtered out; an `allow` answers the call
with `decisionReason:{type:"mode",mode:"auto"}`. Ported as
`PolicyPermissionGate::accept_edits_fast_path`.

🚨 The filter is load-bearing and is **not** free in this port. Upstream filters
`alwaysAllowRules` with `$He`; this port applies the same predicate through
`rule_is_available_in_mode`, which keys on the mode being `Auto` — and the probe
deliberately passes `AcceptEdits`. `PermissionPolicy::apply_auto_mode_restrictions`
carries Auto's restrictions across the probe. Without it the probe hands back the
very allow the classifier was meant to adjudicate (`Bash(python:*)`), and it also
releases the `CronCreate`/`ScheduleWakeup` carve-out that withholds their
tool-local allow in Auto — a fail-open introduced by the fast path itself.

⚠️ The red-proof for that guard is only honest when the fixture boots in
`Default` and reaches Auto through `set_permission_mode`.
`PermissionPolicy::from_rules(Auto, …)` ends in `set_mode(Auto)` →
`strip_dangerous_for_auto`, which removes `Bash(python:*)` from `allow_rules`
outright, so an Auto-booted fixture never reaches the availability check and
stays green with the guard deleted. The LIVE switch — what the desktop mode
picker does — writes only `mode_override` and never strips.

Not ported: the linked-worktree retry (`vo.decisionReason?.type==="workingDir"
&& UZt()` → `xMn` → `ko(Nn)`). Its absence can only make the probe answer
`false` more often, i.e. send a call to the classifier, never past it.

## 3. 🚨 `AskUserQuestion` raised a second dialog

Oracle `checkPermissions` IS `{behavior:"ask", message:"Answer questions?"}`
(`As` tool object @3607405) — but upstream RENDERS that ask as the
questionnaire: `{event:"ui.render", component:"AskUserQuestion",
drawnBy:"AskUserQuestionPermissionDialog"}`, and `call` reads the chosen labels
back out of `updatedInput.answers`. There is exactly one dialog.

This port draws the questionnaire in `call`, behind `AskUserQuestionResolver`
(TUI bottom pane / desktop `AskUserQuestionPrompt` / mobile sheet). `9ed4d5598`
("Enforce authorization before interactive and executable surfaces") copied the
oracle's `ask` without that half, so the permission layer stacked a generic
"allow AskUserQuestion?" card in front of the question card — the user answered
twice for one question. Changed to `Allow`; `requires_user_interaction()` stays
`true`, so the always-allow-rule suppression and the hook-rescue block are
unaffected, and the tool is deliberately absent from `ojo`.

⛔ Do not restore the `ask` without first moving the questionnaire onto the
permission surface.

## 4. 🚨 `defaults_per_tool` was on the wrong side of the oracle's default

The oracle's tool factory (`At`, `src_167837625.js` @8300):

```js
checkPermissions:(n,a)=>{let{tool:r,call:i}=t(a);
  return r.checkPermissions?r.checkPermissions(n,i)
                           :Promise.resolve({behavior:"allow",updatedInput:n})}
```

Only a `passthrough` reaches `tBn`'s mode tail
(`C.behavior==="passthrough"?{...C,behavior:"ask",…}:C`), so **a tool whose
object declares no `checkPermissions` never prompts on mode alone** — deny/ask
rules, `requiresUserInteraction` and the MCP ceiling still bind above it.

Scanning every `At({…})` literal in the binary for the presence of a
`checkPermissions` key gives the authoritative split. Eleven rows were wrong:

| row | was | now | oracle |
|---|---|---|---|
| `TaskCreate` | Deny | Allow | `uw`, none |
| `TaskUpdate` | Deny | Allow | `pw`, none |
| `TaskStop` | Deny | Allow | `Kg`, none |
| `CronDelete` | Deny | Allow | `cw`, none |
| `ExitWorktree` | Deny | Allow | `ile`, none |
| `ListMcpResourcesTool` | Deny | Allow | `X5`, none |
| `ReadMcpResourceTool` | Deny | Allow | `AW`, none |
| `ReadMcpResourceDirTool` | *(no row)* | Allow | `vW`, none |
| `WaitForMcpServers` | *(no row)* | Allow | `eD`, none |
| `ReportFindings` | *(no row)* | Allow | `t0`, none |
| `PushNotification` | *(no row)* | Allow | `UR`, none |

`TaskCreate` is the one users hit constantly — "create task" prompted in every
mode. The other half of the same scan is pinned too: `Bash`, `Write`, `Edit`,
`NotebookEdit`, `WebFetch`, `WebSearch`, `SendMessage`, `RemoteTrigger`,
`EnterWorktree`, `CronCreate`, `REPL`, `PowerShell` all DO declare
`checkPermissions` and keep reaching the prompt, so a blanket flip would have
been wrong.

Counts moved: oracle-parity 42 → 46 rows (14 deny / 32 allow), table 81 → 85.

⚠️ `ExitWorktree` becoming `AllowByDefault` also short-circuits the Plan-mode
backstop (the divergence carve-out only covers `LocalApp*` + `Workflow`).
Upstream allows it in every mode including plan, so this follows the oracle —
but it is a plan-mode behavior change, recorded here deliberately.

## 5. The auto-mode availability gate, re-read at 2.1.270

```js
function aC(){if(WSe())return!1;if(eqe())return!1;if(!Xce(nt()))return!1;return!0}   // P0
function R8(){if(eqe())return"settings";if(WSe())return"circuit-breaker";
              if(!Xce(nt()))return"model";return AFn()}                              // One
function XD(e=He()){return e==="anthropicAws"||e==="anthropicGoogleCloud"}
```

* 🚨 **Fixed:** the model gate's "counts as first-party" test is `XD` —
  `anthropicAws` **or** `anthropicGoogleCloud`. The port had `anthropicAws`
  alone, so a session on `anthropicGoogleCloud` running `claude-sonnet-4-6`,
  `claude-opus-4-6` or any haiku was refused entry to Auto mode with
  `auto mode unavailable for this model`. Every existing test used
  `anthropicAws`, so nothing was red.
* **Not ported, deliberately:** `R8`'s tail is `AFn()` — the
  `fastModeBreakerReason` latch — adding a fifth reason
  `auto mode unavailable while fast mode is on · run /fast off`, plus `iBe()`'s
  null-reason fallback `auto mode is unavailable right now`. This port has a
  fast mode but does not feed a breaker latch into the gate; adding the variant
  without the latch would be a reason nothing can reach. Wire the latch first.
* `jqt`/`AutoGateDenialReason::Provider` has no 2.1.270 counterpart at all —
  `Xce` no longer consults a provider opt-in. Kept unreachable rather than
  removed in this pass.

---

## 6. The other modes, re-read at 2.1.270

Verified IDENTICAL, no change needed:

* **The Shift+Tab cycle** `sKe` (`src_178976794.js`): `default → acceptEdits →
  plan → (bypass? | auto? | default)`, `bypassPermissions → (auto? | default)`,
  `dontAsk → default`. `permission::next_permission_mode` matches arm for arm.
* **The mode config map** (`src_165140675.js` @64830): `Manual` / `Plan` /
  `Accept edits` / `Bypass Permissions` / `Don't Ask` / `Auto`. Byte-identical
  to `PermissionMode::title`.
* **`ACCEPT_EDITS_ALLOWED_COMMANDS`** (`ZHo`, `src_169588164.js` @2089126) is
  still `["mkdir","touch","rm","rmdir","mv","cp","sed"]`.
* **Plan-mode ask copy**: `Cannot write to ${path} while in plan mode.` and
  `Cannot call ${name} while in plan mode.` (⚠️ upstream raises the second only
  for MCP tools — `e.mcpInfo && !isReadOnly && passthrough && mode==="plan"`;
  this port's whole-allowlist backstop is its own construct, since upstream
  enforces plan mode by not ADVERTISING mutating tools.)
* **`dontAsk`** denies with `KZ(e.name)`, and the deny suffix `Q$t + bBr` is
  carried verbatim in `DENIAL_WORKAROUND_GUIDANCE` (modulo the brand word).
  ⚠️ The literal gate reports this one as a MISS because the port stores as ONE
  Rust literal what the binary concatenates from two JS constants at runtime —
  an instrument artifact, not a divergence. Verified by direct byte search.
* **The file-write mode order** — safety check → plan → acceptEdits → allow
  rule → ask — matches `authorize_inner`.

Three changed:

### 6a. 🚨 The plan-mode bypass asked the wrong question

```js
function zj(e,n){return e==="plan"&&n===!0&&!Ae()}        // src_165140675.js @64480
function Ae(){return!n().host.launchOptions.isInteractive()}  // src_164505306.js @77144
```

Plan counts as `bypassPermissions` only in an INTERACTIVE launch. This port had
`!bypass_killswitch_active` in that slot — a different predicate entirely — so a
HEADLESS run launched with `--dangerously-skip-permissions` and then put into
plan mode took a blanket allow that upstream withholds. Added
`PermissionPolicy::interactive_session` (defaults `false`, fail-closed) and wired
it at all four policy constructions.

⚠️ The killswitch conjunct is KEPT rather than removed: upstream's `zj` genuinely
does not consult it (only the cycle's `oKe` does, as `!bS()`), so deleting it for
byte-parity would REOPEN the plan bypass on a session whose administrator
disabled bypass mode. A strictly-narrowing divergence, recorded not removed.

Identifying `Ae` mattered: minified names are chunk-local, so it was resolved by
finding the chunk whose exports cover **37/37** of the importing chunk's list.

### 6b. The `provider` auto-denial reason no longer exists upstream

`R8` (`One`) in 2.1.270 returns `settings` / `circuit-breaker` / `model` /
`AFn()`. There is no `provider` tag, `Xce` consults no provider opt-in, and the
string `auto mode requires CLAUDE_CODE_ENABLE_AUTO_MODE=1` occurs **zero** times
in the binary (the env-var NAME survives only inside settings allowlists).
`AutoGateDenialReason::Provider` and the vestigial `provider_allows_auto_mode`
are deleted — an unreachable variant that prints copy the oracle does not have is
a divergence in the port's own direction.

### 6c. Shift+Tab could never reach Auto

`sKe`'s `plan` and `bypassPermissions` arms consult
`I1(e) = !!e.isAutoModeAvailable && aC()`. The TUI passed `false`
unconditionally, justified by a comment — "its classifier is an unwired stub" —
that §1 has made false. `auto_available` is now computed by
`apps/cli::auto_mode_cycle_available` (the same `aC()` inputs as the boot
downgrade) and threaded to the bottom pane.

## Remaining known divergences (not addressed here)

1. **The port's mode fallback is a table, not the tool's own answer.**
   Upstream's `tBn` treats `checkPermissions`'s result as the base decision and
   only converts `passthrough` to `ask`; this port folds rules + tool policy
   into `PermissionPolicy::authorize` and patches the gap with
   `defaults_per_tool` + `read_only_default_auto_allows`. §4 aligns the table's
   membership, not the mechanism. A tool added to this port without a row still
   fails closed to a prompt.
2. **`fd(e) = e==="auto" || e==="plan" && xS()`** — the "plan mode with auto
   active" arm has no port counterpart; Plan mode never reaches the classifier.
   **Attempted 2026-09-13 and reverted, because the arm would have been dead
   code.** Two earlier readings were both wrong and are corrected here:

   * *"The latch has nowhere to live."* It does: `xS()` is a session latch
     (`$$().active`, set by `Rk`) and every transition upstream applies it in
     the mode setter, which in this port is `PolicyPermissionGate::set_permission_mode`
     — one clean file. `prePlanMode` is only needed for restoring auto on plan
     EXIT, a different behaviour.
   * *"The opt-out makes it dormant upstream."* It does not. `kkn() = aC() && l6n()`,
     `l6n() = hNt("useAutoModeDuringPlan")`, and `hNt(k)` is
     `![…tiers].some(s => s?.[k] === false)` — true unless a tier explicitly
     says false. Default ON.

   **Closed 2026-09-14: not an observable divergence.** The reachability probe
   was run again, this time printing the ASK REASON rather than only the
   outcome, in both modes:

   | scenario | Auto | Plan |
   |---|---|---|
   | plain `Bash` | `PermissionMode{Auto}` → classifier | `PermissionMode{Plan}` → prompt |
   | explicit ask rule | `MatchedRule` → prompt | `MatchedRule` → prompt |
   | `Read` outside cwd | `PermissionMode{Auto}` | `PermissionMode{Plan}` |
   | MCP tool | `PermissionMode{Auto}` → classifier | `PermissionMode{Plan}` → prompt |
   | `Edit` on `~/.ssh/config` | `PermissionMode{Auto}` → classifier | `PermissionMode{Plan}` → prompt |
   | `Bash` with a subshell | `PermissionMode{Auto}` → classifier (blocked) | `PermissionMode{Plan}` → prompt |
   | unknown tool | `PermissionMode{Auto}` → classifier | `PermissionMode{Plan}` → prompt |
   | `WebSearch` | `PermissionMode{Auto}` → classifier | `PermissionMode{Plan}` → prompt |

   Every ask Plan mode raises carries `PermissionMode{Plan}`, and the only other
   reason observed is `MatchedRule`. Upstream bails out of the classifier for
   BOTH: `Tn = Z$n(M.decisionReason)` is the `plan_mode_floor` reason, and `Rt`
   is the matched-ask-rule reason. So upstream prompts on exactly these too —
   `fd`'s plan arm exists there to admit reasons this port never produces in
   Plan mode, because the plan backstop raises the ask first and stamps its own
   reason on it.

   That also retires the follow-up this report proposed a day earlier (splitting
   the floor out of `reason_allows_classifier`): the split would admit
   non-plan reasons, and there are none to admit. The one theoretical opening is
   a `SafetyCheck{classifier_approvable}` raised in Plan mode; the auto-edit
   safety path that produces it is pre-empted by the plan backstop in every
   shape measured (`.lingxi/settings.json`, `~/.ssh/config`). A future reason
   producer that bypasses the backstop would reopen this.
3. ~~**Classifier-unavailable handling.**~~ Half of this was wrong and the other
   half is now fixed; see §7.1. What remains is narrower: when NO classifier is
   bound at all (a host that never filled `loop_classifier_handle`), the local
   table's verdict stands and a `Pass` still prompts. That is deliberate — a
   bare `PolicyPermissionGate` has no provider to fail closed against.
4. **`R8`'s new `fast-mode` tail.** 2.1.270 adds a fifth reason
   (`auto mode unavailable while fast mode is on · run /fast off`) plus `iBe()`'s
   null fallback (`auto mode is unavailable right now`). This port has a fast
   mode but feeds no breaker latch into the gate; adding the variant without the
   latch would be a reason no code path can reach. Wire the latch first.
   Re-verified 2026-09-13: `R8()` returns `AFn()` (`fastModeBreakerReason`), and
   the only writer is `PQe`, which sets it from
   `PFn({model,fastMode,disableFastMode}) → s = disableFastMode && (fastMode || xwr(model))`.
   `disableFastMode` is a REMOTE `tengu_auto_mode_config` field. With no remote
   config to read it from, the port's latch would be pinned false and the reason
   permanently unreachable — cf. [[a-graduated-rollout-flag-leaves-the-gate-off-forever]].
5. ~~**One wasted round-trip on a hook `ask`.**~~ **Wrong, and backwards.**
   `dKo` pays that round-trip too: `xe = v.hookAskFloor===!0` is read inside
   `De`, the ALLOW callback, and the classifier runs before it. So
   `resolve_detailed_or_abort` running the classifier and having the turn loop
   restore the `Ask` was the FAITHFUL path all along. The divergence was on the
   other path, which skipped the classifier entirely — see §7.2.

6. ~~**The sandbox-network action reaches the classifier under a name its own
   rule does not use.**~~ **Landed 2026-09-14** (`a0c6448f1`): the call site now
   passes `SandboxNetworkAccess`, and `ive`'s block arm is ported — a denied
   `host:port` is not asked about again. The allow arm is not: upstream keys it
   on a transcript watermark (`CLe`) this callback is never given, and a cached
   allow without one would outlive the transcript that justified it. Committed
   as HEAD's file plus three hunks, because the shared copy of
   `apps/engine-desktop/src/lib.rs` carries another session's work. Original
   finding below.

   **The sandbox-network action reaches the classifier under a name its own
   rule does not use.** Upstream's third `bke` consumer is `mUe`, which
   synthesises a tool call named `XV = "SandboxNetworkAccess"` carrying
   `{host, port}` and classifies it (`severitySite: {key:"sandboxNetwork"}`),
   failing closed on `unavailable`. The port reaches the same decision through
   `sandbox_network_ask_callback`, which calls
   `permission_gate.check("Sandbox Network Callback", …)` — and
   `transcript_blocks` renders that name straight into the classifier prompt.
   The bundled policy's rule is LABELLED `Sandbox Network Callback` but its body
   says "A `SandboxNetworkAccess` action", so the classifier is told to look for
   a name the port never sends. The mode mapping itself is equivalent
   (`cVe`: auto→classify, bypass→allow, dontAsk→deny, else ask — which is what
   the port's gate does per mode), and upstream additionally MEMOISES the
   verdict per `(host, port, transcript-key)` via `getOrClassify`, which the port
   does not: every outbound connection pays a fresh round-trip. Both fixes live
   in `apps/engine-desktop/src/lib.rs` (two sites, one a test), which is carrying
   another session's uncommitted work.

---

## 7. Second pass, 2026-09-13 — three more defects in the auto path

### 7.1 A classifier that gives no verdict was counted as a denial

`dKo` keeps three deny-shaped outcomes OUT of the consecutive-denial counter,
and says so twice — once structurally, once in a log line:

```js
let Mo = Yn.shouldBlock && !Yn.unavailable && !Yn.transcriptTooLong
         && !Yn.refusedBySafeguard && Jr===void 0;
…
if(Yn.unavailable){ … t("Auto mode classifier unavailable, denying with retry guidance (fail closed)")
                    return {behavior:"deny",decisionReason:{…,reason:gde},message:$7t(e.name,Yn.model,Yn.httpStatus,Yn.errorKind)} }
if(Yn.refusedBySafeguard){ … t("… denying (exempt from the denial counter)")
                    return {behavior:"deny",decisionReason:{…,noVerdict:!0},message:Det(Yn.reason,{refused:!0})} }
…
let rs = ZJ(v, xft);      // ← the counter, reached ONLY by a real block
```

The port folded all of it into one `AutoModeClassifierVerdict::Deny`, which
`auto_mode_classifier_result` feeds straight into
`denial_tracking::record_auto_deny`. `limits::MAX_CONSECUTIVE` is 3, so **three
provider hiccups in a row tripped the local breaker** and dropped Auto mode back
to prompting for the rest of the session — the exact failure the classifier path
exists to prevent, reached without anything ever judging an action.

The second, quieter half: the model was told `Auto mode classifier blocked
action: …`, a judgment that was never made, instead of `$7t`'s "wait a moment
and try this action again … read-only operations do not require the classifier".

Fixed by giving the verdict enum a fourth arm, `NoVerdict { reason, message }`,
produced by `loop_llm::classify` for a transport error, a timeout, and a bare
safeguard refusal, and denied by `policy_gate` without touching the counter.
A parse failure stays a counted `Deny` — upstream's `e$e` arms carry
`shouldBlock:!0` with no `unavailable` flag, so they are real blocks.

| test | plant that turns it red |
|---|---|
| `a_classifier_that_gives_no_verdict_never_trips_the_denial_breaker` | `record_auto_deny` + `trip` back in the `NoVerdict` arm |
| `a_classifier_that_blocks_does_trip_the_denial_breaker` (premise) | — it is the premise: it proves the counter is live on this path |
| `loop_llm::tests::an_unanswered_query_is_a_no_verdict_with_retry_guidance` | pins `$7t`'s copy byte-for-byte |
| `loop_llm::tests::a_bare_refusal_is_a_no_verdict_not_a_block` | |
| `loop_llm::tests::a_refusal_behind_a_fast_block_keeps_stage_ones_verdict` | the other side: `stage1VerdictStands` IS a verdict, still counted |

### 7.2 A hook `ask` floor threw away the classifier's DENY

`dKo` applies `hookAskFloor` inside `De`, and `De` is only ever called on the
allow paths:

```js
let De=(rs)=>{ …
  if(xe){ if(U==="dontAsk") return {behavior:"deny",…};
          return {...M, updatedInput:rs.updatedInput} }   // M is the ASK
  return {behavior:"allow",...rs}};
```

Every `Yn.shouldBlock` arm returns its deny *before* `De` exists in the control
flow. So upstream: the classifier runs under the floor, its ALLOW is discarded
(the hook's ask stands), and its BLOCK still blocks.

`decide_outcome_with_context` skipped the classifier outright when the floor was
set. That got the allow side right for the wrong reason and silently dropped the
deny side: a PreToolUse hook returning `ask` in front of a genuinely dangerous
action turned it into a user prompt instead of a denial. (The *other* port
path, `resolve_with_mode`, never knew about the floor and was therefore already
faithful — see the correction to remaining divergence 5.)

| test | plant that turns it red |
|---|---|
| `a_hook_ask_floor_does_not_turn_a_classifier_block_into_a_prompt` | restore `if ctx.hook_ask_floor { None } else { … }` around the classifier call |
| `a_hook_ask_floor_still_discards_a_classifier_allow` | same plant (it asserts the classifier ran) |

### 7.3 An over-long transcript was reported as a provider outage

`Yn.transcriptTooLong` is the one classifier failure `dKo` does NOT resolve as a
deny:

```js
if(Yn.transcriptTooLong){ …
  if(e.name===ht) return {behavior:"allow",updatedInput:n,decisionReason:{type:"mode",mode:"auto"}};
  …
  if(F.shouldAvoidPermissionPrompts) throw new Ye("Agent aborted: auto mode classifier transcript exceeded context window in headless mode");
  … return {...M, decisionReason:Wmt(M,{type:"other",reason:eut})} }
```

`ht` is `"Agent"` — spawning a subagent is how a session ESCAPES an over-long
transcript, so gating it behind a transcript it cannot shorten deadlocks.

The port lost the distinction at the transport boundary: `ProviderTransport`
did `.map_err(|error| error.to_string())`, so `LlmError::ContextOverflow` — a
TYPED error the crate already raises — arrived as an anonymous string and
resolved as an outage. Every action in a long Auto-mode conversation would have
been denied with "wait a moment and then try this action again", advice that can
never come true: the transcript only grows.

Fixed by giving `Transport::query` a two-arm error type (`QueryError::{Unavailable,
TranscriptTooLong}`) that keeps the two apart, and adding
`AutoModeClassifierVerdict::TranscriptTooLong` with all three of `dKo`'s arms —
`Agent` allowed, headless aborted, everything else back to the prompt with
`eut`'s "/compact" copy.

| test | plant that turns it red |
|---|---|
| `an_over_long_transcript_falls_back_to_the_prompt_not_a_deny` | return a fail-closed deny from the `TranscriptTooLong` arm |
| `an_over_long_transcript_still_lets_the_agent_tool_through` | same plant |
| `an_over_long_transcript_aborts_a_session_that_cannot_prompt` | same plant |
| `loop_llm::tests::a_context_overflow_is_a_transcript_too_long_not_an_outage` | route the `TranscriptTooLong` transport error back through `unreachable_classifier` |

Not ported: the `q$t` parentheticals beyond `" (timed out)"` (this build's
transport carries no HTTP status), `$7t`'s six server-verdict arms (there is no
server classifier here), and the `type:"other"` decision-reason tag — the port's
prompt fallback types it `"classifier"`.

### 7.4 Still open

Divergences 1, 2 and 4 above are unchanged. 2 (`fd`'s `plan && xS()` arm) is the
largest: it needs a session-level auto-active latch AND `prePlanMode`, neither of
which this port has — plan exit here does not return to the pre-plan mode at all,
so the latch has nowhere to live yet.

---

## 8. The classifier's OTHER consumer: the subagent handoff review

`dKo` is not the only caller of the two-stage classifier. `EZe` is the second,
reached from the subagent-completion paths:

```js
async function EZe({agentMessages:e,tools:n,toolPermissionContext:r,…,handback:M}){
  if(r.mode!=="auto"||M==="send"||M==="flagged")return null;
  if(M==="withheld")S=void 0;
  if(!x$n(e,n)&&!S?.trim())return null;
  let U=await bke(e,A$n(S,{severity:F!==null}),n,r,s,{isSubagentLoop:!0,severityEligible:!0,severitySite:$pr,…});
  …}
```

and its verdict is PREPENDED to what the parent model reads:
`cu.content=[{type:"text",text:xg.warning},...cu.content]`, on both the async-
and sync-agent completion paths.

⚠️ Scope of that claim: `mode !== "auto"` is `EZe`'s OWN gate, which is the half
that matters against a port gate that can never open. The two call sites add
conditions of their own (`if(!pe()){if(F){…}}` on the async path) that this pass
did NOT resolve — the locals are minified and the names are reused across
scopes in that chunk. So: every auto-mode handoff is admitted by `EZe`; whether
every completion reaches `EZe` is unverified.

**In this port it never runs, and the reason recorded in the code is stale on
both halves.** `tools/agent/src/classifier_handoff.rs` gated everything on
`feature('TRANSCRIPT_CLASSIFIER')` behind an OFF-by-default env var, and
documented the result as "a faithful Rust port is a NO-OP on the common path".

1. **The flag no longer exists.** `TRANSCRIPT_CLASSIFIER` is 0 hits across all
   1697 chunks of 2.1.270. `EZe`'s only gate is `mode !== "auto"` plus the
   `handback` kind, so every auto-mode handoff is admitted to the review. This
   is the
   [[a-graduated-rollout-flag-leaves-the-gate-off-forever]] shape again: the
   port's gate cannot open, and its tests stay green because they assert the
   OFF default.
2. **The classifier it was waiting for is here.** The module calls the
   two-stage classifier "a LARGE deferred subsystem with no Rust analog"; that
   is `permission::loop_llm` plus `SessionLoopClassifier`, landed in
   `af24c7042`. `EZe` calls the same `bke` the tool path calls, differing only
   in its options.
3. **It was never wired at all.** `classify_handoff_if_needed` had ZERO call
   sites repo-wide — the env flag was not even the first thing stopping it.

The copy had drifted too: 2.1.270 spells it `subagent` throughout where the
port pinned `sub-agent`, the unavailable warning is now a builder
(`kae(model, httpStatus, errorKind)`) rather than a fixed string, and there is a
third outcome (`kind:"refused"`) the port did not carry.

Landed here: the module now holds the 2.1.270 copy byte-for-byte — including a
test that no string says `sub-agent` — the dead flag and the always-`None`
function are gone, and the module doc states the gap instead of denying it.

### 8.1 The wiring, landed

The three blockers listed here resolved once the crates they cross were clean:

1. **The seam.** `LoopPermissionClassifier` gained `classify_handoff(transcript,
   final_text)`, defaulted to `Pass` so a host that binds only the tool-call
   classifier keeps today's behaviour. The transcript crosses as a PATH, not as
   messages, so the reading and rendering stay in the orchestrator, where the
   file-IO helper (`auto_mode_io::secure_read_capped`) and the transcript
   renderer already live.
2. **The path from the agent tool.** `PermissionGate::review_subagent_handoff`
   (default: review nothing) returns a `HandoffReview` —
   `Flagged{reason}` / `Refused` / `Unavailable{model,detail}`, `EZe`'s
   `{warning, kind}` minus the copy, which stays with the agent tool that
   renders it.
3. **The transcript.** `SubagentSpawner::transcript_path` already existed; the
   JSONL-to-messages recipe is the one `tasks/handlers/human_resume.rs` uses.

`PolicyPermissionGate` applies `EZe`'s two port-reachable gates — `mode ==
"auto"`, and "nothing to review" when there is neither a transcript nor a
hand-back — before paying for a round-trip. The `handback` kinds
(`send`/`flagged`/`withheld`) have no port counterpart. Whatever the review
concludes, the work is still DELIVERED: this path returns copy to prepend,
never a denial.

| test | plant that turns it red |
|---|---|
| `a_flagged_handoff_review_is_prepended_to_the_subagent_result` (end to end, through `AgentTool::call`) | disable the call site → `content` comes back `[]` |
| `a_handoff_review_maps_each_verdict_to_its_own_warning` | — all four arms in one test |
| `only_auto_mode_reviews_a_handoff` | asserts the CALL COUNT is 0, not just the outcome |
| `a_handoff_with_nothing_to_review_skips_the_round_trip` | same |
| `a_child_transcript_renders_the_reviewable_calls_only` | the render is what the review judges; a torn last line must not cost the verdict |
| `a_hand_back_cannot_forge_the_tags_around_it` | `Qk` — agent-authored text must not break out of its own fence |

Still not ported: `$mt(wke().value, $pr)`'s severity site (a remote config, so
only `dVo`'s non-severity instruction is reachable), the `handback` kinds, and
`kae`'s parameterised arm — the verdict does not carry the model or failure kind
back, so the port renders `kae("")`, the same arm upstream's own `.catch` sites
take.

---

## 9. The bundled prompt: what is verifiable, and what is not

`permission/src/bundled/auto_mode_270_system.txt` (129 KB) decides every
auto-mode verdict this port makes. It arrived with the classifier substrate in
`af24c7042` carrying **no capture note**, and its name asserts more than the
binary can confirm.

Measured against 2.1.270 — the 207 MB Mach-O and all 1697 `// @bun @bytecode`
chunks, with `\uXXXX` / `\xXX` escapes decoded before comparing:

| | lines |
|---|---|
| total lines in the bundled prompt | 294 |
| comparable (≥40 chars, ignoring short/blank) | 218 |
| **present in the binary byte-for-byte** | **85** |
| **absent from the binary entirely** | **133** |

The 85 are the whole framing: the role paragraph, the transcript / `{"meta":…}`
/ outcome-line / host-context reading rules, and the numbered user-intent rules.

The 133 are the HARD BLOCK list, the SOFT BLOCK list, the ALLOW exceptions, the
Definitions section, and the Environment slot defaults — i.e. the rules
themselves. They are not escaped, not compressed, and not in some chunk the
split missed: `grep -a -F` straight over the 207 MB executable finds

```
Logging/Audit Tampering        0        You are a security monitor…   1
Toolchain Bootstrap            0        Data Exfiltration             3
Unverifiable Deletion Target   0        HARD BLOCK                    9
sh.rustup.rs                   0        Err on the side of blocking   3
```

— the controls hit, the rule bodies do not.

**So 2.1.270 does not ship its rule bodies locally.** It assembles the prompt
from a policy it receives, which is what the `<user_*_to_replace>` slot
machinery and `_mt`'s `<cc_automode_permissions>` wrapper exist for. This port
has no such server, so bundling a policy is not optional — but the bundled
rules cannot be called byte-aligned with 2.1.270, because 2.1.270 has no local
bytes to align them to. That is a **divergence by necessity**, not a defect, and
the only thing wrong with it was that nothing said so.

Also corrected here: `default_system_is_byte_identical_and_local_slots_preserve_defaults`
read as a claim about the oracle and is not one — it compares the template's
default rendering against the shipped default, both of which live in this repo.
Renamed, and joined by `oracle_verified_framing_is_still_in_the_bundled_prompt`,
which pins six framing anchors that occur exactly once in the raw executable and
exactly once in the bundled file. The rule bodies deliberately get no anchor:
there is nothing to pin them to, and copying one out of the file under test is
the tautology the test exists to avoid.

### Method notes — three instrument failures on the way to those numbers

Worth recording, because each one produced a confident wrong number first:

1. **Escapes.** A first pass decoded only `\n` and reported 6.9% coverage. The
   binary stores every em dash as `\u2014`; nearly every prose line in this
   prompt contains one. Decoding `\uXXXX` / `\xXX` moved it to 39%.
2. **Haystack too small.** Searching only the chunk that holds `dKo` looked like
   it might be the cause of the remaining misses. Re-running across all 1697
   chunks gave the identical 39%, which is what made the result trustworthy —
   the comparator printed its own coverage counts both times.
3. **Render-vs-source.** Some misses were `- **Slot**: value` lines that the
   binary stores without the list marker. A probe that strips `- ` and falls
   back to a 90-char middle slice moved exactly one line, which is what ruled
   this out as the explanation rather than assuming it either way.

The rule-body absence survived all three, and then survived a fourth check
against the raw executable with no extraction step at all.

---

## 10. Not this subsystem: the cron UI's hardcoded strings

A `/code-review max` pass over this work also flagged ~75 hardcoded English
strings in the iOS/Android cron UI. They are recorded here only so the finding
is not lost, because they are **not on main and not this session's files**:

```
git show HEAD:clients/android/.../cron/CronScreen.kt | grep -c 'Text(\s*"\|text = "'   →  1
the working copy of the same file                                                    → 15
```

Thirty cron files are dirty. The review counted against the working tree, so the
strings are inside an in-flight rewrite.

**That rewrite is ownerless.** Its author's session ended; all five live sessions
have disclaimed it. The uncommitted cluster is the `/loop` + wakeup-scheduler
work (`run_queued_batch`, `stop_dynamic_loop`, `runtime_wakeup.rs`), cron
automation (`AutomationRunStatus`, `claim_automation_run`, `CronTask.automation`),
`cron_native.rs`, the cron UI strings below, and the `taskOnly` pair in
`CronModels.swift`. It is preserved at `refs/recovered/working-state-2026-09-13`
(`8d2e3c676`) so it cannot be lost with the worktree.

⚠️ A correction to an earlier version of this note: it named `cdf462b27`
("Collapse a quiet /loop streak…") and `e0530991e` as pointing at the owner.
They do not. Every commit in this repo carries the same git author, so a commit
trail identifies the WORK, never the session — and routing on it sent this
finding to a session that had never touched a cron file. Four sessions routed it
to that same session for the same reason, compounded by a first-person "blocked,
to be committed later" note in the project-shared memory directory, which reads
as your own backlog unless you check `originSessionId`.

Four hazards for whoever does it, each confirmed from two independent records:

1. `clients/translations/*.json` is the SOURCE. The iOS `.xcstrings` and Android
   `strings.xml` catalogs are GENERATED — editing a catalog directly is
   overwritten.
2. iOS keeps placeholders in the KEY and Android in the VALUE, so one visible
   string with an interpolation commonly needs TWO keys.
3. The generated artifacts can drift AHEAD of the source. A previous pass ran
   the generator and it DELETED five strings that were in use; diff the
   generated output before committing it.
4. The Android tests that pin those strings are INSTRUMENTED. `./gradlew test`
   never runs them, so a green JVM run says nothing about whether the catalog
   still resolves.

### 10.1 A trust-gate hole in the same cluster — carry this when you land it

The review pass also found a workspace-trust bypass in that automation work.
**It is not on main and main is not vulnerable**: every symbol on the path is
absent from HEAD —

```
mod cron_native            0 files at HEAD
set_automation_firer       0
should_start_native_scheduler  0
dispatch_automations       0
NativeCronFirer            0
cron/src/automation        0
```

It lives entirely in the unshipped cluster, so there is nothing to fix here and
nothing was committed for it. It is written down because the fix currently
exists only in a working-tree copy of an UNTRACKED file, which the recovery ref
predates — landing the cluster without this would land the hole.

**The defect.** `should_start_native_scheduler` requires only
`config.host_workspace_trusted.is_none()`, so the native automation firer is
attached even when the user DECLINED the trust dialog; and `CronScheduler::tick`
calls `dispatch_automations()` before its own `!session_cron_enabled` early
return, so that function is reached with no trust gate of its own. Cloning a
hostile repo that carries `<repo>/.lingxi/scheduled_tasks.json` with an active
`automation` entry and a due cron then runs the attacker's prompt, with tool
use, on the first tick — after the user said no.

**The fix, for whoever lands it.** Gate the EXECUTION edge, not the wiring —
`NativeCronFirer::fire_automation`, before handing the run to the supervisor:

```rust
if let Some(orchestrator) = self.current.upgrade() {
    if !orchestrator.workspace_trusted().await {
        return Err("paused:Trust this workspace before running scheduled tasks".into());
    }
}
```

Gating the wiring instead would be wrong twice over: it cannot see a trust
decision made mid-session, and it leaves `dispatch_automations`'s own path
ungated. The `paused:` prefix matches the bridge's `run_scheduled_turn`, so the
run is RETRIED once trust is granted rather than retired.

## Test state

`cargo test -p permission --all-features`: **1640 unit + 22 auto-mode-scope + 10
further binaries, 0 failed.** `cargo test -p tool-agent --all-features`: 178
passed / 0 failed.

`permission/tests/auto_mode_classifier_scope.rs` now holds 22 tests. Every guard
across §1, §2, §7 is red-proofed by planting the old behavior back and checking
that the failure NAMES the right test — a guard whose plant survives is not a
guard:

| plant | goes red |
|---|---|
| restore the three-tool classifier gate | 4 tests |
| disable the `Sft` fast path | `sft_safe_allowlist_answers_without_the_classifier` |
| disable the acceptEdits probe | `accept_edits_simulation_answers_edits_and_file_shell_commands` |
| drop `apply_auto_mode_restrictions` from `rule_is_available_in_mode` | `…does_not_honour_a_suspended_dangerous_allow_rule` |
| drop it from the loop-tool carve-out | 2 tests |
| put `record_auto_deny` + `trip` back in the `NoVerdict` arm | `a_classifier_that_gives_no_verdict_never_trips_the_denial_breaker` |
| restore `if ctx.hook_ask_floor { None } else { … }` around the classifier call | 2 hook-floor tests |
| return a fail-closed deny from the `TranscriptTooLong` arm | 3 transcript tests |
| route the `TranscriptTooLong` transport error through `unreachable_classifier` | `a_context_overflow_is_a_transcript_too_long_not_an_outage` |
| edit one framing sentence in the bundled prompt | `oracle_verified_framing_is_still_in_the_bundled_prompt` (naming the anchor) |

Two premise tests carry their own weight, because "it did not fire" is only
evidence if the mechanism could have fired: `a_classifier_that_blocks_does_trip_the_denial_breaker`
proves the counter is live on that path, and `a_hook_ask_floor_still_discards_a_classifier_allow`
proves the classifier ran under the floor at all.

## Two red tests that are NOT from this work

Both live in files this pass never touched, and both are caused by another
session's uncommitted edits in the shared checkout:

* `orchestrator::handle_impl::tests::hot_resume_restores_compaction_visibility_and_deferred_tools`
  — `git diff orchestrator/src/handle_impl.rs` is a one-line change to the
  `restore_effort_from_resume` guard.
* `engine-desktop` doc-test `DesktopConfig (line 5754)` — the example is missing
  the `host_workspace_trusted` field, which exists only in the working copy
  (`git log -S` finds no commit for it).
