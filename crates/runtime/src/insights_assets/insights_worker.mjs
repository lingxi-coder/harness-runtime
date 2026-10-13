import * as sourceModule from "./insights_source_actions.mjs";
import { render_report_html } from "./insights_html_worker.mjs";
import { createInsightsActionHandler, runInsightsWorker } from "./insights_worker_protocol.mjs";

const handle = createInsightsActionHandler({
  metadataModule: sourceModule,
  sourceModule,
  renderHtml: render_report_html,
  runtimeVersion: Bun.version,
});

await runInsightsWorker({ handle });
