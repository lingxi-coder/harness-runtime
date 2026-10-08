# Native 2.1.293 plugin grapheme and width-access probe

This fixture runs a public Native CLI plugin startup with one fixed loopback
Messages stub response; it never contacts a real model or external service. At
module registration, the plugin records whether a width API is exposed and
queries `Intl.Segmenter` over fixed UTF-16 strings, then emits one raw JSON
marker. Inputs include lone
high/low surrogates, combining sequences, and ZWJ emoji sequences. Each sample
records its original UTF-16 code units and each grapheme segment's code units,
so lone units are not inferred from Unicode scalar strings.

The hook plugin sandbox exposes `Intl.Segmenter`, but does not expose a `Bun`
global or `stringWidth` on the allowed `claude-code` module. The fixture records
that absence rather than substituting the separately installed Bun executable;
therefore these Native captures establish grapheme segmentation only, not the
embedded width function's result. The current receipt preserves this boundary.

Run from the Harness repository root:

```sh
CLAUDE_NATIVE_BINARY=/path/to/claude \
python3 docs/parity/current-alignment/fixtures/native-bun-unicode-width/run-native.py
```

The runner pins Native 2.1.293 by version and SHA-256, disables all model tools,
uses a dummy key and isolated HOME/config/cwd, and binds its only model response
to loopback. The selected raw marker and receipt are checked in under
`native-baseline/2.1.293/`; full stdout/stderr and runtime config stay in
ignored `run-output/`.
