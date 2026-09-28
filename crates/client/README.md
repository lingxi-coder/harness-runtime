# Client integration

Shared contracts and runtime integration for desktop, mobile, and terminal clients.

| Module | Responsibility |
| --- | --- |
| `protocol` | Transport-independent commands, events, DTOs, and protocol version |
| `presentation` | Shared tool summaries, structured diffs, text rendering, and styles |
| `adapter` | Runtime-to-DTO conversion, event emission, and interactive request brokers |

`adapter` uses `protocol` and `presentation`. Neither may depend on `adapter`;
`presentation` also stays independent of the client wire DTOs. The source-level
boundary tests protect these rules after consolidation into one crate.

## Features

- Default: protocol only.
- `presentation`: enables shared presentation without the runtime adapter.
- `adapter`: enables runtime integration and includes `presentation`.
- `uniffi`: generates native-binding metadata for the enabled modules.
- `test-support`: includes `adapter` and exposes `adapter::MockSink` for downstream tests.
  Declare this feature on a dev-dependency, not the production dependency.

Presentation uses pure shell classification helpers from `tool-api`, not
`tool-shell`. It still inherits `tool-api`'s transitive dependencies; the feature
split does not make that dependency tree fully lightweight.

The `adapter::lowering` entry points remain stable. Model catalog and transcript
conversion are implemented in private domain modules. Request brokers are
transport-independent; `TurnEventEmitter` emits turn boundary events and does
not execute a turn.
