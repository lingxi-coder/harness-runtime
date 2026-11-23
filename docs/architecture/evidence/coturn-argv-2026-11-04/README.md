# Coturn fixture argument validation

Source commit `41f66b3b4fe6a7b0587507d8a5664e1d97554eb5` sets only the
fixture entrypoint to `turnserver`. The comparison of the two normalized Compose
configurations verifies that every other service setting is unchanged. Docker
inspection records the actual executable and complete argument list beginning
with `-n -v`, the pinned image, all 18 host bindings on `127.0.0.1`, read-only
root, dropped capabilities, `CAP_NET_BIND_SERVICE`, tmpfs mounts, and
`no-new-privileges`.

The already-built core test binary recorded in `identity.json` ran both ignored
TURN regressions against the real local fixture: two passed, including relay-only
data/audio/video and 20 close/reallocate cycles on the same source port. Its
core source tree and nested Cargo.lock are identical to the binary's build
commit. No rebuild or image pull was needed. The captured server log contains
neither the prior `Unknown argument` nor a missing-default-config warning;
its incorrect-credentials error is deliberately caused by the negative test.
The isolated container and its network were removed after the run.

The compressed files preserve raw output byte-for-byte. `identity.json` records
both compressed and uncompressed hashes; the verification script and normalized
Compose inputs are included. This validation neither registers any source audit
nor changes the App dependency policy or its outstanding review gaps.
