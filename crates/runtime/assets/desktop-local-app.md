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
