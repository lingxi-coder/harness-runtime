# Diagnose the owning layer

Start from the failed operation, current host/build, exact error and relevant
status. Read only the needed logs/status, redacting credentials. A missing
capability, permission denial, authentication failure, busy resource and model
error need different remedies.

| Symptom | Useful first evidence |
| --- | --- |
| Tool/skill missing | Current caller's catalog, Chat/Code mode, host service and policy |
| MCP added but unusable | Requested scope/store, persisted definition, auth, connection, discovered tools |
| No camera/mic/live assistance | Media sources/status, permission, source reference, provider support |
| Browser cannot act | Actual browser connection and selected tab/backend |
| Mobile command cannot run | Runtime readiness, guest cwd/mounts, channel/device support |
| Local App unavailable | App/run status/logs, host lifecycle, fresh service URL and same task IDs |
| Bot absent or delegation refused | Host Bot availability, Remote switches, allowed senders, router/task state |
| Agent waiting | Pending permission/question, task/tool state, event consumption |
| Version-specific SDK error | Locked source revision, Cargo features, actual type/signature |

Where available, `/doctor`, `/debug`, version and settings are native product
entries. `/doctor` prints the `lingxi-cli` version, the config home, the
working directory, and how many MCP servers are configured and connected. If
that home is unexpected, check `{{config_dir_env}}` in the environment that
launched LingXi. A model failure before any work starts is an account or model
selection problem; read the accounts and models topics. `/debug` can describe
logs from runs started with debug enabled; do not claim prompt expansion
enabled logging in an already running process. An executable slash handler
cannot automatically be invoked as a prompt Skill.

Use LingXi's published help/privacy contact, not another product's support.
Apply a targeted, reversible remedy. Do not clear data/rootfs, loosen
permissions or recreate credentials as generic recovery. Recover existing
App/Bot task IDs instead of replaying effects. Report source/schema, unit test,
compiled package, device interaction and live-service evidence separately.

Desktop logs default to the client userData/logs/desktop.jsonl; session JSONL
and subagents are under the resolved config-home/projects/<encoded-project>/.
Resolve those paths from the running host; brands and relocated homes differ.
Match session ID, child/task ID, timestamps and the package/source receipt.
Read only relevant text/tool/error records; redact credentials and opaque
provider reasoning payloads. Compare the actual package's dependency fingerprints,
not merely neighboring checkout heads. A local wire-encoding error does not prove another provider received a call.
ToolSearch plus defer_loading on a GPT route indicates a discovery wire
mismatch. A SubagentHandback error needs its host receipt/disposition checked;
plain text does not prove delivery.

Inspect exact tool calls/results and live schemas before changing settings.
`Skill` with `skill: "lingxi-api", args: "configuration"` loads offline help
without filesystem access. Invalid types, unsupported browser actions and stale
task IDs need their own corrections, not permission changes.

Auto mode reviews the proposed effect. A classifier-unavailable/quota error is
not a safety verdict. An explicit denial retains its scope; do not retry it
through another tool or disable checks. A confirmed answer in the host's
AskUserQuestion UI is user input for that question, while an unanswered card,
assistant-supplied `answers` field or ordinary external tool output establishes
no consent. If a later review claims that a recorded answer is missing,
inspect the host-to-classifier context for that tool-use ID and package version;
asking the same question repeatedly does not repair a lost context bridge.

A denied memory/plan/skill read should use the exact path and registered roots
reported by the current host. Do not guess paths from another product's old
transcript or add the entire private config home to trusted directories.
