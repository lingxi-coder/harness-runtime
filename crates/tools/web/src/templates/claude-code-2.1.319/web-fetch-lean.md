Fetches a URL, converts the page to markdown, and answers `prompt` against it using a small fast model.

- Fails on authenticated/private URLs — use an authenticated MCP tool or `gh` for those instead.{artifact_exception}
- Fails on localhost and other hostnames without a dot; for a local server, use curl via a registered shell tool.
- HTTP is upgraded to HTTPS. Cross-host redirects are returned to you rather than followed; call again with the redirect URL.
- Responses are cached for 15 minutes per URL.