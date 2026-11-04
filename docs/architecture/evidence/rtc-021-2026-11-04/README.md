# RTC 0.21.1 migration validation

These are raw local validation outputs, not source-audit approvals. `identity.json`
records the integrated Harness commit and exact component trees. The tested core,
media and test-server trees and nested Cargo.lock are unchanged between their
respective test runs and that commit. The evidence-only commit can change the
repository commit without changing those code trees.

Logs are gzip-compressed without modifying their contents; `identity.json`
contains hashes for both the compressed files and their uncompressed bytes.

All commands ran on aarch64 macOS with Rust 1.94.0, a task-owned Cargo home and
one build job. The network tests used local socket access. The nested workspace
was locked and offline; `metadata.json` is its all-features dependency graph.

* `cargo test --locked --offline -p lingxi-webrtc-core --lib -- --test-threads=1`
  passed 28 tests; the two fixture tests were run separately.
* With the pinned `compose.turn.yaml` Coturn image bound to loopback,
  `cargo test --locked --offline -p lingxi-webrtc-core --lib -- --ignored
  --test-threads=1 --nocapture` passed both forced-relay tests, including 20
  allocations closed and recreated on the same source port. `coturn.log.gz` contains
  the unmodified fixture output. Only the task-owned container and network were
  removed afterwards. The server logged an unnamed `Unknown argument` at
  startup. The image entrypoint evaluates `echo -n` to an empty argument;
  `coturn-entrypoint.log.gz` reproduces that expansion without networking.
  The fixture log confirms the image found no config file, then allocated and
  released the tested relays using the explicit command-line settings.
* `cargo check --locked --offline -p lingxi-webrtc-media --tests --features
  gstreamer-media` passed. The repository's `python3 scripts/test-media.py`
  passed 53 media and three mobile tests with its restricted plugin set,
  including actual VP8 and Opus processing.
* The native test-server and its original probe used an isolated loopback port.
  The probe checked HTTP signaling, 16384 exact DataChannel bytes, no microphone
  echo, 50 Opus packets, and separate camera/screen VP8 RTP payloads. This does
  not claim a Docker server, physical devices, or production relay acceptance.

The App's combined graph, security gates and product-level tests are recorded
separately in its worktree. No claim is made here that the 0.21.1 dependencies
have received complete `safe-to-deploy` source audits.
