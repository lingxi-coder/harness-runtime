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
entries. `/debug` can describe logs from runs started with debug enabled; do
not claim prompt expansion enabled logging in an already running process.
An executable slash handler cannot automatically be invoked as a prompt Skill.

Use a targeted reversible remedy justified by the evidence. Do not clear user
data, reset rootfs, loosen permissions, recreate credentials, re-pair devices,
or edit transcripts as a generic recovery step. For dropped App/Bot connections,
query the original task/session rather than submitting the same action again.

Report what the check established: source/schema, unit/fake-host test, compiled
package, device interaction or live service outcome. A build alone does not
prove physical camera, signed credentials, remote approval or task completion.

Source anchors: Harness bundled `debug`/`doctor` commands, host status and tool
results; product acceptance records for the exact build under investigation.
