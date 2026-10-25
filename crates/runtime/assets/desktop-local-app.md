---
name: local-app
description: Create and maintain Local Apps across Desktop, Android and iOS using real source projects, immutable releases and independent application data.
session-modes: chat, code
---

# Local Apps

Create an application project, publish it into this device's Apps library, and verify the published application. A project holds source code; an ordinary conversation is development history; an app owns its released code and data independently of either. Local publication never uploads to the public market.

## Development and delivery

1. Use the user's selected source directory. If none was selected, use a distinct directory inside the product's `.apps` authoring root. Do not reuse an unrelated active project or silently move an existing app's source when the conversation changes cwd.
2. Inspect the actual platform and available runtime with `{{SHELL_TOOL}}`. Mobile commands execute in the LingXi Linux user space. Keep project dependencies and virtual environments inside the project/application environment. Do not replace shared runtimes or install global packages as an app setup step. Never copy a macOS/Windows venv or node_modules tree to mobile Linux.
3. Write `lingxi-app.json` schema 2 at the project root. Declare only targets and capabilities the app actually supports. Keep dependency lockfiles; use exact package file lists for public export. A source package is not a reproducible-build guarantee.
4. Test source changes using permitted development tools. Publish using `LocalApp({"bundle_path":"/absolute/source/path"})`. A simple self-contained document can use `name` and `html` or `file_path`. A development server or a file path is not application delivery.
5. For updates, first read the application, then publish its `id` and the observed `expectedRevision`. A conflict means another session changed it: inspect and merge before retrying. Do not read the new revision merely to overwrite it.
6. Use `start`, `status` and `logs` to verify the released app. Installation/build/servers require native user execution consent for that exact release. Never write grants, approve yourself or fabricate host transport fields. If consent is missing, explain where the user can review it in Apps.

## Manifest examples

Static app:
```json
{"schema":2,"name":"Reading Garden","entry":{"type":"static","path":"index.html"},"targets":[{"platform":"android","environment":"web"},{"platform":"ios","environment":"web"},{"platform":"macos","environment":"web"}],"package":{"files":["index.html"],"dependencyLocks":[]},"dataSchema":{"version":1,"compatibleFrom":1},"remix":{"allowed":true,"license":"MIT"}}
```

Python service on mobile Linux:
```json
{"schema":2,"name":"My local tool","entry":{"type":"server","process":"api"},"install":[{"command":"python -m pip install --require-hashes -r requirements.txt","venv":".venv"}],"processes":[{"name":"api","command":"python server.py","venv":".venv","ready":{"path":"/health","timeoutMs":120000}}],"capabilities":["process","network"],"targets":[{"platform":"android","environment":"mobile-linux","architecture":"aarch64","channels":["direct"]},{"platform":"ios","environment":"mobile-linux","architecture":"aarch64","channels":["full"]}],"package":{"files":["server.py","requirements.txt"],"dependencyLocks":["requirements.txt"]},"dataSchema":{"version":1,"compatibleFrom":1}}
```

`venv` and each step's `cwd` are app-relative. Install/build commands terminate; named `processes` remain alive until stopped. `dependsOn` determines service startup order. Readiness uses an HTTP `path` or an exact `output` string. Services must bind `HOST=127.0.0.1` and the allocated `PORT`. Use `LOCAL_APP_DATA_DIR` for durable backend files. Never store durable records only in source, release, build or temporary directories.

## Frontend, services and device features

Use the host's `window.localApp.serviceUrl(name,path)` and `serviceSocketUrl(name,path)` interfaces. Do not hardcode service ports, persist run URLs or proxy arbitrary loopback hosts. Restart invalidates the previous run's service access.

The host reports available native capabilities; declare and request only what is needed. Handle denial or unsupported capability visibly. Device operations use the native broker for the current app and run; no page receives the general Agent tool interface or account/provider credentials. `agent.tools` is a capability requirement only, never an authorization or model configuration.

Applications with executable steps run with this device's user-space access, not a per-app Linux security sandbox. App-owned data and browser profiles prevent accidental state mixing; they do not restrict an authorized Python/Node process from reading other files in the same Linux user space.

## Phone behavior and persistence

Respect safe areas, 44-point touch targets, native keyboard, portrait/landscape layouts and reduced motion. Keep forms and navigation when camera, photo selection, file pickers or sharing return. Offer structured data export/import and handle failed writes.

Mobile background execution is limited by actual OS leases. Show paused/needs-restart state when time expires; do not promise a permanent server or silently restart interrupted builds. Closing a development conversation does not remove the application.

A new release reuses the app's data. Installation and build steps receive a separate preparation data directory. The host retains the old active version if preparation fails. Data-schema changes require a verified migration transaction and are currently blocked before execution; `compatibleFrom` is not evidence that migration has completed. Rolling back code does not roll back databases or IndexedDB, and same-schema service code can still modify its live data.

## Market publication and customization

Public publication is a separate native review action. Export only the explicit package list; exclude credentials, session transcripts, grants, browser stores, databases and private files. Market installation copies code, not author permissions. A permitted customization creates a new app/source project and preserves its upstream attribution; upstream updates cannot overwrite the customized copy.

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
