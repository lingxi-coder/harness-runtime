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

Product help and privacy requests use the address published on the product
privacy page. A screen that links to another product's website is not LingXi
support. Do not send the user there.

Use a targeted reversible remedy justified by the evidence. Do not clear user
data, reset rootfs, loosen permissions, recreate credentials, re-pair devices,
or edit transcripts as a generic recovery step. For dropped App/Bot connections,
query the original task/session rather than submitting the same action again.

Report what the check established: source/schema, unit/fake-host test, compiled
package, device interaction or live service outcome. A build alone does not
prove physical camera, signed credentials, remote approval or task completion.

Source anchors: Harness bundled `debug`/`doctor` commands, host status and tool
results; product acceptance records for the exact build under investigation.

Desktop logs default to the client userData/logs/desktop.jsonl; session JSONL
and subagents are under the resolved config-home/projects/<encoded-project>/.
Resolve those paths from the running host; brands and relocated homes differ.
Match session ID, child/task ID, timestamps and the package/source receipt.
Read only relevant text/tool/error records; redact credentials and opaque
provider reasoning payloads. Compare the actual package's dependency fingerprints,
not merely neighboring checkout heads. A local unsupported-capability encoding
error does not prove a request was sent to a different provider. In particular,
ToolSearch plus defer_loading on a GPT route indicates a discovery wire mismatch.
Host-supplied SubagentHandback unavailable indicates a host dispatch problem;
verify its receipt/disposition rather than treating plain text as delivered.
