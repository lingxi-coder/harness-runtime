# Native Claude Code 2.1.289 Agent API loopback acceptance

This is a separate enabled-Mod run from the earlier
[session.start smoke](mods-native-acceptance-2.1.289.md). It exercised the
actual Native `$.agent.list()` and `$.agent.spawn(...)` APIs and observed the
child transition from `running` to `completed` in Native's task registry. The
machine-readable summary is
[`mods-native-agent-acceptance-2.1.289.json`](mods-native-agent-acceptance-2.1.289.json);
the full raw receipt and stdout/stderr remain in the temporary directory
listed below.

The pinned binary is `/tmp/harness-claude-code-2.1.289-darwin-arm64`,
SHA-256 `03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69`.
The source-pinned fixture runner is
[`mods_native_agent_acceptance_289_loopback.mjs`](../../scripts/tests/mods_native_agent_acceptance_289_loopback.mjs).
Run it with:

```sh
node scripts/tests/mods_native_agent_acceptance_289_loopback.mjs \
  --execute /tmp/harness-claude-code-2.1.289-darwin-arm64
```

## Fixture and bounds

The script creates a fresh temporary plugin, session working directory,
`CLAUDE_CONFIG_DIR`, and temporary directory. The `session.start` handler
calls `$.agent.list()`, launches one `Explore` child with `$.agent.spawn()`,
lists the task immediately, waits 3.5 seconds with `$.clock.sleep()`, and lists
again. A module-scoped guard prevents repeat invocation from spawning another
child. The native API wrapper and result shape are covered by the existing
[Agent API oracle](mods-agent-2.1.289.md); the actual launch enters Native's
async Agent stream path at `[194849961,194850600)`, SHA-256
`953e0221437e2f90475e88e4f2157ee1440938fa6637df73a13062fbbbf5eff6`.
Native has one explicit `session.start` dispatcher call at
`[205788724,205789464)`, SHA-256
`fa0cdf555259d293125289387ca95297d74121f848d304149e7e13ab5d976f28`; the
Agent child stream path does not dispatch a second `session.start` call.

The local HTTP server binds only to IPv4 `127.0.0.1`. It accepts one
source-pinned `HEAD /api/hello` and at most two model POSTs at
`/v1/messages?beta=true`: one child request and one root request. It checks
the `x-api-key` against a fixed dummy value and records only whether it
matched; it never stores or prints the header value, and never relays a
request. Every other peer, route, key, or request count is rejected. The
child response is delayed 2.5 seconds so that the immediate list can observe
the launched task before its completion.

Native receives a constructed environment with the dummy API key, the
loopback base URL, fresh `CLAUDE_CONFIG_DIR`, and source-pinned telemetry
flags. `HOME` is left unset; no real provider, proxy, or account credential is
passed. Normal Native managed-settings and operating-system credential
lookups remain enabled. The telemetry flags are not a general network
firewall; this acceptance establishes only that the configured model traffic
was handled by the non-relaying local stub.

The process has a 60-second deadline. Timeout sends SIGTERM to its detached
process group and schedules SIGKILL after three seconds. The script closes the
listener and checks for remaining process-group members before writing the
receipt.

## Result

The run passed with exit code 0 and no timeout. `$.agent.list()` returned an
empty list before the spawn. `$.agent.spawn()` returned model
`claude-sonnet-5-5` and a string Agent ID. The immediate list showed that row
as `running`; after the delayed child response and Native settle, the same
row was `completed` and carried `spawnedBy` for the fixture plugin. Both list
calls completed without error.

The stub accepted exactly two POSTs with the dummy key: one child request and
one root request. It also accepted the one health probe, saw no unexpected
request, and did not relay externally. Stdout was 7,143 bytes with SHA-256
`36407adc13d4836f6cf281e6891910a8e3ddce6886088f7ff3ee5525595a9cb4`;
stderr was empty. The full receipt is
`/var/folders/2c/gch8wj4s0d52rygkyd6b9f_h0000gn/T/mods-native-agent-acceptance-289-uXgceg/receipt.json`;
raw stdout and stderr are alongside it. The earlier successful session.start
smoke's source files and receipt were not overwritten.

This proves one Native enabled-Mod Agent launch/list/settlement path against
fixed loopback model replies. It does not establish general provider
compatibility, all permission modes, other agent types, or system-wide
network isolation.
