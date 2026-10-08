# Fixture provenance and copy map

The source acceptance tree is
/Users/luolingfeng/lingxi/.worktrees/claude-alignment-integration/acceptance.
It remains in place until the integration owner switches references. It is
provenance only; runners resolve inputs relative to their own script location.

| Original source | Portable destination or disposition |
|---|---|
| acceptance/current-matrix.md | current-matrix.md |
| acceptance/fixtures/commands/cases.json and run-native.py | fixtures/commands/cases.json and run-native.py |
| acceptance/fixtures/commands/*.stdout and *.stderr | Current fixtures/commands/native-baseline/ stdout/stderr are fresh 2.1.293 outputs from the portable runner; the original source receipts remain in acceptance/ and the source outputs are not relied on as the portable baseline. |
| acceptance/fixtures/commands/requests/* and sessions/* | Excluded from checked-in baseline: these raw Native artifacts include injected user-global instruction/account context. The fixture recreates them under ignored fixtures/commands/run-output/ for same-run comparison. |
| /private/tmp/mods-current-hook-native-regexp-cases-v1668/matcher-cases.json and inputs | fixtures/hook-regexp/matcher-cases.json and inputs/ |
| acceptance/fixtures/hook-regexp/{run-native.py,prepare_native_plugin.py,native-receipt.json,native.stdout,native.stderr} | Runner and corpus inputs are in fixtures/hook-regexp/; current native-baseline/ and event inputs are freshly captured with a fixed session ID. |
| acceptance/fixtures/mod-middleware/{plugin.json,hooks/*,middleware.test.ts,run-native.py,native-receipt.json,native.stdout,native.stderr} | Static plugin/test inputs are in fixtures/mod-middleware/; current native-baseline/ outputs and receipt are freshly captured by the portable runner. |
| acceptance/fixtures/mod-middleware/unregister-probe and native-baseline/unregister-probe | fixtures/mod-middleware/unregister-probe/ contains the rerunnable test input; captured stdout/stderr remain under native-baseline/. |
| acceptance/fixtures/mod-tool-register/{README.md,run-native.py,native-receipt.json,requests/*.tools.raw.json} | Runner and shared cases.json are in fixtures/mod-tool-register/; current native-baseline/ receipt, stdout/stderr and exact tools slices are freshly captured by the portable runner. |
| acceptance/fixtures/mod-tool-register/requests/*.request-*.json | Excluded from checked-in baseline: Native metadata.user_id may identify an account. Fresh full request bodies remain available only in ignored fixtures/mod-tool-register/run-output/; exact tools slices are preserved under native-baseline/requests/. |
| New provider-error public Native fixture | fixtures/provider-error/cases.json fixes the 2.1.293 HTTP 400 response bytes and message code units; fixtures/provider-error/native-baseline/ contains raw stdout, stderr, JSON stdout copies, response body bytes and a receipt. No full provider request or generated config is checked in. |
| /private/tmp/mods-current-native-293-session-append-v1628/{RECEIPT.json,hooks/register.js} | fixtures/session-append/native-plugin/{RECEIPT.json,hooks/register.js} |
| /private/tmp/mods-current-native-harness-append-gate-v1647/{native-append-byte-test-receipt.json,oracle-cases.json,NATIVE-PROVENANCE.json} | fixtures/session-append/harness-byte-gate/* |
| acceptance/receipts/{RECEIPT.json,NATIVE-PROVENANCE.json,native-append-byte-test-receipt.json} | provenance/source-acceptance-receipts/* and fixtures/session-append/harness-byte-gate/native-append-byte-test-receipt.json |
| Harness integration root M0-CHECK.txt | evidence/M0-CHECK.txt |

The historical Native executable was
/private/tmp/mods-current-native-293-reference-v1614/claude. Its version is
2.1.293 and SHA-256 is
4e21122a227857da1178aca3299700c1fd7f2b77c93f12e73c2c76db796a105e. This
temporary path is provenance only. New runs must receive an executable path
explicitly through CLAUDE_NATIVE_BINARY.

Generated Native configs, runtime caches, backups, xcrun_db, generated plugin
compiler output, node_modules, and user-account configuration are excluded.
Runners write new data under ignored per-fixture run-output/ directories.
