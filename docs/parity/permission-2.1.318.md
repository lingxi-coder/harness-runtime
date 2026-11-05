# Permission alignment with Claude Code 2.1.318

Audit date: 2026-11-04 (America/Los_Angeles).

## Baseline and evidence

Both the [official latest release](https://github.com/anthropics/claude-code/releases/tag/v2.1.318)
and the official npm `@anthropic-ai/claude-code/latest` metadata resolved to
2.1.318 during this audit. The npm package is a native-platform launcher, not
an independent JavaScript implementation.

The inspected macOS arm64 executable is 319,598,464 bytes, SHA-256:

```text
677a2ea7bf576cedd20e191d973914ed8a41796f53a63b1923381ddc4d5b5e7d
```

Oracles below read this executable directly, check its digest, and evaluate
selected native JavaScript functions with explicit dependencies. They do not
launch a model session, execute model-generated commands, or prove equivalence
outside their declared inputs. File offsets are zero-based byte offsets in
this exact executable.

The classifier policy was recovered from the embedded compressed asset, rather
than inferred from an older prompt. The loader module is at
284491807..284492157; `permissions_external-2ede28fe.txt.zst` occupies
315029580..315061280. The decoded policy is 96,102 bytes, SHA-256
`bfb494467ebd7003029b1d5da21f99a598a6238162d5b7933329abdf45625088`.
The oracle also applies the native policy container and framing functions.

REA static analysis of the extracted rule-parser and permission/configuration
chunks is retained as
`ev_889b17a509fefcdc2f8217232eb1f8ac72ca819c5c6a3013e704afaeb07f9726`.
Both chunks parsed; its semantic graph reached the 100,000-node ceiling and is
partial. This Evidence is supporting static inspection, not a whole-program
equivalence certificate. The scripts and fixtures are the reproducible local
behavioral evidence.

## Changes

| Surface | Result |
| --- | --- |
| Rule syntax and aliases | Match current malformed-parenthesis handling, `Task` → `Agent`, removed historical output aliases, and raw UTF-16 round trips. |
| Shell rule matching | Reject NUL, use JavaScript whitespace/prefix semantics, and retain wildcard behavior. |
| Shell read-only inference | Replay 74 current flag configurations and the active native getter/callbacks. Split shell words on Bash blanks/newlines, preserving Unicode whitespace and other control characters inside executable names. Apply Native's public unquoted-glob command allowlist before table dispatch, retaining quoted/escaped literals. Keep the native GH table inert; reject executable preprocessors, browser launches, and remote/configuration controls. |
| File rules | Match native non-Unicode case folding and character classes, including reversed ASCII range removal, empty classes from odd trailing backslashes and legacy control escapes as range endpoints; drop invalid allow expressions and preserve deny/ask literal paths. Merge overlapping source ranges, render compact folded ranges, reuse repeated classes and expand markers in one pass. Bound source, expansion and compiled-regex resources; exhausted patterns cannot grant access or retract an ask/deny. Normalize skipped gitignore patterns according to behavior and retain rooted physical twins for the session. |
| Read-deny search filtering | Ignore negated exclusions and preserve residual globs when the search cwd is inside a rule root. Before opening/listing each candidate, apply a live Read-deny snapshot with the same case folding and path-spelling checks as direct Read. Capture policy-root aliases and retain the requested search-root spelling alongside the canonical walk root, including symlink search roots. Keep the changed-file snapshot's original single-spelling check separate. |
| Path safety | Check all symlink spellings, match Windows/WSL colon semantics, expand removal targets before evaluation, and protect glob deletion of additional working-directory roots. |
| Parser exhaustion | Enforce time/node budgets and route exhausted parses to a fail-closed Ask instead of the unavailable-parser fallback. |
| Explicit Ask | Run the tool-local check: its Deny wins, and its richer Ask keeps `matchedAskRule`; do not let bypass, whole-tool grants, Auto fast paths, Hook allow, or Mod projection erase it. |
| Model/mode gates | Fold native model identities before eligibility checks; update current Haiku and built-in allowlist behavior. |
| Persistent bypass consent | Accept user/managed consent; invocation/project/local settings do not grant persistent consent. |
| MCP deny URLs | Check all proxy `mcp_url` upstreams as well as the direct URL. |
| MCP organization ceilings | Filter blocked tools from primary/subagent catalogs; an Ask ceiling forces a prompt and hides permanent grants. |
| MCP server policies | Separate server-policy rules from organization ceilings; preserve SDK and remote dynamic startup gates and the remote Ask-only bypass exception. |
| SDK parsing | Discard an invalid optional SDK `tools` field as native does; keep HTTP/SSE declarations strict. |
| SDK lifecycle | Publish the entire initialize policy batch before replying or starting handshakes, and preserve pending declarations through catalog refreshes. Atomically register or merge repeated SDK initializes under the same lifecycle lock, retaining first-registration options. Serialize permission snapshot capture/publication across all catalog and initialize producers, without deleting other rule sources. Retire pending declarations atomically with complete server removal so same-name registrations start a fresh policy lifetime. Agent-owned tools retain eligible SDK ask/deny in their own rule policy; child mode and the remote Ask-only bypass exemption apply without changing the parent bucket or organization ceilings. |
| Auto classifier | Replace the old policy/framing with the actual 2.1.318 asset; retain native ASCII XML parsing and strict first-stage Allow requirements. Determine stage-one refusal exemption after setting aside an unqualified fast Allow: yes/refusal still counts as Deny, while no/refusal or unparseable/refusal remains exempt when stage two has no verdict. Apply physical deadlines to non-stream requests and to stream headers, bodies, heartbeats and error bodies. Retry private body failures using the same budget as headers, discard failed partial output, and preserve terminal driver decisions such as `x-should-retry:false`. Ordinary streams retain their output replay guard. |
| TUI prompt intent | Start the quiet window after a successful terminal frame, anchor composer-release grace to its corresponding completed frame, and independently guard queued prompts. Escape retains its initial short grace without renewing or inheriting the selector's rolling refusal window. |

The explicit-Ask oracle uses native `Ybn` at 270619491..270620181. The
permission-prompt oracle executes the native input predicate at
296713746..296714577. These are different layers: a correct core verdict alone
does not establish that the terminal cannot accept stale input.

## Reproduction

Run the following from `harness-runtime`, passing the baseline executable as
the script argument:

```sh
node scripts/tests/permission_318_rules_oracle.mjs /path/to/claude-2.1.318
node scripts/tests/permission_318_guard_audit.mjs /path/to/claude-2.1.318
node scripts/tests/read_only_318_oracle.mjs /path/to/claude-2.1.318
node scripts/tests/permission_modes_318_oracle.mjs /path/to/claude-2.1.318
node scripts/tests/permission_318_ask_oracle.mjs /path/to/claude-2.1.318
node scripts/tests/permission_318_classifier_oracle.mjs /path/to/claude-2.1.318
node scripts/tests/mcp_318_policy_url_oracle.mjs /path/to/claude-2.1.318
```

The TUI oracle lives in the product checkout at
`apps/cli/tui/tests/oracle/permission_prompt_318.mjs`.

## Validation

All eight artifact oracles passed, including 1,911 model/mode cases, 4,509
read-only table cases plus 11 public glob-gate cases, 15 prompt-input cases
including Native's actual `Vk` frame callback, the complete classifier prompt,
14 classifier parser cases, 32 fast-path cases and three stage-one refusal
cases, and the declared rule, guard and MCP cases. The final npm and GitHub latest checks
still resolved to 2.1.318.

| Check | Result |
| --- | --- |
| `cargo test -p permission --offline` | 1,727 passed across unit/integration suites |
| Same suite with `--features bash-ast` | 1,909 passed |
| MCP registry unit suite | 727 passed in the preceding review-fix pass, including deterministic concurrency, slow-first-handshake, pre-registration policy and removal regressions |
| File tools and tool-MCP unit suites | 396 + 169 passed, including exact case folding, policy/search-root aliases, SDK child isolation, remote bypass and organization Ask composition |
| Real SDK/runtime MCP lifecycle and catalog tests | 35 passed |
| Runtime agent-scoped MCP builders | 2 passed, covering organization metadata and private SDK ask/deny enforcement |
| Runtime and CLI persistent-consent regressions | 1 + 1 passed |
| Organization Ask through Mod/Hook and prompt context | 1 passed |
| Real ApiService classifier deadlines and retry budget | Eight classifier-related tests and 13 retry-scope tests passed (one shared test). Coverage includes non-stream and two streaming protocols; headers/body/heartbeats/error bodies; mixed header/body budget exhaustion; discarded partial output; terminal HTTP retry declines with complete or stalled bodies; private-output versus ordinary replay guards. Three ordinary stream watchdog/admission regressions also passed. |
| TUI permission tests | 101 passed, including repeated Escape, the delayed-release frame and later-question cases |
| Product CLI compilation | `cargo check -p cli --offline` passed |
| Mobile runtime feature, macOS host type-check | `cargo check -p harness-runtime --no-default-features --features mobile --offline` passed |
| Changed Rust files and patch whitespace | rustfmt and `git diff --check` passed |

The third review-fix pass reran the full default/AST permission suites,
file-tool and tool-MCP unit suites, runtime agent-scoped builders and TUI
permission tests. The updated rule and prompt-input artifact oracles also
passed. Registry, general SDK lifecycle, classifier and the other six artifact
oracle results are retained from the preceding review-fix pass; consent,
Mod/Hook, CLI compilation and mobile checks are retained from initial alignment.

The five latest fixes cover a complete Default-mode Bash authorization for
Unicode executable-name separators, actual Grep/Glob calls behind the live
gate (with ASCII/Greek deny cases and Kelvin-sign/long-s non-matching controls),
requested symlink search roots, child SDK tool construction and permanent
grant/bypass boundaries, Native class-escape fixtures plus full rule precedence,
and repeated Escape. Native's complete Agent SDK lifecycle and complete search
tool were not executed; these two integrations are established by the product
regressions rather than claimed as whole-Native equivalence.

The Unicode regression's 320-UTF-16-unit repeated wide-class pattern now
expands to 352,128 regex bytes instead of 31,625,152. The isolated optimized
conversion/expansion probe took about 0.10 ms after warming the fold table;
the first single-class call, including table initialization, took 1.63 ms.
These are transformation measurements, not full regex compilation or tool-call
latency claims. The regression also compiles the actual folded regex, exercises
the public matching path, merges 2,048 overlapping ranges and checks exhausted
positive/negated allow/ask/deny behavior. Independent membership comparisons
against the previous enumerating algorithm agreed on 190 boundary cases.

The deep runtime test uses `RUST_MIN_STACK=16777216`, the same budget set in
the harness workspace's `.cargo/config.toml`. MCP callback tests were rerun
with local loopback binding available after the sandbox denied their mock
listeners. Initial intermediate failures are not counted as passes above;
the table records the successful final executions. Existing compilation
warnings remain outside this permission change.
Product checks used the checkout's existing local-path patches to this harness
and `llm-client`. Validation-generated lockfile changes were removed during the
initial run; later review-fix passes left both lockfiles unchanged from their
start, preserving the product's concurrent local-path lockfile edits. Product
dependency pins are integrated separately with clean release-lock resolution;
local-path patches remain confined to the working tree. The regression runs
did not install or update applications.

The read-only pipeline regression deliberately distinguishes a rule denial
from the offline classifier table. Native's read-only fast path precedes that
table: with no explicit Read deny or path policy, a pipeline of read-only
children does not reach classification. The hard-denial regression therefore
uses a real network-exfiltration shape, and the native read-only behavior has
its own separate regression; no extra sensitive-filename rule was invented
to make an obsolete test pass.

## Brand extensions and limits

Brand names, configured home directories, provider/model routing, Local App
host authorization, Workflow, and installed host-owned tools retain their
product behavior. These extensions must not weaken explicit deny rules or
organization ceilings. A retained product tool is not evidence that native
2.1.318 still advertises it: notably, native removed `TaskOutput`, and its old
Auto safe-list entry has been removed here.

The local default classifier prompt is covered. Cross-session host payloads,
non-default server policy overrides, inference-profile metadata, and real
remote connector identities require their owning host and are not established
by local injected probes. The terminal's native scroll/revealer exceptions
have no direct counterpart in the current TUI dialog. Platform-specific device
acceptance and arbitrary shell-parser equivalence are separate from the
bounded regression suite.

These boundaries must remain explicit: passing fixtures or retaining an old
comment marked “accepted divergence” does not justify a claim of complete
cross-platform equivalence.
