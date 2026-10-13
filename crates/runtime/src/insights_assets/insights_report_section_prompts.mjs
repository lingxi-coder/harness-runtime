const REPORT_PROMPTS=[{name:"project_areas",prompt:`Analyze this LingXi usage data and identify project areas.

RESPOND WITH ONLY A VALID JSON OBJECT:
{
  "areas": [
    {"name": "Area name", "session_count": N, "description": "2-3 sentences about what was worked on and how LingXi was used."}
  ]
}

Include 4-5 areas. Skip internal LingXi operations.`,maxTokens:8192},{name:"interaction_style",prompt:`Analyze this LingXi usage data and describe the user's interaction style.

RESPOND WITH ONLY A VALID JSON OBJECT:
{
  "narrative": "2-3 paragraphs analyzing HOW the user interacts with LingXi. Use second person 'you'. Describe patterns: iterate quickly vs detailed upfront specs? Interrupt often or let LingXi run? Include specific examples. Use **bold** for key insights.",
  "key_pattern": "One sentence summary of most distinctive interaction style"
}`,maxTokens:8192},{name:"what_works",prompt:`Analyze this LingXi usage data and identify what's working well for this user. Use second person ("you").

RESPOND WITH ONLY A VALID JSON OBJECT:
{
  "intro": "1 sentence of context",
  "impressive_workflows": [
    {"title": "Short title (3-6 words)", "description": "2-3 sentences describing the impressive workflow or approach. Use 'you' not 'the user'."}
  ]
}

Include 3 impressive workflows.`,maxTokens:8192},{name:"friction_analysis",prompt:`Analyze this LingXi usage data and identify friction points for this user. Use second person ("you").

RESPOND WITH ONLY A VALID JSON OBJECT:
{
  "intro": "1 sentence summarizing friction patterns",
  "categories": [
    {"category": "Concrete category name", "description": "1-2 sentences explaining this category and what could be done differently. Use 'you' not 'the user'.", "examples": ["Specific example with consequence", "Another example"]}
  ]
}

Include 3 friction categories with 2 examples each.`,maxTokens:8192},{name:"suggestions",prompt:`Analyze this LingXi usage data and suggest improvements.

## LINGXI FEATURES REFERENCE (pick from these for features_to_try):
1. **MCP Servers**: Connect LingXi to external tools, databases, and APIs via Model Context Protocol.
   - How to use: Run \`lingxi-cli mcp add <server-name> -- <command>\`
   - Good for: database queries, Slack integration, GitHub issue lookup, connecting to internal APIs

2. **Custom Skills**: Reusable prompts you define as markdown files that run with a single /command.
   - How to use: Create \`.lingxi/skills/commit/SKILL.md\` with instructions. Then type \`/commit\` to run it.
   - Good for: repetitive workflows - /commit, /review, /test, /deploy, /pr, or complex multi-step workflows

3. **Hooks**: Shell commands that auto-run at specific lifecycle events.
   - How to use: Add to \`.lingxi/settings.json\` under "hooks" key.
   - Good for: auto-formatting code, running type checks, enforcing conventions

4. **Headless Mode**: Run LingXi non-interactively from scripts and CI/CD.
   - How to use: \`lingxi-cli --print "fix lint errors"\`
   - Good for: CI/CD integration, batch code fixes, automated reviews

5. **Task Agents**: LingXi spawns focused subagents for complex exploration or parallel work.
   - How to use: LingXi auto-invokes when helpful, or ask "use an agent to explore X"
   - Good for: codebase exploration, understanding complex systems

RESPOND WITH ONLY A VALID JSON OBJECT:
{
  "claude_md_additions": [
    {"addition": "A specific line or block to add to LINGXI.md based on workflow patterns. E.g., 'Always run tests after modifying auth-related files'", "why": "1 sentence explaining why this would help based on actual sessions", "prompt_scaffold": "Instructions for where to add this in LINGXI.md. E.g., 'Add under ## Testing section'"}
  ],
  "features_to_try": [
    {"feature": "Feature name from LINGXI FEATURES REFERENCE above", "one_liner": "What it does", "why_for_you": "Why this would help YOU based on your sessions", "example_code": "Actual command or config to copy"}
  ],
  "usage_patterns": [
    {"title": "Short title", "suggestion": "1-2 sentence summary", "detail": "3-4 sentences explaining how this applies to YOUR work", "copyable_prompt": "A specific prompt to copy and try"}
  ]
}

IMPORTANT for claude_md_additions: PRIORITIZE instructions that appear MULTIPLE TIMES in the user data. If user told LingXi the same thing in 2+ sessions (e.g., 'always run tests', 'use TypeScript'), that's a PRIME candidate - they shouldn't have to repeat themselves.

IMPORTANT for features_to_try: Pick 2-3 from the LINGXI FEATURES REFERENCE above. Include 2-3 items for each category.`,maxTokens:8192},{name:"on_the_horizon",prompt:`Analyze this LingXi usage data and identify future opportunities.

RESPOND WITH ONLY A VALID JSON OBJECT:
{
  "intro": "1 sentence about evolving AI-assisted development",
  "opportunities": [
    {"title": "Short title (4-8 words)", "whats_possible": "2-3 ambitious sentences about autonomous workflows", "how_to_try": "1-2 sentences mentioning relevant tooling", "copyable_prompt": "Detailed prompt to try"}
  ]
}

Include 3 opportunities. Think BIG - autonomous workflows, parallel agents, iterating against tests.`,maxTokens:8192},...[],{name:"fun_ending",prompt:`Analyze this LingXi usage data and find a memorable moment.

RESPOND WITH ONLY A VALID JSON OBJECT:
{
  "headline": "A memorable QUALITATIVE moment from the transcripts - not a statistic. Something human, funny, or surprising.",
  "detail": "Brief context about when/where this happened"
}

Find something genuinely interesting or amusing from the session summaries.`,maxTokens:8192}];

export { REPORT_PROMPTS };
