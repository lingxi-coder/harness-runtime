# Local Apps, platforms and Agent interfaces

This topic explains capabilities and integration choices. For requested app
authoring, continue with the available `local-app` skill; do not reload guidance
already present. An App owns released code and data. For inline interactive
results, use `visualize` when available. External builder plugins are optional.

## Execution host and presentation

Choose the execution host separately from the viewing client and Agent interface.
Desktop can use compatible scripts, compiled programs and services; Mobile Linux
restrictions do not apply. Current schema-2 entries are `static` and `server`.
Desktop's `native` target describes execution, not native GUI publication.
Standalone native-window, tray or headless-only entries need host support;
explain that gap instead of inventing fields or substituting a website.

Mobile uses browser presentation and its supported Mobile Linux backend runtime.
Keep Mobile hosting available for long tasks; recommend Desktop without requiring
it. Viewing a Desktop-hosted app on Mobile leaves execution and data on Desktop.

## Publication and ownership

The shared `LocalApp` tool offers publish, list, read, start, stop, status, logs
and agent_catalog actions according to its live schema. Read an existing app
before modifying it, keep its source Project and app ID, and publish with the
observed `expectedRevision`. Resolve conflicts against the newer source rather
than blindly retrying. Source Project, immutable release, current run and
durable business data are distinct resources.

Local publication adds to the device's Apps library; it does not publish to a
public market. Version switching, source restore/remix, permission grants,
background authorization and public uploads can be native-only operations.
Explain the corresponding Apps UI when no model tool exists. `agent_catalog`
lists public model and built-in/MCP tool identifiers; it neither returns
credentials nor grants access.

## Choose the Agent interface

| Need | Interface |
| --- | --- |
| An Agent calls Desktop app operations independently of a page | Standard MCP through app-owned stdio servers or a process-bound Streamable HTTP endpoint. |
| An Agent operates on a Mobile-hosted page | Optional WebMCP through the supported browser integration. |
| An app requests the LingXi host Agent | The pinned official AG-UI 1.0 contract and the supported App SDK. |

Desktop `mcpServers` declarations require `mcp` capability consent and native
targets. The host starts/connects them and registers tools under
`lingxi-local-apps` without modifying user/project MCP settings. It reconnects
broken channels without replaying calls, and removes run-owned connections on
stop, revocation, replacement or uninstall. Existing chats receive catalog
changes. Release/definition changes require fresh discovery and grant review.
The gateway supports ordinary tools, not resources/prompts, MCP tasks, sampling
or elicitation. See `local-app` for the exact manifest and verification workflow.

`agent.tools` separately requests existing catalog tools for App Agent runs;
exporting tools never grants their use. Host policies still apply. Credentials
and approvals stay native-owned. A browser gateway URL is not an MCP endpoint.
For independent MCP services use [configuration](configuration.md). Desktop's
old WebMCP injection and polyfill are removed; do not add compatibility paths
or old-data migrations. Mobile app-owned MCP process lifecycle is not enabled.
Browser App SDK calls do not imply a native-process API.

## App Agent state and permissions

Generate only schema 2 packages and the current host API and Agent persistence
format. Do not add compatibility aliases, old API decoders, old Agent-data
migrations or old-pending cleanup hooks. Current business data remains separate;
an authorized business-schema change still requires the host's verified backup
and migration transaction.

Persistent Chats keep a host-owned `threadId` and a fresh `runId` per user turn.
Send only the new user Message, with `agent.file(file)` for images/documents.
The host retains successful history/assets and summarizes model context; apps
must not rebuild tool history or turn tool results into new user turns. Select
approved Skills per run: prior messages retain neither active Skills nor grants.
Context, storage, run and OS limits still apply.

After a connection loss, read the saved task, events or Chat history and use
`connectAgent({runId})` to observe the same run without starting another request.
Preserve unavailable data and original IDs/input. Never recreate a Chat or
replay a tool effect to recover a lost page Promise.

App Agent accepts validated images, PDF and supported UTF-8 content. Do not
infer audio/video, remote URL, provider-handle or main-chat Media access. Speech
needs a separately supported and authorized transcription-to-text flow; this
does not grant cloud transcription or add audio upload to the App SDK. Check
the current input schema and grants, and disclose missing integration.

On Desktop, Apps library → the app's Agent button opens its native settings:
enable Agent, select an available model and the declared tools/skills, then
save permissions. Unavailable tools are shown separately; changing permissions
cancels running tasks. A manifest declaration is only a request, not a grant.
On mobile use the corresponding native App Agent authorization UI; a browser
page cannot self-approve or borrow a Desktop session's tools.

For multi-user apps, enforce each user's identity, data and Chat access in the
application. App Agent thread IDs are conversation identifiers, not user
isolation. Multiple users do not imply parallel Agent runs; consult the current
host limits and authoring skill.

## Opening and recovery

Mobile pages that use WebMCP load the host bridge where required; static HTML
injection does not establish that every server page already has it.

Desktop uses app windows. Mobile defaults to the OS-resolved external browser;
supported optional opening methods are independent of the execution host.
Use browser camera/file controls for app UI. Mobile-hosted processes depend on
the host lifecycle and finite background leases. Remote Desktop-hosted apps
stay on Desktop through the authenticated gateway. Preserve browser pending
state, durable data and task IDs during handoff/recovery; do not create another
Agent task merely because a connection was lost.

Source anchors: Harness `crates/local-app/assets/skill/SKILL.md`,
`crates/tools/ui/src/desktop_local_app.rs`, `crates/runtime/src/app_agent_input.rs`;
product `docs/{LOCAL_APP_AGENT_PROTOCOL,MOBILE_LOCAL_APP_AGENT,LOCAL_APP_REMOTE_ACCESS}.md`.
