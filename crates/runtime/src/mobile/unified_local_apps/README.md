# Standalone mobile Local App host

`MobileLocalAppServiceHandle::new(config_home, optional_runtime, platform)` is a process-wide service for one canonical config HOME. It does not create an engine, read a Firebase account, or boot Linux. The Android/iOS FFI packagers export `build_android_local_app_service` and `build_ios_local_app_service`, accepting the existing standalone Linux runtime handle.

Native clients call `request_json(String)` asynchronously. Responses are `{ok:true,value:...}` or `{ok:false,error:{code,message}}`. All service tasks run on the Local App executor. Model tool adapters use `request_for_tool(Value)`: dropping the caller cancels its operation, with a run-scoped cleanup owner. SDK process spawn is held by an independent owner until its returned handle is registered, including on iOS where dropping the native spawn future is not safe. Failed reap retains the live owner and a `stopping` run so Stop can retry.

Canonical library operations use the shared `local_app::v2::LibraryRequest`. Runtime additions are `configureTarget`, `createAuthoring`, `start`, `stop`, `restart`, `repair`, `suspend`, `resume`, and `status`. `start` accepts `{appId,revision?}` and returns `{run,url,origin}`. Native clients cannot bypass runtime preparation via `prepareRun`, `updateRun`, or `activateRelease`. `configureTarget` declares actual platform/ABI/version/channel/native capability support; repeating the same declaration is idempotent. Default support is a plain static web app with no native capabilities. Execution consent is checked before Linux boot/install/build. `suspend` reaps only this app and persists a suspended state; resume creates a new run, never revives a PID.

A staged update prepares a new work directory and services while the prior run is still usable. Only after readiness succeeds does it stop the old run and switch the persistent origin. Preparation failure restores the prior live run. Data compatibility is checked by the library before preparation and activation. A change of data schema version is conservatively rejected with `dataMigrationRequired`; no data transaction is claimed. Install/build/venv steps get disposable preparation data, while actual services receive the persistent data directory. A persistent port conflict is an error, never an origin rotation.

The native first-create flow calls `createAuthoring` and registers the full pending authoring binding before model source editing: the trusted `sourceDirectory`, `workspaceId`, typed `projectRef`, and `authoringSessionId`. The host mints the authoring binding and app ID; the model cannot choose those IDs or bind them. On first publish, the host assigns the app ID and commits the authoring binding with the release transaction. For a staged publish, the trusted original source path resolves authoring identity while the staged directory supplies the release bytes; editing the source after staging does not silently change that snapshot.

The native gateway URL is `http://127.0.0.1:<originPort>/__run/<runId>/<entryPath>`. A registered service is reachable at `/__run/<runId>/__service/<name>/<path>`. Applications receive `PORT`, `HOST=127.0.0.1`, `LOCAL_APP_ID`, `LOCAL_APP_RUN_ID`, `LOCAL_APP_DATA_DIR`, `LOCAL_APP_BASE_PATH`, and dependency `LOCAL_APP_<NAME>_URL`/`LOCAL_APP_<NAME>_PORT` (names uppercase with punctuation replaced by underscores), matching Desktop. Native SDKs derive service URLs from the actual run, not a page-supplied target address. Static nested entry paths retain their directory as the document base.

Root-absolute framework requests are redirected with HTTP 307 into the current epoch only when their full same-origin Referer belongs to that run. This preserves POST bodies and makes module URLs epoch-bound. Old or absent epoch Referers are rejected. Upstream local redirects are similarly rebased. HTTP request bodies and response bodies stream concurrently; SSE and binary WebSocket frames remain byte streams. Stop revokes the listener and existing connections. Host/Origin checks, strict body framing, bounded headers, contained static paths, and host CSP apply. Frames are disabled; external network content requires the declared network capability. The host's app-origin recovery Service Worker is network-first for live navigation and caches only its fixed offline recovery page. It never caches app pages, request payloads, files or Agent results, and it never replays a POST.

Mobile application content opens in the **OS-selected installed external browser**, not an embedded WebView/WKWebView. Browser support is feature-detected; authors must not assume a browser brand or package. Desktop retains its own window. Static HTML receives the run-scoped `/__run/<runId>/__host/webmcp.js` script before application scripts. The pinned upstream polyfill preserves a native `document.modelContext` when present. HTML returned by an App server is proxied without rewriting, so a server-backed page that needs the fallback must include `<script src="/__host/webmcp.js"></script>` before code that uses `document.modelContext`. That root-absolute alias redirects only when its Referer belongs to the current run, to `/__run/<runId>/__host/webmcp.js`; the gateway does not rewrite HTML or inject a script into server responses. Static pages receive the same script automatically. The gateway also serves `/__run/<runId>/__host/sdk.js` (root-absolute `/__local_app_sdk.js` redirects only from the current run). Authors load the Agent SDK before SDK calls; native Desktop preloads remain authoritative there. File and camera selection use browser inputs and OS permissions, independently of LingXi execution and Agent grants. Native management UI must not claim it can revoke external browser site permissions or erase its site data.

The gateway serves the shared official AG-UI SDK 1.0.3 and its authenticated
custom transport binding. `agent.createClient` returns an official AbstractAgent;
`agent.file(File)` constructs standard content parts. Thin run/start/send methods
accept RunAgentInput, and task event batches contain standard BaseEvents with a
separate replay sequence. `forwardedProps["com.lingxi.localApp"]` holds versioned
host options; it is not a permission grant. Tasks/Chats CRUD remains host-owned.
No compatibility aliases, old pending-store loader or prototype-message migration
is supported. Obsolete Agent data is deleted only within its known private scope.

Standard inline image/document parts are validated and decoded only at the host
execution boundary, with eight files, 25 MiB/file and 50 MiB/new message limits.
Private Chat assets preserve exact standard message semantics for cold history;
their filesystem references never leave the host. The bound HTTP transport must
reconstruct and validate the complete standard input before admission. There is
no public two-stage upload or arbitrary-path input. Host/Origin/current-epoch
Referer and strict framing checks remain required. Stop and release replacement
revoke existing connections and task access.

The browser retains full pending input in its new App/revision-scoped IndexedDB
store for native foreground handoff. The wake link and ready marker identify the
standard runId; they contain no prompt, files, paths or credentials. Claiming the
matching reservation submits only that saved input. Ordinary reload reads events
or history; it never replays a model request. Native approval remains outside
page JavaScript. Detaching a subscription differs from explicit cancellation.

On iOS26 the native foreground action obtains a real BGContinuedProcessingTask
before opening the browser and reserves the exact App/revision/runId for a
bounded receive window. Admitted work has its own task limits under the finite
OS lease. iOS27 LongRunningIntent receives a complete RunAgentInput and returns
the actual terminal AG-UI event JSON, not an admission receipt or a new reply
format. Task metadata alone never proves completion: success requires the real
standard terminal event. On Android the runtime is owned by a foreground service
with visible Stop controls. Neither platform promises an always-running process.

Host tests cover the gateway and lifecycle, plus an explicitly fake SDK boundary for cancellation/reap faults. They do not substitute for the required signed Android/iPhone Python/Node service tests or generated bindings/native package checks.
