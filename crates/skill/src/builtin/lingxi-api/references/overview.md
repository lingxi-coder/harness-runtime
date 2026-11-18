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

For “what can you do?”, identify the host and give a short task-oriented
overview based on its visible catalog/search tools. Native UI, SDK declarations
and another host's tools are not model-callable grants. Remote viewers operate
on their selected host, not a local copy of its files. Models, credentials,
network, hardware and OS permission determine actual readiness.

The installed npm command is `lingxi`. Help and `/doctor` call the same program
`lingxi-cli`, also its name in a source checkout. Check the distribution channel
for current builds and install links. Desktop runs the engine locally. Teams
and Remote Control each require a signed-in account. A remote session belongs
to its host. Provider accounts supply model access independently of subscriptions.

Source anchors: Harness `crates/runtime/src/{desktop,mobile}`,
`crates/tool-api/src/registry.rs`; product host composition and Remote Control.

For membership and messaging read `teams`. For provider routing read `accounts`.
