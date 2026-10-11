# Native 2.1.295 PRINT HTTP 422 capture

This is a repository-local copy of the completed one-shot capture in `acceptance/native-print-http422-error-copy-295/attempt-01`. Native 2.1.295 ran once against the localhost stub, sent one Messages request, returned exit code 1, and did not time out. The raw response is HTTP 422 with an `invalid_request_error`; raw Native stdout contains result `API Error: 422 NATIVE_295_PRINT_HTTP_422_ERROR_COPY_PROBE`, `is_error: true`, `api_error_status: 422`, and `terminal_reason: api_error`.

`capture.json`, `capture-manifest.json`, and `capture-runner.py` retain the capture receipt, exact runner manifest, and exact runner source used. The binary SHA-256 is `0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70`; the runner SHA-256 is `d32199315bb52a43498f4c4b54c5ed0e40867efdf99addfe1389cad3658524e9`. The fixture is a source/result capture, not a claim of full request or envelope parity.

All raw artifacts retain their captured bytes. `SHA256SUMS` covers the receipt, source, request/response, stdout/stderr, and sandbox records.
