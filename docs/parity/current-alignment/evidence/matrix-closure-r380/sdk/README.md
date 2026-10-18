# SDK evidence delivery (r380)

This package retains three narrow, offline evidence paths for the `llm-client`
revision recorded in `source-bindings.json`:

- **Statusless Anthropic SSE errors:** the current public SDK decoder consumes
  three fixed Native 2.1.293 raw SSE fixtures. The test checks raw event order,
  raw provider events separately from normalized SDK events, partial text before
  timeout error, UTF-16 source units including the escaped lone surrogate, and
  timeout-versus-API-error classification. The public-SDK run passed 1/1.
- **Foundry candidate transport:** the public client uses the production
  `HttpTransport` and an explicit dummy API key to send one request to a local
  `127.0.0.1` stub, then consumes its fixed six-frame SSE response. The run
  passed 1/1 and retains the exact request and response bytes. No external
  provider was contacted.
- **Extracted Native 2.1.295 SSE parser/APIError source:** the checked-in
  extracted source was exercised against the same three fixed 2.1.293 raw
  vectors. This is an extracted-source execution, not a Native process or a
  2.1.295 public-output capture. `extract_native_ranges.py` verifies the pinned
  Native artifact fingerprint and extracts only the two cited source ranges;
  the full Native artifact and 309 KB parser source chunk are not included.

## Reproduction

The `Cargo.toml` and `Cargo.lock` files beside the probe sources are historical
inputs from their original acceptance location. Their relative `llm-client`
dependency paths resolve there, but will not resolve after this package is
copied into H docs. Do not edit those archived manifests. Use the preparation
helper to verify the package manifest and explicit SDK revision, then make
temporary probe crates whose only manifest change is the caller-provided SDK
path. It copies the lockfiles byte-for-byte and leaves this delivery package,
its receipts, and its source hashes untouched.

Set `SDK_PATH` to the `llm-client` checkout at the exact revision in
`source-bindings.json`, and `PREPARE_SCRIPT` to this package's copied
`prepare_cargo_probes.py` path:

```sh
SDK_PATH=/path/to/llm-client
PREPARE_SCRIPT=/path/to/evidence/prepare_cargo_probes.py
PREPARED=$(python3 "$PREPARE_SCRIPT" --sdk-path "$SDK_PATH")
cargo test --offline --manifest-path "$PREPARED/current-sdk-probe/Cargo.toml" --test current_sdk_statusless_sse -- --nocapture
cargo test --offline --manifest-path "$PREPARED/foundry-public-transport-probe/Cargo.toml" --test foundry_public_transport -- --nocapture
node /path/to/evidence/extracted-295-probe/extracted-295-sse-probe.mjs
```

The helper prints the new scratch directory and writes `prepared-probes.json`
there. It does not invoke Cargo. The extracted source probe uses packaged
fixtures and can be run from any working directory using its copied path. No
target folder, build cache, keychain credential, or provider account is
packaged. The Native range extractor is opt-in and requires the caller to
provide the pinned artifact path:

```sh
python3 /path/to/evidence/extract_native_ranges.py ./claude-code-2.1.295 ./native-295-source-ranges
```

The original Rust receipts used Rust 1.94.0, offline mode, two jobs, and debug
info disabled. The copied historical probe manifests are not themselves the
post-copy run manifests; only temporary manifests created by the helper use
the explicit `SDK_PATH`.

## Receipts and limits

`receipts/` contains the original logs verbatim. It retains both initial
assertion failures and later passing runs. The first failures exposed probe
expectations that did not account for legal raw `ProviderEvent(message_start)`
behavior; they were corrected to assert raw provider events separately from
normalized public SDK events. They were not production SDK failures. Do not
reinterpret the initial logs as passing runs.

The statusless-SSE raw oracle is Native 2.1.293, while the parser/APIError
source binding is Native 2.1.295. No 2.1.295 public capture is claimed. The
Foundry run establishes one candidate public HTTP/SSE path over localhost; it
does not establish cloud Foundry behavior or Native wire parity. No SDK
production files were changed for these probes.

`receipt.json` gives each run's exact scope and status. `source-bindings.json`
records repository pins, source-range hashes, fixture hashes, and probe-source
hashes. `contentmanifest.json` records the byte length and SHA-256 of every
packaged file except itself; the preparation helper verifies it before making
scratch copies.
