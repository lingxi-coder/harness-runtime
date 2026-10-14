# Native Mods acceptance preparation, Claude Code 2.1.289

This note separates prior static CLI attempts from a bounded, reviewable
enabled-Mod runtime run. The Native command ran only after the local fixture,
loopback stub, and source pins were ready. It passed a narrow `session.start`
hook and one print-mode model exchange; this file does not claim full Native
event or API parity.

The pinned executable is
`/tmp/harness-claude-code-2.1.289-darwin-arm64`, SHA-256
`03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69`.
Machine-readable source pins and attempt receipts are in
[mods-native-acceptance-2.1.289.json](mods-native-acceptance-2.1.289.json).

## What the executable can test

The installed-Mod feature gate is Native `Bwn()`:

```js
var _K="tengu_plugin_hooks_modules";
var IIe=!0;
var XD=()=>!Nt()&&k(_K,IIe);
var Gje=()=>XD()&&!Vr("hooks")&&!Sr();
var Bwn=()=>Gje()&&!Ab()&&!sy();
```

The exact source range is `186995853..186996283`, SHA-256
`e16734710a4789740915a939963a653009023728ada96fc451fc7a660831b0af`.
`--bare` sets the simple-mode condition `Nt()` and skips user-installed
plugin hooks, so a bare validation or run cannot establish that this gate
works. A non-bare run can still be refused by the rollout flag, hooks safe
mode, or managed policy. The harness leaves all three decisions to Native and
reports a refusal as a failed acceptance attempt.

`--print` supports `--plugin-dir`, `--output-format stream-json`,
`--include-hook-events`, `--no-session-persistence`, `--verbose`, and a positive
`--max-budget-usd`. The root Commander source is pinned at
`196786288..196922063`, SHA-256
`2e61ffe78bf6a0303f974364ef3e75426bc285a4074e693b43d14696f536434b`;
option-level hashes are listed in the JSON receipt. This gives one Native
session, one explicit local plugin directory, and one local model response.
The fixture's `session.start` hook writes a marker only inside its temporary
working directory. The fixed prompt and local reply make the provider request
synthetic and observable.

`claude plugin eval` is not the chosen first runtime gate. The captured help
output says that it launches full Claude children for cases, defaults to
with/without plugin arms when a plugin resolves, and can publish reports when
the account supports publishing. `--no-publish`, `--no-scaffold`,
`--mocks record`, one run, and concurrency one constrain that command, but it
still requires model responses. The prior eval invocation stopped at the
untrusted-plugin gate before it executed. Direct `--print` with a local model
endpoint is the smaller runtime check.

The telemetry helper source is `179441125..179441433`, SHA-256
`1d399b4e227889efaf535677cf7a7acb7f10ef254c6a98eaf8b5988e01ef492c`. It
returns `essential-traffic` when
`CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` is set, and otherwise returns
`no-telemetry` for `DISABLE_TELEMETRY` or parsed-true `DO_NOT_TRACK`. The
existing source-pinned GrowthBook analysis shows that non-default telemetry
mode disables its remote evaluation. These flags are not a general network
firewall; the proposed script separately routes the Anthropic API base URL to
a listener bound only to `127.0.0.1`, has no proxy variables, checks the API
key against one fixed dummy value, and never forwards requests.

The first explicit attempt showed another source-backed loopback request before
the model call. Native preconnect helper `C()` sends `HEAD
<ANTHROPIC_BASE_URL>/api/hello` in first-party mode when no proxy, Unix socket,
or client certificate is configured. Its exact range is
`195704072..195704507`, SHA-256
`5686da9d96f5cf21ccb26242895db1e9273cc4e4e106dc2511d492ee6d2dea92`. The
stub now accepts this one additional health route with an empty 200 response
and records it separately from the model request.

Normal startup also performs source-proven credential and managed-settings
lookups. The CLI main entry calls `Aeo()` (`196873198..196873234`, SHA-256
`62d6ef1797d4709fa27f4ad496391c2170ef529104dd862f983a18c468499ea1`). When
not in simple/bare mode, `Aeo()` runs two `security find-generic-password`
queries for legacy API-key slots (`181473311..181473870`, SHA-256
`f57bcfe5fed4550548d27be2bf84b2531b69e2889daac2486bf7c8ae69dd0fff`); root
preAction waits for that work through `V3t()` (`196841215..196841279`, SHA-256
`c8067967704b43d0eb446c9c875ad485296e48f98923209e603ff8e56a88ccd2`). We do
not stub `security`, capture its output, or dump credentials. The constructed
child environment supplies a dummy API key and a loopback base URL. Managed
settings are read normally and remain authoritative; the run does not alter
or bypass them.

## Reviewable run

The harness is
[mods_native_acceptance_289_loopback.mjs](../../scripts/tests/mods_native_acceptance_289_loopback.mjs).
Without `--execute`, it prints a review-only plan and does not spawn Native.
With `--execute <binary>`, it checks the binary hash, creates a fresh temp
plugin/config/session, binds a local Anthropic Messages stub, and invokes one
non-bare `--print` session. The stub accepts only `POST /v1/messages?beta=true` from
IPv4 loopback with the exact dummy `x-api-key`, plus the source-pinned
`HEAD /api/hello` preconnect. Every other peer, route, or credential is
rejected and recorded without exposing the supplied value. It does not connect
to any upstream service. Print-mode stream-json also requires `--verbose`. A
bounded result requires the health probe, one accepted local model request, a
zero exit code, the marker written by `session.start`, and the exact stub
response in stdout. The runner applies a 60-second deadline, signals the
detached Native process group with SIGTERM, escalates to SIGKILL after three
seconds, and closes remaining group processes and the loopback listener.

The script constructs an environment rather than inheriting one: `PATH`,
`TMPDIR`, `LANG`, `CLAUDE_CONFIG_DIR`, the dummy `ANTHROPIC_API_KEY`, the
loopback-only `ANTHROPIC_BASE_URL`, and the three source-pinned telemetry
flags. It leaves `HOME` unset and passes no OAuth, provider, proxy, or account
credentials. The config, session files, raw output, and receipt stay in a new
temporary directory. Native's routine startup lookups remain enabled; no
credential output is captured.

The candidate fixture prepared for review is under
`/tmp/mods-native-acceptance-289-candidate`. Its manifest, hook module, and
working-directory marker contract are hashed in
`fixture.sha256.json`. The executable harness creates the same fixture from
its source constants in a fresh directory, so it does not depend on or mutate
that candidate copy.

Two initial harness runs failed before a complete model response: the first
omitted `--verbose`, and the second rejected Native's observed
`/v1/messages?beta=true` route. Both raw receipts remain in the temporary
directories named in the JSON file. The third run passed: Native listed the
temporary plugin in `system/init`, its `session.start` hook wrote the fixed
marker, the local stub accepted one Messages request with the exact dummy key,
and print output contained the stub's fixed reply. Its complete receipt is
`/var/folders/2c/gch8wj4s0d52rygkyd6b9f_h0000gn/T/mods-native-acceptance-289-X9iqMD/receipt.json`;
stdout was 4,720 bytes with SHA-256
`03c1d2a74217057ecaea8208ddfb9f7e51a88c15a5b9c77d45a2e3565e78fe07` and
stderr was empty. The provider endpoint was loopback; the stub did not relay.
This validates one real Native plugin load and one hook effect, not more.

## Earlier attempts retained

All prior raw outputs remain in their original temporary directories and are
summarized in the JSON receipt. Earlier attempts r1-r3 remain separate from
the bounded runtime harness. The first attempt used an explicit temporary
`HOME` and is superseded. The second used an isolated `CLAUDE_CONFIG_DIR` but
ran from the repository directory; `plugin eval` refused the untrusted fixture
before running, while its config directory received telemetry-failure queue
files. That queue state is not proof of network isolation. The third used a
fresh config, fixture working directory, no `HOME` override, and the three
telemetry flags: static `plugin validate` succeeded, while `plugin eval` again
stopped at the untrusted-plugin gate. None of r1-r3 ran a Mod hook or made a
model request. Their success/failure output is evidence about command gates
only, not runtime behavior.
