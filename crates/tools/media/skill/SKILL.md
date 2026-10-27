---
name: media
description: Use the host's available tools to capture, inspect, record, transcribe, synthesize, play, or manage live media.
---

# Media

Use the live tool schemas to perform the requested audio or visual task. Start capture, recording, playback, or a realtime session only when the request calls for it. Use source, recording, media, operation, and session references returned by the host; do not invent references or substitute URLs. These instructions do not grant OS permissions. Report permission denial, cancellation, busy state, or unavailable capability as returned, without repeatedly reopening prompts.

The host may support only some operations. A supported operation can still be unavailable at the moment because it needs permission, a model, or a free device resource. Check `media_status` when readiness matters, then report the result clearly.

## media_sources

List the sources available on this device. Pass a returned source reference to later operations that need a source; do not guess a camera, microphone, or file identifier.

## media_status

Check current support and readiness when needed. Do not infer that an operation is unsupported from a temporary busy state, missing permission, or unavailable model.

## media_capture

Use when the user asks to capture media from a listed source. Hosts may provide native camera and photo-library sources as well as audio sources. Analyze a still image only when actual image content is attached to the tool result; source references, dimensions, or byte counts alone do not reveal what it depicts. A captured audio asset needs `media_transcribe` before reporting its speech content. A video or other media reference and its metadata do not prove you saw or heard the content. Respect a cancellation or permission result without retrying automatically.

## media_record

Start a recording from a listed source that advertises recording support. The available source may record audio or video. Keep the returned recording reference and pass that exact reference to `media_stop` when the requested recording is complete. A started recording produces media; metadata alone is not a transcript or a basis for claiming that you heard or saw it.

## media_transcribe

Use the schema's explicit source discriminator: a live source for live recognition, or a stored media reference for an existing recording. Do not describe live recognition as transcription of a prior recording. Report transcript text only when the operation returns it.

## media_synthesize

Create speech audio from the requested text and retain the returned media reference if the user wants it played or reused. Preserve the requested wording and language.

## media_play

Play the exact stored or synthesized media reference requested. Do not use an unrelated source or start playback when the user only asked to create audio.

## media_speak

Speak the requested text directly on the device. Preserve the user's wording and language. Do not read private data aloud unless the user requested that.

## media_session_start

Use `intent: "observation"` when the user wants to inspect a live source without a conversational Agent. Use `intent: "conversation"` only when the user wants live assistance; the host may hand the current turn to its realtime Agent after it commits the session-start result. Choose the requested audio/video mode and pass only source references returned by `media_sources`.

If the host does not support conversation, report that live coaching is unavailable. Ask before substituting an observation session that cannot provide the requested live assistance.

## media_session_update

Update only the session identified by the returned session reference. Use supported settings from the live schema; a session reference does not authorize access to other sessions or sources.

## media_observe

Request an observation from an exact source, recording, stored-media, session, or operation reference returned earlier. Describe visual content only when its image pixels are present in the result. For audio, report only returned transcript or event text.

## media_wait

Wait on an exact source, recording, stored-media, session, or operation reference returned earlier. Use a bounded wait when the schema offers one, and keep the reference if the operation is still running.

## media_stop

Stop or finalize only the typed recording, operation, or session reference requested. Use the recording reference returned by `media_record`; report whether the host finalized the recording or closed the operation/session.
