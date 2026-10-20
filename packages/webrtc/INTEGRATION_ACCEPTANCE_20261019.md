# WebRTC main integration acceptance — 2026-10-19

The standalone workspace is integrated from checkpoint `19a467080aa757bff58b0b18d49706688f5bdf15`, including its prerequisite base `1a958874`. Existing main had no `packages/webrtc` tree, so importing only the checkpoint's modified files would leave the feature incomplete. This change imports that complete isolated workspace, its upstream licenses and the documented small vendor patches. Owned Rust code is formatted; third-party formatting remains upstream.

The main runtime, SDK dependency, root workspace manifests and lockfile are unchanged. This does not connect WebRTC to the Product Remote service or claim Claude Remote parity. The Claude runtime alignment target remains 2.1.295.

## Fresh checks on the integrated source

| Check | Result |
| --- | --- |
| Core ordinary regression tests | 15 passed; the 2 fixture-dependent tests were executed separately |
| Signaling/test-server regression tests | 4 passed |
| Media regression with the declared plugin fixture and desktop devices | 50 passed |
| Mobile FFI regression with the declared plugin fixture | 3 passed |
| Real Coturn relay-only binary, Opus RTP and two video SSRCs; bad credentials fail closed | Passed |
| Real Coturn allocation release across 20 same-source-port reconnects | Passed |
| Android thermal policy thresholds and continuous cooldown hysteresis | Passed |
| Android Java compilation against API 36 | Passed, deprecated API warnings retained |
| Android JNI compilation with arm64 Android 26 NDK headers | Syntax check passed |
| iOS native capture, probe and Broadcast source against the iOS SDK | Syntax check passed, deprecated API warning retained |
| Cargo metadata, formatting and owned-source whitespace | Passed |

The media fixture is `scripts/test-media.py`; a direct mobile test invocation without its plugin environment correctly failed the fixture prerequisite and was not counted as passing. Reproducible final Cargo runs use `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0` to bound disk use. The final core/server run and the media fixture each passed. Both ignored core tests passed separately using the pinned `compose.turn.yaml` on localhost and one test thread.

Initial disk exhaustion and Docker EOF interrupted earlier attempts. Only task-owned inactive build caches were retired. Following the user's authorization, Docker Desktop was normally restarted; the dedicated Coturn container/network was removed after the successful run. Failure logs remain alongside the final evidence, outside source control.

## Limits

These checks do not establish a fresh Android APK or iOS probe app link, device installation, phone-to-phone relay, restrictive NAT interoperability, TURN TCP/TLS, long-running relay refresh, thermal/energy endurance or Product Remote integration. Existing device receipts remain historical evidence, not new device acceptance for this commit. See `DEVICE_VALIDATION.md` and `README.md` for the remaining platform work.

Evidence is retained in `.buildcache/checkpoint-main-integration-20261019/` in the multi-repository workspace: `webrtc-core-server-final.log`, `webrtc-media-fixture-final.log`, `webrtc-turn-recovered.log`, `turn-fixture-recovered.log`, `android-java.log`, `android-jni-syntax.log`, `ios-probe-syntax.log`, and the thermal-policy receipt/output. No provider call or Native Claude collection was required.
