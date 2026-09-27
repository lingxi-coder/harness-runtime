# Handoff — adjudicating `dead_code`, 2026-09-17

Read this before you try to clean up `dead_code`. I tried, my measurement was
wrong, and it nearly made me delete live code twice. The method below is what
survived; the numbers I produced did not.

Everything here was checked against the tree at `d5856c80f`. Where a claim
could not be checked, it says so.

---

## 0. Where the counts in the source comments came from

Every crate root carries a scoped allow with a number:

```rust
// Dead code kept visible, not swept: this crate had N item(s) rustc could
// reach from nothing when the workspace was measured (2026-09-16).
#![allow(dead_code)]
```

Those N are from **one macOS run, lib targets only**. Section 2 explains why
that is not the same as "N items are deletable", and section 4 says what to
replace it with. **Do not treat those numbers as a work list.**

---

## 1. What IS settled

### 1.1 Roughly 81 items are already adjudicated — leave them alone

They carry a targeted `#[allow(dead_code)]` with a written reason, put there by
whoever last looked. Example, `orchestrator/src/turn_loop.rs`:

```rust
/// NOTE: Task 5 dead code — `FallbackTriggered` interception was removed; this
/// is business logic until then.
#[allow(dead_code)]
async fn reissue_after_model_fallback(…)
```

A blanket `RUSTFLAGS="--force-warn dead_code"` sweep **overrides these** and
re-surfaces decisions someone already made. If you force-warn, filter them out
first — walk up from the item over attrs/doc-comments looking for
`allow(dead_code)`.

### 1.2 The `platforms/posix` credential-fallback cluster is LIVE — do not delete

Nine items look dead on macOS:

```
factory.rs:29   FALLBACK_TOMBSTONE_KIND        factory.rs:197  runtime_fallback_allowed
factory.rs:31   DeferredPlainTextStorage       factory.rs:206  fallback_tombstone
factory.rs:37   its new/storage/…              factory.rs:217  is_fallback_tombstone
factory.rs:114  RuntimeFallbackStorage         factory.rs:417  fallback_directory
factory.rs:122  its new/contains_with_fallback/…
```

They are used at `factory.rs:478`, inside

```rust
#[cfg(not(target_os = "macos"))]
let fallback: Arc<dyn SecureStorage> = match policy { … }
```

so they are **live on Linux and Windows** and compiled out here. The item
definitions are unconditional; only their USE is gated, which is why a
"is there a `cfg` above the definition" heuristic does not catch them. It
didn't catch mine.

The macOS construction was removed deliberately by `eaa0d49b9`
("Prevent repeated macOS credential prompts with a signed broker"), whose own
directive reads:

> Keep the broker caller allowlist limited to signed Desktop and CLI
> identities; **do not add plaintext or legacy-keychain fallback paths**.

⛔ So: not dead, and on macOS not something to reconnect either. If you ever
"clean this up", read that commit first.

---

## 2. Why my measurement was wrong — read this before writing your own

### 2.1 `--all-targets` does NOT mean "counting tests"

`cargo clippy --all-targets` emits diagnostics **per target**. A function used
only by `#[cfg(test)]` code still produces a `dead_code` warning **for the lib
target**. Deduplicating by `(file, line)` keeps that warning, so the item looks
dead even though tests use it.

I built a two-run intersection (lib-only ∩ all-targets) and called the result
"dead everywhere". It proved nothing: both runs contain the same lib-target
warning. The 103 that came out of it is not a real number.

**How it bit**: I deleted six `apps/cli/src/commands/plugin.rs` wrappers
(`run_list`, `run_enable`, `run_disable`, `run_details`, `run_update_command`,
`run_prune_command`) after confirming the dispatcher calls the `*_with_bus`
variants instead. Six tests stopped compiling. `git grep -c run_list` had told
me "12 refs" and I had assumed those were the `_with_bus` spellings without
looking at one of them.

### 2.2 Counting references is not reading them

`git grep -c` answers "how many", which is not the question. The question is
"what are they" — a doc comment, a different symbol with a shared prefix, a
test, or a real caller. Every wrong call I made came from a count I did not
open.

### 2.3 You cannot classify platform-conditional items from one platform

See 1.2. This machine has only `aarch64-apple-darwin` plus two Android targets,
and `cargo check --target aarch64-linux-android` fails here (no NDK linker), so
non-Apple targets could not be measured at all. Any deletion decided from macOS
evidence alone is a guess about Linux and Windows.

---

## 3. Items worth a look (NOT verified — treat as leads)

These surfaced in the bad measurement. Each still needs section 4 applied. They
are listed because their NAMES suggest "built and never connected", which is the
defect class this repository keeps producing — not because I confirmed anything.

| Item | Why it is interesting |
|---|---|
| `platform-api/src/fusion.rs:1131` `publish_terminal` | a terminal-state publisher nothing publishes |
| `cost/src/tracker.rs:1070` `record_external_cost_snapshot` | external cost never recorded ⇒ `/cost` under-reports |
| `cost/src/attempts.rs:193` `submit_budgeted_attempt_receipt` | budget receipts never submitted |
| `session/src/record.rs:188` field `git` | git metadata written into the record, never read back |
| `configuration-admin/src/plugin_install.rs:682,798` `resolve_external_plugin_source`, `materialize_external_plugin_source` | installing a plugin from an external source |
| `apps/cli/src/respawn.rs:339,363` `queue_prepared_resume_if_matches`, `queue_resume` | resume queueing |
| `fusion/src/panel.rs:993` `run_panels` | ⚠️ 83 textual refs, mostly comments — exactly the count-vs-read trap |
| `apps/engine-mobile/src/authoring.rs` (8 items) | a whole QA/verification surface |

---

## 4. The method that actually works

Per item, in this order. Stop at the first step that answers it.

1. **Is it already adjudicated?** Targeted `#[allow(dead_code)]` with a reason
   above it ⇒ leave it. Someone decided.
2. **Read every reference, don't count them.**
   `git grep -n '\bNAME\b' -- '*.rs'` and open each hit. Classify: definition /
   doc comment / different symbol / test / real caller. `git grep -E`'s `\b`
   silently returns nothing — use `-P` when the boundary matters.
3. **Is its USE platform- or feature-gated?** Grep the call sites (not the
   definition) for an enclosing `#[cfg(…)]`. If any is, you cannot decide from
   one platform.
4. **Is it only reachable from tests?** That is a different verdict from dead:
   either make it `#[cfg(test)]` or leave it. Do not delete.
5. **Only then**: delete, wire, or add a targeted allow **with the reason**.
6. **Verify the delete**: `cargo build --workspace --all-features --tests`.
   Not `cargo check` — it is blind to test modules.

### What would make this tractable

Measure on Linux and Windows too and intersect with macOS. Until then the
honest scope is "items with no `cfg` anywhere in their reference set", which is
a much smaller list than any number in the crate-root comments.

---

## 5. What is NOT a task

- The 81 already-adjudicated items (§1.1).
- The `platforms/posix` fallback cluster (§1.2) — live elsewhere, and its macOS
  removal is a standing security decision.
- The crate-root `#![allow(dead_code)]` lines themselves. They exist so
  `clippy -- -D warnings` passes while the debt stays counted and per-crate.
  Deleting one is how you re-open that crate's list — that is the intended
  workflow, one crate at a time.
