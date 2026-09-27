# LingXi Local Apps Runtime OS v2

This document is the human-readable contract for the v2 runtime. The source
catalog is `local-apps::runtime_v2::CapabilityRegistry`; generated SDKs must
consume that catalog rather than inventing capability names.

## Topology

```text
Generated Local App page
        │ window.lingxi.v2 (native bridge; privileged + streams)
        ▼
Host capability router ───── Host SQLite / secure handles / system APIs
        ▲
        │ one physical in-process MCP hub
        │ app_<app-id>__<capability> logical namespaces
        ▼
Conversation Agent / app-owned Agent sessions
        │
        ▼
System background adapter (Android Service/WorkManager, iOS BGTaskScheduler)
```

The loopback server serves static assets only. It is not a privileged API
server. Native bridge calls are host-bound, and the MCP hub binds the app id
from the tool namespace instead of accepting it as an Agent argument.

## Contract surface

| Surface | Examples | Transport | State |
| --- | --- | --- | --- |
| Native bridge | `data.query`, `network.request`, `llm.complete`, `llm.stream` | `window.lingxi.v2` → native command | Host-authorized |
| App MCP | `app_<id>__data_query`, `app_<id>__data_mutate`, `app_<id>__flow_execute` | Host-owned in-process MCP | Dynamic, app-id bound |
| Agent sessions | `agent.sessions.create/list/resume/close`, `agent.send/stream/cancel`, `agent_events_read` | App MCP + native stream adapter | Persistent, budgeted |
| Agent Profile | `agent.profiles.propose-update` | Host storage; apply is reserved for trusted host UI | Revisioned |
| Flows | `app_<id>__flow_execute` / `flow.execute` | Declarative MCP | Acyclic, bounded, host-routed |
| Background | `background.schedule/resume` | System task adapter | Journaled/resumable |

`agent.send/stream/cancel` now run through the existing
`ConversationOrchestrator` turn executor. Each live app session gets its own
orchestrator and an app-scoped MCP registry; the host owns the cancellation
token, output-token budget, MCP/bridge-call budgets, wall-clock budget, stream
ordering, and session turn count. Stream frames are forwarded to the page
through `window.lingxi.v2.agent.onFrame`; LLM and Agent listeners are isolated
by stream channel, so a listener never receives the other subsystem's frames.
The session catalog and bounded
conversation history are stored under the app data root and replayed when the
host recreates the live orchestrator.

`agent.post` without `sessionId` targets the Conversation Agent mailbox. When
an active app-owned `sessionId` is supplied, the event is stored in that
session's independent inbox and exposed to that Agent as the host-bound
`app_<id>__agent_events_read` MCP tool. Both event paths are untrusted data;
the app Agent cannot read a sibling session's inbox.

`flow.execute` is exposed only on an app-scoped MCP transport. The host binds
the app id from the namespace, validates the flow against the v2 capability
registry, rejects recursive/background/streaming/cancel steps, applies a
15-minute flow and 60-second per-step wall-clock budget, and routes every
remaining step through its normal capability authorization path. Step outputs
are returned under their stable step ids; input JSON remains declarative and
cannot contain executable code.

## Prompt layers

Every app Agent turn assembles these layers in order:

1. immutable platform core;
2. immutable runtime/security policy;
3. user-approved App Agent Profile revision;
4. session goal and memory;
5. current turn context and app events as untrusted data.

An app may propose a Profile revision. The proposal is inert until a trusted
host UI approval path applies it; app pages cannot self-approve a prompt
change. The native clients receive a one-time approval token and the Host
validates that token plus the current revision before persisting the next
profile revision. Until a profile is approved, `system_prompt_override` is not
used for app customization because it would replace the platform prompt.

Background scheduling is currently split into two explicit layers: the Host
validates, journals, claims, and executes the declarative flow, while Android
WorkManager/iOS BGTaskScheduler adapters are responsible only for waking the
process and invoking the Host background entrypoint. The persisted journal is
the recovery boundary; it does not silently execute a flow from a foreground
WebView callback.

## Attribution and limits

The host constructs `InvocationContext` containing `app_id`,
`app_instance_id`, `request_id`, optional `turn_id`, principal origin, grant
epoch, capability instance, and call chain. The runtime rejects invalid ids,
zero grant epochs, call chains deeper than 16, duplicate normalized calls,
stream gaps, duplicate terminals, and oversized stream frames.

Headless/background origins cannot start interactive capabilities. Media must
be represented by opaque host-scoped handles in the native adapter; raw paths
are not part of the v2 app contract.

## Versioning

New apps write `runtimeApiVersion: 2` into their manifest and receive only
`window.lingxi.v2`. A legacy manifest remains readable for migration and
rebuild, but runtime startup returns `runtime_api_incompatible` and no v1
bridge object is installed. Old source/data are preserved.
