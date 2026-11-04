# Mbed TLS stream ownership

An input stream remains the caller's responsibility if TLS wrapper creation
fails. Once creation succeeds, a wrapper owns only inputs created for it;
borrowed inputs remain caller-owned. If TLS shutdown fails, the wrapper still
closes an owned input and preserves the TLS failure diagnostic if closing the
input also fails.

Hostname setup failures are reported before configuring transport callbacks or
starting the TLS handshake. The connection test injects both allocation and
invalid-hostname failures and checks that their Mbed TLS error reaches the
diagnostic path; it also exercises successful and later-failure paths.

`scripts/tests/test_mbedtls_stream_ownership.py` extracts the product cleanup
functions into a C harness with injected allocation, setup, teardown, and
socket-close failures. It exercises owned and borrowed inputs in ordinary and
AddressSanitizer builds. This is source-level lifecycle coverage; it does not
test a TLS handshake, certificate validation, or an Android device.
