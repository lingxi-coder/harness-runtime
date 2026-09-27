# Brand baseline migration inventory

This extraction retains the original strict scanner and existing unresolved findings. It does not regenerate the baseline from the new tree.

## Comparison

The original scanner, baseline, and frozen identities were read from the LingXi source commit `01dcc428ba0c12522f6ee29b6474f3a4e0891c47` and run against the source repository with explicit `--root`, `--baseline`, and `--frozen` arguments. That run reported 86 new keys. The four mobile script sources were additionally scanned from their original Git versions because their working-tree copies had become downstream forwarding wrappers.

The first upstream scan reported 126 added keys:

| Classification | Keys | Treatment |
| --- | ---: | --- |
| Pre-existing runtime-owned findings | 82 | Remain failures; not added to baseline |
| Existing baseline keys relocated to repository root | 2 | Map `lingxi-code/Cargo.toml` to `Cargo.toml` for G1 `LingXi`; retain root `.gitignore` G3 `.lingxi` |
| Previously ignored, required source-policy resources now tracked | 40 | Explicit inventory additions below |
| Newly introduced branded build/test output environment variables | 2 | Rename new variables to `HARNESS_RUNTIME_BUILD_ROOT` and `HARNESS_RUNTIME_TEST_OUTPUT_DIR` |

The 40 resource entries are exactly ten files times these four rule keys: G1 `LINGXI`, G1 `\.lingxi`, G3 `.lingxi`, and G3 `LINGXI.md`. The resources are required template assets, previously omitted by the source `.lingxi` ignore rule. Each imported file is byte-identical to its original source file. Hashes also match the matching family record in `docs/local-apps/harness/template-migration-manifest.json`; plugin files are exact mirrors of the same source templates. No scanner exclusion or resource byte change was made.

For each path below, its original source path is obtained by replacing the leading `crates/` with `lingxi-code/` in the LingXi repository.

| Imported path | Bytes | SHA-256 |
| --- | ---: | --- |
| `crates/local-apps/templates/runtime-profiles/babylon-3d/r4/.lingxi/source-policy.json` | 820 | `2a85b55bed99a406764896b606b35088a75116124cb2320fd6bbadcb2bf8d922` |
| `crates/local-apps/templates/runtime-profiles/canvas-2d/r4/.lingxi/source-policy.json` | 790 | `90353becd31ffeca67312ee003b3bd993adc90a2638db1f74a18eb33e9bf048f` |
| `crates/local-apps/templates/runtime-profiles/phaser-2d/r4/.lingxi/source-policy.json` | 819 | `30166b86b60c448e6b2e5756a69ab6988d5d90d483961e85aef9f10aedfef42f` |
| `crates/local-apps/templates/runtime-profiles/react-dom/r4/.lingxi/source-policy.json` | 765 | `8e22f2ddc6ecb3ee58a398cc4d841df4a85ef888b272547947d90ad0895841dc` |
| `crates/local-apps/templates/runtime-profiles/three-3d/r4/.lingxi/source-policy.json` | 790 | `90353becd31ffeca67312ee003b3bd993adc90a2638db1f74a18eb33e9bf048f` |
| `crates/plugins/lingxi-local-app/assets/templates/babylon-3d/r4/.lingxi/source-policy.json` | 820 | `2a85b55bed99a406764896b606b35088a75116124cb2320fd6bbadcb2bf8d922` |
| `crates/plugins/lingxi-local-app/assets/templates/canvas-2d/r4/.lingxi/source-policy.json` | 790 | `90353becd31ffeca67312ee003b3bd993adc90a2638db1f74a18eb33e9bf048f` |
| `crates/plugins/lingxi-local-app/assets/templates/phaser-2d/r4/.lingxi/source-policy.json` | 819 | `30166b86b60c448e6b2e5756a69ab6988d5d90d483961e85aef9f10aedfef42f` |
| `crates/plugins/lingxi-local-app/assets/templates/react-dom/r4/.lingxi/source-policy.json` | 765 | `8e22f2ddc6ecb3ee58a398cc4d841df4a85ef888b272547947d90ad0895841dc` |
| `crates/plugins/lingxi-local-app/assets/templates/three-3d/r4/.lingxi/source-policy.json` | 790 | `90353becd31ffeca67312ee003b3bd993adc90a2638db1f74a18eb33e9bf048f` |

Verified remaining added keys after these migration-only corrections: **82**, all also present in original-source scanner results. The final gate still reports **114** missing historical baseline keys and exits 1. Historical missing baseline entries (including the previous r1 template inventory) remain visible; they are not removed by this migration. Passing the migration comparison is not a claim that the full brand gate is green.
