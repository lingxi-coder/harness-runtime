# Remote Control and WebRTC

Remote Control connects an approved viewer to a selected host's sessions.
Account identity, device approval, administrator policy, host switches and
session/Bot switches govern access. A ready connection is not permission for
every session or media source. Use the native connection/status interface and
tools actually exposed in the current session.

WebRTC is a transport, not a model-callable remote-management API. The product
control link negotiates a separate media peer. Control/session messages,
fallback encrypted tunnels and camera/audio streams have different data paths.
Do not describe Firebase signaling or a control connection as the media stream.

For “use my connected phone's camera”, identify the selected host and source,
read the `media` topic, and check its live source/readiness results. Avoid
opening local hardware when the request names a remote device. A connected
phone does not automatically grant camera, microphone, screen or file access.

The current CLI `remote-control` handler can report `NOT_IMPLEMENTED`; do not
recommend it as an established remote hosting service. Desktop/native settings
are the product route when available. Connection loss, revocation and host
shutdown must be reported as availability changes, not successful continuation.

Bot-capable viewers can list enabled Desktop Bots and open their chats through
the approved connection. They execute on that computer, not on the phone or in
a new cloud worker. See the `bots` topic for task and permission ownership.

For SDK work, inspect `RtcSession`, ICE, DataChannel and codec contracts at the
chosen revision. RTP/audio/video plumbing does not authorize media capture.

Source anchors: Harness `packages/webrtc/{core,media}`; product
`packages/remote-control-client/src/{media,host_media}.rs`,
`apps/electron/src/main/remoteControl` and
`apps/cli/host/src/commands/remote_control.rs`.
