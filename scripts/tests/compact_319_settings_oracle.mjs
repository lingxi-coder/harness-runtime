// Replay the official parser, including parseInt/parseFloat permissive prefixes.
import { readFileSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

const binary = readFileSync(process.argv[2]);
const sha256 = createHash("sha256").update(binary).digest("hex");
if (sha256 !== "1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd")
  throw Error("Unexpected Claude Code baseline");
const ranges = [
  { name: "integer-prefix-patterns-and-t2s", offset: 259477214, bytes: 277 },
  { name: "Qf", offset: 259477491, bytes: 68 },
  { name: "UCe", offset: 271029423, bytes: 411 },
  { name: "UTn", offset: 271034347, bytes: 139 },
];
const source = ranges
  .map(({ offset, bytes }) => binary.subarray(offset, offset + bytes).toString("utf8"))
  .join("\n");
const native = vm.runInNewContext(
  source + "; const WO=100000,pj=1000000; ({parse:UTn});",
  Object.create(null),
  { timeout: 1000 },
);
const cases = [
  "auto", " AUTO ", "reset", "unset", "default", "", " ",
  "100", "200", "1000", "99", "1001", "100000", "200000", "1000000", "1000001",
  "100k", " 200K ", "1m", "0.5m", ".5m", "0.1m", "0.099999m", "150.5k", "150.0005k", "150.00049k",
  "200.9", "1e5", "2E5", "2e2", "1e6", "1e5.5", "2e2.5k", "200xyz", "200xk", "200x",
  "200,000", "200_000", "200 000", "200\u00a0000", "200\u202f000", "2,00,000",
  "+200", "-200", "0", "NaN", "Infinity", "0x200", "100000000000000000000000000000000",
].map((input) => ({ input, expected: native.parse(input) ?? null }));
const fixture = {
  version: "2.1.319", sha256, ranges,
  scope: "UTn auto-compact window parser only. Limits WO=1e5,pj=1e6 from artifact offset 259762652. Command reset/unset/default aliases, settings persistence, and provider display are outside this fixture.",
  cases,
};
const target = fileURLToPath(new URL("../../crates/compaction/tests/fixtures/claude_2_1_319_settings_parser.json", import.meta.url));
const output = JSON.stringify(fixture, null, 2) + "\n";
if (process.argv.includes("--check")) {
  if (readFileSync(target, "utf8") !== output)
    throw Error("Settings parser oracle fixture differs");
  console.log("Claude Code 2.1.319 settings parser oracle matches " + sha256);
} else {
  writeFileSync(target, output);
  console.log(target);
}
