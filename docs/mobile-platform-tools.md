# Mobile platform tools and skills

Android and iOS expose native device operations directly to the model through the ordinary tool registry. A conversation does not need a Local App, an app ID, or a capability manifest to use them. The native Swift/Kotlin adapters remain the implementation boundary and retain OS permission checks.

| Tool | Operation | Input |
| --- | --- | --- |
| `camera` | Capture a photo or choose one from the library; return an image to the model | Existing camera action and options |
| `voice` / `speech` | Recording, transcription, speech and playback supported by the native audio service | Existing audio actions |
| `notification` | Post a local notification | Title, body and optional tag |
| `clipboard` | Read or write plain text | `action`, optional `text` |
| `share` | Present the native sharing sheet | `text` and/or `url` |
| `location` | Get one location fix | `{}` |
| `device_status` | Read battery, charging, coarse connectivity and power state | `{}` |
| `haptics` | Trigger one bounded feedback event | `style` |
| `open_url` | Ask the OS to open an external URL | `url` |
| `calendar` | Read events in a bounded interval | `start_ms`, `end_ms`, optional `limit` |
| `contacts` | Search contacts by name | `query`, optional `limit` |

The three bundled skills `mobile-device`, `mobile-media` and `mobile-personal-context` describe the available tools and task workflows. They enter the same live command registry used by the model skill listing, `Skill` and `/reload-skills`; they do not depend on the Local App plugin. Only groups with available tools are listed. Both Chat and Code sessions can use device tools; Chat still excludes code execution and workspace mutation tools.

## Contracts

- Register device tools only when the host supplies their native backend. Audio continues to project its current supported operations. Permission readiness is not a substitute for support.
- Location, calendar, contacts and external URL access go through ordinary tool permissions and then native OS permissions. Skills do not grant access through `allowed-tools`.
- New device tools use strict typed inputs and reject unknown fields or invalid bounds before native dispatch. Calendar ranges are at most 366 days and 100 events; contact searches are at most 200 characters and 50 results. Personal-data fields and lists are bounded; serialized calendar/contact results have 64/32 KiB budgets and report `truncated` when data is omitted or shortened.
- Native failures return structured error codes such as `permission_denied`, `unavailable` and `timeout`. Cancellation stops waiting; one-shot native providers may complete after cancellation because their interfaces do not expose cancellation handles. An already canceled call never dispatches.
- `open_url` accepts validated HTTP, HTTPS, mail and telephone links. A successful launch request does not prove that a message, payment or other action completed in another app.
- Camera output uses the runtime image-result contract, so subsequent model input includes the actual bounded image.
- Android calendar and contacts request runtime permission only when invoked from a foreground host. A denied or unavailable permission does not become an empty successful query.

## Local Apps and packaging

Generated Local App pages retain their own manifest declarations, app-scoped permission grants and `window.lingxi.v2` bridge. Those isolate untrusted page code and are separate from the direct model tool API.

The product consumes Harness through a canonical Git URL and immutable revision. Rebuilding Android/iOS against the updated revision is required to ship these changes; changing a sibling checkout alone does not update an installed app. FFI callback signatures are unchanged.
