---
name: local-app
description: Build and maintain device-local schema-2 apps with interactive WebMCP tools or independent host Agent tasks when useful.
session-modes: chat, code
---

# Local apps

Create an application project, publish it into this device's Apps library, and verify the published application. A project holds source code; an ordinary conversation is development history; an app owns its released code and data independently of either. Local publication never uploads to the public market. The `LocalApp` tool schema and current host capabilities are authoritative.

## Workflow

1. Build when the user has made a clear request or approved a plan. Ask only for missing decisions that change the result. A one-off chart or explanation can use an inline visualization; an application belongs in the Apps library.
2. Use the source Project and Code Session selected by the native Create/Modify flow. Keep app source in that Project; do not move it because the conversation cwd changes or use an unrelated active Project. Mobile source paths come from the current brand's Linux user; obtain them from the host. The host binds the trusted Project/Session pair. If a saved source is missing, use native Restore Source or the permitted Remix/copy flow rather than silently binding another Project. Never invent or send private source-session, transport, permission, grant or approval fields.
3. A simple static app can be one complete HTML document with inline CSS/JavaScript, published as `LocalApp({"name":"…","summary":"…","html":"…"})`. The host constructs its schema-2 package. For multiple files, frameworks, services or native Agent/device capabilities, create a project containing `lingxi-app.json` and publish with `bundle_path`. If no source Project was selected, use the native flow's distinct `.apps` authoring directory.
<!-- if-shell -->
Inspect the actual platform and available runtime with the `{{SHELL_TOOL}}` tool. Mobile commands run in LingXi's Linux user space. Keep dependencies and Python virtual environments inside the app project; do not install global packages, replace shared runtimes, or copy Desktop `venv`/`node_modules` to mobile Linux. A long simple document may use `file_path` instead of embedding HTML. Check scripts and important interactions before publication.
<!-- end-if-shell -->
4. Write the current schema-2 manifest. Declare only supported targets and needed capabilities; keep lockfiles and list exact package files. Bind services to `127.0.0.1` and the allocated `PORT`; store durable backend data in `LOCAL_APP_DATA_DIR`. Install/build steps use separate preparation data and must not alter live user records. A declared source package alone does not guarantee reproducible builds.
5. For Modify, call `LocalApp` `read` first, edit the saved source Project, then publish to the same app `id` using the observed `expectedRevision`. If the revision conflicts, inspect and merge the newer source before retrying; never blindly retry against a new revision.
6. Fix validation errors, publish, then inspect `start`, `status` and `logs` where available. A source directory, temporary development server, build or browser preview is not a published app or device acceptance.
7. Explain what works and what was actually tested. Do not describe source tests, a browser preview or a successful build as physical-device or real-model acceptance.

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

Static apps do not require Linux processes. A Node/Python app needs a compatible execution target and the user's exact-release consent in the native Apps page. On mobile, require an available Mobile Linux runtime rather than assuming Desktop commands or arbitrary native executables will run. Check architecture, runtime/channel and capability support. Do not modify the shared Linux toolchain. Package only declared source/build outputs and lockfiles; exclude credentials, local grants, user data, browser stores and dependency caches.

For services, `venv` and step `cwd` are app-relative. Install/build commands terminate; named processes can run until stopped, fail, or their host ends, subject to the platform's lifecycle. `dependsOn` sets startup order and readiness uses an HTTP `path` or exact `output`. Obtain URLs with `await window.localApp.serviceUrl(serviceName)` and `await window.localApp.serviceSocketUrl(serviceName)` rather than hardcoded ports or saved run URLs; use the manifest's service name, and restart revokes the old run's service access. Declare only needed native capabilities and handle denial visibly. An `agent.tools` declaration is a capability requirement, not a grant. App pages never receive general host tools or provider credentials. App processes run with this device user's access; they are not isolated by a per-app Linux security sandbox.

## Mobile service lifecycle and recovery

On iOS, opening an App prepares its runtime in the foreground; it does not make an idle service permanently runnable. The system can suspend the host, leaving its process unable to serve requests, or terminate it and its processes. User-initiated Agent work may receive a finite continued-processing lease that the system can cancel. A user force quit generally prevents background relaunch until the user opens the host again. After termination or device restart, do not promise an immediate relaunch or that any in-flight work continues. Do not use idle timers, audio, location or another unrelated background mode as a keepalive.

Design a visible unavailable/reopen state. When the native host is available again, obtain fresh service URLs from its current App session; a previous process, port or URL may no longer be valid. Store durable backend records in `LOCAL_APP_DATA_DIR` and browser records in localStorage/IndexedDB as appropriate, validate them on read, and preserve user data when recovery fails. Browser storage belongs to its browser context and may be unavailable or cleared. Do not claim every browser can reach a suspended host or that reopening automatically restores an in-flight task.

For an existing Agent task, keep its `threadId` and `runId` and inspect that same task read-only with `tasks.get`, events or `connectAgent` when the host is reachable. A host restart marks unfinished runs interrupted; it does not resume their execution. Surface the actual terminal or unavailable state, including uncertain side effects. Never automatically replay a request or tool action; let the user explicitly choose a new attempt after reviewing the input and any effects. The SDK's initial delivery during an authorized iOS handoff is part of starting the original request, not permission to submit it again during recovery. Restore or reconnect only within the existing App and authorization boundaries.

## Page tools with WebMCP

For an interactive app with useful actions, expose its user-facing operations through the standard `document.modelContext` API. Register tools that call the same validated app functions used by the UI, validate domain rules again in each callback, and update/render the page from the result. Use `inputSchema` for argument shape; do not treat schema validation as authorization or business validation.

The host provides the standard API: Desktop and static Mobile pages receive the polyfill automatically, which leaves a native implementation intact. A Mobile server-backed page must include `<script src="/__host/webmcp.js"></script>` before app code that uses the API. A `file://` preview does not have the installed app's host API or prove that it works. Do not overwrite `document.modelContext` or add a manifest capability for it. Reuse an already-installed browser integration for verification; do not silently install/configure one, grant access or open global CDP. If the installed app browser is unreachable, report that it was not verified. The user may choose an integration from Public settings.

Register page tools once for the page lifetime with an `AbortController` signal; abort it during teardown/navigation. Keep the callback small and route it through the same validated function as the UI:

```js
let summaryTools;
async function registerSummaryTool() {
  const context = document.modelContext;
  if (!context || typeof context.registerTool !== "function") return;
  if (summaryTools) return;
  const lifecycle = new AbortController();
  summaryTools = lifecycle;
  try {
    await context.registerTool({
      name: "reading.get_summary",
      description: "Read the current reading summary",
      inputSchema: { type: "object", properties: {}, additionalProperties: false },
      annotations: { readOnlyHint: true },
      execute: async (input, { signal }) => {
        if (!input || typeof input !== "object" || Array.isArray(input) || Object.keys(input).length !== 0)
          throw new TypeError("Expected an empty object");
        if (signal.aborted) throw new DOMException("Aborted", "AbortError");
        return await getReadingSummary({ signal });
      }
    }, { signal: lifecycle.signal });
  } catch {
    lifecycle.abort();
    if (summaryTools === lifecycle) summaryTools = undefined;
    // Keep ordinary UI controls available when tool registration fails.
  }
}

function removeSummaryTool() {
  const lifecycle = summaryTools;
  summaryTools = undefined;
  lifecycle?.abort();
}
void registerSummaryTool();
window.addEventListener("pagehide", removeSummaryTool);
window.addEventListener("pageshow", event => { if (event.persisted) void registerSummaryTool(); });
```

In the installed app browser, check `document.modelContext.getTools()`, execute `reading.get_summary` with `{}` using `executeTool()`, and verify its result against the page. Check that a non-empty input is rejected, teardown/navigation removes the registration, and a BFCache `pageshow` restores it. If `document.modelContext` or `registerTool` is unavailable, keep the ordinary UI usable and do not claim Agent access.

WebMCP exposes page actions to an Agent; `window.localApp.agent` is a separate app-to-host AG-UI interface. Use each only for its direction and purpose.

## Calling the host Agent with AG-UI 1.0

Declare `"capabilities":["agent"]` and `"agent":{"tools":[]}` in the schema-2
manifest when the App needs model understanding or Agent execution. Use the
host-injected `window.localApp.agent` and the official AG-UI 1.0 message/event
contract, backed by the bundled official SDK 1.0.3. Never embed a provider key,
reuse the creating conversation, or expose a Remote Control chat endpoint.

For a short-lived independent request where losing the browser session is
acceptable, construct one standard user message and fresh run and thread IDs.
For an independent Mobile run or any independent request that needs recovery,
prefer the durable `tasks.start` flow below and save those IDs before submission.
The File helper creates the standard image/document content part:

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

Chat creation is a LingXi host operation, separate from the AG-UI
run protocol. Generate a lowercase UUIDv4 `requestId` and persist it with the
exact creation options before calling `chats.create({requestId, title?})`.
Omitted title and an explicitly supplied title are distinct inputs; the returned
handle ID equals `requestId`. A lost response or timeout does not prove that
creation failed. On reopening the same App revision in the same browser origin,
recover saved options with `chats.pendingCreations()` and use only
`chats.creationStatus({requestId, title?})` to check; `null` means not currently
observed and the original request may still arrive. Never automatically replay
create. Only an explicit user retry may call `create` with the same saved ID and
unchanged options. A different input conflicts, and a deleted Chat cannot be
recreated with the same ID. `chats.forgetCreation(requestId)` removes a resolved
browser receipt, not the Chat. Receipts are partitioned by App and revision.
If the App revision changes, browser storage is cleared, or the original ID is
lost, show the Chat list for the user to inspect rather than guessing a match.
If `chats.list` reports `unavailableChats`, show those IDs as unavailable
without deleting or replacing them automatically. Its `nextCursor` covers both
available and unavailable Chats; `hasUncertainChats: true` reports an issue
anywhere in the App, not just the current page. An absent flag means the host
did not report that detail, not that every Chat was verified.
If existing Agent or Chat storage has an unsupported or unverifiable format,
or a required marker is missing, show it as unavailable, preserve the existing
data, and never automatically delete or recreate it.

For a Mobile independent Agent task or any independent request that needs
recovery, use `tasks.start` with the same official input, then
`tasks.get/list/events/subscribe/cancel`. Task metadata includes `threadId` and
`runId`; persist those IDs before submission, so they remain available if the
page switches or the response is lost. For a persistent Chat, send through its
Chat handle instead of `tasks.start`; persist the handle ID (`threadId`) and
the fresh `runId` before sending, and use `chats.open(threadId)` on return.
Then reconnect read-only with
`createClient({threadId}).connectAgent({runId})`; a reload never submits another
model request. Identical input with the same `runId` is idempotent; changed input
under that ID is rejected. Do not use the retired Agent-run
prompt/attachments/requestId or custom state-event protocol. Old Agent records
are discarded, not converted.

Treat an immediate `tasks.get` `task_not_found` as a lookup miss, not a model
outcome. Keep the exact saved input and `runId`; retry only read-only lookup or
reconnection, never `run` or Chat `send`. Await `connectAgent` or the task-event
subscription until the standard terminal `RUN_FINISHED` or `RUN_ERROR` event;
a history refresh or running-state snapshot is not completion. After successful
completion, validate any structured `RunAgentResult.result` against the declared
`outputSchema` and app-specific rules before side effects.

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
Intents require complete standard input. The SDK handles supported handoff and
read-only reconnection; App code keeps the standard run contract. No path
promises indefinite execution or automatic replay. Stop/revocation/release
replacement cancel work; default limits are one active task per App, 20 turns and
ten minutes, and an App may only lower native limits. Background Agent
permission is separate from process permission.
Listen for `localApp:agent-handoff-error`. When `detail.code` is
`pending_request_missing`, use its `runId` to explain that this browser storage
context cannot access the saved input; direct the user to the original tab or a
browser context that shares its storage. Never resend or replace the `runId`
automatically, label it a model failure, or infer a browser brand or private mode.
A new request requires the user's explicit choice after reviewing the input.

Use the current Local App Builder examples for one-shot file understanding and
persistent attachment Chat. Keep business validation and explicit user save
confirmation in the App. Verify actual configured model, errors, File contents,
refresh, cancellation, Chat cold resume and saved data. Builds, mock outputs and
standalone browser rendering are not real-provider or physical-device evidence.

## Updates and data

- Use `list` to find an app, then `read` its source metadata and latest release. Edit the bound source Project and update the same ID with the observed `expectedRevision`; never overwrite concurrent work silently.
- Version switches preserve app data and may require new execution/capability consent. Failed staged preparation keeps the prior release; rolling code back does not reverse database or IndexedDB writes. New code does not receive permissions from a saved model preference alone.
- Keep data formats backward readable. A changed filesystem `dataSchema` requires the host's verified backup/migration transaction; `compatibleFrom` alone is not proof. Do not erase data or bypass a migration refusal.
- Use localStorage/IndexedDB for browser records, validate loaded data, report failed saves, and provide export/import for important records. Installation, publication and execution are different lifecycle steps.
- Publishing to the public market is a separate native review action. Export only declared package files, excluding credentials, transcripts, grants, browser stores, databases and private files. Market installation copies code, not author permissions; a permitted customization creates a separate app and preserves upstream attribution.

## Interface and verification

Use local assets by default; AI/network features must honestly report their connectivity requirements. Support 320 px screens, safe areas, 44 px touch targets, light/dark preference and keyboard accessibility. Label controls and use adequate contrast. Use the Local App Builder plugin for frameworks or larger projects, retaining this schema-2 and host authorization contract. Test reload/storage, denial, cancellation and error recovery as well as the main flow.
