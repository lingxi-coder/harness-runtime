# Native 2.1.295 resources/list_changed refresh reachability

This second bounded run uses Native's standard/fallback MCP protocol path: `server/discover` gets method-not-found, `initialize` advertises `resources.listChanged: true`, and the initial `resources/list` returns one fixed resource. Native exposes and executes `ListMcpResourcesTool`; the model tool_result is captured in the exact session JSONL.

After that tool result, the local MCP server sends one `notifications/resources/list_changed` while the loopback model stub holds the next Messages response. The Native process makes no second `resources/list` request. Because no pending RPC occurs, the harness does not send SIGINT; no cancellation notification is observed. The CLI exits successfully after the bounded 20-second wait. This is a reachability result, not a cancellation pass.

Raw stdio JSON-RPC in/out, notification, model requests/responses, stdout/stderr, session JSONL, debug log, and original receipt are copied byte-for-byte and hashed in `manifest.json`. The isolated `.claude.json` is intentionally omitted.
