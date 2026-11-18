# Teams, chats and messages

Sign in and open Teams (Web: `/teams`). For a personal assistant session, use
New Chat in LingXi. For a conversation with people, select the team first.

| Goal | Steps in the Teams workspace |
| --- | --- |
| Create a team | Open the team switcher → Create team, enter a name and confirm. The new team opens. |
| Invite or join | An owner or admin creates an invitation in team details. The recipient opens the link or enters the code under Join team, reviews the team and confirms. |
| Chat with a member | Direct messages → New message → Direct message. Choose one current member and open the DM. An existing DM with that member reopens. Friends also have a chat in Friends. |
| Start a group or channel | Direct messages → New message → Group, then pick participants and any participating Bots. Use Channels → New channel for a named channel. A group containing only Bots besides you also needs a name. |
| Chat with a Bot | Bot chats → add, choose an available shared Bot and open or start the chat. A Bot DM addresses that Bot automatically. |
| Send a message | Open the intended conversation, compose text or attach a file, wait for uploads to finish and choose Send. On Web/Desktop Enter sends and Shift+Enter adds a line. After reconnecting, check the original message. Web and Desktop retry temporary failures automatically. Use Retry on that entry if the button appears. |
| Let a Bot decide when to reply | In that shared Bot's publication settings enable **Let the Bot decide when to answer** and set its daily limit. This per-Bot option requires a configured Decision service on the maintainer's Mac and defaults off. A participant then selects the Bot under **Bots that answer on their own** in the conversation. An ordinary group or channel message can get a reply without an @, or the Bot can stay silent. With either switch off, unaddressed group messages do not trigger this Bot. |
| Ask one specific Bot | In a group or channel, type @ and pick the participating Bot. A Bot DM already addresses its Bot. Follow an accepted task's card for the result or a question. |
| Publish a Bot to a team | On its Mac, open the team's Bot directory, select the local Bot, choose the audience and files, review and confirm. Members open the published Bot in that team. |

A human team and an agent-worker team have separate participants. Ordinary
Chat/Code can prepare a message or give the UI route. Use a Teams action only
when the caller actually offers one, and report delivery from its receipt.
Keep invitations, conversation membership, task acceptance and completion
distinct. Team membership grants neither device pairing nor host file access.

For integrations start with `packages/teams/{index,messages,execution}.ts`,
`NewConversationDialog.tsx`, `Composer.tsx` and `workspace.ts` in the product.
