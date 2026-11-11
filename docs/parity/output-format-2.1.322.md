# Output format compatibility with Claude Code 2.1.322

The shared CLI output contract targets Claude Code **2.1.322**, darwin-arm64,
SHA-256 `a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450`.
The pin is recorded in `headless-native-baseline-2.1.322.json`; it does not change
the older differential runner's default baseline.

Compatibility compares shared fields and their serialized bytes and key order.
LingXi keeps its provider/account routing and accounting extensions. Generated
UUIDs and elapsed clocks are validated rather than made equal across processes.
Public assistant and tool rows must retain their own accepted transcript UUID,
timestamp and content. Product configuration, capabilities, provider payloads,
token counts and prices remain truthful to the selected host and provider.

## Publication and accounting boundaries

- Publish each accepted assistant block before its dependent tool result. Keep
  source provider envelopes local to their physical stream and match both the
  response ID and model. Providers without a native envelope use the accepted
  transcript row, including its durable identity and exact UTF-16 content.
- Prefer an already-ready provider event, while allowing completed tools to wake
  a silent provider stream. Neither declaration order nor unconditional waiting
  for message-stop determines tool-result order.
- Keep latest-response usage, usage since the last accepted result, and
  cumulative session/model totals separate. Result admission consumes the
  interval only after the outbound queue accepts that result. Restore and
  correction handling preserve this boundary.
- Aggregate public usage by model without merging the internal provider/account
  charges. Preserve provider identity when model names collide, both cache-write
  lifetimes, authoritative quotes, signed corrections and session attribution.
- Carry immutable permission facts with each accepted tool publication. The
  native no-UI diagnostic changes presentation after an actual denial; it grants
  no permission.

The command-replay regression remains enabled. It snapshots registered command
metadata before host shutdown and allows LingXi's built-in asset guidance in
the Skill prefix while retaining the immutable native replay fixtures.

## Regression checks

```sh
cargo test -p cost --lib --locked \
  --config scripts/tests/output_format_322_profile.toml
RUST_MIN_STACK=16777216 cargo test -p harness-runtime --lib --features desktop \
  --locked --config scripts/tests/output_format_322_profile.toml \
  headless:: -- --test-threads=1
```

The comparison profile disables runtime development diagnostics and assertions,
retains the CLI's larger unoptimized worker stack, and leaves release settings
unchanged. Captures and timing-sensitive lifecycle tests use this same profile.

`scripts/tests/output_format_322_captures.py` checks an archived set of controlled
native-a/native-b/LingXi captures. It slices usage, costs and shared payloads from
the original wire bytes; it does not parse and reserialize them for comparison.
The supplemental probe archive contains 20 fixtures and 24 result frames across
SDK turns, resume, interruption, retries, budget failures, permissions, text,
per-block publication and fast/slow/multiple-tool readiness.

```sh
python3 scripts/tests/output_format_322_captures.py /path/to/captures \
  --output /path/to/shared-check.json
node scripts/tests/headless_native_differential.mjs \
  --baseline docs/parity/headless-native-baseline-2.1.322.json
```

The latter command lists the repository matrix without executing it. Capturing
requires `--execute`, an explicit host configuration and a fresh output
directory; the supplemental probe packages and raw captures are retained in the
local task evidence archive, rather than embedded in this repository.

## Opt-in real-account acceptance

`headless::run::live_output_322_tests::native_322_live_multi_provider_accounts`
uses the production SDK print driver and explicitly selected existing accounts.
It is ignored during ordinary test runs. Supply `LINGXI_OUTPUT_322_LIVE_CONFIG`
with a secret-free JSON configuration:

```json
{
  "output": "/absolute/fresh/test-output",
  "account_home": "/absolute/configured/account-home",
  "store_scope": "configured-store-scope",
  "broker_client": "/absolute/installed/signed/credential-client",
  "channel": "development",
  "cases": [
    {"label": "provider-a", "account_id": "existing-account-a", "model": "model-a"},
    {"label": "provider-b-1", "account_id": "existing-account-b1", "model": "model-b"},
    {"label": "provider-b-2", "account_id": "existing-account-b2", "model": "model-b", "tool_probe": true},
    {"label": "provider-a-repeat", "account_id": "existing-account-a", "model": "model-a"}
  ]
}
```

Run a test-host copy signed with the configured development CLI identity so the
installed broker can authenticate its caller. Credentials stay in process memory.
The test writes copied account metadata, session rows and accounting under its
fresh output directory; it does not modify the configured accounts or broker
authorization. Exact-account selection uses the trusted host's existing
route-only lifecycle API under the SDK operation fence. Private account profiles
are intentionally absent from the public model catalog.

```sh
RUST_MIN_STACK=67108864 \
LINGXI_OUTPUT_322_LIVE_CONFIG=/absolute/live-config.json \
  /absolute/signed-test-host \
  --exact headless::run::live_output_322_tests::native_322_live_multi_provider_accounts \
  --ignored --test-threads=1
python3 scripts/tests/output_format_322_live.py /absolute/fresh/test-output \
  --config /absolute/live-config.json --reference /path/to/captures \
  --output /absolute/live-check.json
```

The live check reconciles intervals, cumulative usage, selected account identity,
request counts and the durable integer cost ledger. Multiple accounts can share
one model aggregate while retaining separate charges. It also checks every
assistant/tool frame against its accepted transcript and verifies actual tool
execution. OpenAI-compatible live streams use canonical accepted blocks;
provider-native partial forwarding is exercised by the controlled Anthropic
fixtures. These finite checks do not claim equality of all provider values or
every possible trace, or acceptance of a packaged Desktop or Mobile app.
