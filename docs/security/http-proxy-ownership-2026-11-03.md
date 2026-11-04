# HTTP proxy cleanup ownership, 2026-11-03

The server TLS wrapper can borrow the proxy stream. Connection failure cleanup now closes/frees the server wrapper before the proxy, matching normal client shutdown. A proxy authentication retry still retains the proxy. The original error diagnostic is saved before cleanup; cleanup restores it if present, otherwise clears any diagnostics introduced by cleanup. Checking the original diagnostic's presence avoids changing libgit2's no-error state through restore of the static no_error value.

Two Codex AI-assisted reviewers traced actual HTTP CONNECT callbacks and libgit2 stream/error contracts. The static finding concerned certificate rejection after handshake: TLS shutdown could use an already-freed proxy BIO under the old cleanup order. That product execution was not dynamically demonstrated.

An independent review found and corrected an initial no-error-state regression before this commit. The final exact http_client_connect body is tested with controlled stream/error callbacks for six branches: host setup failure with existing borrowed streams, proxy error, server error, direct error, proxy retry without a diagnostic, and host setup failure without a diagnostic where cleanup introduces one. Normal and ASan tests passed. Three isolated copy mutations (proxy-first, missing clear, unconditional save) failed their intended runtime assertions; an unused mock helper was annotated for negative compilation only. Results are in the adjacent controls JSON. No real network, TLS protocol, CA concurrency, device or complete libgit2 runtime acceptance is claimed.

Source SHA-256: f048f956a11016a563ad76ce587d432ad992f83f06d54e0166dd39f70714843e

Test SHA-256: 572c92f70a1c3e9c4fb64cfe56203f43cb1ffbb8a9dd8ed32b67a24f2de320d2

Shared CA lifecycle and the full native cryptographic source audit remain unresolved; this is a development checkpoint.
