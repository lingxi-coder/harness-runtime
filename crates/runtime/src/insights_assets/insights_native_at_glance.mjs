// Source-exact zt from Claude Code 2.1.295 src_207151635.js. The Me
// binding below captures the actual source-built prompts/results and leaves
// request dispatch to the provider-neutral Rust caller.
import { REPORT_PROMPTS } from "./insights_report_section_prompts.mjs";
const Ot = REPORT_PROMPTS;
function _(value, replacer, space) { return JSON.stringify(value, replacer, space); }
let Me;
async function zt(e,s,n){let a=Array.from(s.values()).slice(0,50).map((S)=>`- ${S.brief_summary} (${S.outcome}, ${S.claude_helpfulness})`).join(`
`),l=Array.from(s.values()).filter((S)=>S.friction_detail).slice(0,20).map((S)=>`- ${S.friction_detail}`).join(`
`),m=Array.from(s.values()).flatMap((S)=>S.user_instructions_to_claude||[]).slice(0,15).map((S)=>`- ${S}`).join(`
`),y=_({sessions:e.total_sessions,analyzed:e.sessions_with_facets,date_range:e.date_range,messages:e.total_messages,hours:Math.round(e.total_duration_hours),commits:e.git_commits,top_tools:Object.entries(e.tool_counts).sort((S,L)=>L[1]-S[1]).slice(0,8),top_goals:Object.entries(e.goal_categories).sort((S,L)=>L[1]-S[1]).slice(0,8),outcomes:e.outcomes,satisfaction:e.satisfaction,friction:e.friction,success:e.success,languages:e.languages},null,2)+`

SESSION SUMMARIES:
`+a+`

FRICTION DETAILS:
`+l+`

USER INSTRUCTIONS TO LINGXI:
`+(m||"None captured"),w=await Promise.all(Ot.map((S)=>Me(S,y,n))),r={};for(let{name:S,result:L}of w)if(L)r[S]=L;let f=r.project_areas?.areas?.map((S)=>`- ${S.name}: ${S.description}`).join(`
`)||"",h=r.what_works?.impressive_workflows?.map((S)=>`- ${S.title}: ${S.description}`).join(`
`)||"",T=r.friction_analysis?.categories?.map((S)=>`- ${S.category}: ${S.description}`).join(`
`)||"",R=r.suggestions?.features_to_try?.map((S)=>`- ${S.feature}: ${S.one_liner}`).join(`
`)||"",I=r.suggestions?.usage_patterns?.map((S)=>`- ${S.title}: ${S.suggestion}`).join(`
`)||"",O=r.on_the_horizon?.opportunities?.map((S)=>`- ${S.title}: ${S.whats_possible}`).join(`
`)||"",z={name:"at_a_glance",prompt:`You're writing an "At a Glance" summary for a LingXi usage insights report for LingXi users. The goal is to help them understand their usage and improve how they can use LingXi better, especially as models improve.

Use this 4-part structure:

1. **What's working** - What is the user's unique style of interacting with LingXi and what are some impactful things they've done? You can include one or two details, but keep it high level since things might not be fresh in the user's memory. Don't be fluffy or overly complimentary. Also, don't focus on the tool calls they use.

2. **What's hindering you** - Split into (a) LingXi's fault (misunderstandings, wrong approaches, bugs) and (b) user-side friction (not providing enough context, environment issues -- ideally more general than just one project). Be honest but constructive.

3. **Quick wins to try** - Specific LingXi features they could try from the examples below, or a workflow technique if you think it's really compelling. (Avoid stuff like "Ask LingXi to confirm before taking actions" or "Type out more context up front" which are less compelling.)

4. **Ambitious workflows for better models** - As we move to much more capable models over the next 3-6 months, what should they prepare for? What workflows that seem impossible now will become possible? Draw from the appropriate section below.

Keep each section to 2-3 not-too-long sentences. Don't overwhelm the user. Don't mention specific numerical stats or underlined_categories from the session data below. Use a coaching tone.

RESPOND WITH ONLY A VALID JSON OBJECT:
{
  "whats_working": "(refer to instructions above)",
  "whats_hindering": "(refer to instructions above)",
  "quick_wins": "(refer to instructions above)",
  "ambitious_workflows": "(refer to instructions above)"
}

SESSION DATA:
${y}

## Project Areas (what user works on)
${f}

## Big Wins (impressive accomplishments)
${h}

## Friction Categories (where things go wrong)
${T}

## Features to Try
${R}

## Usage Patterns to Adopt
${I}

## On the Horizon (ambitious workflows for better models)
${O}`,maxTokens:8192},W=await Me(z,"",n);if(W.result)r.at_a_glance=W.result;return r}

export async function captureNativeQueries(aggregate, facets, sections = []) {
  const byName = new Map(sections.map((row) => [row.name, row.value]));
  const captured = { sections: [], at_a_glance: null };
  const previous = Me;
  Me = async (entry, data) => {
    const prompt = entry.prompt + `\n\nDATA:\n` + data;
    if (entry.name === "at_a_glance") {
      captured.at_a_glance = prompt;
      return { name: entry.name, result: null };
    }
    captured.sections.push({
      name: entry.name,
      prompt,
      max_tokens: entry.maxTokens,
    });
    return { name: entry.name, result: byName.get(entry.name) ?? null };
  };
  try {
    await zt(aggregate, new Map(facets.map((row) => [row.session_id, row.value])), null);
    return captured;
  } finally {
    Me = previous;
  }
}
