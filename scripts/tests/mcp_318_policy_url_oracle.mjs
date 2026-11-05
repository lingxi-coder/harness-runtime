// Replay only the hash-pinned native session-proxy upstream URL extractor.
import { createHash } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

const binary = readFileSync(process.argv[2]);
const sha256 = createHash("sha256").update(binary).digest("hex");
if (sha256 !== "677a2ea7bf576cedd20e191d973914ed8a41796f53a63b1923381ddc4d5b5e7d")
  throw Error("unexpected Claude Code baseline");
const start = binary.indexOf("function ARs(", 257482802);
const end = binary.indexOf("function cPr(", start);
if (start !== 259518296 || end - start > 300)
  throw Error("upstream URL function range unavailable");
const routeStart = binary.indexOf('var ce=["/v2/ccr-sessions/"', 259501504);
const routeEnd = binary.indexOf(",ue=", routeStart);
if (routeStart < 0 || routeEnd - routeStart > 250)
  throw Error("upstream URL route constants unavailable");
const routeSource = binary.subarray(routeStart, routeEnd).toString("utf8");
const extractor = vm.runInNewContext(
  routeSource + ";" + binary.subarray(start, end).toString("utf8") + ";ARs",
  { URL },
  { timeout: 1000 },
);
const filterStart = binary.indexOf("function Qj(", 266000000);
const filterEnd = binary.indexOf("function ", filterStart + 12);
if (filterStart !== 266987976 || filterEnd - filterStart > 700)
  throw Error("model catalog filter function range unavailable");
// Keep the organization-ceiling predicate unchanged. The other catalog
// filters are fixed to an ordinary admitted session so this probe establishes
// only the blocked/ask/allow distinction, not whole-catalog equivalence.
const filter = vm.runInNewContext(
  binary.subarray(filterStart, filterEnd).toString("utf8") + ";Qj",
  {
    he: (tools) => tools,
    poo: () => false,
    QSe: () => false,
    Xl: () => [],
    Xe: () => [],
    H0n: () => null,
    xe: () => false,
    _Yt: () => false,
  },
  { timeout: 1000 },
);
const catalogTools = [
  { name: "mcp__srv__allow", mcpInfo: { effectiveMaxPermission: "allow" } },
  { name: "mcp__srv__ask", mcpInfo: { effectiveMaxPermission: "ask" } },
  { name: "mcp__srv__deny", mcpInfo: { effectiveMaxPermission: "blocked" } },
  { name: "mcp__srv__local-deny" },
];
const parserStart = binary.indexOf("function rjn(", 257482802);
const parserEnd = binary.indexOf("function lMr(", parserStart);
const policyStart = binary.indexOf("var G={always_allow:", 257482802);
const policyEnd = binary.indexOf("function dMr(", policyStart);
const sdkListStart = binary.indexOf("function Ie(", 257935226);
const sdkListEnd = binary.indexOf("var bR=", sdkListStart);
if (parserStart !== 257934789 || policyStart !== 257937157 || policyEnd !== 257938204 ||
    sdkListStart !== 257936383 || sdkListEnd !== 257936504)
  throw Error("MCP policy producer range unavailable");
const nativePolicies = vm.runInNewContext(
  binary.subarray(parserStart, parserEnd).toString("utf8") + ";" +
  binary.subarray(policyStart, policyEnd).toString("utf8") + ";" +
  binary.subarray(sdkListStart, sdkListEnd).toString("utf8") + ";({Nyt,i4i,Ie})",
  {
    D: (entry) => entry !== null && typeof entry === "object" && !Array.isArray(entry),
    Me: (policy) => ["always_allow", "always_ask", "always_deny"].includes(policy),
    // Test names contain only native-normalized ASCII letters/underscores.
    Pr: (server, tool) => `mcp__${server}__${tool}`,
  },
  { timeout: 1000 },
);
const policyCases = [
  ["http", "dynamic", false, true],
  ["http", "dynamic", true, true],
  ["http", "dynamic", true, false],
  ["sse", "dynamic", true, true],
  ["http", "user", true, true],
  ["http", "agent", true, true],
  ["http", "claudeai", true, true],
  ["stdio", "dynamic", true, true],
  ["claudeai-proxy", "dynamic", true, true],
  ["sdk", "dynamic", false, false],
  ["sdk", "agent", true, true],
].map(([type, scope, remote, seedEnabled]) => {
  const tools = ["allow", "ask", "deny"].map((name) => ({
    name, permission_policy: `always_${name}`,
  }));
  const config = { type, scope, tools };
  let expected = { allow: [], ask: [], deny: [] };
  if (type === "sdk") expected = nativePolicies.Nyt({ srv: config });
  else if (remote && seedEnabled) {
    const result = nativePolicies.i4i({
      alwaysAllowRules: {}, alwaysAskRules: {}, alwaysDenyRules: {},
    }, { srv: config });
    expected = {
      allow: result.alwaysAllowRules.mcpServerPolicy ?? [],
      ask: result.alwaysAskRules.mcpServerPolicy ?? [],
      deny: result.alwaysDenyRules.mcpServerPolicy ?? [],
    };
  }
  return { type, scope, remote, seedEnabled, tools, expected };
});
const sdkSchemaStart = binary.indexOf('zOs=f(()=>u({type:C("sdk")', 257482802);
const sdkSchemaEnd = binary.indexOf(",jSe=", sdkSchemaStart);
const sdkSchemaBytes = binary.subarray(sdkSchemaStart, sdkSchemaEnd);
if (sdkSchemaStart !== 258326041 ||
    !sdkSchemaBytes.toString("utf8").includes("tools:T(ar()).optional().catch(void 0)"))
  throw Error("SDK optional-tools catch declaration unavailable");
const sdkInitializePolicies = [
  null,
  "not a list",
  { name: "not a list" },
  [],
  [{ name: "deny", permission_policy: "always_deny", org_max_permission: "blocked" }],
  [{ name: "unknown", permission_policy: "invalid" }, { name: "boolean", permission_policy: false }],
  [{ name: "plain" }, { permission_policy: "always_deny" }, { name: 7 }, null],
].map((input) => ({ input, expected: nativePolicies.Ie(input) ?? null }));
const inputs = [
  "https://proxy.test/v2/session_ingress/shttp/mcp/a?mcp_url=https%3A%2F%2Fupstream.test%2Fmcp",
  "wss://proxy.test/v2/session_ingress/mcp/ws/a?mcp_url=https%3A%2F%2Fone.test&mcp_url=https%3A%2F%2Ftwo.test",
  "https://proxy.test/v2/ccr-sessions/session/mcp?mcp_url=https%3A%2F%2Fupstream.test",
  "https://proxy.test/v1/code/sessions/session/mcp?mcp_url=https%3A%2F%2Fupstream.test",
  "https://proxy.test/ordinary?mcp_url=https%3A%2F%2Fupstream.test",
  "https://proxy.test/v1/code/mcp?mcp_url=&mcp_url=not+a+url",
  "https://proxy.test/v1/code/mcp?MCP_URL=https%3A%2F%2Fupstream.test",
  "https://proxy.test/v1/code/mcp?mcp_url=https%3A%2F%2F%E4%B8%AD%E6%96%87.test%2F%E8%B7%AF%E5%BE%84",
  "https://proxy.test/ordinary?hint=/v1/code/&mcp_url=https%3A%2F%2Fupstream.test",
  "https://proxy.test/ordinary?mcp_url=https%3A%2F%2Fupstream.test#/v2/ccr-sessions/",
  "https://proxy.test/v1/code/mcp?mcp_url=%FF",
  "not a URL /v1/code/?mcp_url=https://upstream.test",
];
const fixture = {
  version: "2.1.318",
  sha256,
  function: "ARs",
  byteRange: [start, end],
  cases: inputs.map((url) => ({ url, expected: Array.from(extractor(url)) })),
  catalogFilter: {
    function: "Qj",
    byteRange: [filterStart, filterEnd],
    tools: catalogTools,
    expectedNames: Array.from(filter(catalogTools, {}), (tool) => tool.name),
    scope: "Organization ceiling predicate with unrelated catalog filters stubbed to admit; local permission rules are outside this function.",
  },
  startupPolicies: {
    functions: ["rjn", "Nyt", "i4i"],
    byteRanges: [[parserStart, parserEnd], [policyStart, policyEnd]],
    startupGateOffset: 278315381,
    cases: policyCases,
    scope: "SDK tighten-only rule producer and remote startup seed with valid normalized tool names; full SDK host and remote refresh lifecycle excluded.",
  },
  sdkToolsSchema: {
    factory: "zOs",
    byteRange: [sdkSchemaStart, sdkSchemaEnd],
    sourceSha256: createHash("sha256").update(sdkSchemaBytes).digest("hex"),
    invalidToolsField: "discard optional tools field; retain otherwise-valid SDK server",
    scope: "Hash-pinned static schema declaration; the full Zod parser is not replayed.",
  },
  sdkInitializePolicies: {
    function: "Ie",
    byteRange: [sdkListStart, sdkListEnd],
    cases: sdkInitializePolicies,
    scope: "Actual pure native SDK initialize policy-list normalization; registered-server lifecycle is tested in the runtime.",
  },
  scope: "Native upstream URL extraction and isolated organization catalog filtering; connector identity aliases and the complete live policy pipeline are not replayed.",
};
const output = fileURLToPath(new URL(
  "../../crates/mcp/tests/fixtures/claude_2_1_318_policy_upstream_urls.json",
  import.meta.url,
));
const formatted = JSON.stringify(fixture, null, 2) + "\n";
if (process.argv.includes("--write")) writeFileSync(output, formatted);
else if (readFileSync(output, "utf8") !== formatted)
  throw Error("native upstream URL fixture differs; regenerate with --write");
console.log(`Claude Code ${fixture.version}: ${fixture.cases.length} upstream URL cases, ${policyCases.length} startup policy cases, catalog filter and SDK schema declaration match ${sha256}`);
