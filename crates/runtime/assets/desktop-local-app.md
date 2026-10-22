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
- `capabilities`: `process`, `files`, `clipboard`, `notifications`, `camera`, `microphone`. Install/build/process steps imply `process`. The host asks once for new app capabilities and remembers the user's choice; the manifest cannot approve itself.

## Native APIs

The window provides `window.localApp`: `info()`, `clipboard.read()/write(text)`, `notify(body)`, `process.exec(command)`, and `files.pickDirectory()/list(root,path)/read(root,path)/write(root,path,text)`. A picked folder returns a `token` used as `root`; `"data"` addresses the app's own data folder. Declare the matching capability and handle denial. Camera and microphone use standard web APIs and OS permission prompts. Local programs run with the user's native account permissions. No LingXi account or model credentials are exposed.

## Maintenance

`list` finds app ids; `read` returns the manifest and source location; `status` reports its live phase; `logs` returns recent output; `stop` terminates the app-owned processes. A new revision keeps app data. Closing the window normally stops the app; the user can enable background running in Apps. Quitting LingXi stops all apps. A visualization can be published using `visualization_id`; its confirmed state is carried into the standalone app.
