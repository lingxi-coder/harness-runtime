# CLI background-session snapshot migration

`cli-bg-session-snapshot.patch` carries the reviewed seven-line CLI adaptation
for the required `BgSessionSnapshot` interface. It passes the live conversation
and original model-reminder attachment payloads through the background-session
forker into `orchestrator::bg_snapshot::history_to_jsonl_lines`.

The current product workspace still pins the canonical harness-runtime Git
dependency to `4dfd9abef1dc293a9617d35387165cb2256b5d98`. Apply this patch only
after the runtime change is committed and the product workspace migrates its
canonical Git revision to a commit that contains the new interface. The
adaptation is saved here instead of remaining in the active product checkout.

The runtime regression exercises the live handle boundary, a capture host,
serialized attachment replay, MCP baseline deduplication and disconnection.
The actual CLI compilation and daemon-worker dispatch remain unverified until
that dependency migration. No runtime commit or product pin change is included.
