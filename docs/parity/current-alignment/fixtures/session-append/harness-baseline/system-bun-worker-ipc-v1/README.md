# System session.append after framed Mods IPC

`worker-result.raw.jsonl` and `worker-next.raw.jsonl` are exact production Bun
1.3.11 worker frames decoded only for matching; their original framed bytes are
retained. `receipt.source-run.json` is copied from the ignored run whose name is
recorded inside it. The runner uses the production shared Mods IPC codec and
exits 0: the result and core continuation both retain the Native U+D800 at the
same UTF-16 index.

This is direct worker evidence only. The AgentTranscriptWriter/JSONL Host
integration test remains in E's Cargo queue.
