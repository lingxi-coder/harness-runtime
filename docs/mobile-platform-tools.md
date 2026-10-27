# Mobile platform tools and skills

Android and iOS expose native device operations directly to the model through the ordinary tool registry. The native Swift/Kotlin adapters remain the implementation boundary and retain OS permission checks.

| Tool | Operation | Input |
| --- | --- | --- |
| `media_capture` | Capture from a supported source; return actual image content for visual analysis | Discovered source and bounded capture options |
| `media_record` / `media_stop` | Record and finalize an owner-scoped recording | Discovered source, then returned typed recording reference |
| `media_transcribe` / `media_synthesize` / `media_play` / `media_speak` | Recognition, synthesis and playback through the shared media service | Explicit live/stored input or returned media reference |
| `media_sources` / `media_status` | Discover supported sources and inspect readiness | Optional kind, usage or typed target |
| `media_session_start` / `media_session_update` / `media_observe` / `media_wait` | Manage and observe a bounded realtime session | Returned source/session/operation references |
| `notification` | Post a local notification | Title, body and optional tag |
| `clipboard` | Read or write plain text | `action`, optional `text` |
| `share` | Present the native sharing sheet | `text` and/or `url` |
| `location` | Get one location fix | `{}` |
| `device_status` | Read battery, charging, coarse connectivity and power state | `{}` |
| `haptics` | Trigger one bounded feedback event | `style` |
| `open_url` | Ask the OS to open an external URL | `url` |
| `calendar` | Read events in a bounded interval | `start_ms`, `end_ms`, optional `limit` |
| `contacts` | Search contacts by name | `query`, optional `limit` |

The bundled skills `mobile-device`, `media` and `mobile-personal-context` describe available workflows. `media` is shared with Desktop. They enter the live command registry used by the model skill listing, `Skill` and `/reload-skills`; they do not depend on the Local App plugin. A skill is listed when its tools are available and grants no device access. Both Chat and Code sessions can use device tools; Chat still excludes code execution and workspace mutation tools.

## Contracts

- Register device tools only when the host supplies their backend. The media service projects structural support for discovery and exact owner-specific routes/readiness before execution. Permission readiness is not a substitute for support.
- Location, calendar, contacts and external URL access go through ordinary tool permissions and then native OS permissions. Skills do not grant access through `allowed-tools`.
- New device tools use strict typed inputs and reject unknown fields or invalid bounds before native dispatch. Calendar ranges are at most 366 days and 100 events; contact searches are at most 200 characters and 50 results. Personal-data fields and lists are bounded; serialized calendar/contact results have 64/32 KiB budgets. `truncated` is true when data is omitted or shortened, or a native result reaches the requested limit. Because native providers return capped arrays without `has_more`, a full page may have additional matches and is conservatively marked even when it contains exactly every match; the flag is not proof of additional records.
- Native failures return structured error codes such as `permission_denied`, `unavailable` and `timeout`. Cancellation stops waiting; one-shot native providers may complete after cancellation because their interfaces do not expose cancellation handles. An already canceled call never dispatches.
- `open_url` accepts validated HTTP, HTTPS, mail and telephone links. A successful launch request does not prove that a message, payment or other action completed in another app.
- Camera output uses the runtime image-result contract, so subsequent model input includes the actual bounded image. The shared JPEG fallback samples during decoding before allocating full-resolution pixels and normalizes EXIF rotation/mirroring before re-encoding. Returned dimensions describe the delivered image; orientation-normalized original dimensions remain in resize metadata. Progressive JPEG coefficient storage remains subject to the decode memory budget.
- Android calendar and contacts request runtime permission only when invoked from a foreground host. A denied or unavailable permission does not become an empty successful query.

## Packaging

The product consumes Harness through a canonical Git URL and immutable revision. Rebuilding Android/iOS against the updated revision and regenerating native bindings is required to ship these changes; changing a sibling checkout alone does not update an installed app. See [Media service](architecture/media-service.md) for the common host and native port boundary.
