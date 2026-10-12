# Bash sandbox prompt oracle — Claude Code 2.1.295

The checked-in fixture [`native_sandbox_instructions_295_goldens.json`](../../../crates/orchestrator/tests/fixtures/native_sandbox_instructions_295_goldens.json) contains five source-oracle outputs consumed by `native_sandbox_attachments.rs`. Expected values were generated from the bounded `cmn()` renderer and source-bound pure helpers, never from the Rust renderer.

The Native source is `/Users/luolingfeng/.local/share/claude/versions/2.1.295`, SHA-256 `0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70`. `cmn()` is at byte offset `3530601` in `src_194447854.js`, SHA-256 `ae64b2c8c87be0e990738da478adbbe5cd6a8201cef7953d7684cf59bd780120`. The independently run source extractor and golden generator are retained in the R31 acceptance workspace at `acceptance/source-alignment-295-r31/sandbox-source-oracle/run.mjs`; it verifies these source-bound helper hashes before evaluating the function:

| Source behavior | Extracted source | SHA-256 |
| --- | --- | --- |
| Sandbox filesystem policy helpers | `src_191362209.js` | `2feb2c7861c6f63d9839c2d9113ca5c8a69ccf369561f18eec0988384ea61943` |
| JS process-environment truthiness | `src_186092222.js` | `aacaa8fe4c03be1a305899c7c9cab668105e8fa89dac7450a3361c43431bf2cb` |
| Subprocess scrub policy | `src_188508971.js` | `c685a2dfd00fecdd0a777d544e9594bcb380d21e037f2c95d70a04a24f5b30be` |
| `/sandbox` command-change helper | `src_188693222.js` | `681f216602a81e1bd7b8891f3a679dcf6ff837a3539b737183165565369917dd` |
| HTML/control escaping | `src_190627245.js` | `2ff6c9f0d2bebea8d34132841b8af36ff0ad2f4f330fd2d1f43e58cb1c58e5bd` |
| JS-compatible unique filtering | `src_186447803.js` | `988fa7a5b80a4c24398285288c666e7f95d8b0b90089d6c8e2215eadaef9c834` |

The oracle uses mocked Native host/config facts and does not initialize the Native bundle, launch an app, read an account, or make a network request. Its only output normalization is the approved product-name substitution `Claude Code` → `LingXi Code`. The cases cover strict boundary/bypass guidance, relaxed filesystem policy and interactive clipboard guidance, auto-mode network lists, allowed-host-only network behavior with a lone-surrogate path, and JS object-key ordering/number/UTF-16 stringification.

To refresh the checked-in fixture, run the external R31 source oracle and compare its output byte-for-byte with this file. Do not derive expected values from the Rust implementation.
