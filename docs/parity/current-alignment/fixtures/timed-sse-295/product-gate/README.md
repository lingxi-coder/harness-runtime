# Product CLI gate for frozen 2.1.295 timing fixtures

`run-product.py` accepts the exact Product CLI binary path and exercises the public `--print --output-format json` entrypoint with an isolated HOME/config and a loopback-only Anthropic endpoint. It writes raw request headers, request bodies, response bodies, stdout, stderr, and a per-run receipt under the ignored `run-output/` directory.

The three cases use only the existing frozen 2.1.295 fixtures:

- Default JSON print receives the recorded fixed SSE stream and must send `stream: true`.
- Default JSON print receives the fixed 404 body and then the captured nonstream Anthropic message; it must request `stream: true` followed by `stream: false`.
- The explicit `--no-stream` control receives the same captured nonstream message and must send `stream: false` on its only request. This direct opt-out case has no separate Native capture; its receipt records that boundary rather than claiming Native parity for its timing fields.

Run after E provides the canonical Product binary:

```sh
python3 run-product.py --binary /absolute/path/to/product-cli --product-revision <checkpoint-sha>
```

The runner verifies result content, request stream sequence, raw request/response capture, expected timing-key presence/absence for the two Native-backed cases, and finite nonnegative timing values. It does not compare elapsed millisecond values byte-for-byte. The request body and headers are preserved as received; the dynamic loopback port is recorded separately. The runner does not read user settings or credentials, and it does not contact an upstream service.
