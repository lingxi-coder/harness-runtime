---
name: claude-api
description: Guide Anthropic SDKs and Claude API migrations, especially `/claude-api upgrade python` for the Python 1.x client surface.
---

# Claude API

Use this skill when migrating or upgrading Anthropic SDK integrations, with
special care for `/claude-api upgrade python`.

For Python upgrades, target the Anthropic 1.x SDK surface:

- Replace legacy client construction with `from anthropic import Anthropic`
  and `client = Anthropic(...)`.
- Migrate legacy completions-style calls to `client.messages.create(...)`.
- Update response handling to the Messages API shape instead of the old
  completion text payloads.
- Migrate exception handling to current SDK errors such as
  `anthropic.APIError`, `anthropic.APIConnectionError`,
  `anthropic.RateLimitError`, and `anthropic.APIStatusError`.
- Configure request deadlines with `anthropic.Timeout(...)`; do not pass
  `httpx.Timeout(...)` directly to the Anthropic client.

Keep the diff narrow: preserve the caller's provider/model wiring, reuse the
existing retry and logging structure where it still fits, and update tests or
examples that pin old imports, old client names, or old response shapes.
