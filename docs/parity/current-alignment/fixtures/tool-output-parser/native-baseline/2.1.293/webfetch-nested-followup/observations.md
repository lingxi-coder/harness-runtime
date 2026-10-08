# Native WebFetch parser aggregate/null follow-up

Reference and setup are pinned in `native-receipt.json`; the captured results
are separate from and do not replace the original five-case baseline under
`webfetch-nested/`. Both new cases use unique fixed session IDs, the same fixed
loopback URL scheme, two accepted local Messages requests, and the same public
WebFetch `tool.call` continuation hook. The Native `next(event)` operation
again returned `SSL WRONG_VERSION_NUMBER` before the loopback resource received
a request; the recorded parser input is the hook's fixed replacement result.

`bytes-missing-slug-number-seeded-true` omits required top-level `bytes`, then
sets `artifactRead.slug` to a number and `artifactRead.seeded` to `true`. Native
aggregates all three issues, in this order:

1. `/bytes`: expected number, received undefined.
2. `/artifactRead/slug`: expected string, received number.
3. `/artifactRead/seeded`: `invalid_value`, `values: [false]`, message
   `Invalid input: expected false`.

The exact complete tool result, model-facing `<tool_use_error>` content, session
row, output SSE and hook marker are retained in that case directory. This
establishes field order for this composite input; it is a parser-only
observation, not a full Fetch operation.

`artifact-read-null` is rejected as an object type error at `/artifactRead`,
with message `Invalid input: expected object, received null`. Native therefore
distinguishes omission (accepted for this optional field) from explicit null.
