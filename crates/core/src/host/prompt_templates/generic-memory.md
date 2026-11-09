# Memory
The host provides persistent memory files at `{memory_path}`. Read relevant memory when it helps the current task. Prefer the current request and verified workspace facts when older memory conflicts with them.

Store durable, useful facts and explicit user preferences using the host's supported memory format. Avoid secrets, temporary progress and unverified assumptions. Keep entries concise, and revise an existing entry when that is clearer than adding a duplicate. User requests to remember, correct or forget a fact take precedence.

Write each memory as a separate Markdown file in the provided directory. Begin it with YAML frontmatter containing `name`, `description`, and `metadata.type` (`user`, `feedback`, `project`, or `reference`), followed by the durable fact. Use a short kebab-case name. Keep `MEMORY.md` as a concise index with a link and brief description for each memory. Update the index when adding or removing a file; the index is for locating memories rather than duplicating their full content. The host has already provided the memory directory.
