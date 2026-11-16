# Real background computer-use fixture

This disposable macOS fixture exercises the ignored real `ComputerTool` test through screenshot, text drag, raw key/click delivery, and a window move between Spaces. It uses only `org.lingxi.raw-probe.target` and `org.lingxi.raw-probe.shadow`; fixture content is the string `seed` and an event-counting canvas.

From the `harness-runtime` repository root, with Xcode command-line tools and Python 3 available:

```sh
FIXTURE="$PWD/crates/platforms/macos-computer-control/examples/fixtures/raw-background"
OUT="$(mktemp -d "${TMPDIR:-/tmp}/lingxi-raw-background.XXXXXX")"
python3 "$FIXTURE/fixture.py" build --out "$OUT"
python3 "$FIXTURE/fixture.py" prepare --out "$OUT" --space-source
LINGXI_RAW_NATIVE_PROBE_DIR="$OUT" CARGO_PROFILE_RELEASE_LTO=false \
  cargo test --release --locked --offline -p tool-computer-use --lib \
  actual_background_protocol_drag_key_and_current_space_guard -- --ignored --nocapture
python3 "$FIXTURE/fixture.py" cleanup --out "$OUT"
```

`build` requires an empty output directory. It discovers the selected SDK with `xcrun --show-sdk-path`, compiles the source beside this README, and signs the two temporary bundles ad hoc. Nothing is installed in `/Applications` and no OS permissions are changed. The native test process needs its existing Accessibility and Screen Recording grants.

`prepare` requires an unlocked screen. It refuses to proceed while either exact fixture bundle is already running. It starts and activates only its owned shadow, then verifies its regular desktop before creating the source. With `--space-source`, the target creates its own temporary fullscreen primary window and movable auxiliary window, then the launcher activates only the latched owned shadow. The launcher waits for the new window's actual Space membership, which can lag its fullscreen notification. The test must move the auxiliary from that source Space to the shadow's regular desktop before screenshot/drag/key operations. macOS may place the shadow on a different existing desktop from the initial one; both IDs are recorded. Omit `--space-source` for a same-Space run; that proves the already-here guard rather than a real move.

State, precise PID/executable/start-time ownership, original Space IDs, and the tool receipt remain under `$OUT`. Inspect them without changing the UI:

```sh
python3 "$FIXTURE/fixture.py" probe --out "$OUT"
"$OUT/target.app/Contents/MacOS/RawFixture" --probe-state
"$OUT/target.app/Contents/MacOS/RawFixture" --running-fixtures
```

Run `cleanup` even after a failed test or prepare. It sends SIGTERM only when the saved PID, `proc_pidpath`, and precise process start timestamp still match. App exit removes the fixture-owned fullscreen Space; cleanup verifies its disappearance and preservation of all pre-existing Space IDs. It never calls a Space deletion API, signals by process/bundle name, or guesses a user's foreground application to restore. An ownership mismatch is reported without signalling that process. Build products and receipts belong only in the temporary output directory, not in Git.
