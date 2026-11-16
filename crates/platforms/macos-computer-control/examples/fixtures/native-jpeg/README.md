# Independent native JPEG reference

The eight checked-in PNGs are synthetic inputs: black, checkerboard, gradient,
color ramp, alpha, sRGB ICC, Display P3 ICC and Display P3 ICC with alpha.
`Reference.swift` independently applies the recovered display and window encoder
chains at quality 0.75. The generated JPEGs are specific to the current OS codec;
generate them on the same Mac as the Rust test instead of checking them into Git.

From the SDK root:

```sh
SOURCE="$PWD/crates/platforms/macos-computer-control/examples/fixtures/native-jpeg"
OUT="$(mktemp -d "${TMPDIR:-/tmp}/lingxi-native-jpeg.XXXXXX")"
cp "$SOURCE/"*.png "$OUT/"
xcrun swiftc "$SOURCE/Reference.swift" -module-cache-path "$OUT/swift-module-cache" -o "$OUT/reference"
"$OUT/reference" "$OUT"
LINGXI_COMPUTER_JPEG_REFERENCE_DIR="$OUT" CARGO_PROFILE_RELEASE_LTO=false \
  cargo test --release --locked --offline -p platform-macos-computer-control --lib \
  native_seam_matches_independent_swift_appkit_bytes -- --ignored --nocapture
```

No GUI, input, TCC changes, or screen capture are performed by this encoder
comparison. `MakeProfileFixtures.swift` can regenerate the three ICC inputs into
an explicitly chosen directory; the other five PNGs remain supplied inputs.
This proves the tested same-input encoder chains, rather than executing the
proprietary native addon or proving all possible capture/composition pixels.
