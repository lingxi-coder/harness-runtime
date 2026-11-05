// Replay only the hash-pinned native ask-rule resolver, without launching Claude.
import { readFileSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import vm from "node:vm";

const binary = readFileSync(process.argv[2]);
const sha256 = createHash("sha256").update(binary).digest("hex");
if (sha256 !== "677a2ea7bf576cedd20e191d973914ed8a41796f53a63b1923381ddc4d5b5e7d")
  throw Error("Unexpected Claude Code 2.1.318 artifact");
const start = binary.indexOf("async function Ybn(", 270618500);
const end = binary.indexOf('var pV="permission_check_crashed"', start);
if (start < 0 || end < start || end - start > 2000) throw Error("Missing ask resolver");
const context = vm.createContext({
  Hoe: (_tool, input) => ({ parsedInput: input }),
  Bxe: () => {},
  RPr: new WeakSet(),
  Lot: () => undefined,
  Wxe: () => false,
  od: (name) => `Allow ${name}?`,
});
const resolve = vm.runInContext(
  binary.subarray(start, end).toString("utf8") + ";Ybn",
  context,
  { timeout: 1000 },
);
const askRule = { source: "session", ruleBehavior: "ask", ruleValue: { toolName: "Bash" } };
const cases = [
  { name: "allow", toolResult: { behavior: "allow" } },
  { name: "passthrough", toolResult: { behavior: "passthrough" } },
  { name: "deny", toolResult: { behavior: "deny", decisionReason: { type: "other", reason: "clamp" } } },
  { name: "safetyAsk", toolResult: { behavior: "ask", decisionReason: { type: "safetyCheck", reason: "Dangerous rm operation", classifierApprovable: false } } },
  { name: "pathAsk", toolResult: { behavior: "ask", decisionReason: { type: "other", reason: "path" }, suggestions: [{ type: "addDirectories" }] } },
  { name: "ruleAsk", toolResult: { behavior: "ask", decisionReason: { type: "rule", rule: askRule } } },
  { name: "crash", throws: true },
];
for (const test of cases) {
  const result = await resolve({ name: "Bash", checkPermissions: async () => {
    if (test.throws) throw Error("controlled tool check failure");
    return test.toolResult;
  } }, { command: "fixture" }, {}, askRule);
  test.expected = {
    behavior: result.behavior,
    reasonType: result.decisionReason?.type,
    matchedAskRule: result.matchedAskRule !== undefined,
    suggestions: result.suggestions !== undefined,
  };
}
const fixture = {
  version: "2.1.318", sha256, range: [start, end], cases,
  scope: "Ybn ask-rule resolution with injected tool results; no live model, tool execution, or complete permission-walk claim",
};
const path = new URL("../../crates/permission/tests/fixtures/permission_ask_2_1_318.json", import.meta.url);
const text = JSON.stringify(fixture, null, 2) + "\n";
if (process.argv.includes("--write")) writeFileSync(path, text);
else if (readFileSync(path, "utf8") !== text) throw Error("Native ask fixture changed");
console.log(`Claude Code 2.1.318 ask resolver: ${cases.length} cases match (${start}..${end})`);
