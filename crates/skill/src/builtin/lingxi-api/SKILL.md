---
name: lingxi-api
description: LingXi (灵犀) product help. Create teams, chats or DMs, add friends (好友), send messages, manage or share Datasets (数据集), configure Bot Decision replies without @ (Bot 自主回复), publish or share Local Apps (共享 Local App), pair devices, or use the CLI and host APIs. For a new personal Bot use create-bot.
---

# LingXi API guide

Help the user understand or use LingXi, in their language. This guide ships
offline. It describes product capabilities. The current host, session, tool
schemas, and returned status decide what can run here.

## Read the matching topic before answering

Use `/lingxi-api <topic> [question]` or ask in ordinary language, for example
`/lingxi-api 如何发送团队消息`. With `Skill`, pass the topic as `args` when known.
Skill installation/MCP uses `configuration`. Start a failed product workflow
in its own topic. For a general tool failure use `troubleshooting`. Load only relevant topics.
File-based installations read the linked references beside this file; bundled
topics load offline through `Skill`, without probing private folders or a CLI.

| Topic | Read when the question concerns |
| --- | --- |
| [overview](references/overview.md) | Product surfaces, installable builds, command names |
| [teams](references/teams.md) | Create or join a team, roles and invites, open a DM, group or channel, send, edit or recall a message, Team Bots, lists and documents |
| [friends](references/friends.md) | Add a friend by username, email or friend link, requests, block or remove, share a Bot with a friend, invite a friend to a team |
| [datasets](references/datasets.md) | Data picker, schema and records, fixed/latest/edit grants, data sources and reports, Local App, visualization, Bot and Task bindings |
| [accounts](references/accounts.md) | Host-global provider accounts and execution routing |
| [configuration](references/configuration.md) | Settings, permission mode, MCP, hooks, plugins, skills, slash commands |
| [media](references/media.md) | Camera, microphone, transcription, speech, live assistance |
| [remote-control](references/webrtc.md) | Pair your devices and continue a host session. Also accepts `webrtc` |
| [local-app](references/local-app.md) | Publish on Desktop or Mobile, make a market listing public, share a source ZIP privately, or open a running app on a paired device |
| [bots](references/bots.md) | Create a personal Bot, publish it to a team or friend, audience and quotas, hand over, configure Decision, diagnose missing replies, delegate |
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

Use LingXi's branded paths and the current provider. Answer in Chat or Code
according to the request: give the actual UI steps for a product task and the
owning contract for an integration. Team member messages use the Teams
composer. Coordinator `SendMessage` addresses agent workers.

For actions, use the registered domain skill: `local-app` for App authoring,
`media`, `visualize`, `workflow-authoring`, `update-config` or mobile skills
where available. Reading help does not activate hardware, call an extra model,
change settings or publish an App. A skill adds no tools or permissions.

Keep accounts and grants with their host owners. Preserve pending questions,
permissions and task IDs during recovery; accepted/running/completed differ.
Inspect the selected source revision for SDK symbols. Label guide, source,
test and live evidence; repeating this guide is not independent verification.
