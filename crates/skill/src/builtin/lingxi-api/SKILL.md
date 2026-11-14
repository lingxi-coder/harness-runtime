---
name: lingxi-api
description: Explain, configure, debug, or integrate LingXi (灵犀) CLI, sessions, settings, tools, models, and host APIs. For app building use local-app; for ordinary coding use task tools.
---

# LingXi API guide

Help the user understand or use LingXi, in their language. This guide ships
offline. It describes product capabilities. The current host, session, tool
schemas, and returned status decide what can run here.

## Read the matching topic before answering

The first argument is a topic id. Call `Skill` with `skill: "lingxi-api"` and
`args: "<topic>"`, or use `/lingxi-api <topic> [question]`. Request the known
topic on the first call: skill installation/MCP uses `configuration`, failed
calls use `troubleshooting`. Load only relevant topics and reuse loaded ones.
File-based installations read the linked references beside this file; bundled
topics load offline through `Skill`, without probing private folders or a CLI.

| Topic | Read when the question concerns |
| --- | --- |
| [overview](references/overview.md) | Product surfaces, installable builds, command names |
| [teams](references/teams.md) | Teams identity, membership, connectors, Bot boundaries |
| [accounts](references/accounts.md) | Host-global provider accounts and execution routing |
| [configuration](references/configuration.md) | Settings, permission mode, MCP, hooks, plugins, skills, slash commands |
| [media](references/media.md) | Camera, microphone, transcription, speech, live assistance |
| [webrtc](references/webrtc.md) | Browser Remote Control. The CLI remote-control handler is not that page |
| [local-app](references/local-app.md) | Local Apps, publication, app-owned MCP, WebMCP, and AG-UI |
| [bots](references/bots.md) | Persistent Bots, Bot memory, routines, delegation |
| [devices](references/devices.md) | Computer Use, browser, Android automation, native device tools |
| [mobile-linux](references/mobile-linux.md) | Mobile Shell/Git, guest paths, rootfs |
| [sessions](references/sessions.md) | Resume, instruction files, config home, history, memory |
| [automation](references/automation.md) | Schedules, wakeups, Workflow, background tasks |
| [agents](references/agents.md) | Subagents, agent view, /fork, /subtask, Fusion |
| [models](references/models.md) | Model choice, provider profiles, credentials |
| [integrations](references/integrations.md) | Rust SDKs, native hosts, Bridge, protocol ownership |
| [troubleshooting](references/troubleshooting.md) | Version, logs, failed tools, unavailable features |

## Apply the guide to this session

Use the current host, caller catalog and tool schema. Feature support,
visible tools, permission, readiness and successful execution are separate
facts. Native UI and SDK declarations do not create model-callable tools.

Use LingXi's branded paths and provider-neutral routing, not remembered Claude
Code commands or models. Sessions covers instruction files; agents covers
/fork and /subtask; configuration covers settings, skills and ordinary MCP.
App-owned MCP follows the App lifecycle.

For actions, use the registered domain skill: `local-app` for App authoring,
`media`, `visualize`, `workflow-authoring`, `update-config` or mobile skills
where available. Reading help does not activate hardware, call an extra model,
change settings or publish an App. A skill adds no tools or permissions.

Keep accounts and grants with their host owners. Preserve pending questions,
permissions and task IDs during recovery; accepted/running/completed differ.
Inspect the selected source revision for SDK symbols. Label guide, source,
test and live evidence; repeating this guide is not independent verification.
