# Local rustls-native-certs dependency

This is the published `rustls-native-certs` 0.8.4 crate, archive SHA-256
`dab5152771c58876a2146916e53e35057e1a4dfa2b9df0f0305b07f611fdea4d`,
upstream Git revision `9d1f11e5da42f061c9a5aebbcde48a1b843afff2`.
The original crate metadata and Apache-2.0, ISC, and MIT licenses are retained.

The downstream patch snapshots the raw `SSL_CERT_FILE` and `SSL_CERT_DIR`
values once and checks whether either key is present before processing the
paths. Explicit configuration, including empty or invalid values, never falls
back to the platform's certificate store. Platform defaults are used only when
both keys are absent. The environment regression test launches each case in a
separate process, including directory lists, empty components, invalid paths,
PEM fixtures, and Unix non-UTF-8 values.

The two PEM fixtures are the public example certificates already embedded in
the upstream `src/lib.rs` documentation. They contain no private keys.
