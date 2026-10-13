import { createInterface } from "node:readline";

const ACTIONS = [
  "metadata_for_transcript",
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
  "report_timestamp",
];
const SOURCE_TRANSFORMS = ACTIONS.filter((action) =>
  action !== "metadata_for_transcript" && action !== "report_timestamp");
const REQUIRED_Q4_OPEN_RESULTS = [
  "fleetFork",
  "fleetBackground",
  "daemon",
  "skillDoctor",
  "logout",
];

function requireObject(value, name) {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new TypeError(`${name} must be an object`);
  }
  return value;
}

function requireString(value, name) {
  if (typeof value !== "string") throw new TypeError(`${name} must be a string`);
  return value;
}

function requireArray(value, name) {
  if (!Array.isArray(value)) throw new TypeError(`${name} must be an array`);
  return value;
}

function requireQ4OpenResults(value) {
  const q4 = requireObject(value, "q4_open_results");
  for (const name of REQUIRED_Q4_OPEN_RESULTS) {
    if (typeof q4[name] !== "boolean") {
      throw new TypeError(`q4_open_results.${name} must be an explicit boolean`);
    }
  }
  return q4;
}

/**
 * Creates the line-protocol action handler used by the desktop Bun worker.
 * The Native-source module owns all string/object transforms; this module
 * validates the RPC envelope and forwards raw JS values without normalizing
 * them through a UTF-8 or Rust-shaped DTO.
 */
export function createInsightsActionHandler({
  metadataModule,
  sourceModule,
  renderHtml,
  runtimeVersion = null,
  now = () => new Date(),
}) {
  if (typeof metadataModule?.metadata_for_transcript !== "function") {
    throw new TypeError("metadata module must export metadata_for_transcript(path, q4OpenResults)");
  }
  if (typeof renderHtml !== "function") throw new TypeError("renderHtml must be a function");
  for (const name of SOURCE_TRANSFORMS) {
    if (typeof sourceModule?.[name] !== "function") {
      throw new TypeError(`insights source module must export ${name}`);
    }
  }

  const transform = (name) => {
    const handler = sourceModule?.[name];
    if (typeof handler !== "function") {
      throw new Error(`insights source action is unavailable: ${name}`);
    }
    return handler;
  };

  const handlers = {
    metadata_for_transcript: (payload) => {
      const p = requireObject(payload, "payload");
      const q4 = requireQ4OpenResults(p.q4_open_results);
      return metadataModule.metadata_for_transcript(
        requireString(p.transcript_path, "transcript_path"),
        q4,
      );
    },
    format_transcript: (payload) => {
      const p = requireObject(payload, "payload");
      return transform("format_transcript")(p.metadata, p.messages);
    },
    sort_sessions_by_start_time: (payload) => transform("sort_sessions_by_start_time")(
      requireArray(requireObject(payload, "payload").sessions, "sessions"),
    ),
    warmup_only: (payload) => transform("warmup_only")(
      requireObject(payload, "payload").facets,
    ),
    aggregate: (payload) => {
      const p = requireObject(payload, "payload");
      return transform("aggregate")(p.sessions, p.facets);
    },
    plan_facet_query: (payload) => {
      const p = requireObject(payload, "payload");
      return transform("plan_facet_query")(
        requireString(p.session_id, "session_id"),
        p.metadata,
        p.formatted_transcript,
      );
    },
    finish_facet_prompt: (payload) => {
      const p = requireObject(payload, "payload");
      return transform("finish_facet_prompt")(p.plan, p.summaries);
    },
    parse_facet_result: (payload) => {
      const p = requireObject(payload, "payload");
      return transform("parse_facet_result")(
        requireString(p.session_id, "session_id"),
        p.response,
      );
    },
    report_section_queries: (payload) => {
      const p = requireObject(payload, "payload");
      return transform("report_section_queries")(p.aggregate, p.facets);
    },
    parse_report_section: (payload) => {
      const p = requireObject(payload, "payload");
      return transform("parse_report_section")(
        requireString(p.name, "name"),
        p.response,
      );
    },
    at_a_glance_query: (payload) => {
      const p = requireObject(payload, "payload");
      return transform("at_a_glance_query")(p.aggregate, p.facets, p.sections);
    },
    parse_at_a_glance: (payload) => transform("parse_at_a_glance")(
      requireObject(payload, "payload").response,
    ),
    recommendation: (payload) => {
      const p = requireObject(payload, "payload");
      return transform("recommendation")(p.aggregate, p.facts);
    },
    render_report: (payload) => {
      const p = requireObject(payload, "payload");
      return transform("render_report")(
        p.aggregate,
        p.sections,
        p.at_a_glance,
        p.recommendation ?? null,
        renderHtml,
      );
    },
    report_timestamp: () => transform("report_timestamp")(now),
    render: (payload) => {
      const p = requireObject(payload, "payload");
      return renderHtml(p.data, p.insights, p.auto_mode_reco ?? null);
    },
    ping: () => ({
      runtime: "bun",
      bunVersion: runtimeVersion,
      metadataModule: "metadata-source/insights-native-actions.mjs",
      sourceModule: "insights_source_actions.mjs",
      actions: ACTIONS,
    }),
  };

  return async function handle(request) {
    if (request === null || typeof request !== "object" || Array.isArray(request)) {
      throw new TypeError("request must be an object");
    }
    const action = requireString(request.action, "action");
    const handler = handlers[action];
    if (typeof handler !== "function") throw new Error(`unknown insights worker action: ${action}`);
    return handler(request.payload ?? {});
  };
}

/** Convert one input line to one protocol response, retaining the request id. */
export async function dispatchInsightsLine(line, handle) {
  let request;
  try {
    request = JSON.parse(line);
    const result = await handle(request);
    return { id: request.id ?? null, kind: "result", result };
  } catch (error) {
    return {
      id: request?.id ?? null,
      kind: "worker_error",
      detail: String(error?.message ?? error),
    };
  }
}

/** Run the persistent worker loop. Every input line receives exactly one line. */
export async function runInsightsWorker({ input = process.stdin, output = process.stdout, handle }) {
  const lines = createInterface({ input, crlfDelay: Infinity });
  for await (const line of lines) {
    const response = await dispatchInsightsLine(line, handle);
    output.write(`${JSON.stringify(response)}\n`);
  }
}
