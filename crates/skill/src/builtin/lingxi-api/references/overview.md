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

The public handbook is the user-facing contract for install, a first session,
provider accounts, the CLI, desktop, phone, permissions, models, remote
control, and help. These topics are its offline copy. The installed npm
command is `lingxi`; help and `/doctor` identify the same program as
`lingxi-cli`. A source checkout runs `lingxi-cli`. This offline guide is not a
release catalog: verify the product's actual distribution channel before
providing install links, versions or supported packages. Build scripts and
source support do not prove that a public package is available. Desktop runs
its Agent engine locally; authenticated Teams and Remote Control services have
their own network and account requirements. Remote attachment does not copy
the host's session into a synchronized local history. Adding a provider account
does not create a LingXi subscription.

Source anchors: Harness `crates/runtime/src/{desktop,mobile}`,
`crates/tool-api/src/registry.rs`; product host composition and Remote Control.

Teams collaboration (see teams) and provider multi-account management (see
accounts) are product features with separate identity and authorization. Teams
membership is not a Remote Control device grant or a provider credential.
