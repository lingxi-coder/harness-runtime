# DeepSeek Search/Fetch integration

DeepSeek's independent APIs are called by the SDK, using the exact active
first-party DeepSeek profile and its current host credential. They do not
require a second model generation or an alternate search connection.

- `WebSearch` uses `/v1/tools/search`. Query and domain filters are validated
  before tool budget consumption. The SDK encodes `site:` / `-site:` operators;
  the adapter also filters returned URLs to enforce the domain constraints.
  Title, URL, short snippet, long summary and source metadata reach the tool
  result. Empty results report zero billable searches and no generation tokens.
- `WebFetch` uses `/v1/tools/fetch` on an eligible DeepSeek route. The existing
  URL validation and HTTP-to-HTTPS upgrade run before dispatch. Provider
  failures are surfaced without retrying through local HTTP. The returned
  Markdown follows the tool's existing prompt/apply and output path. Independent
  provider fetch bypasses the shared local URL cache to avoid reusing a page
  across different provider accounts.
- Other providers retain their existing hosted-search and fetch paths. Custom
  DeepSeek gateway origins are never redirected to the first-party API.
- Native model-side Responses search is separately supported by the SDK on
  `deepseek-v4-pro`; `deepseek-flash` is rejected for that hosted tool. Independent
  Search/Fetch is usable from both conversation models.

Each independent operation sends one request. HTTP status and rate-limit /
timeout classification are preserved. SDK errors retain numeric Retry-After;
there is no automatic retry loop. Hosts must pace account-wide requests:
Search and Fetch each allow 5 QPS. Successful nonempty requests cost $0.00075
per API call according to the published DeepSeek price; the SDK exposes request
counts and cost estimates. These independent services do not enter model-turn
COGS accounting. Search tool telemetry records the actual billable search
count and zero generation tokens.

The workspace and `Cargo.lock` pin the published SDK Git revision
`e986d0841e5cc6b5151bdaeabca1c6dd156771df`. No local SDK path patch is used.

Focused validation:

```sh
cargo test -p orchestrator --test deepseek_hosted_search --offline
cargo test -p tool-web --lib --offline
```

These tests inject transports and credentials. They do not contact DeepSeek or
prove live account access, provider result quality, or production billing.

Official contracts: [Search/Fetch](https://api-docs.deepseek.com/guides/search/)
and [Responses](https://api-docs.deepseek.com/guides/responses_api/).
