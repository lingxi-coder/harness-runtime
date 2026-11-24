# Friends / 好友

Friends gives two signed-in people a private one-to-one chat outside any team.
Open Friends (Web: `/friends`; Desktop: the Friends rail mode; iOS and
Android: Friends). A friendship is a hidden two-person team: it never appears
in the Teams list and does not count toward team limits.

| Goal | Steps |
| --- | --- |
| Set up your profile | Friends → Profile settings: set a display name and an optional username (3–24 letters, numbers or underscores; it can be changed but not removed). Choose **Let people find me by username** and **Let people find me by email**. |
| Add a friend | Add friend → Find someone, enter an exact `@username`, an email address or a friend link and choose Look up. Add an optional note, then send the request. If they had already asked you, this accepts. |
| Share your friend link | Add friend → My friend link → Copy link or Share link. Anyone who opens it and confirms becomes your friend right away, without your approval. Reset link stops the old link immediately; Turn off link disables it until you create a new one. |
| Answer requests | Friend requests → Received: Accept or Decline. Sent: Cancel request. |
| Add a teammate | Team details → Members → Add friend on that member. Shared membership is the proof, so search settings do not apply. |
| Chat | Select the friend → Open chat. Messages, replies, edits and recall work as in a team DM. |
| Remove or block | Friend menu → Remove friend or Block. Both end the friendship and delete the chat for both people. Blocking also stops them from finding you or sending requests until you unblock in Blocked people. Unblocking does not restore the friendship. |

Discovery is consent-based. Username search needs your username setting.
Email lookup finds only a verified address with the email setting on, and
your email address is never shown. A friend link works whatever the search
settings. Every miss looks the same: unknown, hidden, blocked or yourself.
The note is visible only to the two of you and never appears in a push.

## Shared Bots with a friend

In the friend chat open **Shared Bots**. Shared Bots runs on its maintainer's
Desktop; the maintainer can read every conversation with it. To share your
own Bot with this friend, open LingXi Desktop → that chat → Shared Bots →
**Share a Bot with** the friend, review and confirm. The friend gets one Bot DM per
Bot; a friendship has no groups or channels. **Stop sharing** blocks new
requests and keeps the history; sharing again restores it. Removing or
blocking the friend closes every Bot DM between you, and a new friendship
starts with no Bots.

## Invite a friend to a team

A team owner or admin opens the team details → **Invite friends**, picks a
current friend and chooses Invite. The friend sees the team's name and your
name under **Team invitations** in Friends and can Join or Dismiss. Nothing
else is shared until they join, always as a member. An invitation expires
after 7 days and is then listed as Expired. Repeating it while it is live
refreshes the same invitation without notifying again. The sender cannot see
whether it was read or declined; a joined member appears in the member list.

## Limits and errors

| Limit | Value |
| --- | --- |
| Friends per person | 500, checked for both people |
| Outgoing pending requests | 50; cancel one to send more |
| Lookups | 30 per 10 minutes |
| Requests, friend-link adds and team invites | 30 together per 10 minutes |
| Live team invitations held by one friend | 20 |
| Display name / note | 80 / 200 characters |

**No match found** means check the spelling or ask for a friend link. **Too
many lookups** clears by itself after a few minutes. An invitation that can no
longer be honoured (expired, team deleted, sender no longer an admin or friend)
is removed when the friend tries to join. A full team or the 20-team limit
keeps the invitation so the friend can retry later.

Ordinary Chat/Code cannot send friend requests or friend messages unless the
current caller offers such an action; give the UI route instead. Friendship
grants neither device pairing nor access to the friend's files or Local Apps.

Source anchors (product): `docs/{FRIENDS,FRIENDS-CLIENTS}.md`,
`packages/teams/{friends,FriendsMain,FriendBotsPanel,AddFriendView}.ts(x)`,
`apps/firebase/src/friends.mjs`.
