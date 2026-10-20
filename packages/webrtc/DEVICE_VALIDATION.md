# Device validation checkpoint — 2026-10-16

These are standalone acceptance applications. Product host integration and Remote
have not been accepted.

## iPhone connectivity

- Initial Rust TCP error: EHOSTUNREACH, diagnostic code -1065.
- Native Network.framework preflight distinguished `wifi_denied` (2) from
  `local_network_denied` (3). The user enabled the application's WLAN permission.
- The next connection reached the server but HTTP /answer returned 400.
- Docker reported `too many voice candidates`. Four media sections repeat host
  candidates on a multi-interface phone. Video sessions now admit at most 128
  occurrences; audio retains 32. Duplicate occurrences still count before peer
  mutation, and the 64 KiB SDP size bound is unchanged.
- Fourteen core tests passed. The additional boundary regression establishes a
  real connection at 128 and rejects 129 before applying the offer or answer.
- Physical iPhone audio/camera then connected with error 0, nonzero microphone
  and decoded playback peaks, and returned camera frames.

## Stutter diagnosis

Observed camera return rate before optimization: about 1.8–2 fps. Docker CPU was
about 1%, so the fixture server was not CPU-bound.

A 15-second Time Profiler recording attached to the physical iPhone process.
The exported sample weights totaled 18.526 CPU seconds. Inclusive sample shares
(overlapping categories, not additive):

| Path | CPU sample share |
| --- | ---: |
| NativeAudio service | 80.2% |
| Rubato resampling | 54.2% |
| Sonora processing | 25.1% |
| Native YUV plane packing | 10.5% |
| VP8 encoder-related frames | 0.4% |

Numerous hot leaf frames were unoptimized Rust iterator, pointer precondition and
SIMD helper checks. The mobile build helper now uses opt-level 2 and disables dev
debug assertions. Host regression test settings are unchanged.

Preview polling also capped display at 10 Hz. Both fixtures now poll at 30 Hz,
update text/receipt files at 2 Hz, and select the active camera or screen source
instead of alternating independent sources into one preview. iOS ReplayKit's
capture gate now targets 30 fps. Camera and shared VP8 encoder targets were also
raised to 30 fps; screen encoding scales the longest side to at most 1280 pixels.

After optimized builds first restored the configured 15 fps, the user requested
higher frame rates. The 30 fps build was installed on both phones. iPhone reported
30.0 fps with actual microphone input and default speaker peak 0; Android reported
28.6–30 fps. Aggregate camera counters confirm sustained rates near 30 fps (iPhone:
2996 frames over 99.8 seconds; Android: 6484 over 216.5 seconds).

Mirroring displayed “iPhone microphone is not available from Mac” and a black
camera preview. It was closed; after manual unlock the microphone and camera
worked directly on the device. A later diagnostic reinstall completed codec
self-tests but remained UIApplicationStateInactive before media startup. The
fixture now records startup state separately and disables automatic display sleep
during active testing. Direct foreground camera and microphone capture now pass.

## Screen capture diagnosis

ReplayKit initially returned -5833 (capture-stack startup failure) with zero video
callbacks. The video-only AVCaptureSession now leaves AVAudioEngine's shared audio
session configuration alone; ReplayKit's own camera and microphone are explicitly
disabled. A subsequent physical run started ReplayKit and returned app-screen VP8
frames. These changes and a fresh launch were applied together, so the individual
cause of -5833 has not been isolated.

The screen fixture now includes a continuously moving marker. Native capture APIs
may emit fewer samples for static content; the fixture separately records capture
callbacks, successful Rust submissions and decoded network return frames. The
30 fps gate retains its deadline phase rather than resetting to each accepted
callback, preventing scheduling jitter from repeatedly skipping the next frame.
With camera and screen active, an iPhone 23.566-second window returned camera
27.28 fps and screen 26.73 fps; speaker peak remained 0 and failure code 0.

A second physical iPhone Time Profiler capture (15 seconds, both video streams)
sampled 11.296 CPU seconds. The shared worker accounted for 0.67 CPU seconds;
YUV packing was the largest leaf at 12.8%, and scalar GStreamer scaling routines
were another 10.8%. The shared YUV packer now bulk-copies contiguous visible rows
while retaining validated bounds, row padding and interleaved chroma handling.
The post-change physical iPhone rerun returned 1503 camera frames and 1503
screen frames over 50.600 seconds: 29.70 fps for both streams. Default speaker
peak remained 0 and transport/media failure remained 0. Short reporting windows
still fluctuate, so this is an aggregate throughput result, not a frame-pacing
or latency percentile guarantee.

Android single-app MediaProjection consent was completed using the normal system
picker. Logs identified a repeated content-size callback loop: every identical
1080x2340 notification resized the same 590x1280 virtual display and replaced its
ImageReader surface, generating another notification. The adapter now preserves
the existing surface when scaled dimensions match. Capture and network return
then started; a 31.071-second window returned camera 28.23 fps and screen 17.44 fps.
The Activity also rebinds to an existing foreground service after system-picker
recreation. Native C++ preview conversion now builds with -O2. System gfxinfo subsequently
reported 0.68% janky frames, with 99th-percentile UI draw time 6 ms, yet screen
throughput still lagged. The Android capture gate now schedules against image
presentation timestamps and accepts the sample nearest each 30 Hz deadline,
with counters separating rate-limited and invalid-timestamp drops. A 21.631-second physical window then measured capture 24.92 fps, successful
submission 23.35 fps, and decoded screen return 23.11 fps, with zero timestamp
rejections. Camera return remained 28.76 fps. The moving-marker fixture now uses
vsync invalidation rather than a 33 ms delayed draw, so the test producer itself
can continuously exceed the 30 fps capture target. The final physical rerun measured 21.832 seconds: native screen callbacks
59.50 fps, successful submissions 30.05 fps, and both camera and screen decoded
return 29.41 fps. No invalid timestamps, transport/media failures or default
speaker output were recorded. The original free-rotation setting was restored.

## Acoustic feedback correction

The user reported a sharp sound during the original microphone-to-same-speaker
echo fixture. Both device fixtures and the local server were stopped immediately.
Microphone RTP is now consumed without returning it to the same device. This
removes the fixture's acoustic feedback loop; audio-quality acceptance is separate.

A manual button requests a synthetic 200 Hz, one-second Opus speaker fixture at
peak 0.01 with 80 ms fades. The generator decodes all 50 packets and verifies
960 samples per packet and decoded peak below 0.02 (observed 0.01098). Playback is
off until requested, expires after 50 packets, cannot be extended by repeated
requests, and is dropped if scheduling falls behind rather than burst-played.

Real host-to-Docker validation passed: 16384 exact binary DataChannel bytes,
no received microphone loopback packet, and exactly 50 downlink packets after the
explicit speaker command. Device defaults also show speaker peak 0 while microphone
capture continues. Forty-nine media tests, the small-native-stack regression and
three server tests passed. Subjective voice/audio quality remains unaccepted.

Diagnostics also detect DataChannel closure and require a bounded binary echo
heartbeat. Capture callback counts alone must not keep a disconnected fixture in
the connected state. Android background server-loss acceptance passed in the follow-up below; iOS remains pending.

## Remaining acceptance

- Subjective audio quality and frame-pacing/latency percentile acceptance beyond
  the aggregate dual-stream throughput measurements above.
- MediaProjection/ReplayKit stop/restart, permission cancellation, interruption,
  route changes and background/foreground behavior.
- iOS cross-app/system broadcast requires a Broadcast Upload Extension; current
  ReplayKit capture is limited to the diagnostic application.
- Desktop/CLI host/device integration, forced TURN and complete four-client media
  acceptance precede Remote implementation and byte-differential claims.

## Research-first follow-up

After the user requested online investigation before further coding, implementation
changes were paused. The following primary sources were checked; the final Android
measurement above validates the version already installed before that request.

- [scrcpy screen frame-rate documentation](https://github.com/Genymobile/scrcpy/blob/master/doc/video.md#frame-rate):
  capture is content-driven and variable; static-content fps is not a throughput
  ceiling. The moving-marker experiment distinguishes this from codec/network loss.
- [Google WebRTC ScreenCapturerAndroid](https://webrtc.googlesource.com/src/+/refs/heads/main/sdk/android/api/org/webrtc/ScreenCapturerAndroid.java):
  screen capture ignores the requested framerate, serializes capture changes, and
  resizes the existing virtual display on modern Android. Its stop path explicitly
  unregisters the projection callback before stopping to avoid recursive callbacks.
- [Android ImageReader documentation](https://developer.android.com/reference/android/media/ImageReader#acquireLatestImage()):
  real-time consumers should acquire the latest frame and promptly close images;
  holding too many acquired images can stall production. Current adapter uses
  acquireLatestImage and try-with-resources.
- [Google WebRTC framerate-controller fix](https://webrtc.googlesource.com/src/+/59947d28711f9ecec460e7ae83a769be5bd70794%5E%21/):
  the upstream change reports a requested-20/actual-15 fps regression and replaces
  its older controller. This is a relevant reference case, not proof of an identical
  root cause here. The timestamp controller's jitter-aware phase handling provides
  a concrete algorithm and regression-test reference for any future changes.
- [Android MediaProjection callback contract](https://developer.android.com/reference/android/media/projection/MediaProjection.Callback):
  capture-size changes require updating both display and surface. No exact public
  duplicate-size-loop case was established; that diagnosis comes from local logs.

Future unresolved issues should first be matched against primary documentation,
maintainer issues and mature source implementations, then tested against local
stage counters before selecting a minimal implementation change.


## Android thermal follow-up (2026-10-16)

Online research preceded the implementation. The [Android Thermal API guide](https://developer.android.com/games/optimize/adpf/thermal)
recommends proactively reducing workload before severe throttling and delaying
quality recovery until the device can sustain it. The existing
[GStreamer synchronous video converter](https://gstreamer.freedesktop.org/documentation/video/videoconverter.html)
provides reusable color conversion without an extra asynchronous pipeline.

Before changes the Samsung SM-S9310 reported real thermal status 3 (SEVERE),
HAL skin 45.8 C and battery 45.7 C. The probe was stopped to cool. Charging was
active throughout the observations, so these readings do not isolate app power.
The capture device thread used 57.1% of one CPU in the original sample; the main
thread used 32.1%. The diagnostic fixture had an indefinitely running motion
marker and rendered its own returned shared screen, producing continuous redraw,
capture and encode work.

Changes:

- Replace scalar two-pass RGBA conversion with one cached synchronous GstVideoConverter
  per capture thread. Reuse owned input/output buffers, preserve foreign-plane
  bounds checks, zero padding, explicit limited-range BT.601 and owned output.
- Keep motion generation off by default. The manual benchmark lasts at most
  30 seconds and ends on backgrounding or moderate heat. Suppress returned
  self-screen preview outside that bounded benchmark, avoiding capture feedback.
- At moderate thermal status, admit camera and screen frames at 15 fps before
  entering shared conversion/encoding. Request a supported <=15 fps Camera2 range
  where available; otherwise retain the admission bound. Normal target remains
  30 fps. Recovery requires 30 continuously cool seconds, preventing rapid toggles.
  Severe heat stops media and rejects restart until cooling; no OS thermal
  overrides are used. The UI and receipts expose thermal status and target fps.
- Suspend Activity polling in the background. A separate service health check
  still detects transport loss and releases devices. Preserve the stop reason
  and write a separate lifecycle receipt even after the native handle closes.
- Unregister MediaProjection callbacks before stopping projection, following the
  upstream WebRTC stop sequence; preserve the projection foreground-service type
  when Start is pressed during sharing.

With optimized conversion and the full dynamic preview benchmark, a 23.883-second
physical sample measured camera 29.94 fps, screen 30.02 fps, screen callbacks
60.00 fps and successful submissions 29.98 fps, with failure 0 and no default
speaker output. The capture thread sampled 24.0% CPU instead of 57.1%; main thread
23.5% instead of 32.1%. These are short thread samples, not an energy measurement.
Thermal status reached 2, so this does not establish sustained cool 30 fps.
The subsequent adaptive build on the actually warm device reported target 15 fps,
actual camera approximately 15.1 fps and failure 0. A subsequent 183.096-second
camera/audio run averaged 14.998 decoded camera fps with failure 0 throughout.
Real HAL skin temperature fell from 42.7 to 42.3 C and AP from 48.4 to 46.9 C;
thermal status remained 2. This observation includes camera preview but no screen
projection, and is not comparable to the full dual-video workload as an energy
benchmark. Battery remained about 42.7 C while charging. The app sampled about
80% of one CPU in this reduced workload. Media was stopped after measurement.

Fifty media tests and the mobile small-native-stack test pass. Android and iOS
Rust static libraries build with the converter; the Android APK builds and runs.
A host-only thermal policy regression checks normal operation, immediate hot-state
reduction, interrupted cooldown and 30-second continuous recovery:

```sh
mkdir -p /tmp/lingxi-thermal-policy
javac -d /tmp/lingxi-thermal-policy \
  platform/android-probe/app/src/main/java/com/lingxi/webrtc/probe/ThermalVideoPolicy.java \
  platform/android-probe/app/src/test/java/com/lingxi/webrtc/probe/ThermalVideoPolicyTest.java
java -cp /tmp/lingxi-thermal-policy com.lingxi.webrtc.probe.ThermalVideoPolicyTest
```

Android background server-loss check passed: send the Activity to Home, stop the
Docker peer, and observe lifecycle receipt failure -1226 with running=false,
stopping=false and sharingScreen=false. Restore the Docker peer afterward.
The iPhone media receipt collected during that experiment was stale and is not
evidence of iOS server-loss acceptance. The new iOS library has been cross-built,
but its updated app has not yet been installed or device-validated.

Long-duration energy/thermal acceptance, motion under the thermal cap, real-device
cooldown recovery and severe-stop behavior remain separate checks. Policy tests
exercise thresholds without artificially heating or overriding the device.


## Host-only forced TURN acceptance (2026-10-16)

The shared Rust API now accepts `SessionConfig` / `IceConfig` before socket
creation. Defaults preserve direct ICE. Configuration bounds eight server entries,
sixteen total URLs, 2048 bytes per URL and 1024 bytes per credential field; upstream
URL parsing validates schemes and TURN authentication requirements. Debug output
omits URLs, usernames and passwords. Relay-only mode requires a TURN server.
The pinned async driver supports UDP relay; secure or TCP STUN/TURN is explicitly
rejected before creating a peer rather than silently skipped by the driver.
TCP/TLS TURN implementation remains pending. The current mobile C ABI accepts bounded ICE JSON and validates relay-only candidate admission; device forced-relay acceptance remains pending.

The official [Coturn Docker instructions](https://github.com/coturn/coturn/blob/master/docker/coturn/README.md)
and [current server option reference](https://github.com/coturn/coturn/blob/master/README.turnserver)
were checked before deployment. `compose.turn.yaml` pins official Coturn
4.18.0-r0-alpine by digest, publishes only 127.0.0.1:3478 and sixteen UDP relay
ports, and uses explicit local test credentials. CLI and DTLS are off by this
version's default; TLS is disabled for this host-only fixture. The binary's file
capability requires NET_BIND_SERVICE even with unprivileged listener ports; this
matches the [upstream container issue](https://github.com/coturn/coturn/issues/1126).
Other capabilities remain dropped. Loopback peers are allowed only for this local
two-peer acceptance topology; it is not a production deployment configuration.

```sh
docker compose -p lingxi-webrtc-turn -f compose.turn.yaml up -d
CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_DEV_INCREMENTAL=false \
  cargo test -p lingxi-webrtc-core --locked --offline forced_turn -- --ignored --nocapture
docker compose -p lingxi-webrtc-turn -f compose.turn.yaml down
```

Real test passed: offer and answer each contained two candidate occurrences, all
`typ relay`; exact 16384-byte binary data passed in both directions; Opus RTP and
two distinct video SSRCs passed. The video payloads exercise VP8 packetization/RTP
transport, not a new full encoder/decoder acceptance. An incorrect TURN password
yielded no direct candidate fallback. The test is explicitly ignored in ordinary
CI and requires the local fixture. Fifteen ordinary core tests pass separately.

The first attempt used wildcard binding while Coturn published loopback only.
Some Wi-Fi/VPN-interface allocations never completed before the existing
15-second gathering deadline. Explicit 127.0.0.1 binding matches this fixture and
passed; it does not establish multi-interface gathering recovery. The diagnostic
Docker peer used by phones remains separate. This acceptance does not yet prove
phone-to-phone relaying, real restrictive NAT traversal, relay refresh longevity,
TURN/TCP/TLS, production credentials or Remote byte parity.
