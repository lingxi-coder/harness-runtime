---
name: llm-sidequery
description: Guide a Local App's own JS source through the bounded requestLlmChat/streamLlmChat side-query bridge — payload shape, limits, and the errors each one throws.
---

# Run a bounded LLM side query from a Local App

Own guiding correct use of `requestLlmChat`, `streamLlmChat`, and
`onLlmStreamFrame` from *inside an app's own page code* — the one-shot,
stateless model call every scaffolded template's `lib/lingxi-bridge.js`
already exports. This skill explains and reviews that JS surface; it never
stands up an MCP server, and it never lets the page pick a provider or a
model — a side query always rides the user's live model selection and
spends the user's own quota.

## The two calls

- `requestLlmChat(request)` resolves once with the whole answer:
  `{text, stopReason, truncated}`.
- `streamLlmChat(request)` resolves once the stream ends, with the same
  shape plus a `streamId`; the INCREMENTAL text arrives separately, through
  `onLlmStreamFrame` callbacks — subscribe before calling `streamLlmChat`,
  not after, or the first frames are missed.

Both throw if `window.lingxi.v2` is not ready yet; guide app code to check
for that (the shipped `getLingXiBridge()` helper, or an equivalent
null-check) rather than assuming the bridge exists on first paint.

## What a request may contain

- `system?` — a short string.
- `messages` — 1 to 20 turns, each `{role: "user"|"assistant", content}`.
  There is no `system`-role turn; the system prompt is its own field, not a
  message. `content` is either a bare string (shorthand for one text part)
  or an array of parts: `{type:"text", text}`, or an attachment part
  `{type:"image"|"document"|"media", mediaId}` (preferred — a handle from a
  prior device capture) or `{type:"image"|"document"|"media", mimeType,
  base64}` (inline, for something small the app itself generated, like a
  canvas export).
- `maxTokens?` — silently clamped to a small bounded range; asking for more
  never raises the real ceiling.
- `temperature?` — `0.0` to `1.0`, or the whole request is refused.

`image/*` and `application/pdf` attachments are supported. Audio is not —
guide app code to call `device.transcribeSpeech` first and send the
transcript as text instead of attaching an audio file. Never set `stream`
inside a `requestLlmChat` payload; it is refused outright — use
`streamLlmChat` for a streamed answer instead.

## What comes back, and what can go wrong

The answer text is bounded and may arrive truncated — on length, or because
the model spent its whole output budget without ever writing an answer, in
which case the call fails loudly rather than resolving to an empty string.
Guide app code to retry with a larger `maxTokens` in that case, not to treat
the failure as "the model had nothing to say."

Only one side query may be in flight per app at a time; a second concurrent
call is refused rather than queued — guide app code to wait for the first
to settle (or fail) before firing another, not to retry immediately.

Expect these failures as real, named outcomes rather than generic errors:
a malformed payload, an undeclared-capability refusal (the manifest never
declared `llm`), a denied permission prompt, "busy" (a call already in
flight), "unavailable" (no model attached, or the provider call itself
failed), an unsupported media kind, a truncated-budget answer, and a
timeout. Each names what happened; relay the specific one rather than a
generic "the AI call failed."

## Boundaries

- Never let app code choose a model, a provider, or pass tool definitions —
  there is no field for either; a side query always rides the user's live
  model selection.
- Never send audio inline — point app code at `device.transcribeSpeech`.
- Never assume `requestLlmChat` streams just because `stream` was set in
  its payload — it does not; use `streamLlmChat`.
- Never invent a bridge method beyond `requestLlmChat`/`streamLlmChat`/
  `onLlmStreamFrame` — a persistent, multi-turn conversation with its own
  session lifecycle is `$llm-agent`'s job, not this one's.
- Never describe this as an MCP server or something the user configures —
  it is a Host bridge operation on `window.lingxi.v2`, always present once
  the app has declared and been granted the capability.
