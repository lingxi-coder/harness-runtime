# Authorization gate for mobile Linux runtimes

These files are mandatory before `LINGXI_MOBILE_LINUX_ENABLED=1` can be used in CI or release builds.

Required artifacts:

- `AUTHORIZATION_MANIFEST.json` — machine-readable index of all grant files
- `proot-authorization.txt` — written authorization covering modification, linking, and store redistribution of the approved PRoot fork/binaries
- `ish-authorization.txt` — written authorization covering modification, linking, and App Store redistribution of the approved iSH-based runtime pieces
- `alpine-redistribution.txt` — written authorization or redistribution memo covering the packaged Alpine rootfs artifacts and package set

Rules:

1. Missing files must fail the build.
2. `AUTHORIZATION_MANIFEST.json` must include SHA-256 hashes for each listed file.
3. The hash in the manifest must match the file on disk.
4. Placeholder/sample files do not satisfy the gate.
5. The manifest `status` must be exactly `approved`, and each required artifact
   ID must point to its fixed filename listed above.
6. The mobile build must pin the lowercase SHA-256 of
   `AUTHORIZATION_MANIFEST.json` in
   `LINGXI_MOBILE_LINUX_AUTHORIZATION_SHA256`. Android/iOS runtime probes
   reject an arbitrary existing file, symlink, oversized file, or digest
   mismatch and remain `BlockedByLicense`.

The sample manifest in this directory is for shape only. Rename it to `AUTHORIZATION_MANIFEST.json` and replace placeholder hashes only after legal review has produced the real evidence files.
