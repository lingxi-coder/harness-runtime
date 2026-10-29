# Media and realtime assistance

Load the current `media` skill for execution if available. Its instructions
track registered operations. The Media host owns sources, recordings, stored
assets, asynchronous operations and live sessions. Use their returned typed
references; do not turn a filename, URL or device label into a guessed source ID.

| Intent | Existing operations to look for |
| --- | --- |
| Find a camera/microphone or check readiness | `media_sources`, `media_status` |
| Take a still image or select media | `media_capture` |
| Record and finalize | `media_record`, then `media_stop` with its recording reference |
| Recognize speech | `media_transcribe`, choosing live source or stored media explicitly |
| Produce an audio asset / speak text | `media_synthesize` / `media_speak` |
| Play an asset | `media_play` |
| Observe or converse live | `media_session_start`, update/observe/wait/stop as supported |

`observation` inspects a live source without a conversational Agent.
`conversation` hands the current interactive main Agent to realtime assistance
after admission; it is not a new subagent or scheduled task. A successful
start must precede claims of live assistance. Do not substitute observation for
conversation without the user's agreement. Video-only conversation and provider
image/video support depend on the actual connector and schema.

Only attached image pixels establish what was seen. Media IDs, dimensions,
timestamps and byte counts are metadata. Audio content requires returned
transcription/event text before reporting what was said. A recording started
successfully is not a completed recording or transcript.

OS permission, resource contention, installed recognition/speech models and
provider capabilities are separate from feature support. Respect cancellation
and permission denial. Automatic local audio service selection does not imply
permission to upload recordings to a cloud provider.

For a remote device, first establish the selected connection and use sources
actually exposed by that host. For App Agent input and permissions, read the
`local-app` topic; main-chat Media access does not transfer to an App Agent.

Source anchors: Harness `crates/tools/media/skill/SKILL.md`,
`crates/media-api`, `crates/runtime/src/media_skill.rs`; product
`apps/audio-provider/src/media_host.rs` and `media_attachment.rs`.
