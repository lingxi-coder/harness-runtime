# Embedded WebRTC implementation

This isolated Cargo workspace keeps the new media dependency graph out of existing
product/mobile builds until its platform adapters are ready. It is real Codex-derived
Rust transport, not a second Agent runtime. See NOTICE.md for the frozen upstream.

## Currently implemented

- Host-owned Tokio executor; no helper process, stdio protocol, process exits or global environment mutation.
- Opus RTP sender and bounded incoming RTP buffers, candidate validation and TCP admission limits (32 candidate occurrences for audio; 128 for four-section camera/screen sessions, validated before peer mutation).
- Reliable ordered binary DataChannel, bounded ingress (saturation closes the channel), 16 KiB fragment limit, send deadline.
- Explicit close plus best-effort cleanup when dropped on a running executor.
- Host-supplied bounded UDP STUN/TURN settings and relay-only policy; local forced-relay transport acceptance.
- Shared Sonora AEC/noise processing, Rubato resampling and Opus audio codecs.
- GStreamer VP8 encoding/decoding, independent camera/screen tracks and bounded RTP playout.
- Native PCM/YUV/RGBA capture and render ports, mute epochs and capture/playback clocks; cached synchronous RGBA conversion.
- Android diagnostic thermal admission, bounded motion benchmark and independent background connection cleanup.
- Desktop CPAL adapter and standalone Android/iOS device diagnostics with static GStreamer plugins.
- Authenticated local Docker DataChannel/VP8 return peer with manual finite Opus speaker test: POST /offer, POST /answer.

## Validate

```sh
cargo test --manifest-path packages/webrtc/Cargo.toml -p lingxi-webrtc-core --locked
```

Network tests require permission to bind loopback UDP/TCP sockets. They exercise
real negotiation, binary bytes, Opus RTP, packet loss, slow TCP and invalid SDP.

From this directory:

```sh
export LINGXI_TEST_TOKEN=local-webrtc-test-token
# For phones use the Docker host's LAN IP, not a container address.
export LINGXI_ADVERTISED_IP=127.0.0.1
sh scripts/prepare-docker.sh
docker compose up -d --build
curl http://127.0.0.1:18080/health
```

All signaling requests require Authorization: Bearer <LINGXI_TEST_TOKEN>.
POST /offer returns {id,sdp}. An answering peer applies the offer, creates an
answer, finishes ICE gathering, then POSTs {id,sdp} to /answer. The peer accepts
the server-created lingxi-agent channel; received bytes are echoed as
binary without JSON conversion, except the reserved diagnostic speaker command.
Microphone RTP is consumed without same-device echo; the manual speaker button
requests a quiet one-second synthetic Opus fixture. Idle peers expire after 120 seconds; unanswered
offers expire after 30 seconds. This is an echo fixture, not the Remote relay.
HTTP defaults to host loopback. For local phone testing, set both
LINGXI_SIGNALING_BIND_IP and LINGXI_ADVERTISED_IP to the Mac's LAN address,
connect the phone to the same Wi-Fi, and enable iOS Local Network permission.
This fixture uses local HTTP; production signaling/authentication is separate.
Stop with docker compose down.

## Remaining planned work (not yet accepted)

1. Integrate the validated libraries into product Desktop/CLI/mobile host owners; the standalone diagnostics are not product integration.
2. Large-record framing, control/file lanes, TURN TCP/TLS and mobile/real-NAT forced-relay acceptance (host UDP relay acceptance passed). The current C ABI accepts bounded ICE JSON; this is not device relay acceptance.
3. Complete device audio/camera/screen/lifecycle acceptance, including the implemented iOS system-wide Broadcast Upload Extension. App-screen capture and system broadcast have separate owners and acceptance records.
4. Production Remote relay with account/device auth, persistent journal, cursors and epochs.
5. Four-client Remote UI/CLI and existing Harness control-plane integration.
6. Claude 2.1.295 frozen runtime reference and the self-hosted Remote contract matrix. Remote uses LingXi service/protocol ownership; no Anthropic service integration or byte-parity claim is implied.

## Current evidence (2026-10-16)

The Rust transport compiles for aarch64-apple-ios and aarch64-linux-android.
The narrowly scoped nix target fix is described in vendor/README.md.
A real host-to-Docker test returned 16384 arbitrary binary bytes unchanged.
Reproduce it after starting Compose:

```sh
LINGXI_TEST_TOKEN=local-webrtc-test-token cargo run --locked -p lingxi-webrtc-test-server --example probe
```

Current local checks additionally pass 15 transport tests, three server fixture tests, 50 shared media tests and the mobile small-native-stack regression.
The latter protects Sonora initialization from the 512 KiB native callback stack;
AEC construction runs on a dedicated 16 MiB diagnostic worker instead.

Android and iPhone physical-device codec fixtures both passed 33 Opus packets and
5 VP8 frames. Both phones connect to Docker with actual microphone capture and
camera VP8 return near 30 fps. Default speaker output is silent; microphone RTP
is not echoed back to its own speaker. Subjective audio quality remains separate.
iPhone app-screen capture and Android single-app MediaProjection return frames
through the same Rust VP8 transport. Simultaneous camera/screen performance and
remaining lifecycle checks are detailed in DEVICE_VALIDATION.md. iOS system-wide
broadcast is implemented but its cross-app device acceptance is still pending. The diagnostic UI shows connecting/error states and
stops native devices after failure.

Device test applications are in platform/android-probe and platform/ios-probe.
Build the Rust static library using scripts/mobile-cargo.py with the official
GStreamer 1.28.8 platform SDK, then build the matching Gradle/Xcode project.
iOS links the platform GStreamer archive once at the application boundary.

Run shared media checks with:

```sh
CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_DEV_INCREMENTAL=false python3 scripts/test-media.py
```

No full-device or mobile TURN acceptance, production Agent tool execution, Remote
functionality or Claude byte alignment is claimed.

Device performance and acoustic feedback findings, acceptance boundaries and
current checkpoints are recorded in [DEVICE_VALIDATION.md](DEVICE_VALIDATION.md).


Use `RtcSession::with_config(SessionConfig { ice: IceConfig { servers, relay_only: true }, ..Default::default() })`
for host-supplied relay configuration. Credentials belong to the existing host
owner. UDP STUN/TURN is currently supported; TCP/TLS is explicitly rejected.
The separate loopback-only Coturn fixture and ignored transport acceptance test
are documented in DEVICE_VALIDATION.md. Native device probes accept explicit ICE configuration; defaults use direct ICE.
