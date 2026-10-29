# Mobile Linux execution

Mobile Shell and Git use the Mobile Linux runtime when available. Native host
paths, guest-visible paths and app-relative paths are distinct. Obtain the
working directory, mounts and execution capability from the current host/tool
result; do not translate a Desktop path by guessing.

Android uses its supported PRoot-based environment; supported physical iOS
builds use their iSH-based environment. An iOS simulator is not evidence that a
real guest runtime is present. App-store channel policy and installed rootfs
state can change which operations are available.

For execution, inspect an available Shell/Git tool's schema and returned status.
If the runtime is unavailable, explain its reason and native setup/recovery
entry. Do not run the command in the host shell and label that Mobile Linux
success. Rootfs status/verification/repair/reset are host/FFI management
operations; they are not all model tools just because the SDK declares them.
Reset is not an automatic remedy for an ordinary path or dependency error.

Keep per-project dependencies and virtual environments in the intended project.
Do not copy Desktop binaries, `venv` or `node_modules` into another architecture,
replace the shared toolchain, or move durable App data into build caches.
Long-running services and PTYs depend on native lifecycle/background policy.
An accepted launch must be followed by output/exit/readiness evidence appropriate
to the task. Reconnection does not prove an interrupted process continued.

SDK integrations own probe, boot, process handles, streaming output, stdin/PTY,
mounts and shutdown. Read the selected `mobile-linux-api` contract and platform
wrapper before writing code; Shell input parameters are not that SDK contract.

Source anchors: mobile-linux-runtime `crates/mobile-linux-api`; Harness
`crates/runtime/src/mobile/host/{linux_runtime,assembly}.rs`,
`crates/tools/{shell-mobile,git-mobile}`.
