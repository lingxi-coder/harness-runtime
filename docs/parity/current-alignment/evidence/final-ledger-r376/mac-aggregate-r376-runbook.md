# r376 macOS aggregate receipt

`make-mac-aggregate-r376.py` validates saved evidence and creates exactly one
new package receipt. It does not build, sign, launch, or automate the UI. Its
pins are fixed to Product
`17593eaa66dc762d5c9c581ba348398c914bdbcd`, Harness
`44f2eef5baf8ae16254934dbca451cbfd528d955`, and SDK
`a363c5b3a237d53eea12c3abfb3b98648ef15d3e`. It rejects the old r375 package
and any evidence not bound to these P/H pins.

The output follows the package-receipt shape consumed by
`finalize-repair-receipt.py` (`status: passed` plus the `pins` map), with hashes
for the package ZIP, `app.asar`, logs, driver result, actual CUA observation,
screenshot, and the validation scripts. A successful package receipt covers
only this macOS Flare package/Host/GUI scope; it is not a 42-row Native parity
receipt, notarization claim, or physical-device result.

## Save the package wrapper evidence

Use fresh paths for every input. `PACKAGE_LOG` is the explicit log path consumed
by the existing wrapper; `WRAPPER_STDOUT` captures its source-guard JSON and
completion sentinel separately. The wrapper exit sidecar must contain exactly
`exit_code=0`.

```sh
BASE=/Users/luolingfeng/lingxi/.worktrees/claude-alignment-integration
P_SHA=17593eaa66dc762d5c9c581ba348398c914bdbcd
H_SHA=44f2eef5baf8ae16254934dbca451cbfd528d955
SDK_SHA=a363c5b3a237d53eea12c3abfb3b98648ef15d3e
RUN_DIR="$BASE/acceptance/mac-final-r376-$(date -u '+%Y%m%dT%H%M%SZ')"
mkdir -m 700 "$RUN_DIR"
PACKAGE_LOG="/private/tmp/flare-r342-r376-$(date -u '+%Y%m%dT%H%M%SZ').log"
WRAPPER_STDOUT="$RUN_DIR/wrapper.stdout.log"
WRAPPER_EXIT="$RUN_DIR/wrapper.exit"

set +e
"$BASE/acceptance/final-ledger-r376/run-repaired-h-package.sh" \
  "$P_SHA" "$H_SHA" "$PACKAGE_LOG" >"$WRAPPER_STDOUT" 2>&1
wrapper_rc=$?
set -e
printf 'exit_code=%s\n' "$wrapper_rc" >"$WRAPPER_EXIT"
test "$wrapper_rc" -eq 0
```

The helper requires three successful guard records in wrapper stdout and the
normal wrapper's final success sentinel. Read the fresh output root and app/ZIP
paths from those package logs. Confirm the app version from the path; do not
reuse any path or digest from `mac-final-r375-20261017`.

## Run and retain the independent static audit

Run the static verifier against that exact app and final P/H pair. Save both the
log and an exit sidecar, with the latter containing exactly `exit_code=0`.

```sh
P_ROOT="$BASE/lingxi-app"
APP_PATH='<exact LingXi Code.app path from the new wrapper log>'
STATIC_LOG="$RUN_DIR/static-audit.log"
STATIC_EXIT="$RUN_DIR/static-audit.exit"

set +e
LINGXI_MAC_TEAM_ID=AZ4AX7J833 /opt/homebrew/opt/node@25/bin/node \
  "$P_ROOT/acceptance/mac-package-audit-r354-prep/verify-final-macos-package.mjs" \
  "$APP_PATH" "$P_SHA" "$H_SHA" >"$STATIC_LOG" 2>&1
static_rc=$?
set -e
printf 'exit_code=%s\n' "$static_rc" >"$STATIC_EXIT"
test "$static_rc" -eq 0
```

The helper cross-checks the independent audit's Product/H identity and
`app.asar` SHA against the actual file, and compares its Bun identity with the
Host driver result. The normal wrapper log must also show both static package
verification and packaged startup smoke passed, and the ZIP hash must match the
actual ZIP bytes.

## Run the packaged Host driver and actual visible CUA

Run the r375 Host driver with the new app path and a fresh results directory.
Save its complete terminal log and exact exit status. During its GUI handoff,
use actual visible CUA on the same app process: verify the Packaged Host Client
UI title, Message label/placeholder, Submit button, and initial result; type
`packaged-host-cua-value`, append `xy`, and click Submit. Confirm the visible
input and final Submit callback results, then capture a screenshot and stop the
handoff with its `STOP-HANDOFF` control.

```sh
GUI_DIR="$RUN_DIR/gui"
mkdir -m 700 "$GUI_DIR"
HOST_LOG="$RUN_DIR/host-driver.log"
DRIVER_EXIT="$RUN_DIR/host-driver.exit"

set +e
/opt/homebrew/opt/node@25/bin/node \
  "$BASE/acceptance/product-js-gates-fbe2a847-20261014/packaged-host-driver-r375/run-packaged-host-mod-smoke.mjs" \
  --app "$APP_PATH" \
  --results-dir "$GUI_DIR" \
  --gui-handoff \
  --handoff-timeout-seconds 1800 >"$HOST_LOG" 2>&1
driver_rc=$?
set -e
printf 'exit_code=%s\n' "$driver_rc" >"$DRIVER_EXIT"
test "$driver_rc" -eq 0
```

After the driver completes, create a separate `cua-observations.json` from the
actual on-screen observation. It must use schema `mac-cua-evidence-r376-v1` and
contain `status: "passed"`, `observation_method: "actual-visible-CUA"`,
`recorded_at` with a timezone, `native_version: "2.1.295"`, exact `product_sha`,
`harness_sha`, and `sdk_sha`, plus:

- `app_path`: the exact `APP_PATH` used for packaging and CUA.
- `app_pid`: the PID in both the driver's terminal `GUI_HANDOFF` record and its
  final JSON result.
- `driver_result_sha256`: SHA-256 of the exact completed driver result JSON.
- `screenshot_path` and `screenshot_sha256`: the saved PNG/JPEG from this real
  visible interaction.
- `visible_ui`: exactly `{"title":"Packaged Host Client UI","input_label":"Message","input_placeholder":"Type a value","button_label":"Submit","initial_result":"Waiting for a value."}`.
- `native_cua_actions`: the actual three actions and visible results, in order:

Use this exact shape, filling the bracketed values only from the current run
and actual visible CUA observation:

```json
{
  "schema": "mac-cua-evidence-r376-v1",
  "status": "passed",
  "recorded_at": "<ISO-8601 timestamp with timezone>",
  "observation_method": "actual-visible-CUA",
  "native_version": "2.1.295",
  "product_sha": "17593eaa66dc762d5c9c581ba348398c914bdbcd",
  "harness_sha": "44f2eef5baf8ae16254934dbca451cbfd528d955",
  "sdk_sha": "a363c5b3a237d53eea12c3abfb3b98648ef15d3e",
  "app_path": "<exact LingXi Code.app path>",
  "app_pid": 12345,
  "driver_result_sha256": "<SHA-256 of completed Host driver result JSON>",
  "screenshot_path": "<absolute PNG or JPEG path>",
  "screenshot_sha256": "<SHA-256 of screenshot bytes>",
  "visible_ui": {
    "title": "Packaged Host Client UI",
    "input_label": "Message",
    "input_placeholder": "Type a value",
    "button_label": "Submit",
    "initial_result": "Waiting for a value."
  },
  "native_cua_actions": []
}
```

Replace `native_cua_actions` with the actual actions below, not the empty array
in the template. Compute the two digests from the saved files (for example with
`shasum -a 256`); do not reuse the r375 CUA observation or screenshot.

```json
[
  {
    "typeText": "packaged-host-cua-value",
    "visible_input": "packaged-host-cua-value",
    "visible_result": "Input callback received: packaged-host-cua-value",
    "focus": "Message"
  },
  {
    "typeText": "xy",
    "visible_input": "packaged-host-cua-valuexy",
    "visible_result": "Input callback received: packaged-host-cua-valuexy",
    "focus": "Message"
  },
  {
    "click": "Submit",
    "visible_result": "Submit callback accepted: packaged-host-cua-valuexy",
    "focus": "Submit"
  }
]
```

The helper intentionally requires this separate CUA observation and screenshot;
the driver's programmatic `visibleCuaExpectation` is not accepted as a GUI
observation. It also checks that the CUA PID matches the completed driver's
handoff PID and that the CUA record binds the exact driver-result hash.

## Aggregate only after all gates pass

Select the exact package app and ZIP paths from the fresh wrapper output, the
completed driver JSON path under `GUI_DIR`, and the screenshot/observation made
above. Then create the new receipt. The output must not already exist.

```sh
"/Users/luolingfeng/.cache/codex-runtimes/codex-primary-runtime/dependencies/python/bin/python3.12" \
  "$BASE/acceptance/final-ledger-r376/make-mac-aggregate-r376.py" \
  --wrapper-log "$PACKAGE_LOG" \
  --wrapper-stdout "$WRAPPER_STDOUT" \
  --wrapper-exit "$WRAPPER_EXIT" \
  --static-audit-log "$STATIC_LOG" \
  --static-exit "$STATIC_EXIT" \
  --app "$APP_PATH" \
  --zip '<exact new package ZIP path>' \
  --driver-log "$HOST_LOG" \
  --driver-result '<exact completed driver JSON path>' \
  --driver-exit "$DRIVER_EXIT" \
  --cua-observations "$RUN_DIR/cua-observations.json" \
  --cua-screenshot '<exact saved PNG/JPEG path>' \
  --output "$BASE/acceptance/final-ledger-r376/receipts/mac-final-r376.json"
```

The helper refuses missing, failed, stale-pin, or internally mismatched inputs
and does not overwrite an existing receipt. It creates a passing aggregate only
after the wrapper, static audit, Host driver terminal result, and actual CUA
evidence all match the pinned P/H/SDK bundle.
