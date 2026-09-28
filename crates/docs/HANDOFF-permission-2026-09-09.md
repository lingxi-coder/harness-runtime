# Handoff — `permission` alignment vs Claude Code 2.1.263

Session ended 2026-09-09. Full audit report:
`lingxi-code/docs/permission-byte-alignment-2.1.263-2026-09-07.md`.
This file is the shorter thing: what was reviewed, what shipped, and what the
next person should pick up — with the reason each open item is still open.

---

## 1. What shipped

| commit | what |
|---|---|
| `7e475d02b`…`887d755d2` | P0 `blockReadsOutsideWorkingDirectories`, all surfaces; plus `H_n` / `OG` / `imr`. One wrong scoping claim retracted in the last of these. |
| `80578f31f` | `eO`/`sle` env-var safe list under the read block (prefix `Eun` + `env` command); de-duplicated the 39-entry table into one const; fixed a self-inflicted parallel-test flake. |
| `18acc0e53` | `Amo` — catastrophic removals hidden inside command substitutions (#7 `>64` bail, #8 possibly-empty variable path) + the missing `HL` circuit breaker on #6 + the `bun` read-only length bail. |
| `a97aab746` | Recorded why the last three curated items are not this crate's to patch. |
| `2934d12e7` | **Subagent declared-`bypassPermissions` clamps** (`bs(Rn)`'s two inner arms). |
| `b44f3b3c1` | Those clamps fed from both `PoolSubagentSpawner` composition roots. |

Test state at the time of writing: `permission` **1489/0** default, **1652/0**
under `--features bash-ast`; `agent` **20/20** on `permission_mode`.

---

## 2. 🚨 Read this before you touch anything

**HEAD was broken for a while on 2026-09-08/09 and it was not a merge conflict.**
Another session ran a directory-level `git add` and swept `agent/src/handle.rs`
into its own commit `e3092176f` **without** `agent/src/permission_mode.rs`. The
result was a five-argument call against a three-argument definition: the whole
workspace failed to compile, and the session that did it had no way to know.
Fixed by `2934d12e7`.

- **The detector is `git diff --cached --stat` BEFORE every commit.** `handle.rs`
  showed 161 changed lines where I had added 32. A row whose line count does not
  match what you wrote means somebody else has been in that file.
- **To commit only your own hunks** (`git add -p` is unavailable in this
  harness): `git show HEAD:<file> > tmp` → replay your edits against `tmp` using
  **content anchors, not line numbers** → `git hash-object -w tmp` →
  `git update-index --cacheinfo <mode>,<blob>,<path>`. The working tree keeps the
  other session's changes. Verify with `git diff --cached -U0 | grep '^@@'`.
- Re-take your anchors from the **current** HEAD each time; HEAD moves.

---

## 3. The subagent bypass clamps — what was actually wrong

Oracle `bs(Rn)`, chunk offset **164393262**:

```js
let as = Y6(je, on), So = as ?? e.permissionMode;          // `ye`, then `ve`
if (So && (as || Rn.mode!=="bypassPermissions" && Rn.mode!=="acceptEdits" && Rn.mode!=="auto")) {
  let ys = So;
  if (YYe() && !as && (So==="bypassPermissions"||So==="acceptEdits"||So==="auto"))
    warn(`Subagent declared permissionMode: ${So} inside a confined evaluation run; keeping parent mode '${Rn.mode}'.`), ys = Rn.mode;
  else if (So === "bypassPermissions") {
    let ws = ey(), _s = !1;
    if (ws || !1 || Rn.restricted) warn(`… not running in a contained no-internet environment …`), ys = Rn.mode;
  }
  Ar = {...Ar, mode: ys};
}
```

The port had the **outer** guard byte-accurate and **neither inner arm**. That
guard only suppresses the definition fallback when the parent is *already*
permissive — so a restrictive parent is exactly the case it lets through. A
discovered agent file with

```yaml
permissionMode: bypassPermissions
```

raised a `default` session's child to full bypass, including inside a confined
evaluation run.

**🚨 The refusal copy names a predicate that does not exist.** It says "not
running in a contained no-internet environment", but:

```js
function pAn(){if((bn()||{}).permissions?.disableBypassPermissionsMode==="disable")
  return "Bypass permissions mode was disabled by settings";return}
function ey(){return pAn()!==void 0}
```

`ey()` is only that settings bit; the middle disjunct is a constant-folded `!1`
(dead in this build). The two live conditions are
`disableBypassPermissionsMode === "disable"` and `--restricted`. **Resolve every
minified predicate in a refusal before trusting the sentence it prints.**

`YYe()` is the same confined predicate already ported for `OG` (allow-rule
filter) and `H_n` (hook-allow suppression) — this was its **third** consumer,
and missing it meant those two landed gates could be walked around by one
frontmatter line.

---

## 4. Open items

### 4.1 ✅ The two composition roots compile

`b44f3b3c1` was committed without ever being built. It has since been through
repeated `cargo build --workspace --all-features --tests` runs (2026-09-10) in a
private `CARGO_TARGET_DIR`; both roots type-check.

### 4.2 ✅ The `hooks` env flake is gone (`e1b411b7d`)

Both halves are fixed. `CLAUDE_CODE_EVAL_CONFINED` became a per-executor flag
(`with_eval_confined`) in an earlier session; the last `set_var` in that binary —
`LINGXI_SESSIONEND_HOOKS_TIMEOUT_MS` — is now `with_session_end_timeout_ms`.
`grep -c 'std::env::set_var' hooks/src/executor_test.rs` is **0**.

🚨 The conversion alone did NOT pin the new plumbing: a mutation that ignores the
override still passed, because the fixture declares no per-hook timeout so the
default is the 1500ms floor, and the assertion was `elapsed < 2s` — true either
way. That bound never proved the deadline VALUE was honoured. Tightened to 500ms,
which separates the configured 50ms from the floor.

### 4.3 ✅ ANSWERED — and it was not the symmetry (`ea711b1a0`)

The question was whether the boot-agent frontmatter fold should also check
`cfg.restricted` / `YYe()`, by analogy with the subagent clamps. The boot fold is
`Qu` (`src_173281385.js` @8342) and it checks **neither**:

```js
let b = !O_() && (kM() || Boolean(ne().bypassPermissionsModeAccepted)),
    k = m === "bypassPermissions" && !b ? void 0 : m;
```

`O_()` is the `disableBypassPermissionsMode` killswitch — which the port already
had — and `kM()` is `skipDangerousModePermissionPrompt` in any tier. So the
killswitch is only **half** the gate: bypass must also have been **earned**,
either by the user accepting the disclaimer once
(`bypassPermissionsModeAccepted`) or by a tier waiving the prompt.

Without that half, one frontmatter line in a discovered `.lingxi/agents` file
granted full bypass at startup to a user who was never asked — and agent files
arrive with a checkout. Both flags already existed in the port, wired only to the
background-session downgrade; upstream applies them regardless of session kind.
The predicate is `permission::boot_agent_may_adopt_bypass`.

⛔ Note for the next reader: ⚠️-shaped symmetry arguments ("this other seam gates
on X, so this one should too") pointed at the wrong predicate here. The fold was
worth reading in the binary precisely because the guess was plausible.

### 4.4 The three curated permission items (unchanged, each structurally blocked)

1. **`Sandbox: ignoring permission rules and sandbox.filesystem entries from
   disabled setting source ${src}`** — deliberately NOT done. (a) The oracle loads
   every source and filters at the sandbox fold; this port filters at **load**
   (`setting_source_flags`, `apps/cli/src/init.rs:352`), so the disabled source was
   never read. (b) It is a `{level:"info"}` debug sink and `apps/cli/src/lib.rs`
   has no structured logger at that seam (44 `eprintln!`, zero `tracing`), so the
   only available spelling would print a line the binary never shows anyone.
2. **Plan-mode approval / consent floor** (28 strings) — zero substrate; spans
   plan mode × artifacts × teammates × the auto-mode classifier, three of which
   this crate does not own. Plan it as a cross-subsystem feature, not a patch.
3. **Session-scoped `BK` allowances** (plan files, tool-result files, scratchpad,
   job `tmp/`, `Rzt()` bundled skill refs) — the crate has no session-directory
   plumbing. Their absence makes the read block **stricter**, never looser.

### 4.5 The remaining P2 tier — and 🚨 why its numbers are not a task count

MCP approval (13), remote-control / `--print` permission host (4), host-asserted
`classifierContext` (2). Both of the latter already have substrate
(`permission/src/host_context.rs`, `classifier.rs`, `policy_gate.rs`,
`mcp_policy.rs`), so those are upper bounds on *copy*, not on work.

🚨 **"343 further strings not yet themed" is NOT a backlog.** Same over-capture as
the retracted "434 absent strings": the extraction is keyword-driven and pulls
minified code. Re-running it on 2026-09-08 gave 574 "MCP" and 311 "subagent"
candidates, overwhelmingly code. **Verify an extraction's scope before quoting any
count from it.** Only the hand-curated P0/P1 sections were ever a real list.

### 4.6 Repo hygiene

`.git/gc.log` is present and every commit warns about too many unreachable loose
objects; auto-gc has stopped. `git prune` rewrites the object store, so it is not
safe while other sessions are reading and writing this checkout. Do it when the
tree is quiet.

---

## 5. Two method notes worth keeping

**Red-proofs without the build lock.** `effective_child_mode` is a pure function,
so the five mutation tests were run by concatenating hand-copied enum definitions
with the **verbatim** text of `permission_mode.rs` from `fn mode_rank` to EOF
(tests included) into one file and running `rustc --test --edition 2021`. Seconds
per cycle, no cargo, no lock. All five mutations — each arm disabled, the two arms
merged into one shared clamp, the `!as` exemption dropped, `Some(parent)` swapped
for `None` — were caught by the test written for them. This does **not** replace
compiling the real crate; it replaces the mutation loop.

**If you plant mutations in a shared checkout**, keep the pristine copy somewhere
durable (`~/.claude/...`), not in the scratchpad — that directory is wiped on
session restart. And do not let a Python harness buffer its stdout: when the
starved harness was killed, the first three mutation results were lost with it.

---

## 6. Where things live

- Audit report: `lingxi-code/docs/permission-byte-alignment-2.1.263-2026-09-07.md`
- Regression gate: `lingxi-code/scripts/checks/perm_verify_literals.py` (run from the
  **repo root**; `perm_extract_literals.py` hardcodes `lingxi-code/permission/src`
  and takes the OUTPUT path as `argv[1]`)
- Oracle binaries: `~/.local/share/claude/versions/{2.1.260,2.1.261,2.1.263,2.1.265}`.
  Split into chunks on `// @bun @bytecode` before grepping for function bodies.
  🚨 Non-ASCII is stored escaped and not uniformly (em-dash raw, `×` as `\xD7`).
