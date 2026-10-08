# Captured public-plugin scope

Native 2.1.293, SHA-256 `4e21122a227857da1178aca3299700c1fd7f2b77c93f12e73c2c76db796a105e`. The exact marker is `unicode-oracle-marker.raw.txt` and its JSON bytes are `unicode-oracle.raw.json`. It proves `Intl.Segmenter` behavior for six fixed UTF-16 sequences, including lone high/low units and ZWJ sequences. It also records `Bun` and `process` absent, `stringWidth` unavailable, and the allowed `claude-code` module exports without `stringWidth`. This Native plugin surface therefore does not yield width values; host Bun results are not substituted for Native evidence.

The plugin loader rejected a bare static `string-width` import on this same binary; it permits relative imports and the `claude-code` module only. The full model request remains in ignored `run-output/`; the receipt records its byte count and hash, plus the hook/module hashes, single loopback POST, response bytes, stdout and stderr.
