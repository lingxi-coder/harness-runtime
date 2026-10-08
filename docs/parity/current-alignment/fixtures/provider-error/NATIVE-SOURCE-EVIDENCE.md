# Native 2.1.293 provider-error source locations

This note records static source locations in the same pinned Native binary
used by the runtime fixtures. It is source evidence only; it is not a
runtime test result or a Harness pass.

- Native version: 2.1.293.
- Binary SHA-256: `4e21122a227857da1178aca3299700c1fd7f2b77c93f12e73c2c76db796a105e`.
- `jJn` request-authority/first-party classification begins at bundle offset
  `193013843`; its HTTP 500 branch begins at `193040188`.
- The status page constant `eLe` is `https://status.claude.com`.
- Related `pi` / `Of` / `$g` error-policy logic begins near offset
  `184417525`.
- The first-party host check is `URL.host === "api.anthropic.com"`. The 500
  branch for another authority includes an inference-gateway notice; the
  Native fixture's loopback authority is treated as non-first-party.

The runtime authority used in the 500 candidate acceptance test is fixed from
the checked-in 2.1.293 raw result: `127.0.0.1:57288`. The candidate injects a
fixed `Transport`; it does not connect to that authority. Its purpose is to
carry Native's actual request authority into the Host formatter and compare
the complete UTF-16 result. The raw Native response, stdout and provenance
receipt remain the execution oracle; these offsets only explain the observed
policy.
