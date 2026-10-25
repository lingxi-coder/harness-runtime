---
name: local-app
description: Build and publish a device-local schema-2 app, including camera input or independent host Agent tasks when needed.
session-modes: chat, code
---

# Local apps

Use the `LocalApp` tool to publish an app into the user's device library. Desktop, Android and iOS use schema 2, independent of the creating conversation. The tool schema and the current host's capabilities are authoritative.

## Workflow

1. Build immediately when the user supplied a clear request or approved a plan. Ask only for missing decisions that affect the result. A one-off chart or explanation can use an inline visualization; an app belongs in the Apps library.
2. A simple static app can be one complete HTML document with inline CSS/JavaScript, published as `LocalApp({"name":"…","summary":"…","html":"…"})`. The host constructs its schema-2 package. For Agent/device capabilities, multiple files, packages or services, create a project containing `lingxi-app.json` and publish with `bundle_path`.
<!-- if-shell -->
   Write and check the project with the `{{SHELL_TOOL}}` tool. Keep dependencies and Python virtual environments inside the project. A long simple document may use `file_path` instead of embedding `html` in the tool call. Check scripts and the important user interactions before publication.
<!-- end-if-shell -->
3. For a project, declare only the needed capabilities, exact package source files, targets and repeatable install/build/process steps. Bind servers to `127.0.0.1` and their assigned `PORT`. Use `LOCAL_APP_DATA_DIR` for durable backend data; installation/build steps receive separate preparation data and must not migrate live user records.
4. Fix whatever the tool reports, publish, and inspect `start`, `status` and `logs` where available. An application card and its entry in Apps are delivery; a source directory or temporary development server alone is not.
5. Explain what works and what was actually tested. Do not describe source tests, a browser preview or a successful build as physical-device or real-model acceptance.

## Project manifest

Use the current schema only. A minimal project looks like:

```json
{
  "schema": 2,
  "name": "My app",
  "entry": {"type": "static", "path": "index.html"},
  "targets": [
    {"platform": "macos", "environment": "web"},
    {"platform": "windows", "environment": "web"},
    {"platform": "linux", "environment": "web"},
    {"platform": "android", "environment": "web"},
    {"platform": "ios", "environment": "web"}
  ],
  "package": {"files": ["index.html"], "dependencyLocks": []},
  "dataSchema": {"version": 1, "compatibleFrom": 1}
}
```

Static apps do not require Linux processes. A Node/Python app needs a compatible execution target and the user's exact-release consent in the native Apps page. On mobile, require an available Mobile Linux runtime rather than assuming Desktop commands or arbitrary native executables will run. Check architecture, runtime/channel and capability support. Do not modify the shared Linux toolchain. Package only declared source/build outputs and lockfiles; exclude credentials, local grants, user data and dependency caches.

## Calling the host Agent with AG-UI 1.0

Declare `"capabilities":["agent"]` and `"agent":{"tools":[]}` in the schema-2
manifest when the App needs model understanding or Agent execution. Use the
host-injected `window.localApp.agent` and the official AG-UI 1.0 message/event
contract, backed by the bundled official SDK 1.0.3. Never embed a provider key,
reuse the creating conversation, or expose a Remote Control chat endpoint.

For an independent request, construct one standard user message and fresh run
and thread IDs. The File helper creates the standard image/document content part:

```js
const content = [{type: "text", text: "Describe the selected file."}];
if (file) content.push(await localApp.agent.file(file));
const {result, newMessages} = await localApp.agent.run({
  protocolVersion: "1.0",
  threadId: crypto.randomUUID(),
  runId: crypto.randomUUID(),
  messages: [{id: crypto.randomUUID(), role: "user", content}],
  forwardedProps: {
    "com.lingxi.localApp": {version: 1, outputSchema: applicationSchema}
  }
});
```

`agent.run` and Chat `send` use the official client and return official
`RunAgentResult` (`result`, `newMessages`) only after successful completion.
They reject failed, cancelled, interrupted or truncated runs. Display a validated
structured `result`; do not extract JSON from Markdown. Read model text from
standard messages and render it as text rather than executable markup.

For streaming interfaces use `agent.createClient()` (an official `AbstractAgent`
subclass), `addMessage`, `runAgent` and official subscriber callbacks. Raw
`runAgent` follows official SDK semantics: inspect `onRunErrorEvent` and the
normalized `onRunFinishedEvent({outcome})` rather than treating Promise settlement
alone as success. `abortRun` requests cancellation; a terminal cancelled outcome
confirms it. A new independent request needs a fresh client/thread and exactly
one user message. For multiple turns create/open an App Chat, use its handle ID
as `threadId`, and send a fresh `runId` for each turn. Chat history is paged as
`{messages: Message[], nextCursor?}`. The host owns the real Harness transcript;
client-provided history must match canonical messages.

For durable recovery use `tasks.start` with the same official input, then
`tasks.get/list/events/subscribe/cancel`. Task metadata includes `threadId` and
`runId`. Persist those IDs, then reconnect read-only with
`createClient({threadId}).connectAgent({runId})`; a reload never submits another
model request. Identical input with the same `runId` is idempotent; changed input
under that ID is rejected. Do not use the retired prompt/attachments/requestId
or custom state-event protocol. Old Agent records are discarded, not converted.

The creating chat can call `LocalApp({"action":"agent_catalog"})` to discover
exact public tool IDs. Discovery grants nothing. Declare tool and Skill IDs in
the release, get native grants, then request them only in
`forwardedProps["com.lingxi.localApp"]` (`version:1`, optional `tools`, `skills`,
`outputSchema`, `maxTurns`, `timeoutMs`). Standard top-level AG-UI frontend tools
cannot grant host tools. The current host profile rejects unsupported frontend
tools, state, context and resume features explicitly. Pages cannot approve tools
or questions. MCP grants bind the exact connection, configuration revision, raw
tool name and definition; Skills add instructions, not execution permission.

Use standard browser File/camera selection with user confirmation before model
submission. Files are limited to eight per new message, 25 MiB each and 50 MiB
total; bytes and model support are checked by the host. Supported inputs are
validated images, PDF and UTF-8 text/document formats. Audio/video, remote URL
sources, provider file handles and arbitrary binary input are unsupported. Do
not supply native paths or manually encode the File. Preserve the draft on photo
return and release camera tracks when done.

Mobile pages open in the operating system's resolved external browser, including
OEM/third-party browsers. Do not choose a browser package or embed WebView.
Android uses its user-visible foreground runtime. iOS26 uses an explicit native
foreground handoff and a finite continued-processing lease; iOS27 user-invoked
Intents require complete standard input. The SDK handles handoff/recovery; App
code keeps the standard run contract. No path promises indefinite execution or
automatic replay. Stop/revocation/release replacement cancel work; default limits
are one active task per App, 20 turns and ten minutes, and an App may only lower
native limits. Background Agent permission is separate from process permission.

Use the current Local App Builder examples for one-shot file understanding and
persistent attachment Chat. Keep business validation and explicit user save
confirmation in the App. Verify actual configured model, errors, File contents,
refresh, cancellation, Chat cold resume and saved data. Builds, mock outputs and
standalone browser rendering are not real-provider or physical-device evidence.

## Updates and data

- Find an app with `list`, then `read` its source metadata and latest revision. Edit the source project and update the same ID with `expectedRevision`; never overwrite concurrent work silently.
- Version switches preserve app data and may require new execution/capability consent. New code does not receive permissions from a saved model preference alone.
- Keep data formats backward readable. A changed filesystem `dataSchema` requires the host's verified backup/migration transaction; do not erase data or bypass a migration refusal.
- Use localStorage/IndexedDB for browser records, validate loaded data, report failed saves, and provide export/import for important records. Installation, publication and execution are different lifecycle steps.

## Interface and verification

Use local assets by default; AI/network features must honestly report their connectivity requirements. Support 320 px screens, safe areas, 44 px touch targets, light/dark preference and keyboard accessibility. Label controls and use adequate contrast. Use the Local App Builder plugin for frameworks or larger projects, retaining this schema-2 and host authorization contract. Test reload/storage, denial, cancellation and error recovery as well as the main flow.
