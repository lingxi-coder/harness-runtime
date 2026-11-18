# Persistent Desktop Bots

In Bot-capable builds, a Bot is a persistent persona with its own configuration
home, workspace, chats, memory, skills and routines. Its `botId` identifies that
resource; a chat's session ID, a delegated task ID and a subagent ID identify
different resources. A Bot is not just a named one-turn subagent.

The implemented Desktop Bot host runs on macOS. Android, iOS and Web viewers
can reach enabled Bots through an approved Remote Control connection when their
build supports it. Work executes on the computer. An old build, disconnected
host, empty list or disabled Bot does not establish an always-running cloud Bot.
Check the host's actual availability and selected Bot; explain the native Bots
page when the current caller has no Bot management tools.

For conversational setup, use the `create-bot` Skill (or `/create-bot` with the
owner's request). It clarifies the goal and presents a Bot proposal. Desktop's
**Review & create** action fills the native creation form; the owner edits it
and chooses **Create Bot**. A proposal alone is not a created Bot.

The owner creates/configures Bots in that UI: persona, model, explicit permission
mode, bound read/write folders, imported skills, permitted senders, routines and
remote access. Updates use the observed revision. Skill import copies once;
it does not stay synchronized with the original. Bot memory and configuration
belong to that Bot, not the owner's ordinary chats or another Bot. Do not edit
host-managed settings, persona, router tokens or grants to widen access.

## Delegation from a Bot session

Only when the current catalog exposes the scoped `bots` MCP router, use its
tools (normally `mcp__bots__list_bots`, `delegate_task`, `get_task`, `message_bot`,
`cancel_task`, `attach_artifact`, each under that same MCP prefix). Inspect the
actual schemas rather than constructing a private IPC/RPC request.

List eligible recipients and check `accepts_tasks_from_you`. Delegate a bounded
goal with needed input files; the recipient runs with its own permissions and
workspace. Retain the returned `task_id`; collect state/result with `get_task`.
Reuse an idempotency key only for the same submission. Follow-ups target that
task, not an invented chat. Files and artifacts are copied through the router's
permitted workspace paths; do not read another Bot's private directory.

`submitted`/`working` are not completion. `input-required` needs the owner's
answer in the receiving Bot's chat; another Bot cannot approve for the owner.
Report `completed`, `failed`, `canceled` or `rejected` from actual state. Parent
turn interruption is not proof that a background process has terminated.

Routines and tasks need the Desktop host alive; login-item and keep-awake
settings have explicit OS/user controls and do not make work survive every
shutdown. Remote access requires the computer and Bot switches plus device
approval. If the Bot feature or router is absent, explain the supported route
without silently substituting a subagent, Cron job or cloud deployment.

Source anchors (Bot-capable product): `apps/electron/src/main/bots/{service,
store,routerTools,botSettings}.ts`, `apps/electron/src/shared/bots.ts`,
`apps/electron/src/main/remoteControl/remoteBots.ts`,
`packages/remote-control-client/src/session/bots.rs`.

For sharing, ask who will run the Bot. A paired device can open your personal
Bot on your Mac. Share a running Bot with a friend in the friend's Shared Bots
panel, or publish to a team's Bot directory. A hosted service (`/services`)
serves its chosen audience. A Bot Market recipient installs a personal copy
and chooses their own model and resources.

For a missing reply, identify the conversation and read its visible status.
Check host availability, publication, permission and quota. In a team, open
the task card for a wait or failure. When a shared Bot and the conversation
both enable Decision auto-reply, an ordinary message can get a reply without
an @. Silence is a valid choice even in an opted-in Bot DM. When Decision
returns no verdict for an otherwise unaddressed message, a group stays quiet
and a Bot DM proceeds. In a personal Bot router conversation,
follow up on the original task ID after completion or failure.
