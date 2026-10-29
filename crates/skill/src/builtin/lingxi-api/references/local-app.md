# Local Apps, AG-UI and WebMCP

Load the `local-app` skill when present for the maintained schema-2 authoring
workflow. Use `visualize` for an inline interactive result when that tool/skill
is available. A Local App owns released code and data; it is more than a preview.
An external builder plugin is optional, not a prerequisite implied by this guide.

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
lists public model identifiers; it neither returns credentials nor grants access.

App-to-Agent communication uses the pinned official AG-UI contract. Agent-to-App
page tools use WebMCP. They are opposite directions, not interchangeable wire
formats. Host approvals and provider credentials remain native-owned. An App
cannot approve its own request or obtain general host administration access.

Current App Agent input has a restricted content profile (image, PDF, supported
UTF-8 content). Do not infer audio/video, arbitrary remote URL, provider-handle,
or main-chat Media access. Check the actual input schema and app grants.
For recorded speech, a separately supported and authorized transcription flow
can produce text for App Agent input. This does not add a direct audio-upload
contract to the App SDK or authorize cloud transcription. If the app has no
such flow, explain that integration gap instead of promising it already works.

On Desktop, Apps library → the app's Agent button opens its native settings:
enable Agent, select an available model and the declared tools/skills, then
save permissions. Unavailable tools are shown separately; changing permissions
cancels running tasks. A manifest declaration is only a request, not a grant.
On mobile use the corresponding native App Agent authorization UI; a browser
page cannot self-approve or borrow a Desktop session's tools.

Server-rendered pages must load the host WebMCP bridge where required; static
HTML injection does not establish that every server page already has it.

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
