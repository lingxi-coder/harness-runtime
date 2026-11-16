# Independent native display capture reference

This manual comparison uses only the two disposable raw-background fixture
bundles. It requires their existing TCC grants, an unlocked regular desktop,
and a successful raw-background test that moved the target auxiliary window
onto the shadow's desktop. Keep the fixtures running until this comparison
finishes; use the raw-background launcher's exact-identity cleanup afterwards.
Run that cleanup even if reference generation or a comparison fails.

Follow [the raw-background instructions](../raw-background/README.md) through
the test command, postponing its final cleanup until this comparison finishes.
Retain that run's `OUT` as `FIXTURE_OUT`, and run from the SDK root:

```sh
SOURCE="$PWD/crates/platforms/macos-computer-control/examples/fixtures/native-capture"
FIXTURE_OUT="$OUT"
CAPTURE_OUT="$(mktemp -d "${TMPDIR:-/tmp}/lingxi-native-capture.XXXXXX")"
python3 "$SOURCE/configure.py" --fixtures "$FIXTURE_OUT" --out "$CAPTURE_OUT" --host-root com.lingxi.code
xcrun swiftc "$SOURCE/Reference.swift" -module-cache-path "$CAPTURE_OUT/swift-module-cache" -o "$CAPTURE_OUT/reference"
"$CAPTURE_OUT/reference" "$CAPTURE_OUT"
LINGXI_NATIVE_CAPTURE_REFERENCE_DIR="$CAPTURE_OUT" CARGO_PROFILE_RELEASE_LTO=false \
  cargo test --release --locked --offline -p tool-computer-use --lib \
  native_mcp_capture_matches_independent_sck_reference -- --ignored --nocapture
python3 "$PWD/crates/platforms/macos-computer-control/examples/fixtures/raw-background/fixture.py" cleanup --out "$FIXTURE_OUT"
```

`configure.py` reads the portable fixture's `owned.json`, verifies each exact
PID/executable/start timestamp and fixed bundle ID, and derives marker/crop
coordinates from its fresh canvas state. It queries the actual main display ID
and bounds; no machine-specific window or display IDs are supplied. It refuses
stale processes, off-Space windows or out-of-bounds markers, and does not move
windows or activate applications. `--host-root` is the owning brand's bundle
root; the example uses LingXi Code's root.

The Swift reference reconstructs the pinned native ScreenCaptureKit filter,
target dimensions, fractional crop, and AppKit encoding. The Rust test exercises
the actual registered MCP tools, compares owned/foreign 8x8 pixel patches and
the complete static fractional-zoom JPEG. Dynamic desktop pixels are outside
that byte comparison. This is an independent reconstruction, not execution of
the proprietary addon. Generated images and receipts stay in `CAPTURE_OUT`;
never add real screen captures, executables, or generated module caches to Git.

The backing pixel size comes from `CGDisplayCopyDisplayMode` and its pixel
dimensions. Do not assume `CGDisplayPixelsWide` supplies that size for a HiDPI
mode: the measured Retina case reported 1512x982 there and 3024x1964 in the
display mode. The native driver rounds each logical crop dimension times its
backing scale before applying the image budget. A full-display budget can hide
the wrong scale because its aspect ratio is unchanged; a small zoom exposes it.
