# Claude Code 2.1.216 observable oracle

Captured from `claude 2.1.216 (Claude Code)` on 2026-07-21. These fixtures
cover only public, observable CLI/protocol behavior; they are not evidence about
Anthropic's private implementation.

- `mcp-help.txt`: public MCP commands relevant to H7.
- `remote-control-help.txt`: the vendor-hosted M6 surface. LingXi parses this
  surface but intentionally fails before connecting because the private relay
  contract is unavailable.
- The protocol golden for `mcp serve` is MCP `2025-06-18`: initialize returns
  a tool capability and serverInfo, followed by a working `tools/list`.

Secrets, account identifiers, hostnames and redirect URLs are not retained.
