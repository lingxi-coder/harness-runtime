---
name: create-bot
description: "Help users create a persistent LingXi Bot through a conversation: clarify its purpose, write its persona, and prepare a configuration for the Desktop Bot creation form. Use for new Bots, not one-turn subagents or third-party messaging bots."
---

# Create a Bot

A Bot is a persistent assistant with its own persona, memory and conversations.
Help the user turn their goal into a useful Bot, in the language they use.

## Shape the Bot together

Start from what the user already told you. When the request is vague, ask one
or two focused questions about the work it should do and the result they want.
Offer concrete choices when that makes answering easier. Ask in ordinary chat;
do not require a questionnaire or an AskUserQuestion tool. If the request is
already clear, propose a draft immediately. Use reasonable defaults for minor
details and let the user refine them.

Write a short, recognizable name, a one-sentence description, and a standalone
persona. The persona should explain its role, recurring workflow, expected
output, tone, and when to clarify or report uncertainty. Incorporate the user's
constraints. It must work in a fresh Bot conversation without this chat's history.
Avoid invented credentials, integrations, data sources, schedules, or claims
that an ability is already configured. If the user requests scheduled work,
describe it as a follow-up to configure after creation.

Discuss folders, tools or skills only when relevant. The owner chooses the
actual model, folders and permissions in the creation form. A persona cannot
grant access. Do not edit Bot storage, host configuration, router tokens or
other Bots to create this one.

## Present a usable draft

Once there is enough information, briefly explain the proposed Bot and emit
one complete fenced block with language `lingxi-bot`. The block must contain
valid JSON with exactly these keys:

```lingxi-bot
{
  "schemaVersion": 1,
  "name": "Writing partner",
  "description": "Helps turn rough ideas into clear, concise writing.",
  "persona": "You are a writing partner. Clarify the audience and purpose when they are missing. Preserve the user's meaning and voice. Offer a concise revision, then explain significant changes. Flag facts that need verification rather than inventing sources."
}
```

Use the user's actual request, not the example text. Name: 1–80 characters,
no control characters. Description: at most 500 characters. Persona: nonempty,
at most 64 KiB in UTF-8. Prefer a concise, specific persona. Escape newlines
and quotes correctly inside JSON strings. Do not add model IDs, file paths,
permissions, remote access, or skill installation to this block.

Desktop renders this block as a preview with **Review & create**. Tell the user
they can refine the draft in chat or open that card, edit the fields, and choose
**Create Bot**. The card only proposes a configuration: emitting it does not
create a Bot. Creation succeeds only when the native form reports success.
For a revision, emit a complete replacement block so it can be used on its own.

If the current client does not render the card, explain how to copy these fields
into Desktop → Bots → New Bot → Set up manually. Do not claim a Bot was created,
substitute a subagent, or invent a management tool when that capability is absent.
