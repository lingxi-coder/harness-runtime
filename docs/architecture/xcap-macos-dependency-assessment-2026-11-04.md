# xcap dependency and macOS capture assessment (2026-11-04)

## Scope and recommendation

For the current Product and Harness consumer, evaluate a macOS-only copy of
the published xcap 0.9.8 crate, retaining its macOS implementation and common
API while removing other platforms and their manifest dependencies. Use the
archive checksum below to review provenance, and call the upstream
`friendly_name` API for display labels. This removes an unreachable platform
subgraph at its source; it does not silence or filter any advisory. The copy
will need a clear maintenance owner and upstream comparison on future updates.
This is a candidate, not approval to ship: it needs a real macOS consumer build,
a full locked Product graph, and screen/permission behavior checks.

The Product graph checked at
`17d987cdd1ad05a4072a3e445f6b3c020a28758f` resolves Harness
`ae5a65b1f4816cd26a06d462573ebc9841f14f96`. Its only xcap parent is
`platform-macos-computer-control` on `cfg(target_os = "macos")`; xcap's
zbus edge is on `cfg(target_os = "linux")`. Those target conditions cannot
both hold for one build. The unfiltered, all-target dependency graph still
contains the packages, and the actual supply-chain check reports their
advisories. The [reduced metadata evidence](evidence/xcap-product-baseline-2026-11-04.json)
records the exact package IDs, targets, command, and original JSON SHA-256.
This conclusion is specific to that Product graph and Harness integration;
it does not claim that xcap is generally macOS-only. The current Product
main observed during this comparison is
`5ce972b8c1a547f54808114681327cb731155fcf`; a Git diff from the metadata
snapshot shows no changed TOML file or Cargo lockfile and no changed xcap
references in Rust/TOML files. The top-level Cargo.lock SHA-256 is
`0fa2e3751d53c3c3e6d455aad3cb5ecaf826909f6314ecd8caf8d005f998f015`.
A fresh locked, frozen, all-features metadata run on 5ce972 returned 1052
packages and byte-identical JSON to the original snapshot. The Harness
comparison branch was fast-forwarded to
`ac70a2bd150f913d90535947cc28c8fa0824883d` before preparing this report.
Before the candidate commit, it also merged main
`274e8f3ea6b955bccdf0534af63126eca69f5437`; that commit changes seven
branding-related source/baseline files, not the xcap consumer or Cargo
manifests/lockfile.

Harness builds its real backend only on macOS and returns `None` elsewhere
([source](https://github.com/lingxi-coder/harness-runtime/blob/ac70a2bd150f913d90535947cc28c8fa0824883d/crates/platforms/macos-computer-control/src/lib.rs#L14-L35)).
Its capture policy builds an opaque canvas, composites individually captured
authorized windows in reverse enumeration order, and masks unapproved windows
([source](https://github.com/lingxi-coder/harness-runtime/blob/ac70a2bd150f913d90535947cc28c8fa0824883d/crates/platforms/macos-computer-control/src/macos/capture_policy.rs#L19-L79)).
Changing backend therefore requires validation of per-window capture, owner,
geometry, ordering, and permission behavior, not just an image of the display.

## Options examined

| Option | Evidence and tradeoff |
| --- | --- |
| Published xcap 0.9.8, unchanged | Maintained upstream release, but its Linux manifest still requires zbus 5.17, so upgrading the version alone does not remove the advisory path. It also references the Linux wayshot Git branch. |
| xcap upstream refactor `9780e04ee954edfdfb4044f5ec402a5b90cf4b7f` | Its [manifest](https://github.com/nashaofu/xcap/blob/9780e04ee954edfdfb4044f5ec402a5b90cf4b7f/Cargo.toml#L69-L81) refers to `ashpd` Git `main`. A full SHA on xcap does not make a fresh downstream resolution of that transitive branch immutable. Its [screenshot path](https://github.com/nashaofu/xcap/blob/9780e04ee954edfdfb4044f5ec402a5b90cf4b7f/src/linux/wayland_capture.rs#L84-L102) attempts wlroots screencopy after any portal error, including denial/cancellation. Its [recorder construction](https://github.com/nashaofu/xcap/blob/9780e04ee954edfdfb4044f5ec402a5b90cf4b7f/src/linux/wayland_video_recorder.rs#L61-L110) returns the stream while local session/proxy bindings go out of scope; it connects PipeWire [without a portal-provided remote FD](https://github.com/nashaofu/xcap/blob/9780e04ee954edfdfb4044f5ec402a5b90cf4b7f/src/linux/wayland_video_recorder.rs#L123-L128). The [capture thread and stop method](https://github.com/nashaofu/xcap/blob/9780e04ee954edfdfb4044f5ec402a5b90cf4b7f/src/linux/wayland_video_recorder.rs#L295-L322) do not show quit/join. These are source observations, not a verified runtime failure or proof of the resolved advisory graph. |
| Local xcap 0.5.2 Linux zbus-6 fork | Implemented and tested on actual Linux ARM64 D-Bus with five portal tests under each of the default and Tokio runtime features. Its portal worker retains the session and handles rejection without fallback. It keeps Linux code the current product cannot reach and carries more custom capture implementation. The inherited PipeWire capture thread still lacks an explicit quit/join; portal-worker lifetime tests do not prove complete capture-thread cleanup. |
| macOS-only xcap 0.9.8 source copy | Removes Linux/Windows/etc. code and dependencies while keeping the official macOS implementation, including recorder source. This introduces an intentionally narrower vendored dependency and update responsibility. The suggested consumer adaptation is necessary; compile and runtime behavior still need validation. |
| New ScreenCaptureKit-based backend | Would change the capture implementation and entail substantial permission, window filtering, geometry, packaging, and OS support verification. A screenshot-manager API has newer OS availability; existing helper scripts mention macOS 13, but these scripts alone do not establish the application's minimum OS. No equivalent replacement was demonstrated. |

Published xcap 0.9.8 archive SHA-256:
`da270fd7c8581d43d731cb690bcc791fc0a7b3bc96e131a4b6552d0379640a9e`.
The archive's `.cargo_vcs_info.json` identifies upstream revision
`ef9e2a3892c8b972b5964aece131192d5777c660`; the published crate is
[Apache-2.0](https://github.com/nashaofu/xcap/blob/ef9e2a3892c8b972b5964aece131192d5777c660/LICENSE).
The previous published 0.5.2 archive identifies revision
`ec943c954ed37bd8fc2c9035f26fb447c1a6b140`.

## macOS behavior comparison

- In 0.5.2, `Monitor::name()` uses `NSScreen.localizedName()`. In 0.9.8
  [`name()` returns a display model label and `friendly_name()` obtains the
  localized name](https://github.com/nashaofu/xcap/blob/ef9e2a3892c8b972b5964aece131192d5777c660/src/macos/impl_monitor.rs#L133-L143).
  Use `friendly_name()` in the consumer to preserve the intended display label;
  0.9.8 has an upstream model-name fallback.
- Both releases enumerate on-screen windows in CoreGraphics order and capture
  an individual window with `OptionIncludingWindow`. The newer release
  additionally logs a screen-recording preflight warning. See
  [0.5.2 enumeration](https://github.com/nashaofu/xcap/blob/ec943c954ed37bd8fc2c9035f26fb447c1a6b140/src/macos/impl_window.rs#L160-L193),
  [0.9.8 enumeration](https://github.com/nashaofu/xcap/blob/ef9e2a3892c8b972b5964aece131192d5777c660/src/macos/impl_window.rs#L161-L201),
  and [0.9.8 per-window capture](https://github.com/nashaofu/xcap/blob/ef9e2a3892c8b972b5964aece131192d5777c660/src/macos/impl_window.rs#L375-L384).
  Upstream filters windows during enumeration; a source comparison alone does
  not establish the privacy behavior for every window or permission state.
- Both implementations use `CGWindowListCreateImage` for these images, not
  ScreenCaptureKit; see [0.9.8 capture](https://github.com/nashaofu/xcap/blob/ef9e2a3892c8b972b5964aece131192d5777c660/src/macos/capture.rs#L10-L20).
  The 0.9.8 macOS capture and monitor sources are byte-identical to the
  corresponding files at refactor commit 9780e04.
- The existing `image = "=0.25.6"` comment was about avoiding edition 2024
  dependencies. The workspace uses Rust 1.94.0. Cached image 0.25.10 declares
  Rust 1.88 and moxcms 0.8.1 declares Rust 1.85; the historical edition
  constraint does not justify retaining that exact pin. Resolution and
  compilation still have to be checked against the actual graph.

This comparison is source/metadata evidence, not a macOS build, a live capture,
or a full Product supply-chain pass. Those validations remain required for the
candidate and again after integration with the final Harness revision.

## Candidate validation

The candidate copied the 0.9.8 macOS source and shared public wrapper files
byte-for-byte from the archived crate, preserving the upstream license and
manifest. It adapted the display label to `friendly_name()`, removed the
obsolete image pin, and updated the Harness lock to image 0.25.10. No Linux
capture implementation or advisory exception was introduced.

On the candidate Harness tree, `cargo metadata --format-version 1
--all-features --locked --frozen` succeeded for the complete, unfiltered
743-package graph. It contains one local xcap 0.9.8 edge from the macOS
consumer and no `zbus`, `zbus_names`, `zvariant`, or
`zvariant_derive` package. `cargo deny` 0.19.8, using this metadata and
RustSec database revision `965bf9ad083b58e6340d65829d85217b18be9290`,
passed advisories, bans, licenses and sources with the existing configuration.
Warnings include the already-yanked `clipboard-win` 5.4.1 and duplicate
versions. The [command and output evidence](evidence/xcap-macos-candidate-2026-11-04.json)
includes the initial failed attempts and the successful check; no warning was
reclassified or ignored for this result.

This is still not evidence of a macOS consumer build, live capture, permission
behavior, the final Product graph, or final CI. Those checks remain necessary
before considering the dependency migration complete.
