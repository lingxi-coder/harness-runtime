# Mobile package stage report — 2.1.295 (r378)

The r378 iOS simulator build and Android Play debug gates are recorded at the
following pins: Harness production `6548b9d5e702f8e9d42772412cd65b72881e24b4`,
Harness docs `4fb6583072c12f51558ab12125667847a7998c4d`, Product source
`343fe8528697d95b3cc0e21d9651727fa1b0a4e0`, Product documentation-only delivery
`7f8e022d1aef248fbd64e3db379af7c4903f12de`, SDK
`a363c5b3a237d53eea12c3abfb3b98648ef15d3e`, Mobile Linux
`6b033d41a2b6b736fda1e8f996f55a4b20bd885d`. The prior iOS compile repair is
commit `04b1b6ea095a3e35e6ab76e12d1d78966339d9dd`; the r378 Product source adds
the Android `ClientTextDto` import, removes a duplicate display helper, and
updates the iOS test-only selector to use UTF-16 catalog identity.

On iOS, the arm64 simulator debug FFI/XCFramework build exited 0 and the saved
manifest records a complete current UniFFI metadata archive plus a promoted
simulator slice whose SHA-256 is `0d6af1cc4b6e1846f8e05a5c5e2784c882dfc0a9aef5fc6241e572170a974813`.
The app and test bundles compiled successfully with `build-for-testing`. The
focused 23-test run recorded 22 passes and one selector-order failure. After the
selector correction, the affected UTF-16 identity test passed in a separate
one-test run. This is not a 23-test, zero-failure run. The `test-without-building`
attempt was interrupted after an Xcode internal test-operation failure, so
simulator UI runtime behavior remains unverified. The independent iOS audit
records the Swift source and metadata provenance; its original Cargo static
archive was retired after successful XCFramework promotion, while the combined
and promoted framework archive hashes remain available.

On Android, current debug `libandroid_aar.so` outputs for `arm64-v8a` and
`x86_64` were promoted atomically and verified against their staged hashes. The
production UniFFI generator was built from the checked-in
`apps/ios/ffi/src/bin/uniffi-bindgen.rs` source with UniFFI 0.32.2 and ran against
the exact staged arm64 library. Its generated Kotlin includes the existing
production message and callback-buffer repairs and was hash-compared after
promotion. The regular Gradle `verifyPlayMobileLinuxNative`, app Kotlin compile,
and app unit-test Kotlin compile gates passed. `ChatViewModelReducerTest`
recorded 97 tests, 0 failures, and 0 errors.

The Android result is a current JNI and Kotlin compile/test gate; it does not
include an APK or device run. The iOS result includes an app/test bundle build and
the scoped simulator test described above. Physical-device behavior is not
claimed. The normal Flare wrapper completed the signed development-channel package with
exit 0; static package verification, isolated packaged smoke, and the postbuild
guard passed. The ZIP is 281,885,759 bytes with SHA-256
`c355b7a1609ae77be478229687dc312d128d567018fedce3fc3dc26993936762`.

Actual visible native CUA passed on the exact p-7f8 app. The accessibility URL
points inside that signed app's `app.asar`; the isolated `TjFqow` workspace was
visible. CUA clicked Sidebar Alpha, typed `packaged-host-cua-value`, appended
`xy` while retaining focus and value, then clicked Submit. The visible field
retained `packaged-host-cua-valuexy` and the callback accepted that full value.
The [saved AX tree](evidence/mobile-package-r378/cua-final-ax.txt) and
[screenshot](evidence/mobile-package-r378/cua-submit.png) are included with the
[observation record](evidence/mobile-package-r378/cua-observations.json).

The supplementary packaged-host driver reached `stage: complete` with
`stopReason: control-file`, then exited 1 because its global process-exit check
matched the separate historical p-17593 app (PID 31038). Its raw result remains
failed. The independent cleanup audit confirms the owned current app PID 27257
exited, the synthetic provider credential was cleared, and provider/CDP ports
57865/57915 closed. The independent [macOS audit](../../../../acceptance/mobile-package-r378/verification/mac-gui-audit.md)
accepts the scoped package, visible CUA, and owned cleanup. It classifies the
initial sandboxed codesign verification failure as a filesystem-access limit:
the same read-only verification passed with elevated access and the signed app
meets its designated requirement. The scoped r378 result is **accepted with a
driver diagnostic**; the supplementary driver itself is not a pass. The earlier
name-based route opened p-17593 and performed only Raise/Tab; no input was
accepted there, and no retry or termination of that historical app is claimed.

The package remains development-signed and is not notarized. Sidebar dragging,
physical-device behavior, all 42 parity contracts, and public Native byte
parity are outside this scoped result. See the [package receipt](../../../../acceptance/mobile-package-r378/mac/package-receipt.md),
[full Flare log](../../../../acceptance/mobile-package-r378/mac/flare-package-r378-20261018T132052Z.log),
[macOS audit](../../../../acceptance/mobile-package-r378/verification/mac-gui-audit.md),
and [selected gate excerpts](evidence/mobile-package-r378/gate-excerpts.md).

The [independent iOS audit](../../../../acceptance/mobile-package-r378/verification/ios-audit.md),
[independent Android audit](../../../../acceptance/mobile-package-r378/verification/android-audit.md),
[structured receipt](evidence/mobile-package-r378/receipt.json), and [selected
gate excerpts](evidence/mobile-package-r378/gate-excerpts.md) preserve the pins,
artifact hashes, full-log hashes, and source provenance. Original command output
remains under `acceptance/mobile-package-r378/`.
