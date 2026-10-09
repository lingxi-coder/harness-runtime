# Native 2.1.295 Mod UI contract probe

This fixture uses the pinned Native `claude plugin test` entry point to observe
terminal `ui.render` results and the public `$.ui.notify(text)` call shape. It
does not establish desktop notification delivery: the isolated plugin-test
host rejects a valid notify call with `no implementation for ui.notify`.

The frozen result demonstrates that a Button can carry string and Text
children. Native concatenates their text into `props.label`, preserves the
child tree when it contains Text nodes, keeps those children when an explicit
label is supplied, and omits `children` when a sole string child is folded into
the label. Press handles are runtime-generated, so the raw captures preserve
their original values.

To rerun with the exact binary:

```sh
CLAUDE_NATIVE_BINARY=/path/to/2.1.295 python3 docs/parity/current-alignment/fixtures/mod-ui-render-295/run-native.py
```

The runner writes fresh process output and a receipt under ignored
`run-output/`. The checked-in `native-baseline/2.1.295/` holds the initial raw
evidence and its SHA-256 manifest. The fixture is limited to this public test
entry point and does not claim a rendered desktop notification or whole UI
parity.
