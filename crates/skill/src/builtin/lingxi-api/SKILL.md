---
name: lingxi-api
description: Explain, configure, troubleshoot, or integrate LingXi CLI, sessions, settings, tools, models, and host APIs. For app building use local-app; for ordinary coding use task tools.
---

# LingXi API guide

Help the user understand or use LingXi, in their language. This guide ships
offline. It describes product capabilities. The current host, session, tool
schemas, and returned status decide what can run here.

## Read the matching topic before answering

The first argument is one topic id. In a bundled host, call `Skill` with
`skill: "lingxi-api"` and `args: "<topic>"`, for example `args: "bots"`. A user
can enter `/lingxi-api <topic> [question]`. For a file-based installation, read
the linked reference next to this file. Answer from that topic. Do not answer
a product question from memory, and do not load every topic for a general
question. Load a second topic only when the task crosses domains.

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

## Stale training prior

A remembered Claude Code or other-product path is not LingXi behavior. The
loaded topic wins. Do not send the user to another product's docs. Anthropic
identifiers stay valid only on an actual Anthropic backend.

| Prior | Read |
| --- | --- |
| `CLAUDE.md` or a `.claude` directory is the instruction home | sessions |
| `claude mcp`, or a desktop import, is how you add a server | configuration |
| `/agents` opens a manager | agents |
| `/fork` always starts an in-session subagent | agents |
| `remote-control` opens the browser remote page | webrtc |
| `settings.local.json` changes the permission mode | configuration |

## Apply the guide to this session

Distinguish an implemented feature, a tool this caller can see, permission to
use it, resource readiness, and a successful result. An absent tool can still
have a native UI or SDK entry. Explain that entry. Do not invent a
model-callable command. Unknown readiness stays unknown.

For an action, use the domain skill when it is registered, then the live tool
schema. `media`, `local-app`, `visualize`, `workflow-authoring`,
`update-config`, and mobile device skills apply only where registered. This
guide does not add tools, permissions, workspace, or Bot grants.

Route Local App build, modify, or publish work to the registered `local-app`
skill. Explaining an app does not publish it. App-owned Desktop MCP uses the
App lifecycle. Other MCP servers use configuration. Do not write an App
declaration into global settings.

Read-only questions do not activate devices, change connections, write
settings, or call a model. For an action, use its authorized entry and check
the result. Saved settings, authentication, connection, and discovered tools
are separate facts. Media bytes and metadata are separate. Accepted, running,
and finished background work are separate.

These references are not a live catalog. For SDK symbols, read the selected
revision. Label evidence as bundled documentation, inspected source, host
status, a test, or a live operation. Another answer from this same guide is
not independent verification.
