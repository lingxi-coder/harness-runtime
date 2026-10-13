// Protocol adapters for source-bound Claude Code 2.1.295 Insights transforms.
// The source-sensitive literals and pure helpers below are copied from the
// exact pinned ranges documented in insights-source/implementation-source-map.md.
import {
  Ce,
  Te,
  metadata_for_transcript,
} from "./metadata-source/insights-native-actions.mjs";
import { REPORT_PROMPTS } from "./insights_report_section_prompts.mjs";
import { captureNativeQueries } from "./insights_native_at_glance.mjs";
import { It, Mt, St } from "./insights_native_aggregate.mjs";

const FACET_GUIDELINES = `Analyze this LingXi session and extract structured facets.

CRITICAL GUIDELINES:

1. **goal_categories**: Count ONLY what the USER explicitly asked for.
   - DO NOT count LingXi's autonomous codebase exploration
   - DO NOT count work LingXi decided to do on its own
   - ONLY count when user says "can you...", "please...", "I need...", "let's..."

2. **user_satisfaction_counts**: Base ONLY on explicit user signals.
   - "Yay!", "great!", "perfect!" → happy
   - "thanks", "looks good", "that works" → satisfied
   - "ok, now let's..." (continuing without complaint) → likely_satisfied
   - "that's not right", "try again" → dissatisfied
   - "this is broken", "I give up" → frustrated

3. **friction_counts**: Be specific about what went wrong.
   - misunderstood_request: LingXi interpreted incorrectly
   - wrong_approach: Right goal, wrong solution method
   - buggy_code: Code didn't work correctly
   - user_rejected_action: User said no/stop to a tool call
   - excessive_changes: Over-engineered or changed too much

4. If very short or just warmup, use warmup_minimal for goal_category

SESSION:
`;

const FACET_SCHEMA = `

RESPOND WITH ONLY A VALID JSON OBJECT matching this schema:
{
  "underlying_goal": "What the user fundamentally wanted to achieve",
  "goal_categories": {"category_name": count, ...},
  "outcome": "fully_achieved|mostly_achieved|partially_achieved|not_achieved|unclear_from_transcript",
  "user_satisfaction_counts": {"level": count, ...},
  "claude_helpfulness": "unhelpful|slightly_helpful|moderately_helpful|very_helpful|essential",
  "session_type": "single_task|multi_task|iterative_refinement|exploration|quick_question",
  "friction_counts": {"friction_type": count, ...},
  "friction_detail": "One sentence describing friction or empty",
  "primary_success": "none|fast_accurate_search|correct_code_edits|good_explanations|proactive_help|multi_file_changes|good_debugging",
  "brief_summary": "One sentence: what user wanted and whether they got it"
}`;

const LONG_SESSION_SUMMARY = `Summarize this portion of a LingXi session transcript. Focus on:
1. What the user asked for
2. What LingXi did (tools used, files modified)
3. Any friction or issues
4. The outcome

Keep it concise - 3-5 sentences. Preserve specific details like file names, error messages, and user feedback.

TRANSCRIPT CHUNK:
`;

function sessionHeader(metadata) {
  const lines = [];
  lines.push(`Session: ${metadata.session_id.slice(0, 8)}`);
  lines.push(`Date: ${metadata.start_time}`);
  lines.push(`Project: ${metadata.project_path}`);
  lines.push(`Duration: ${metadata.duration_minutes} min`);
  lines.push("");
  return lines;
}

// Source `mt` with its `Ce` result already captured by the corpus adapter.
function mt(metadata, messages) {
  const lines = sessionHeader(metadata);
  for (const line of Te(messages, { userTextChars: 500, assistantTextChars: 300 })) {
    lines.push(line);
  }
  return lines.join("\n");
}

function facetPrompt(transcript) {
  return `${FACET_GUIDELINES}${transcript}${FACET_SCHEMA}`;
}

function responseJson(response) {
  if (typeof response !== "string") return null;
  const match = response.match(/\{[\s\S]*\}/);
  if (!match) return null;
  try {
    return JSON.parse(match[0]);
  } catch {
    return null;
  }
}

// Exact shallow cache/query result admission from source `Ae`.
function Ae(value) {
  if (!value || typeof value !== "object") return false;
  const result = value;
  return typeof result.underlying_goal === "string"
    && typeof result.outcome === "string"
    && typeof result.brief_summary === "string"
    && result.goal_categories !== null
    && typeof result.goal_categories === "object"
    && result.user_satisfaction_counts !== null
    && typeof result.user_satisfaction_counts === "object"
    && result.friction_counts !== null
    && typeof result.friction_counts === "object";
}

// Exact Wt warmup-minimal test, preserving Object.keys and JS coercion.
function warmupMinimal(value) {
  const categories = value?.goal_categories;
  const keys = categories ? Object.keys(categories) : [];
  const positive = keys.filter((key) => (categories[key] ?? 0) > 0);
  return positive.length === 1 && positive[0] === "warmup_minimal";
}

function valuesByName(rows) {
  return new Map(rows.map((row) => [row.name, row.value]));
}

export async function format_transcript(metadata, messages) {
  return mt(metadata, messages);
}

// Native Wt uses `sessions.sort((left, right) => right.start_time.localeCompare(left.start_time))`.
// Return the resulting stable input indices so Rust keeps each exact projection
// untouched while using the Bun runtime's locale-sensitive comparator.
export async function sort_sessions_by_start_time(sessions) {
  return sessions
    .map((row, index) => ({ row, index }))
    .sort((left, right) => right.row.start_time.localeCompare(left.row.start_time))
    .map(({ index }) => index);
}

export async function warmup_only(facets) {
  return facets.map((row) => warmupMinimal(row.value));
}

export async function aggregate(sessions, facets) {
  return St(sessions, valuesByName(facets));
}

export async function plan_facet_query(session_id, metadata, formatted_transcript) {
  const units = formatted_transcript.length;
  if (units <= 30_000) {
    return {
      direct_prompt: facetPrompt(formatted_transcript),
      summary_chunks: [],
      opaque_plan: { session_id, summary_header: null },
    };
  }

  const chunks = [];
  for (let index = 0; index < units; index += 25_000) {
    const chunk = formatted_transcript.slice(index, index + 25_000);
    chunks.push({
      prompt: `${LONG_SESSION_SUMMARY}${chunk}`,
      fallback: chunk.slice(0, 2_000),
    });
  }
  const summary_header = [
    `Session: ${metadata.session_id.slice(0, 8)}`,
    `Date: ${metadata.start_time}`,
    `Project: ${metadata.project_path}`,
    `Duration: ${metadata.duration_minutes} min`,
    `[Long session - ${chunks.length} parts summarized]`,
    "",
  ].join("\n");
  return {
    direct_prompt: null,
    summary_chunks: chunks,
    opaque_plan: { session_id, summary_header },
  };
}

export async function finish_facet_prompt(plan, summaries) {
  const transcript = `${plan.summary_header}${summaries.join("\n\n---\n\n")}`;
  return facetPrompt(transcript);
}

export async function parse_facet_result(session_id, response) {
  const value = responseJson(response);
  if (!value || !Ae(value)) return null;
  return { ...value, session_id };
}

export async function report_section_queries(aggregate, facets) {
  const captured = await captureNativeQueries(aggregate, facets);
  return captured.sections;
}

export async function parse_report_section(_name, response) {
  return responseJson(response);
}

export async function at_a_glance_query(aggregate, facets, sections) {
  const rows = sections.map(([name, value]) => ({ name, value }));
  const captured = await captureNativeQueries(aggregate, facets, rows);
  if (captured.at_a_glance === null) {
    throw new Error("Native zt did not produce an at-a-glance prompt");
  }
  return captured.at_a_glance;
}

export async function parse_at_a_glance(response) {
  return responseJson(response);
}

export async function recommendation(aggregate, facts) {
  const decision = It(aggregate, {
    autoModeAvailable: facts.auto_mode_available === true,
    currentSessionIsAuto: facts.current_session_is_auto === true,
    sandboxAutoAllowActive: facts.sandbox_auto_allow_active === true,
    currentSessionSeesNoDialogs: facts.current_session_sees_no_dialogs === true,
    liveArbiterIsAutoClassifier: facts.live_arbiter_is_auto_classifier === true,
    setupSkillAvailable: facts.setup_skill_available === true,
    hasAutoModeEnvironment: facts.has_auto_mode_environment === true,
  });
  return decision ? Mt(decision) : null;
}

export async function render_report(aggregate, sections, at_a_glance, recommendation, renderHtml) {
  const insights = {};
  for (const row of sections) insights[row.name] = row.value;
  if (at_a_glance) insights.at_a_glance = at_a_glance;

  const totalSessions = aggregate.total_sessions_scanned
      && aggregate.total_sessions_scanned > aggregate.total_sessions
    ? `${aggregate.total_sessions_scanned.toLocaleString()} sessions total · ${aggregate.total_sessions} analyzed`
    : `${aggregate.total_sessions} sessions`;
  const stats = [
    totalSessions,
    `${aggregate.total_messages.toLocaleString()} messages`,
    `${Math.round(aggregate.total_duration_hours)}h`,
    `${aggregate.git_commits} commits`,
  ].join(" · ");
  const header = `# LingXi Insights\n\n${stats}\n${aggregate.date_range.start} to ${aggregate.date_range.end}\n\n`;

  const glance = insights.at_a_glance;
  const summaryText = glance
    ? `## At a Glance\n\n${glance.whats_working ? `**What's working:** ${glance.whats_working} See _Impressive Things You Did_.` : ""}\n\n${glance.whats_hindering ? `**What's hindering you:** ${glance.whats_hindering} See _Where Things Go Wrong_.` : ""}\n\n${glance.quick_wins ? `**Quick wins to try:** ${glance.quick_wins} See _Features to Try_.` : ""}\n\n${glance.ambitious_workflows ? `**Ambitious workflows:** ${glance.ambitious_workflows} See _On the Horizon_.` : ""}`
    : "_No insights generated_";

  return {
    insights_json: JSON.stringify(insights, null, 2),
    summary_header: header,
    summary_text: summaryText,
    html: renderHtml(aggregate, insights, recommendation ?? null),
  };
}

export async function report_timestamp(now = () => new Date()) {
  const date = now();
  const pad2 = (part) => String(part).padStart(2, "0");
  return `${date.getFullYear()}-${pad2(date.getMonth() + 1)}-${pad2(date.getDate())}-${pad2(date.getHours())}${pad2(date.getMinutes())}${pad2(date.getSeconds())}`;
}

export { Ce, Te, metadata_for_transcript };
