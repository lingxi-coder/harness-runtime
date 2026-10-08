#!/usr/bin/env node
// Generate independently observed omitClaudeMd results from the pinned native
// extraction. The extracted Fl body executes unchanged with only rbo stubbed.
import fs from "node:fs";
import crypto from "node:crypto";
import vm from "node:vm";
import path from "node:path";

const [directory, output, check] = process.argv.slice(2);
if (!directory || !output || (check && check !== "--check")) {
  throw new Error("usage: node omit_instructions_oracle.mjs CHUNK_DIRECTORY FIXTURE [--check]");
}
const source = fs.readFileSync(path.join(directory, "src_191018893.js"), "utf8");
const start = source.indexOf("async function Fl(");
const end = source.indexOf("function Ll(", start);
if (start < 0 || end < 0) throw new Error("Pinned Fl boundary missing");
const body = source.slice(start, end);
const digest = crypto.createHash("sha256").update(body).digest("hex");
const selectionStart = source.indexOf("[ut,xt]=await Promise.all([E?.userContext??Gv(");
const selectionEnd = source.indexOf(",pt=D9(", selectionStart);
if (selectionStart < 0 || selectionEnd < 0) throw new Error("Pinned Lv selection boundary missing");
const selection = source.slice(selectionStart, selectionEnd);
const selectionDigest = crypto.createHash("sha256").update(selection).digest("hex");
if (selectionDigest !== "72223ce629d9425b86f58549a6788456f56b3d1ff3d4dae3e409f32b54ab47f6") {
  throw new Error("Pinned Lv selection source hash mismatch");
}
const expected = JSON.parse(fs.readFileSync(output, "utf8"));
if (digest !== expected.provenance.sourceSha256) throw new Error("Pinned Fl source hash mismatch");
const cases = [];
for (const source of ["built-in", "policySettings", "plugin", "projectSettings"]) {
  for (const result of ["managed", "empty", "failure"]) {
    const original = { claudeMd: "FULL POLICY", userEmail: "user@example.test", currentDate: "2026-09-30" };
    const context = vm.createContext({ rbo: async () => result === "failure" ? undefined : result === "empty" ? "" : "MANAGED POLICY" });
    vm.runInContext(body, context);
    cases.push({ source, result, original, expected: await context.Fl({ source }, original, {}) });
  }
}
const selectionCases = [];
for (const [name, omit, explicit, result] of [
  ["ordinary_grandchild", false, false, "managed"],
  ["explicit_omitting_fork", true, true, "managed"],
  ["explicit_ordinary_fork", false, true, "managed"],
  ["omitting_grandchild_managed", true, false, "managed"],
  ["omitting_grandchild_empty", true, false, "empty"],
  ["omitting_grandchild_failure", true, false, "failure"],
]) {
  const original = { claudeMd: "MANAGED PARENT POLICY", userEmail: "user@example.test", currentDate: "2026-09-30" };
  const loads = [];
  const context = vm.createContext({
    e: { source: "plugin", omitClaudeMd: omit },
    E: explicit ? { userContext: original } : undefined,
    n: { session: {}, options: {} },
    Gv: async () => { loads.push("Full"); return { ...original, claudeMd: "FULL POLICY" }; },
    rB: async () => ({}),
    rbo: async () => { loads.push("ManagedOnly"); return result === "failure" ? undefined : result === "empty" ? "" : "MANAGED POLICY"; },
  });
  vm.runInContext(body, context);
  const selected = await vm.runInContext(`(async () => { let ${selection}; return {userContext:ht, managedInstructionsOnly:go}; })()`, context);
  selectionCases.push({ name, omit, explicit, inheritedManaged: true, result, original, loads, expected: selected });
}
const generated = {
  provenance: { ...expected.provenance, selectionStartByte: selectionStart, selectionSourceSha256: selectionDigest },
  cases,
  selectionCases,
};
if (check === "--check") {
  if (JSON.stringify(generated) !== JSON.stringify(expected)) throw new Error("Instruction fixture differs from native output");
  console.log(`12 native omitClaudeMd and 6 native Lv selection cases match ${digest}`);
} else {
  fs.writeFileSync(output, JSON.stringify(generated, null, 2) + "\n");
}
