// Execute only hash-pinned wx/Ll functions from the official 2.1.319 artifact.
import { readFileSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

const artifact = readFileSync(process.argv[2]);
const sha256 = createHash("sha256").update(artifact).digest("hex");
if (sha256 !== "1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd") {
  throw Error("unexpected Claude Code baseline");
}
const ranges = [
  { name: "Pe", start: artifact.indexOf("function Pe(e){if(!e)return!1;", 259274359), end: artifact.indexOf("function rs(", 259274359) },
  { name: "wx", start: 269545746, end: artifact.indexOf("var ERo=", 269545746) },
  { name: "Ll", start: 267179950, end: artifact.indexOf("function chs(", 267179950) },
  { name: "gzs", start: 259432706, end: artifact.indexOf("function yzs(", 259432706) },
];
for (const range of ranges) {
  if (range.end <= range.start || range.end - range.start > 512) throw Error("invalid source range");
  range.sha256 = createHash("sha256").update(artifact.subarray(range.start, range.end)).digest("hex");
}
const state = { searchToolsOptIn: false, entrypoint: undefined, platform: "darwin", gitBashAvailable: true };
const context = vm.createContext({
  state,
  // Imported host facts are explicit; no application startup is performed.
  t: () => ({ host: { launchOptions: { searchToolsOptIn: () => state.searchToolsOptIn } } }),
  i: new Proxy({}, { get: (_, key) => key === "CLAUDE_CODE_ENTRYPOINT" ? state.entrypoint : undefined }),
  x: () => state.platform,
  Oae: () => state.gitBashAvailable ? "/host/git-bash" : null,
});
vm.runInContext(ranges.map(range => artifact.subarray(range.start, range.end).toString("utf8")).join("\n"), context, { timeout: 1000 });
const policy = vm.runInContext("() => wx() && Ll()", context);
const cases = [
  [false, null, "darwin", false], [false, null, "linux", false],
  [false, null, "windows", false], [false, null, "windows", true],
  [true, null, "darwin", true], [true, null, "windows", true],
  [false, "local-agent", "darwin", true], [false, "local-agent", "windows", true],
  [false, "local_agent", "darwin", true], [false, "sdk-py", "darwin", true],
].map(([searchToolsOptIn, entrypoint, platform, gitBashAvailable]) => {
  Object.assign(state, { searchToolsOptIn, entrypoint: entrypoint ?? undefined, platform, gitBashAvailable });
  return { searchToolsOptIn, entrypoint, platform, gitBashAvailable, expected: policy() };
});
const fixture = {
  version: "2.1.319", binarySha256: sha256, ranges,
  hostDependencies: "Pe and wx execute the artifact's enabled build constant; searchToolsOptIn, entrypoint, OS, and Git Bash availability are isolated imported host facts. No Auto Mode/model predicate or advertised tool-pool predicate is involved.",
  cases,
};
const target = fileURLToPath(new URL("../../crates/orchestrator/tests/fixtures/claude_2_1_319_plan_hybrid_context.json", import.meta.url));
const output = JSON.stringify(fixture, null, 2) + "\n";
if (process.argv.includes("--check")) {
  if (readFileSync(target, "utf8") !== output) throw Error("plan hybrid-search fixture differs");
  console.log(`Claude Code 2.1.319 plan hybrid-search oracle matches ${sha256}`);
} else {
  writeFileSync(target, output);
  console.log(target);
}
