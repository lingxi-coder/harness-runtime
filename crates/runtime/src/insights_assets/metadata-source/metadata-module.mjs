import { eKt } from "./native-bundle/src_194447854.mjs";
import { Ce, Te } from "./native-bundle/src_207151635.mjs";

export { eKt, Ce, Te };

function hasGeneratedFacetMarker(log) {
  for (const message of log.messages.slice(0, 5)) {
    if (message.type !== "user" || !message.message) continue;
    const content = message.message.content;
    if (typeof content === "string" && (content.includes("RESPOND WITH ONLY A VALID JSON OBJECT") || content.includes("record_facets"))) return true;
  }
  return false;
}

export async function metadata_for_transcript(transcriptPath) {
  let logs;
  try {
    logs = await eKt(transcriptPath, undefined, undefined);
  } catch {
    return [];
  }
  const rows = [];
  for (const log of logs) {
    if (hasGeneratedFacetMarker(log)) continue;
    if (Number.isNaN(log.created.getTime())) continue;
    if (Number.isNaN(log.modified.getTime())) continue;
    rows.push({ metadata: Ce(log), messages: log.messages });
  }
  return rows;
}
