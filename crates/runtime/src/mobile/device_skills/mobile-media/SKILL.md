---
name: mobile-media
description: Use for direct media input or playback on the user's Android or iOS device.
session-modes: [chat, code]
---

# Mobile media

Use the available native tools below directly for the requested media operation. No manifest, app identifier, JavaScript bridge, or generated application is needed. Follow the live tool schema: a device can support only some actions, and support can change with audio configuration.

An OS permission denial, cancellation, or unavailable service is an operation result. Report it without repeatedly reopening native prompts. Start capture only when the request calls for it, and distinguish captured metadata from media that the model can actually inspect.

## camera

Call `camera` with `{"action":"capture","position":"back"}` to take a photo, or `{"action":"pick_from_library"}` to let the user select an image. `position` can be `front` or `back`; capture also accepts `allow_editing`. Check the capture result and use any returned image content for visual analysis. Dimensions and byte length alone are not visible image content; never describe an unseen photo from its metadata.

## voice

Use `voice` for raw microphone recording: `{"action":"start_recording"}`, `{"action":"is_recording"}`, and `{"action":"stop_recording"}`. Start optionally accepts `sample_rate_hz` from 8000 to 48000 and `format`; defaults are 16000 and `m4a`. A recording belongs to the current session. Keep a successful start paired with a stop when the requested capture finishes; do not start repeatedly or leave a recording active accidentally.

Start and status return `recording`. Stop returns `recording:false`, `audio_bytes_len`, and `mime_type`. These fields confirm recording state and metadata, not a transcript or model-readable audio. Do not claim to have listened to audio based on byte length.

## speech

Use only actions listed in the current `speech` schema. `{"action":"transcribe","language":"zh-CN"}` listens for live speech and returns `text`, `language`, and optional `confidence`; `language` is optional. It does not transcribe a previous recording or an arbitrary audio file.

`{"action":"speak","text":"..."}` speaks the requested text. Optional arguments are `language`, `voice`, and `rate` from 0.5 to 2.0; text is limited to 5550 characters per call. A successful result contains `spoken:true` and `duration_ms` after playback completes. Preserve the user's requested content and language, and do not read private data aloud unless that is part of the request.
