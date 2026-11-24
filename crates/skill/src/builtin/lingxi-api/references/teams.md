# Teams, channels, chats and messages

Sign in and open Teams (Web: `/teams`). For a personal assistant session, use
New Chat in LingXi. For a conversation with people, select the team first.
For one-to-one contacts outside a team read `friends`; for Bot setup and
audiences read `bots`; for shared data read `datasets`.

| Goal | Steps in the Teams workspace |
| --- | --- |
| Create a team | Open the team switcher → Create team, enter a name and confirm. The new team opens and you are its owner. |
| Invite or join | An owner or admin opens team details → Invites → Create invite link, chooses the role when joining and shares the link. The recipient opens the link or pastes it or the 32-character code under Join team, reviews the team and confirms. Opening a link never joins automatically. An owner or admin can also invite a current friend from team details → Invite friends. |
| Chat with a member | Direct messages → New message → Direct message. Choose one current member and open the DM. An existing DM with that member reopens. Friends also have a chat in Friends. |
| Start a group | Direct messages → New message → Group, then pick participants and any participating Bots. A group containing only Bots besides you also needs a name. |
| Create a channel | Use the + beside Channels → New channel, enter the channel name, choose its participants and any Bots, then Create channel. |
| Chat with a Bot | Bot chats → add, choose an available shared Bot and open or start the chat. A Bot DM addresses that Bot automatically. |
| Send a message | Open the intended conversation, compose text or attach a file, wait for uploads to finish and choose Send. On Web/Desktop Enter sends and Shift+Enter adds a line. After reconnecting, check the original message. Web and Desktop retry temporary failures automatically. Use Retry on that entry if the button appears. |
| Let a Bot decide when to reply | In that shared Bot's publication settings enable **Let the Bot decide when to answer** and set its daily limit. This per-Bot option requires a configured Decision service on the maintainer's Mac and defaults off. A participant then selects the Bot under **Bots that answer on their own** in the conversation. An ordinary group or channel message can get a reply without an @, or the Bot can stay silent. With either switch off, unaddressed group messages do not trigger this Bot. |
| Ask one specific Bot | In a group or channel, type @ and pick the participating Bot. A Bot DM already addresses its Bot. Follow an accepted task's card for the result or a question. |
| Publish a Bot to a team | On its Mac, open the team's Bot directory, select the local Bot, choose the audience and files, review and confirm. Members open the published Bot in that team. |

## Conversation kinds

A DM has exactly two members. A group and a channel are both multi-person
conversations with a fixed participant list chosen at creation. A channel has
a required name and appears under Channels. Every participant must be a
current team member. Nobody joins a conversation by browsing for it, and
team membership alone does not show a conversation you are not in.
A Bot's maintainer can read every conversation that includes that Bot,
but only participants can send messages there. The platform can also read
team messages; the UI discloses both.

## Messages

Use Reply on a message to quote it, Edit or Recall on your own message, and
Mention (@) to address a member or Bot. Only the author can edit or recall.
Recall clears the text for everyone and shows **Message recalled**. Unread
counts come from each person's read mark. Offline, recently loaded messages
stay readable (**Offline · saved messages**); a pending send keeps its
identity and is retried on reconnect. A rejected send stays on its entry and
can be discarded. If a mentioned member left, update the mention in a new draft.

## Roles, membership and limits

| Role | Can |
| --- | --- |
| Owner | Everything an admin can, plus delete the team, make or remove admins and **Make owner** (hand ownership to a member; the old owner stays an admin). The single owner cannot leave. |
| Admin | Create and revoke invites, invite friends and remove ordinary members |
| Member | Use conversations and shared Bots it is permitted to use, and **Leave team** |

Invite links grant 1–50 uses and expire after 1 hour to 7 days. An expired,
revoked or used-up invite shows an error and needs a new invite. Leaving or
removal ends access to the team's conversations and shared Bots; coming back
needs a new invite. **Delete team** removes access for everyone and cannot be
undone; it is access revocation, not erasure of copies already on devices.
A team holds at most 200 members and a person can be in at most 20 teams.
Friend chats do not count toward either limit.

## Team Bots, lists, documents and data

Team details → Bots lists the published Bots, their maintainer, whether you
may use each one (all members or the maintainer and listed members) and its
availability. A Bot runs on its maintainer's Desktop; while that host is
offline, requests queue. Team members cannot approve the Bot's tools. A Bot
can share a list or a document into a conversation: everyone who can use the
Bot can edit it, and a save after someone else's change asks you to compare
first. Conversation data opens Datasets shared into that conversation; see
`datasets`.

A human team and an agent-worker team have separate participants. Ordinary
Chat/Code can prepare a message or give the UI route. Use a Teams action only
when the caller actually offers one, and report delivery from its receipt.
Keep invitations, conversation membership, task acceptance and completion
distinct. Team membership grants neither device pairing nor host file access.

For integrations start with `packages/teams/{index,messages,execution}.ts`,
`NewConversationDialog.tsx`, `Composer.tsx`, `TeamDetailsDrawer.tsx` and
`workspace.ts` in the product; the contract is `docs/TEAMS.md`.
