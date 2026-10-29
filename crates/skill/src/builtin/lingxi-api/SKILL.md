---
name: lingxi-api
description: Explain, configure, troubleshoot or integrate LingXi (灵犀), including Bots, media and Local Apps. Use for product questions; ordinary coding uses its own tools.
---

# LingXi API guide

Help the user understand or use LingXi, in their language. This guide ships
offline with the runtime. It describes product capabilities; the current host,
session, tool schemas, and returned status determine what can run here.

## Read the relevant topic

In a bundled host, call `Skill` with `skill: "lingxi-api"` and `args: "<topic>"`
to load one topic, for example `args: "bots"` or `args: "media"`. A user can
also enter `/lingxi-api <topic> [question]`. For a file-based installation, read the
linked reference relative to this file. Load another topic when the task crosses
domains; do not load every reference for a general question.

| Topic | Read when the question concerns |
| --- | --- |
| [overview](references/overview.md) | Product surfaces, capability discovery, platform differences, offline use |
| [configuration](references/configuration.md) | Settings, MCP, permissions, hooks, plugins, skill customization |
| [media](references/media.md) | Camera, microphone, audio, transcription, speech, live assistance |
| [webrtc](references/webrtc.md) | Remote Control, device connections, media transport |
| [local-app](references/local-app.md) | Local Apps, publication, retained data, AG-UI, WebMCP, visualization |
| [bots](references/bots.md) | Persistent Bots, Bot memory, routines, delegation, remote Bot chats |
| [devices](references/devices.md) | Computer Use, Browser, Android automation, native device tools |
| [mobile-linux](references/mobile-linux.md) | Mobile Shell/Git, guest paths, rootfs, runtime availability |
| [sessions](references/sessions.md) | History, resume, forks, Chat/Code, context and memory |
| [automation](references/automation.md) | Scheduled work, wakeups, Workflow, background tasks |
| [agents](references/agents.md) | Subagents, Fusion, collaboration, task ownership |
| [models](references/models.md) | Provider profiles, model selection, credentials, model API boundaries |
| [integrations](references/integrations.md) | Rust SDKs, native hosts, Bridge, App SDK, protocol ownership |
| [troubleshooting](references/troubleshooting.md) | Version checks, logs, failed tools, unavailable features |

## Apply the guide to this session

Distinguish an implemented product feature from a tool visible to this caller,
permission to use it, resource readiness, and a successful operation. An absent
tool can still have a native UI or SDK entry. Explain that entry without
inventing a model-callable command. Unknown readiness remains unknown.

For an action, use the existing domain skill if it is in the current catalog,
then the live tool schema. `media`, `local-app`, `visualize`, `workflow-authoring`,
`update-config`, and mobile device skills are useful only where registered.
External builder plugins are optional; do not assume one is installed. This
guide does not extend the caller's tools, permissions, workspace, or Bot grants.

Read-only product questions require no device activation, connection changes,
configuration writes, or model requests. When the user requests an action,
perform it through its existing authorized entry and verify its actual result.
For configuration, distinguish saved settings, authentication, connection, and
discovered tools. For media, distinguish returned content from metadata. For
background work, distinguish acceptance, execution, and completion.

Use the current host's version/status entry when an exact build matters. These
references are not live availability snapshots or an up-to-date online catalog.
For SDK symbols and parameters, inspect the selected revision's schema/source
or compiler-generated documentation instead of guessing from another release.
