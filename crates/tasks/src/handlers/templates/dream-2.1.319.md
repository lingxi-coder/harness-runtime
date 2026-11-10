# Dream: Memory Consolidation

You are performing a dream — a reflective pass over your memory files. Synthesize what you've learned recently into durable, well-organized memories so that future sessions can orient quickly.

Memory directory: `{memory_directory}`
This directory already exists — write to it directly with the Write tool (do not run mkdir or check for its existence).

Session transcripts: `{transcript_directory}` (large JSONL files — grep narrowly, don't read whole files)

{team_memory}---

## Phase 1 — Orient

- `ls` the memory directory to see what already exists
- Read `MEMORY.md` to understand the current index
- Skim existing topic files so you improve them rather than creating duplicates
- If a `sessions/` subdirectory exists, review recent entries there

## Phase 2 — Gather recent signal

Look for new information worth persisting. Sources in rough priority order:

1. **Existing memories that drifted** — facts that contradict something you see in the codebase now
2. **Transcript search** — if you need specific context (e.g., "what was the error message from yesterday's build failure?"), grep the JSONL transcripts for narrow terms:
   `grep -rn "<narrow term>" {transcript_directory}/ --include="*.jsonl" | tail -50`

Don't exhaustively read transcripts. Look only for things you already suspect matter.

## Phase 3 — Consolidate

For each thing worth remembering, write or update a memory file at the top level of the memory directory. Use the memory file format and type conventions from your system prompt's auto-memory section — it's the source of truth for what to save, how to structure it, and what NOT to save.{mcp_memory}

Focus on:
- Merging new signal into existing topic files rather than creating near-duplicates
- Converting relative dates ("yesterday", "last week") to absolute dates so they remain interpretable after time passes
- Deleting contradicted facts — if today's investigation disproves an old memory, fix it at the source

## Phase 4 — Prune and index

Update `MEMORY.md` so it stays under 200 lines AND under ~25KB. It's an **index**, not a dump — each entry should be one line under ~150 characters: `- [Title](file.md) — one-line hook`. Never write memory content directly into it.

- Remove pointers to memories that are now stale, wrong, or superseded
- Demote verbose entries: if an index line is over ~200 chars, it's carrying content that belongs in the topic file — shorten the line, move the detail
- Add pointers to newly important memories
- Resolve contradictions — if two files disagree, fix the wrong one

### Reconcile memories against LINGXI.md

Project LINGXI.md instructions are loaded in your system prompt. For each memory that captures feedback or project conventions (the `feedback`/`project` types, where tagged), check whether it contradicts a LINGXI.md instruction on the same topic:

- **Memory is stale** — LINGXI.md and the memory describe different procedures for the same task: LINGXI.md is the maintained, checked-in source. Delete the memory, or rewrite it to agree if it carries context worth keeping (the *why* is still useful but the *how* is wrong).
- **LINGXI.md may be stale** — the memory is clearly dated after LINGXI.md and explicitly corrects it: do NOT edit LINGXI.md during a dream. Annotate the memory with "contradicts LINGXI.md — verify which is current" and list it in your summary so the user can update LINGXI.md.
- **Not a conflict** — the memory adds detail LINGXI.md doesn't cover, or narrows a LINGXI.md rule with a stated reason. Leave it.

A `feedback` memory's "Why: the user corrected me" framing is not evidence it's newer than LINGXI.md — LINGXI.md may have been updated since.

---

Return a brief summary of what you consolidated, updated, or pruned. If nothing changed (memories are already tight), say so.{additional_context}