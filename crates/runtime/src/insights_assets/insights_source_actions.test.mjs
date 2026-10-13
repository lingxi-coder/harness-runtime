import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import {
  aggregate,
  at_a_glance_query,
  finish_facet_prompt,
  format_transcript,
  metadata_for_transcript,
  parse_at_a_glance,
  parse_facet_result,
  parse_report_section,
  plan_facet_query,
  recommendation,
  render_report,
  report_section_queries,
  report_timestamp,
  sort_sessions_by_start_time,
  warmup_only,
} from "./insights_source_actions.mjs";

const q4OpenResults = {
  fleetFork: false,
  fleetBackground: false,
  daemon: false,
  skillDoctor: false,
  logout: false,
};

const metadataRows = await metadata_for_transcript(
  fileURLToPath(new URL("./metadata-source/fixtures/small-session.jsonl", import.meta.url)),
  q4OpenResults,
);
assert.equal(metadataRows.length, 1);
assert.equal(metadataRows[0].metadata.session_id, "ed32091e-d448-41c9-b7bd-e99bbd50a833");
assert.equal(metadataRows[0].messages.length, 5);
const formatted = await format_transcript(metadataRows[0].metadata, metadataRows[0].messages);
assert.ok(formatted.startsWith("Session: ed32091e\nDate: 2026-10-10T05:51:54.739Z\n"));
assert.match(formatted, /\[User\]: Please remember this fixture conversation/);

const localeRows = ["é", "Z", "a", "A", "!", "_", "a-"].map((start_time) => ({
  start_time,
}));
assert.deepEqual(
  await sort_sessions_by_start_time(localeRows),
  [1, 0, 6, 3, 2, 4, 5],
  "Native's localeCompare order differs from Rust scalar-string ordering",
);
assert.deepEqual(
  await sort_sessions_by_start_time([{ start_time: "same" }, { start_time: "same" }]),
  [0, 1],
  "Native's stable sort retains input order for equal localeCompare values",
);

const metadata = {
  session_id: "s-12345678",
  start_time: "2026-10-11T00:00:00.000Z",
  project_path: "/project",
  duration_minutes: 12,
};
const direct = await plan_facet_query("s-12345678", metadata, "short transcript");
assert.equal(direct.summary_chunks.length, 0);
assert.equal(direct.opaque_plan.session_id, "s-12345678");
assert.ok(direct.direct_prompt.startsWith("Analyze this LingXi session"));
assert.ok(direct.direct_prompt.endsWith('"brief_summary": "One sentence: what user wanted and whether they got it"\n}'));

const long = await plan_facet_query("s-12345678", metadata, "x".repeat(30_001));
assert.equal(long.direct_prompt, null);
assert.equal(long.summary_chunks.length, 2);
assert.equal(long.summary_chunks[0].fallback.length, 2_000);
// The fixed Native prompt is 25,317 UTF-16 units; the registered static
// brand projection replaces one "Claude Code" with "LingXi" (five fewer).
assert.equal(long.summary_chunks[0].prompt.length, 25_317 - ("Claude Code".length - "LingXi".length));
assert.equal(long.opaque_plan.summary_header,
  "Session: s-123456\nDate: 2026-10-11T00:00:00.000Z\nProject: /project\nDuration: 12 min\n[Long session - 2 parts summarized]\n");
const finished = await finish_facet_prompt(long.opaque_plan, ["summary one", "summary two"]);
assert.ok(finished.includes("summary one\n\n---\n\nsummary two"));

const validFacet = {
  underlying_goal: "repair the service",
  goal_categories: { fix_bug: 1 },
  outcome: "fully_achieved",
  user_satisfaction_counts: {},
  friction_counts: {},
  brief_summary: "The repair worked.",
};
assert.deepEqual(await parse_facet_result("s-12345678", JSON.stringify(validFacet)), {
  ...validFacet,
  session_id: "s-12345678",
});
assert.equal(await parse_facet_result("s-12345678", "no JSON here"), null);
assert.equal(await parse_report_section("project_areas", "prefix {\"areas\":[]} suffix").then((x) => x.areas.length), 0);
assert.deepEqual(await parse_at_a_glance('{"whats_working":"clear"}'), { whats_working: "clear" });

assert.deepEqual(await warmup_only([
  { session_id: "a", value: { goal_categories: { warmup_minimal: 1 } } },
  { session_id: "b", value: { goal_categories: { warmup_minimal: 1, fix_bug: 1 } } },
  { session_id: "c", value: { goal_categories: [] } },
]), [true, false, false]);

const session = {
  session_id: "session-12345678",
  start_time: "2026-10-11T12:30:00.000Z",
  duration_minutes: 120,
  user_message_count: 5,
  input_tokens: 100,
  output_tokens: 40,
  git_commits: 1,
  git_pushes: 0,
  user_interruptions: 0,
  tool_errors: 0,
  tool_error_categories: {},
  user_response_times: [10, 20],
  uses_task_agent: false,
  uses_mcp: false,
  uses_web_search: false,
  uses_web_fetch: false,
  lines_added: 4,
  lines_removed: 2,
  files_modified: 1,
  message_hours: [12],
  bash_would_prompt_count: 0,
  permission_mode: "default",
  file_edit_tool_count: 0,
  destructive_command_count: 0,
  tool_counts: { Read: 2 },
  languages: { Rust: 1 },
  project_path: "/project",
  summary: "A focused fix",
  first_prompt: "Fix the service",
  user_message_timestamps: [],
};
const aggregateResult = await aggregate([session], []);
assert.equal(aggregateResult.total_sessions, 1);
assert.equal(aggregateResult.total_messages, 5);
assert.deepEqual(aggregateResult.date_range, { start: "2026-10-11", end: "2026-10-11" });
assert.deepEqual(aggregateResult.multi_clauding, {
  overlap_events: 0,
  sessions_involved: 0,
  user_messages_during: 0,
});

const sections = await report_section_queries(aggregateResult, []);
assert.deepEqual(sections.map((section) => section.name), [
  "project_areas",
  "interaction_style",
  "what_works",
  "friction_analysis",
  "suggestions",
  "on_the_horizon",
  "fun_ending",
]);
assert.equal(sections[0].max_tokens, 8192);
assert.ok(sections[0].prompt.includes("\n\nDATA:\n{\n  \"sessions\": 1,"));
const glancePrompt = await at_a_glance_query(aggregateResult, [], [
  ["project_areas", { areas: [{ name: "Runtime", description: "Built a local worker." }] }],
]);
assert.match(glancePrompt, /## Project Areas \(what user works on\)[\s\S]*Runtime: Built a local worker\./);

const reco = await recommendation({
  auto_mode_stats: {
    auto_sessions: 0,
    non_auto_sessions: 5,
    estimated_prompts_in_non_auto: 3,
    destructive_in_non_auto: 0,
    recent_all_auto: false,
  },
}, {
  auto_mode_available: true,
  current_session_is_auto: false,
  sandbox_auto_allow_active: false,
  current_session_sees_no_dialogs: false,
  live_arbiter_is_auto_classifier: false,
  setup_skill_available: false,
  has_auto_mode_environment: false,
});
assert.equal(reco,
  "Tip: auto mode could have handled up to ~3 permission prompts across 5 of your recent sessions — press Shift+Tab until the mode indicator shows auto.");

const report = await render_report(
  { ...aggregateResult, total_sessions_scanned: 2 },
  [{ name: "project_areas", value: { areas: [] } }],
  { whats_working: "clear", quick_wins: "try this" },
  reco,
  (data, insights, recommendationLine) => `${data.total_sessions}:${Object.keys(insights).join(",")}:${recommendationLine}`,
);
assert.deepEqual(Object.keys(JSON.parse(report.insights_json)), [
  "project_areas",
  "at_a_glance",
]);
assert.equal(report.summary_header,
  "# LingXi Insights\n\n2 sessions total · 1 analyzed · 5 messages · 2h · 1 commits\n2026-10-11 to 2026-10-11\n\n");
assert.equal(report.summary_text,
  "## At a Glance\n\n**What's working:** clear See _Impressive Things You Did_.\n\n\n\n**Quick wins to try:** try this See _Features to Try_.\n\n");
assert.ok(report.html.startsWith("1:project_areas,at_a_glance:Tip:"));
assert.equal(await report_timestamp(() => new Date(2026, 9, 12, 3, 4, 5)), "2026-10-12-030405");

console.log("insights source action contract passed");
