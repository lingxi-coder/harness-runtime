# Media service

Media is an injected host service. The Agent discovers built-in tools and a
bundled `media` skill; loading a skill does not activate devices or grant
permissions. Audio, camera, screen and remote streams share one resource and
ownership model.

## Boundaries

| Component | Responsibility |
| --- | --- |
| `media-api` | Transport-independent operations, owners, opaque references, bounded events and normalized device frames |
| `media-runtime` | Operation lifecycle, cancellation, recording finalization, owner checks, event buffers and backend routing |
| `tool-media` | JSON schemas, Agent-visible results and actual multimodal content |
| `media` skill | Workflow guidance using the tools available in the current host |
| Host adapters | OS permission prompts, capture/playback leases, storage, provider accounts and connection configuration |
| Product connector | Signaling, approved peer selection, ICE configuration and RTC transport lifecycle |

The generic API contains no Firebase paths, account discovery, SDP negotiation,
GStreamer objects or platform SDK handles. Authenticated native bridge DTOs may
carry bounded SDP or media frames between a host and its adapters. Those messages
are host control traffic and do not enter Agent history, AG-UI, turn replay or
remote conversation broadcasts.

Specialized audio/camera contracts live in `media-api`. Existing native consumers
such as Computer Use may use source reexports through `device-api`; those
reexports do not maintain a separate service, router or device lease.

## Agent interface

The public tools are `media_sources`, `media_status`, `media_capture`,
`media_record`, `media_transcribe`, `media_synthesize`, `media_play`, `media_speak`,
`media_session_start`, `media_session_update`, `media_observe`, `media_wait` and
`media_stop`. The old `voice`, `speech`, `camera` tool names and `mobile-media`
skill are replaced, without public aliases.

Each tool is discoverable only when its operation is implemented. Global
capabilities describe structural support; `capabilities_for(owner)` describes the
actual owner's selected route and readiness. Permission, busy devices, missing
models and temporary provider failures do not become capability groups.

Source and resource references come from the host. They are scoped to an owner
and service epoch; an Agent cannot create access by supplying another owner's
reference. Reading an asset requires an ownership check before the storage read.
Image understanding requires image content in the result. An opaque reference or
metadata alone is not a visual observation.

## Lifetimes and data flow

One app-scoped service is shared by Agent tools, native UI and the product's
remote adapter. Every operation carries a host-issued identity, generation,
service epoch, trusted owner and bounded budget. Cancellation and shutdown affect
the matching resources. Recording finalization retains retryable state until the
asset is stored successfully; stop acknowledges a terminal result.

The app service epoch and a native backend's epoch have separate lifetimes.
The audio router validates the caller against the service epoch, then maps the
same operation identity to the selected backend's current epoch. Cancellation
uses that recorded mapping, so a native helper restart does not invalidate an
otherwise valid app service reference or cancel a different operation.

Events are target- and owner-scoped, sequenced and bounded by count and text byte
limits. Captured frames retain source identity and capture time. Continuous PCM
and video flow through bounded device ports, not repeated model tool calls.

`MediaConnector` supplies owner-visible sources and opens `MediaSession` objects.
It owns the RTC pump and the existing native device lease. The host-only
`session_audio` method returns an audio channel from that same session. It does
not open another microphone or create a competing input reader.

Audio rendering distinguishes accepted frames from frames actually played.
Flush and interrupt return playback progress; native realtime commits output
history only after the relevant playback is acknowledged.

## Observation and conversation

An observation session produces bounded observations without starting another
Agent. A conversation session requests a host-only handoff after its tool result
has been committed and ordinary turn work has drained. Native realtime attaches
to the current conversation, tool dispatcher and permission owner under the same
active turn. The UI entry point can still start a normal realtime turn.

The provider connection and user-to-host RTC connection serve different roles.
The host owns provider credentials; the product connector owns peer signaling
and ICE configuration. Neither is a second Agent runtime.

## Verification

Run the `media-api`, `media-runtime` and `tool-media` library tests, then the
affected runtime, bridge and platform checks. Test owner isolation, stale epochs,
timeout cancellation, recording finalization retries, stop acknowledgement,
bounded observations and audio/video mode negotiation. Native compilation and
mock transport tests do not establish physical capture or playback; report
fresh package and device acceptance separately.
