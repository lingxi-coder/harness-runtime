# Selected r378 mobile gate excerpts

The original logs remain under `acceptance/mobile-package-r378/`. Their full-file
SHA-256 values and recorded exit/status data are in [receipt.json](receipt.json).
These excerpts keep the important gate result visible without replacing or
truncating the source logs.

## iOS arm64 simulator and focused tests

From [ios/ffi-sim-arm64-debug.log](../../../../../../acceptance/mobile-package-r378/ios/ffi-sim-arm64-debug.log) (SHA-256
`e97a31a623129e03243a24ebd846576e6100c990a41501abd3b8f2f167732f45`):

```text
Compiling harness-runtime …?rev=6548b9d5…
Compiling mobile-linux-ios …?rev=6b033d41…
Finished `dev` profile [unoptimized] target(s) in 4m 09s
XCFramework successfully written out to: …/LingxiCodeFFI.xcframework
```

From [ios/xcodebuild-for-testing.log](../../../../../../acceptance/mobile-package-r378/ios/xcodebuild-for-testing.log) (SHA-256
`7440994bcda1f0ce26632ac8cbbf9f0378797c0567ba69bf614afde846b8591d`):

```text
** TEST BUILD SUCCEEDED **
```

The first focused run in [ios/xcodebuild-focused-unit-tests.log](../../../../../../acceptance/mobile-package-r378/ios/xcodebuild-focused-unit-tests.log) (SHA-256
`9e2802f11c301822621f1e4bdc45eaed8f4e7a6c3ac3d6c464a05f520e75c0c3`) ended
with:

```text
Executed 23 tests, with 1 failure
```

The selector was corrected to preserve the catalog entry's original UTF-16
identity. The separate target run in
[ios/xcodebuild-selected-slash-identity-test.log](../../../../../../acceptance/mobile-package-r378/ios/xcodebuild-selected-slash-identity-test.log) (SHA-256
`2755e69de07035d7a6e598b68b65cf4dc4c4d84bd142dc512bdad5bb444410b3`) recorded:

```text
Test Case … testSelectedCatalogIdentitySurvivesLossyDisplayCollision … passed
Executed 1 test, with 0 failures
** TEST SUCCEEDED **
```

The [test-without-building attempt log](../../../../../../acceptance/mobile-package-r378/ios/xcodebuild-test-without-building-attempt0.log) and [exit note](../../../../../../acceptance/mobile-package-r378/ios/xcodebuild-test-without-building-attempt0.exit) say it was interrupted
(exit 130) after an Xcode internal test-operation failure. It does not establish
simulator UI runtime behavior.

## Android Play debug

[android/current-jni-cargo.log](../../../../../../acceptance/mobile-package-r378/android/current-jni-cargo.log) (SHA-256
`a098e8e648207ff99b4a2bcb4d0bfce93f7286b13ea791a8ea4bcb35a1b79f95`) contains
both ABI target completions and staging. The structured Android receipt records
the captured current-source command exit 0 and full-hash atomic promotion for
both outputs. [android/jni-promotion.log](../../../../../../acceptance/mobile-package-r378/android/jni-promotion.log) has SHA-256
`421103683cd3f0817c962321841fe6379849520164a1ccf5dda3a166e54cd259`.

The production generator build is bound in
[build/production-bindgen-provenance.json](../../../../../../acceptance/mobile-package-r378/build/production-bindgen-provenance.json); the exact checked-in source hash,
binary hash, generator version, output hashes, and successful run are recorded
in [android/android-r378-receipt.json](../../../../../../acceptance/mobile-package-r378/android/android-r378-receipt.json) (SHA-256
`087adb34d92cb4650210b70c42c5f3269bd9b60e6525c5d7ae215d1aabcd264e`). The
normal Gradle compile log [android/play-debug-compile-r2.log](../../../../../../acceptance/mobile-package-r378/android/play-debug-compile-r2.log) (SHA-256
`6b0f2154f3ae7a22d77cdfb68d7f26e846d84c1898d31dd325aeb9ca4b71552f`) ends:

```text
BUILD SUCCESSFUL in 36s
```

The reducer log [android/play-debug-reducer.log](../../../../../../acceptance/mobile-package-r378/android/play-debug-reducer.log) (SHA-256
`2957edd7f60bcf42a47219a27ae5f0a65049bee64c718d501e3a9f2eb5647e9b`) ends:

```text
BUILD SUCCESSFUL in 3s
```

The receipt and retained JUnit XML agree on 97 tests, 0 failures, and 0 errors;
the [JUnit XML](../../../../../../lingxi-app/apps/android/native/app/build/test-results/testPlayDebugUnitTest/TEST-com.lingxi.code.conversation.ChatViewModelReducerTest.xml) SHA-256 is `a850a20885a1edd90ab20d8c0d2ad3a1f0b80827301a4bec5e3aa7acb67e8a11`.
The independent audit is [verification/android-audit.md](../../../../../../acceptance/mobile-package-r378/verification/android-audit.md) (SHA-256
`7742d9159b8d169c7a3b75ddff125cccf6e3a50b73a4a90fbaccfb161ffe0b26`). It notes
that bindgen stdout is empty, so the structured receipt, generator provenance,
and generated-file hashes establish the successful run. No APK or device run is
represented.

## macOS signed package

The [package receipt](../../../../../../acceptance/mobile-package-r378/mac/package-receipt.md)
records the normal Flare wrapper exit 0, static package verification, isolated
packaged smoke, and postbuild guard exit 0. The signed app is
`/Users/luolingfeng/lingxi/.buildcache/claude-alignment-integration-r51/packages/p-7f8e022d1a-r378-20261018T132052Z-64644/LingXi-Code-0.1.0-mac-arm64/LingXi Code.app`.
The ZIP is 281,885,759 bytes, SHA-256
`c355b7a1609ae77be478229687dc312d128d567018fedce3fc3dc26993936762`. The full
[Flare package log](../../../../../../acceptance/mobile-package-r378/mac/flare-package-r378-20261018T132052Z.log)
has SHA-256 `494ea318ad52bdbd8a0acdc8ad0170451f6e9979d1059c42a328324447c84d7b`.
The wrapper's smoke verifier exercised an isolated temporary app copy. The
development-signed package is not notarized.

## Current signed package CUA and driver cleanup

The actual native CUA evidence identifies the exact p-7f8 app path and isolated
TjFqow workspace. It records Sidebar Alpha selection, visible Message input,
`packaged-host-cua-value`, an appended `xy` with focus/value retained, and a
Submit click whose visible result is `Submit callback accepted:
packaged-host-cua-valuexy`. The committed [observation JSON](cua-observations.json),
[AX snapshot](cua-final-ax.txt), and [screenshot](cua-submit.png) preserve the
observed result and hashes.

The supplementary driver JSON at
[acceptance/mobile-package-r378/gui/packaged-host-mod-2026-10-18T13-40-02-082Z.json](../../../../../../acceptance/mobile-package-r378/gui/packaged-host-mod-2026-10-18T13-40-02-082Z.json)
has SHA-256 `daf1d11be6cec71c0b5898471542693eb9faacb7e3f84e08d61b4812232f2b7e`,
`stage: complete`, `stopReason: control-file`, and `outcome: failed` (exit 1).
The global process-exit predicate matched historical p-17593 PID 31038. Its
[owned cleanup audit](owned-cleanup-audit.json), SHA-256
`9c2f4accef5bdd4da867977d3a4e3cd11ec8af4ece16078af10c9d7e9ad7cc61`, confirms
owned PID 27257 exited, the synthetic credential was cleared, and provider/CDP
ports 57865/57915 closed. No retry or historical-app termination is claimed.

The [independent macOS audit](../../../../../../acceptance/mobile-package-r378/verification/mac-gui-audit.md)
SHA-256 is `4abcbb3e337a39417aa96d2baec1fbe165503246576d37f9b4a68c33cc0af6aa`.
It accepts the scoped functional stage and owned cleanup, and concludes that a
sandboxed `codesign` verification failure was a filesystem-access limitation;
the same read-only check with elevated access passed. Preserve the failed driver
aggregate as a diagnostic. The scoped r378 result is accepted with diagnostic,
not an all-gates-green or all-42 parity result.

