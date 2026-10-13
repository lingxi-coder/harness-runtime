import assert from "node:assert/strict";
import { createInsightsActionHandler, dispatchInsightsLine } from "./insights_worker_protocol.mjs";

const calls = [];
const sourceModule = Object.fromEntries([
  "format_transcript",
  "sort_sessions_by_start_time",
  "warmup_only",
  "aggregate",
  "plan_facet_query",
  "finish_facet_prompt",
  "parse_facet_result",
  "report_section_queries",
  "parse_report_section",
  "at_a_glance_query",
  "parse_at_a_glance",
  "recommendation",
  "render_report",
].map((name) => [name, (...args) => {
  calls.push([name, ...args]);
  return { name, args };
}]));
sourceModule.report_timestamp = (now) => {
  calls.push(["report_timestamp", now]);
  const date = now();
  const pad2 = (part) => String(part).padStart(2, "0");
  return `${date.getFullYear()}-${pad2(date.getMonth() + 1)}-${pad2(date.getDate())}-${pad2(date.getHours())}${pad2(date.getMinutes())}${pad2(date.getSeconds())}`;
};
const handle = createInsightsActionHandler({
  metadataModule: {
    metadata_for_transcript: async (path, q4OpenResults) => [
      { metadata: { path, q4OpenResults }, messages: [] },
    ],
  },
  sourceModule,
  renderHtml: (...args) => `${args[0]}:${args[1]}`,
  runtimeVersion: "1.3.11",
  now: () => new Date(2026, 9, 12, 3, 4, 5),
});

assert.deepEqual(await handle({ action: "ping" }), {
  runtime: "bun",
  bunVersion: "1.3.11",
  metadataModule: "metadata-source/insights-native-actions.mjs",
  sourceModule: "insights_source_actions.mjs",
  actions: [
    "metadata_for_transcript", "format_transcript", "sort_sessions_by_start_time",
    "warmup_only", "aggregate",
    "plan_facet_query", "finish_facet_prompt", "parse_facet_result",
    "report_section_queries", "parse_report_section", "at_a_glance_query",
    "parse_at_a_glance", "recommendation", "render_report", "report_timestamp",
  ],
});
const q4OpenResults = {
  fleetFork: false,
  fleetBackground: true,
  daemon: false,
  skillDoctor: false,
  logout: true,
};
assert.deepEqual(await handle({
  action: "metadata_for_transcript",
  payload: { transcript_path: "/tmp/a.jsonl", q4_open_results: q4OpenResults },
}), [
  { metadata: { path: "/tmp/a.jsonl", q4OpenResults }, messages: [] },
]);
const missingQ4 = await dispatchInsightsLine(JSON.stringify({
  id: 19,
  action: "metadata_for_transcript",
  payload: { transcript_path: "/tmp/a.jsonl" },
}), handle);
assert.equal(missingQ4.kind, "worker_error");
assert.match(missingQ4.detail, /q4_open_results/);
const invalidQ4 = await dispatchInsightsLine(JSON.stringify({
  id: 20,
  action: "metadata_for_transcript",
  payload: {
    transcript_path: "/tmp/a.jsonl",
    q4_open_results: { ...q4OpenResults, skillDoctor: "false" },
  },
}), handle);
assert.equal(invalidQ4.kind, "worker_error");
assert.match(invalidQ4.detail, /skillDoctor must be an explicit boolean/);
assert.deepEqual(await handle({ action: "report_timestamp" }), "2026-10-12-030405");
const rendered = await handle({
  action: "render_report",
  payload: {
    aggregate: {
      total_sessions_scanned: 8,
      total_sessions: 3,
      total_messages: 1275,
      total_duration_hours: 12.6,
      git_commits: 4,
      date_range: { start: "2026-09-01", end: "2026-10-01" },
    },
    sections: [["project_areas", { areas: [] }], ["what_works", { intro: "Good" }]],
    at_a_glance: { whats_working: "Keep this", quick_wins: "Try this" },
    recommendation: null,
  },
});
assert.equal(rendered.name, "render_report");
assert.equal(typeof rendered.args[4], "function");

const sourceCarrierText = "keep the isolated unit: \ud800";
const requestLine = JSON.stringify({
  id: 17,
  action: "plan_facet_query",
  payload: { session_id: "session-a", metadata: { summary: sourceCarrierText }, formatted_transcript: "body" },
});
const decoded = await dispatchInsightsLine(requestLine, handle);
assert.equal(decoded.kind, "result");
assert.equal(decoded.id, 17);
assert.equal(calls.at(-1)[0], "plan_facet_query");
assert.equal(calls.at(-1)[2].summary, sourceCarrierText);
assert.equal(JSON.parse(JSON.stringify(decoded)).result.args[1].summary, sourceCarrierText);

const error = await dispatchInsightsLine(
  JSON.stringify({ id: 18, action: "unknown", payload: {} }),
  handle,
);
assert.deepEqual(error, {
  id: 18,
  kind: "worker_error",
  detail: "unknown insights worker action: unknown",
});

assert.equal(calls.some(([name]) => name === "render_report"), true);

const invalid = await dispatchInsightsLine("{", handle);
assert.equal(invalid.id, null);
assert.equal(invalid.kind, "worker_error");
assert.match(invalid.detail, /JSON/);

console.log("insights worker protocol contract passed");
