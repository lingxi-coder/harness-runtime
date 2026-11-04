# HTTP proxy stream ownership

An HTTP server TLS stream can borrow its proxy stream. On connection failure,
the server wrapper is closed before the proxy so that its shutdown cannot use
a freed input. A retry retains the proxy as before. Cleanup saves and restores
the original failure diagnostic, or clears a cleanup-only diagnostic when
there was no original. Saving a dynamic diagnostic can fail on allocation, in
which case libgit2 supplies its allocation error instead of the original.

`scripts/tests/test_http_proxy_ownership.py` extracts the actual HTTP connect
function and runs it against controlled stream callbacks. It covers borrowed
connections, direct connections, retry, and cleanup diagnostic preservation
in ordinary and AddressSanitizer builds. It does not make network connections
or exercise a TLS handshake.
