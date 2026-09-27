# PS-CALLER-06-2 / 06-5 — deferral re-evaluation, 2026-07-25

Both carry an in-code `OUT OF SCOPE, deferred` placeholder in
`permission/src/powershell_containment.rs`, and both are marked
`security_sensitive: true, impact: under-ask` in
`backlog-retriage-215-2026-07-20.json`.

They are not equally justified. One deferral holds and should be reworded; the
other is **mis-scoped and should be re-filed**, because the gap is wider than
the item says.

## PS-CALLER-06-5 — IMPLEMENTED 2026-07-26 (this section is the prior verdict)

The deferral is no longer in force. The reasoning below was sound about the
oracle's OS gate but drew the wrong conclusion for a port that SHIPS Windows:
"the oracle does not evaluate it on macOS/Linux either" makes omitting it
harmless on macOS and Linux, and says nothing about Windows, where the check is
a live security ask and the port had none.

Implemented in `permission/src/powershell_containment.rs`, gated the same way
the oracle gates it. Two decisions worth carrying forward:

- `cfg!(windows)` (a runtime constant), NOT `#[cfg(windows)]`, so the code is
  compiled and type-checked on every platform rather than only on the one
  nobody develops on here.
- The gate is injectable in tests (`with_windows_host`), so the WIRING is
  tested on macOS too. Otherwise the predicate would be tested everywhere and
  its entry point nowhere — the shape of gap this backlog keeps finding.

Cross-compiling `permission` to `x86_64-pc-windows-msvc` is NOT possible on
this host: a transitive C dependency (`ring`) needs an MSVC toolchain. That is
a toolchain limit, not evidence the code is Windows-clean, and it is why the
`cfg!` + injectable-gate choice above matters.

## PS-CALLER-06-5 — deferral HOLDS (PRIOR VERDICT, superseded)

PowerShell 5.1 resolves commands cwd-first, so an earlier sub-command that
writes `./foo.*` can shadow a later `foo`. The oracle asks about it.

The reason it is safe to defer is stronger than "out of scope": **the oracle
itself gates the check on runtime OS** —
`if (Dt() === "windows" && u.length > 1) { … }`. On macOS and Linux the oracle
never evaluates it either, so a port that omits it is behaviourally identical
on every platform LingXi currently ships. It is a genuine Windows-only gap, not
a silently-skipped guard.

Action: keep deferred; replace the placeholder text with the real reason
(oracle gates on `Dt()==="windows"`), so the next reader does not have to
re-derive it.

## PS-CALLER-06-2 — deferral does NOT hold; the item is mis-scoped

The check asks before running git commands in a directory carrying bare-repo
indicators (`HEAD` / `objects` / `refs` outside a `.git/`), or where `.git` is a
file/symlink redirecting somewhere that cannot be canonicalised as safe.

**Why it matters.** Git will treat such a directory as a git dir and run
**config and hooks from it**. An untrusted archive — a cloned repo, a
downloaded tarball, a fetched dependency — can plant those files and get
arbitrary code execution the next time any git command runs there. That is why
the oracle requires approval first.

**Why the filing is wrong.** The item was raised against the PowerShell caller
battery because that is where the reviewer found the placeholder. But the
oracle applies the SAME probe on the **bash** path: the shell battery at binary
offset `234532392` contains

```js
let c = a && R3r();   // a = "some command in this line is git"
if (c) return { behavior: "passthrough", message: c === "bare-indicators" ? … : … };
```

sitting directly among the `&`-defers-execution, bare-assignment,
unquoted-variable-expansion and UNC-path guards — i.e. the primary Unix path,
not a PowerShell corner.

**And the port has it on neither.** Repo-wide over `permission/src`:

| probe string | hits |
|---|---|
| `bare-repo indicators` | 1 — the deferral comment itself |
| `HEAD/objects/refs` | 2 — both doc comments |
| `run config/hooks from here` | 0 |
| `redirects to a location` | 0 |

and there is no `bare_repo` / git-dir-probe function anywhere. So the guard is
absent on the platform LingXi actually ships, and the backlog records it as a
deferred PowerShell edge case. Deferring the PS battery was a correct scoping
call for that wave; what went wrong is that **no bash-side item was ever
filed**, so the wider gap inherited a narrow item's justification.

Action: re-file as a bash + PowerShell item and schedule it. It is a live
under-ask, not a Windows-only nicety.

## What implementing it costs

The oracle's probe (`R3r`, definition at `226700069`) is roughly 200 lines and
security-critical in both directions — too loose and the guard misses a planted
repo, too tight and every worktree prompts. It needs:

- cwd realpath + NFC-normalise + workspace containment (`K4`);
- `.git` **file** and **symlink** redirect resolution, each canonicalised, with
  the "cannot canonicalise ⇒ plantable" and "resolves outside a `.git/` path
  segment ⇒ plantable" rules, plus an oversize/NUL-byte refusal;
- WSL/UNC handling (`//wsl$/`, `//wsl.localhost/`) including the cross-OS case;
- validity of a real git dir: `HEAD` is a regular file ≤4096 bytes matching
  `^ref:[ \t]*refs/` or a 40/64-hex oid, `objects` and `refs` are directories
  with `X_OK`;
- the `commondir` exclusion — its presence means a linked WORKTREE, which is
  legitimate and must NOT prompt;
- `git_bare_repo_gate` telemetry with the oracle's reason values
  (`gitdir_target_uncanonical`, `gitdir_target_plantable`).

Two call sites: bash battery (`passthrough`) and PowerShell battery (`ask`) —
the behaviours differ and both are byte-specified in the audit.

Estimate: M–L, and it wants its own wave with adversarial tests (planted
indicators, symlink redirect, `.git` file redirect, legitimate worktree,
legitimate nested repo) rather than being bolted onto an unrelated change.
