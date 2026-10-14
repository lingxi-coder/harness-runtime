# Native Mods test entry, Claude Code 2.1.289

This note records the test-runner entry found in the pinned 2.1.289 binary. It
does not claim that the local Mods rollout is enabled or that native event,
result, or error bytes were captured.

The reference executable is
`/tmp/harness-claude-code-2.1.289-darwin-arm64`, SHA-256
`03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69`. The
extracted source manifest is
`/private/tmp/claude-code-oracle-2.1.289/relevant-src/capture.json`. Its
`src_197843226.js` chunk is pinned by SHA-256
`d0da516c1fa6ac395acd7210b50f182a7d4cf3dbb3deb44815726750432389c1` and binary
range `197843226..197890604`. The root Commander chunk
`src_196786288.js` is pinned by SHA-256
`2e61ffe78bf6a0303f974364ef3e75426bc285a4074e693b43d14696f536434b` and binary
range `196786288..196922063`.

The runner command is `claude plugin test <plugin-root>`. It recursively scans
for `*.test.ts` and `*.test.tsx` files, skips dot-directories and
`node_modules`, and refuses more than 1,000 discovered files. The help string
in the pinned binary says each test file imports the kit from
`claude-code/testing`. The runner creates that virtual module with these
exports: `describe`, `expect`, `mock`, `test`, and `tier`.

The runner source confirms the plugin fixture shape used by its harness:
`{ tier, plugins, options? }`, where `tier` is `prepend`, `user`, `append`, or
`builtin`, and each plugin entry is `{ name, tier, source }`. Inline plugin
source is normalized into an exported `register(on)` module; the source
normalizer accepts a `register(...)` function, a function declaration, or an
arrow-function form. A root plugin is validated as a directory and must resolve
a hooks module through `hooks/hooks.json`. The existing fixture declaration
`types/index.d.ts` only augments module `claude-code`; no declaration for
`claude-code/testing` was present in the fixture or pinned source capture. I
therefore record the confirmed import, export names, file pattern, and runtime
fixture schema, but do not infer the full test callback signature from absent
types.

I did not run a new test. The normal CLI root `preAction` starts by awaiting
`Det()` and `V3t()` under the `pre_action_mdm_keychain_ms` timing label. The
same pinned source documents `--bare` as skipping keychain reads, but the
observed `--bare plugin test` invocation returns `Unknown command 'test' in
bare mode`. A fresh `HOME` and `CLAUDE_CONFIG_DIR`, plus `env -i`, prove only
that the prior run did not inherit provider/API credential variables. They do
not prove that the macOS login Keychain was inaccessible or not read. The
prior non-bare run found no test files, did not load the registration module,
and captured/displayed no credentials; keychain access was not instrumented.

The existing 2.1.288 fixture still has no `*.test.ts` or `*.test.tsx`, and its
`hooks/register.js` contains model/provider operations and process, filesystem,
and environment side effects. I did not load it. Static `plugin validate`
success is source/manifest validation only. No rollout conclusion follows from
either that result or the no-test-file run.

If a later run is authorized in an isolated macOS account with no accessible
login Keychain or provider credentials, and a new side-effect-free fixture has
been prepared under the path below, the exact native command to run is:

```sh
env -i PATH=/usr/bin:/bin \
  HOME=/private/tmp/claude-mods-native-289-test/home \
  TMPDIR=/private/tmp/claude-mods-native-289-test/tmp \
  LANG=C.UTF-8 \
  CLAUDE_CONFIG_DIR=/private/tmp/claude-mods-native-289-test/config \
  /tmp/harness-claude-code-2.1.289-darwin-arm64 plugin test \
  /private/tmp/claude-mods-native-289-test/plugin
```

This command is recorded, not run. `--bare` cannot be appended as a workaround
because the pinned binary rejects `plugin test` in bare mode. The previous
raw CLI logs and environment manifests remain in
`/private/tmp/claude-mods-native-289.4vTTGc`; the machine-readable validation
receipt is [mods-native-2.1.289-validation.json](mods-native-2.1.289-validation.json).
