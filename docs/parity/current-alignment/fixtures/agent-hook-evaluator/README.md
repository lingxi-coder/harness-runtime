# Native 2.1.294 `type: agent` Stop hook evaluator probe

This isolated public CLI fixture compares the generic `type: agent` Stop hook
evaluator request. It uses a fixed main response, fixed evaluator decision,
dummy API key, isolated HOME/config, and a Messages stub bound only to
127.0.0.1. Retry is disabled and the CLI process has a 45-second limit. The
runner records the full original request and raw top-level `system` and
`messages` value slices; it classifies evaluator traffic only when the fixed
instruction marker or Stop event marker is present in the captured request
bytes, never by request sequence alone.

Run only with the pinned 2.1.294 executable:

```sh
CLAUDE_NATIVE_BINARY=/path/to/claude-2.1.294 \
python3 docs/parity/current-alignment/fixtures/agent-hook-evaluator/run-native.py
```

The binary version and SHA are checked against `cases.json`; artifacts are
written to ignored `run-output/2.1.294/`. This captures evaluator reachability
and its request fields, not the quality of a live agent's judgment.
