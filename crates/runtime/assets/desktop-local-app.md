---
name: local-app
description: Create, run and maintain desktop Local Apps (本地应用), including multi-file frontends, Node/Python backends and native capabilities; publish into Apps and open an independent window.
session-modes: chat, code
---

# Desktop Local Apps

You are on a desktop. Build a real application project and deliver it with `LocalApp`. The host keeps the project in its application library and opens an independent window. It owns installation, process startup, logs, restart, stop and saved application data. The library remains usable after this chat is closed.

## Workflow

1. Use the user's requirements and sensible defaults. Use existing frontend-design, React and testing skills when relevant. The local-app-builder plugin can supply templates, but no plugin is required and no template or framework is mandatory.
2. Inspect the environment with `Bash`. Install missing tools and dependencies through `Bash` and continue: Node, Python, uv, npm/pnpm/Bun, Vite, TypeScript and normal project dependencies are supported. Prefer existing compatible installations; install project packages in the project and Python packages in a virtual environment. Keep lockfiles. Do not stop just because a package is initially absent. Existing Bash permissions still apply.
3. Write a project with `lingxi-app.json` at its root. Use the manifest below. Static apps may use build output; server apps may use Node, Python, FastAPI, Gradio or another locally installed runtime. Normal modules, assets, network requests, Web Workers, service workers and frameworks are allowed. Do not apply mobile-only offline, one-file or no-backend rules.
4. Run install/build and test the actual UI with the available browser tools. Keep installation and startup commands in the manifest so subsequent launches do not need a model. Every server must bind to `127.0.0.1` and use its injected `PORT`; dependent services receive `LOCAL_APP_<UPPERCASE_SERVICE_NAME>_URL`.
5. Publish with `LocalApp({"bundle_path":"/absolute/project/path"})`. A simple document can instead use `name`, `summary` and `html` or `file_path`. To update, pass the existing `id`. Publishing copies source, skipping .git, node_modules, Python virtual environments and .env secrets. Application data belongs in `LOCAL_APP_DATA_DIR`, localStorage or IndexedDB, not the code directory.
6. Call `LocalApp({"action":"start","id":"..."})`, then inspect `status` and `logs`. Start opens the registered independent window after install/build and readiness succeed. Test this registered application too; a development server or an HTML path alone is not delivery. Repair failures through Bash and republish. Tell the user where to open the app and what was actually tested.

## Manifest

Static example:
```json
{"schema":1,"name":"Reading Garden","summary":"Daily Chinese character games","entry":{"type":"static","path":"dist/index.html"},"install":[{"command":"npm ci"}],"build":[{"command":"npm run build"}]}
```

Python server example:
```json
{"schema":1,"name":"My local tool","entry":{"type":"server","process":"api"},"install":[{"command":"python -m pip install -r requirements.txt","venv":".venv"}],"processes":[{"name":"api","command":"python server.py","venv":".venv","ready":{"path":"/health","timeoutMs":120000}}]}
```

- Each install/build step has `command`, optional relative `cwd`, `env`, `venv`, and optional `python` version. `venv` is relative to the application root even when a step uses another `cwd`, so services can share an environment. A versioned venv uses `uv`; install uv through Bash first if absent. Install steps create a missing virtual environment; start steps activate the same one. Publication excludes virtual environments containing `pyvenv.cfg`, including custom directory names.
- Each process adds a unique `name`, optional `dependsOn` names, and `ready`. Readiness uses an HTTP `path` by default (`/`); a background worker can instead emit an exact `output` string. Install/build commands must exit. Processes remain alive until the app stops.
- Static frontends reach a backend at `/__services/<name>/...` using ordinary fetch. The host allocates service ports. Do not hardcode port 3000/8000.
- For WebSockets use `new WebSocket(await window.localApp.serviceSocketUrl(name, path))`; the URL points to that app's current loopback service. Configure the backend to accept its `lingxi-app://<id>` origin. Server entry `path` optionally selects a relative landing page.
- Dock/Finder launches include common user tool directories and fall back to LingXi's bundled Node/Bun. Install additional runtimes through Bash into a discoverable location (for example `~/.local/bin` or mise shims); record any required custom PATH in the step's `env`.
- `capabilities`: `process`, `files`, `clipboard`, `notifications`, `camera`, `microphone`, `agent`. Install/build/process steps imply `process`. The host asks once for new app capabilities and remembers the user's choice; the manifest cannot approve itself. Agent apps may declare `"agent":{"tools":[]}`; its tool IDs request host catalog entries, not permission grants.

## Native APIs

The window provides `window.localApp`: `info()`, `clipboard.read()/write(text)`, `notify(body)`, `process.exec(command)`, and `files.pickDirectory()/list(root,path)/read(root,path)/write(root,path,text)`. A picked folder returns a `token` used as `root`; `"data"` addresses the app's own data folder. Declare the matching capability and handle denial. Camera and microphone use standard web APIs and OS permission prompts. Local programs run with the user's native account permissions. No LingXi account or model credentials are exposed.

## Calling an Agent from an app

For image recognition, validated structured model output or authorized tool tasks, declare `agent` and use `window.localApp.agent`. Each call is an independent app-owned Harness task, not the chat that created the app. Pass the app context explicitly. The host selects the App's configured model and owns credentials, tool authorization and trusted permission/question UI. Do not create a second direct-provider engine, embed provider keys, or route same-device calls through Remote Control.

```js
const request = {
  requestId: crypto.randomUUID(),
  prompt: 'Describe the meal and state uncertain ingredients and portions.',
  images: [{ media_type: 'image/jpeg', base64: jpegBytesAsBase64 }],
  outputSchema: {
    type: 'object', additionalProperties: false, required: ['description'],
    properties: { description: { type: 'string' } }
  },
  tools: [], maxTurns: 4, timeoutMs: 120000
};
// Persist request before sending. An uncertain retry must reuse it unchanged.
const { runId } = await window.localApp.agent.run(request);
// Persist runId and reconnect after reload; do not submit a new task.
const unsubscribe = window.localApp.agent.subscribe(runId, event => {
  if (event.type === 'state') renderRun(event.run);
  else renderProgress(event.phase);
});
const current = await window.localApp.agent.get(runId);
// Explicit user cancellation: await window.localApp.agent.cancel(runId).
// unsubscribe() removes the listener without cancelling the task.
```

- `run` accepts required `requestId` and `prompt`; optional `images`, `outputSchema`, `tools`, `maxTurns`, `timeoutMs`. It returns `{runId}`, not the final result. App-provided limits can only reduce host limits (defaults: one active task per App, 20 turns, 10 minutes). The app cannot choose credentials, chat IDs, workspace directories or permission modes.
- Images use `{media_type,base64}` with base64 bytes, not a data URL or local file path. Resize camera/file images before sending; the host validates encoding, signature, supported media type and size limits. Obtain a user gesture before capture/upload.
- `get` returns a run with `state`, `sequence`, timestamps, chosen `model`, `limits`, optional `result`, `usage` and `error:{code,message}`. Handle `queued`, `running`, `succeeded`, `failed`, `cancelled`, `interrupted`. Successful structured tasks return schema-validated `result.output`; unstructured tasks may return `result.text`. Never parse a failed response's Markdown as a successful structured result. Usage may contain `inputTokens`, `outputTokens`, `costUsd` and `turns`.
- `subscribe` returns an unsubscribe function synchronously and replays current state before live events. Events have `runId`, increasing `sequence`, and `time`: `{type:'state',run}` or `{type:'progress',phase,text?}`. Ignore duplicate/out-of-order events and stale `get` responses. Phases include `starting`, `thinking`, `tool`, `waiting_for_permission`, `waiting_for_answer`. Direct the user to the trusted LingXi UI when waiting; the app cannot grant its own tools or answer approvals.
- Before writing a tool-enabled manifest, call `LocalApp({"action":"agent_catalog"})` from the creating chat to discover public tool IDs. This does not authorize the app or expose credentials/configuration.
- Declare only needed opaque host tool catalog IDs in `agent.tools` and request a subset per run. No-tool tasks use `[]`. The user grants each tool in Apps. Never derive an MCP authorization ID from a display name or assume all tools on a granted server are allowed. A tool or connection change requires the host to validate its grant again.
- Persist request identity before IPC. Same ID plus identical input is idempotent; different input with the same ID is rejected. Remove temporarily stored photo payloads once accepted, keep `runId`, and use `get`/`subscribe` on reload. Interrupted tasks are not automatically replayed. Only an explicit new user attempt starts another task.
- Closing the window cancels by default; continued Agent execution needs its separate background grant in Apps. Stop, revoke, deletion and application revision replacement cancel tasks. Release camera tracks and listeners on teardown, but do not cancel a task merely because the page reloads. Keep saved results in the app's own storage and require confirmation before recording uncertain model estimates.

Use the local-app-builder plugin's Desktop calorie example when available for camera/choose-photo, request recovery, validated food candidates, editable portions and explicit save. Its calorie ranges are app business logic and are labeled model estimates unless an actually authorized nutrition tool supplies verifiable data. Verify the registered app with a real configured model, reload/cancel/error paths and saved data; mocked outputs or standalone browser rendering do not prove host execution. Mobile retains its separate document-only workflow until its host advertises this Agent API.

## Maintenance

`list` finds app ids; `read` returns the manifest and source location; `status` reports its live phase; `logs` returns recent output; `stop` terminates the app-owned processes. A new revision keeps app data. Closing the window normally stops the app; the user can enable background running in Apps. Quitting LingXi stops all apps. A visualization can be published using `visualization_id`; its confirmed state is carried into the standalone app.
