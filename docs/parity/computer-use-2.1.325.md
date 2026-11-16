# Computer Use parity — Claude Code 2.1.325

Reference verified on November 15, 2026 (America/Los_Angeles). Official
[npm latest metadata](https://registry.npmjs.org/@anthropic-ai/claude-code/latest)
and the [native manifest](https://downloads.claude.ai/claude-code-releases/2.1.325/manifest.json)
resolve to 2.1.325. The installed darwin-arm64 reference matches the manifest's
347,406,976 bytes and SHA-256
`84590fefe6338be9087f5814b61f9a54c5f21524163a4c127cb5a808e31009f2`.

## Actual permission failure

The installed LingXi failed `request_access` at 2026-11-13 17:58:12 local.
Its error was `AX probe stage=hit_test, status=Some(-25208), hasValue=Some(false)`.
Correlated TCC logs allowed Accessibility and ScreenCapture (`authValue=2`,
`error=null`). They attribute bridge-server PID 53160 to responsible main app
PID 35289, `com.lingxi.code.development`. No responsible-process mismatch was
observed. The app was already authorized; the extra target-dependent AX probe
was the immediate failing gate. These logs do not establish successful input
or capture. `-25208` is AX NotImplemented; prior `-25204` failures are
CannotComplete, also insufficient on their own to establish TCC revocation.

The 325 CLI adapter calls native `tcc.checkAccessibility()` and
`checkScreenRecording()` and combines their booleans. LingXi now uses the same
TCC-state boundary, removing the extra global hit test and its fallback.
Target checks remain on actual input operations. Missing OS access still
revokes application/capability grants and opens the existing setup panel
through explicit `request_access`. Successful setup still requires a separate
application-selection request. TCC setup replies now preserve upstream model
text, logical error flag (including the newly-granted reply) and telemetry.
The setup reply’s only text substitution is `Claude desktop app` → `LingXi desktop app`.

## Complete tool and protocol scope

The user confirmed that parity includes the full tool surface and protocol
structure, including background app tools, takeover and teach mode. The old
combined `computer(action)` owner is now hidden from model discovery. Exact
separate `mcp__computer-use__*` tools share one permission, policy, lock,
cleanup and screenshot state owner. Trusted builtin MCP registration prevents
an external server from replacing these tools. Native projection requires the
complete authorized display primitive set, rather than admitting a privileged
owner because one harmless wrapper is visible.

The pinned descriptor corpus covers 42 distinct tool names, 93 distinct tool
descriptors and 295 capability profiles. Each tool's name, description and JSON
schema are replayed from the selected native artifact. The full Darwin tools
JSON reference SHA-256 is
`809fc8e0448c412e99ee498de8fe77df15f34cc2223e1561ec0a338459098bc0`.
Runtime discovery advertises the profile supported by the actual host; optional
features are not advertised merely because their descriptor exists.

Implemented routes include all twelve app_* handlers, application catalog and
grants, foreground display/keyboard/mouse/clipboard/batch, takeover and release,
and teach access/step/batch. The Desktop and CLI distinguish application access,
OS setup, takeover and teach decisions. A setup-only or control-only reply cannot
grant application scope, flags or preferred mode. Ordinary execution checks are
read-only; only explicit setup can request OS permission UI.

Background operations use target-owned AX windows and per-effect policy,
cancellation, process/window identity and user-collision checks. Process-directed
raw events preserve the foreground cursor and focus. Off-Space discovery uses
read-only WindowServer metadata with exact process ownership; input still needs
a live owned AX target. Space movement verifies the destination and resulting
window membership. Runtime availability gates both optional tools together.
Foreground operations retain the turn lease and register the recovered Escape
lifecycle; expected synthetic Escape is consumed without aborting its own call.
Cleanup removes the hook, releases owned inputs and leases, and ends teach mode.

Native MCP image blocks remain intact in stored tool results and are normalized
only at provider egress. Anthropic and OpenAI image mappings retain the exact
PNG/JPEG payload bytes. This is separate from proving an identical JPEG encoder.

## Differential evidence

All nine replay runners reject a different full reference hash and selected
source-range hashes. They evaluate reviewed ranges in isolated JavaScript
contexts without native/network credentials.

| Surface | Differential coverage |
| --- | --- |
| Tool descriptors | 295 profiles, 93 descriptors, 42 tool names |
| Application policy / keys | 722 classification and 431 key cases consumed by Rust |
| Native normalization / lookup | 774 normalization and 5 lookup reference cases |
| Foreground results / scaling | 79 reference outputs, 75 production cases, 7 budgets, 3 focus cases, 9 numeric cases |
| Background result formatting | 208 reasons, 12 labels, 5 summaries |
| Grant helpers / catalog | Labels, IDs, denials, tiers, suggestions, 8 exact catalog cases, 4 window locations |
| Interaction results | 56 takeover/teach/validation cases |
| TCC | 4 boolean states and 12 setup transitions |
| Clipboard helpers | 11 reference effects, including multiline paste and click-tier stashing |
| Builtin MCP instructions | Exact 5,116 UTF-8 bytes / 5,084 UTF-16 units |

The full descriptor corpus fixture SHA-256 is
`dce7ae937bb15c25a5b3fe080c14f9b3044f5aa2f86d8206e555196bf7a0e99d`.
Nine Node replay checks bind the official artifact and recovered ranges. The
foreground runner also compares three fractional zoom transforms by their exact
IEEE-754 bytes. Registered-tool and provider-egress tests preserve all 93 native
descriptors and image payloads. Real native acceptance is explicit and ignored
by ordinary CI; the checked-in fixture launcher operates only its own bundles.

## Native capture and input

On supported macOS, display capture uses `SCScreenshotManager`, the recovered
application-exclusion predicate, and native target dimensions. Zoom sends its
fractional logical source rectangle directly to ScreenCaptureKit with
`scalesToFit`; it does not round the crop into the portable pixel path. The
original CGImage reaches the display encoder's `NSImage -> TIFF ->
NSBitmapImageRep -> JPEG` chain at quality 0.75.

Ordinary window capture instead uses a desktop-independent window filter and
`NSBitmapImageRep(CGImage) -> JPEG` at 0.75, including the recovered child-window
setting when available. These are distinct reference encoder chains. Masked
panel composition remains a conservative host path and is identified separately
from ordinary native capture.

Raw input uses exact process/window routing and a dynamically resolved
window-local event API. Every drag step rechecks ownership and cancellation;
a cancelled press is balanced only to its original still-owned subject. Space
movement dynamically validates its native ABI, excludes full-screen destination
Spaces, and confirms membership before reporting success. Neither mechanism
silently switches Spaces or activates the target. Test setup may create and
switch to its own disposable windows and full-screen Space.

Target AX enablement sets `AXManualAccessibility` as recovered from the native
addon. Successful and unsupported-attribute statuses are cached per launch;
other statuses remain diagnostics and do not become a new TCC permission gate.

## Client protocol

Computer Access interaction records change the native UniFFI layout. The client
contract is therefore 34.0.0, with all new enum variants and fields recorded in
the contract index. Standalone Kotlin/Swift bindings generated from fresh v34
metadata and Swift type checking pass. This does not claim freshly packaged
Android or iOS binaries; their standard builds regenerate matching bindings.

## Reproduce

```sh
for runner in scripts/tests/computer_use*325_oracle.mjs; do
  node "$runner" /absolute/path/to/claude --check
done
CARGO_INCREMENTAL=0 cargo test --offline -p platform-macos-computer-control -p tool-computer-use --lib -- --test-threads=2
CARGO_INCREMENTAL=0 cargo test --offline -p tool-api -p orchestrator --lib image
```

## Final source validation

After integration with upstream `a9ef128d`, the focused native and tool suites
pass 43 and 205 tests respectively; the explicit GUI cases remain ignored in
ordinary CI. Registry/invoker checks pass 31 tests. The UniFFI-enabled client
checks pass 35 tests across Computer Access, snapshots, enum ordinals and the
version/contract index.

The final real fixture run establishes the full 42-tool host profile, an actual
window move from its disposable full-screen Space to the current desktop, text
selection by background drag, raw Command-A selection and four balanced raw
mouse presses/releases. Foreground, cursor, current Space, activation count and
fixture content remain unchanged during tool effects.

Actual registered MCP screenshot/zoom calls also pass exact owned/foreign 8x8
RGBA patches and geometry checks. The static fractional zoom JPEG matches the
independent same-host reference byte for byte: 8,679 bytes, SHA-256
`232ad19b4849bba507c76f530fdd8e521c2a58c24e300c8a7b07b6086adb6bde`.
This comparison uses the display mode's backing pixel dimensions; on HiDPI,
`CGDisplayPixelsWide` may instead report Quartz logical dimensions and is not a
valid substitute. Eight synthetic sRGB/P3/alpha PNG inputs independently match
both recovered AppKit encoder chains.

The portable fixtures and independent source references are checked in under
[`examples/fixtures`](../../crates/platforms/macos-computer-control/examples/fixtures/).
Generated JPEGs, real screen images, binaries and receipts stay outside Git.

## Evidence limits

Exact descriptor and selected model-result bytes are established by the pinned
replay corpus. The native implementations are reconstructed host mechanisms;
the CLI contains optional app-scoped hooks without the complete desktop host
implementation. Loaded-system ABI checks and real fixture effects are separate
evidence from equivalence to that unobserved host.

Controlled encoder and capture checks do not prove every display, color profile,
owned-modal composition, or host callback. Nonempty denied-list callback shapes
and the UTF-8 native effect boundary for lone-surrogate input remain outside the
proven domain. The recovered core retains session snapshots, but does not reveal
the desktop host's turn cleanup. LingXi clears owned snapshots and leases at the
end of a user turn; that is an explicit host lifecycle policy.

The earlier signed local development candidate passed complete package smoke
and run10 (37 checks, eight background foreground/cursor invariants), and was
installed with rollback copies. That package predates this final raw-input,
Space and native capture work. Its package receipt must not be presented as a
fresh build of the final immutable SDK pin.

Task evidence is under `../artifacts/computer-use-325-20261113/`, including
correlated TCC diagnosis, recovered source ranges, native fixture receipts,
encoder comparisons and the publication manifest. REA parent
`ev_a77269a8c87b4841928ee9dddb2623dc6d43e6edd15c806fb2f20d34ce166d55`
records static parsing of three selected chunks, with 5,938 conservative semantic
unknowns; it does not assert runtime reachability.
