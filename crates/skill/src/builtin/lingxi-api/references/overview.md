# Product surfaces and capability discovery

LingXi combines an Agent runtime with product hosts. Desktop, CLI, Android,
iOS, a remotely connected viewer, and a Local App Agent have different services
and permissions. The same runtime does not make their tools interchangeable.

| Capability | Execution owner and useful entry |
| --- | --- |
| Coding and files | Current session tools and permitted workspace |
| Document authoring | `document-authoring` skill and actual file or connected-document tools |
| Camera, audio, live media | Host Media service; `media` skill when present |
| Desktop/browser/device actions | Configured native backend and its actual tools |
| Local Apps and inline visuals | Apps host / Visualization tool, with their skills |
| Persistent Bots | Bot-capable Desktop host; Bot UI and scoped router tools |
| History, memory, subagents | Session runtime and the caller's grants |
| Schedules and durable workflows | Active scheduler / Workflow service |
| Remote Control | Account, approved device, enabled host and session policies |
| Model communication | Provider-neutral llm-client and selected provider profile |
| Mobile command execution | Mobile Linux runtime, mounted paths and host lifecycle |

For “what can you do?”, give a short task-oriented overview and identify the
current host. Use tools already visible to the caller, or a catalog search tool
when present, to establish actual action entries. Do not enumerate a parent's
private tools or present all SDK interfaces as executable abilities. A remote
viewer controls the selected host; it does not acquire that host's files locally.

Offline help remains readable without a model/network service. Actual answers
or operations can still depend on a network model, an installed local model,
credentials, a device, or OS permission. A tool's absence is not evidence that
another product host lacks the feature. Offer that host's native route where known.

The product web console and each SDK have their own delivery state. A page or
API declaration alone does not establish a running hosted Agent or cloud Bot.

Source anchors: Harness `crates/runtime/src/{desktop,mobile}`,
`crates/tool-api/src/registry.rs`; product host composition and Remote Control.
