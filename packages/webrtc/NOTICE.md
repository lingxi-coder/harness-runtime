# Source provenance

Derived from OpenAI Codex, Apache-2.0, revision
322bbf4d8486efd7dbbcf49598711a9e3fefc282.

The initial sources are codex-rs/voice-host/src/{transport,transport_runtime,
audio_track,incoming}.rs and their tests. The process entrypoint, process
hardening, helper IPC and executable-relative runtime loader are not included.
Local changes expose an embedded API and preserve incoming DataChannel bytes.
The shared media processing, callback queues, desktop device adapter and audio
playout derive from the corresponding voice-host modules, including processing,
devices, device_buffers, capture_worker, playback, audio_sink, playout and
input_channel. Process-level runtime/plugin discovery was replaced by explicit
library initialization. Native mobile ports/adapters, VP8 camera/screen transport
and diagnostics are LingXi additions. Third-party codecs and SDK components retain
their respective upstream licenses; they are separate from the Codex source license.
