# Native 2.1.295 MCP listener remote-error probe

Run with the explicitly pinned binary and a Python 3 runtime:

```sh
CLAUDE_NATIVE_BINARY=/path/to/claude-2.1.295 python3 run-native.py
```

The runner rejects a binary whose SHA-256 differs from `cases.json`. It creates a fresh HOME/config, strict MCP config, local stdio server, and loopback-only Anthropic Messages stub with a dummy API key. The stub waits for the server's JSON-RPC `subscriptions/listen` error before returning the model's tool-use response. No upstream service is contacted. One bounded run is preserved under `native-baseline/2.1.295/`; raw files and hashes are listed in that directory's manifest. The isolated `.claude.json` is intentionally not copied.

The captured error did not prevent Native from advertising and calling the MCP tool in this run. See the baseline receipt for the precise request sequence and the limits of that conclusion.
