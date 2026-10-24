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

## Independent Agent tasks

For image recognition, structured model results or approved business tools, add `"agent"` to `capabilities` and `"agent":{"tools":[]}` to the manifest. Use the host's `window.localApp.agent.run/get/subscribe/cancel`, not an embedded API key, the creating chat, or a Remote Control session. Each task has its own context; explicitly pass only the app context needed for this request.

The creating chat may call `LocalApp({"action":"agent_catalog"})` before writing the manifest. It returns public model/tool IDs, never grants or credentials. Request exact tool IDs in the manifest; do not guess MCP IDs. Model selection and individual tool authorization belong to native app details. Permission approvals and questions belong to trusted host UI; the app cannot approve itself.

Persist a request ID before submitting. Retry uncertain submissions with the same ID and exact input; reconnect accepted tasks with `get`/`subscribe` after refresh. Do not automatically resubmit rejected/unconfirmed requests. Handle succeeded, failed, cancelled and interrupted separately. Show structured output only after a succeeded task provides its schema-validated `result.output`; do not extract JSON from Markdown.

Images use `{media_type,base64}`, with no host paths or remote URLs. Native mobile camera/photo pickers return `{mimeType,base64}`; convert the MIME field and resize/compress before submission. The mobile bridge limits the complete request to 24 MiB. Desktop camera access uses standard Web Camera APIs when native capture is absent. Declare camera/photos only where used, retain the draft when a picker closes, and let the user confirm the image before model submission.

Respect native task limits and cancellation. Mobile background Agent execution is currently unsupported; do not promise it. Desktop background Agent permission and ordinary app-process background permission are separate. If no model or required capability is available, show the actual host error and guide the user to native app settings.

## Updates and data

- Find an app with `list`, then `read` its source metadata and latest revision. Edit the source project and update the same ID with `expectedRevision`; never overwrite concurrent work silently.
- Version switches preserve app data and may require new execution/capability consent. New code does not receive permissions from a saved model preference alone.
- Keep data formats backward readable. A changed filesystem `dataSchema` requires the host's verified backup/migration transaction; do not erase data or bypass a migration refusal.
- Use localStorage/IndexedDB for browser records, validate loaded data, report failed saves, and provide export/import for important records. Installation, publication and execution are different lifecycle steps.

## Interface and verification

Use local assets by default; AI/network features must honestly report their connectivity requirements. Support 320 px screens, safe areas, 44 px touch targets, light/dark preference and keyboard accessibility. Label controls and use adequate contrast. Use the Local App Builder plugin for frameworks or larger projects, retaining this schema-2 and host authorization contract. Test reload/storage, denial, cancellation and error recovery as well as the main flow.
