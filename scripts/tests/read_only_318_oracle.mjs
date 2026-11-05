// Pure, hash-pinned read-only table/getopt replay. No shell command is executed.
import { readFileSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import vm from "node:vm";

const binary = readFileSync(process.argv[2]);
const sha256 = createHash("sha256").update(binary).digest("hex");
if (sha256 !== "677a2ea7bf576cedd20e191d973914ed8a41796f53a63b1923381ddc4d5b5e7d")
  throw Error("Unexpected Claude Code 2.1.318 artifact");
const ranges = {
  tables: [263804756, 263825409],
  flags: [263826511, 263828478],
};
function range(start, endAnchor) {
  const end = binary.indexOf(endAnchor, start);
  if (end < start || end - start > 30000) throw Error(`Missing range at ${start}`);
  return binary.subarray(start, end).toString("utf8");
}
const context = vm.createContext({ P: () => "macos", At: () => true, tt: (text, separator) => text.split(separator)[0], kds: () => false });
vm.runInContext('const _Z="__CMDSUB_OUTPUT__",O="__TRACKED_VAR__",C="__UNCERTAIN_VAR__";' +
  range(263499422, "function b4(") + range(263499846, "var ut=") +
  binary.subarray(...ranges.tables).toString("utf8") + ";" +
  range(268576124, "function qKe(") +
  range(binary.indexOf("var pp=/^-[a-zA-Z0-9_-]/", 263826000), "function Wdo(") +
  ";var " + range(268647616, ",d0o=") + ";" +
  ";var bXe=new Set(['-eq','-ne','-lt','-le','-gt','-ge']),E0t=/^-?(0[xX][0-9a-fA-F]+|[0-9]+#[0-9a-zA-Z]+|[0-9]+)$/,cVt={getStore:()=>undefined},u0o=/^[A-Za-z0-9:,]+$/,d0o='tengu_tender_thunder';" +
  range(268650666, ",M$a=") + ";var M$a=s0n;" +
  "var p0o=['echo','printf','wc','grep','egrep','fgrep','head','tail'],cbr=['docker ps','docker images'];" +
  range(268663960, "var f0o=") +
  range(binary.indexOf("function g0o(", 268664000), "var v0o=") +
  ";globalThis.native={g0o,E0o};globalThis.tables={...pVt,...M$a};", context, { timeout: 1000 });

const tokens = [
  ["gh", "search", "code", "symbol", "--json", "path,repository"],
  ["gh", "search", "repos", "lingxi", "--limit=10"],
  ["gh", "pr", "view", "42", "--json", "title"],
  ["gh", "pr", "view", "42", "--web=false"],
  ["gh", "pr", "view", "42", "--template", "title"],
  ["gh", "pr", "list", "--limit", "5"],
  ["gh", "pr", "list", "--limit", "-5"],
  ["gh", "pr", "list", "-L5"],
  ["gh", "pr", "view", "42", "-R", "owner/repo"],
  ["gh", "pr", "view", "https://github.com/owner/repo/pull/42"],
  ["gh", "pr", "view", "42", "--repo=owner/repo/nested"],
  ["gh", "auth", "status", "--show-token"],
  ["gh", "run", "view", "42", "-v", "--log-failed"],
  ["gh", "workflow", "list", "-a", "-L", "10"],
  ["docker", "logs", "--tail", "10", "app"],
  ["docker", "logs", "--tail=10", "--follow", "app"],
  ["docker", "inspect", "--format", "name", "app"],
  ["docker", "inspect", "--format=name", "app"],
  ["docker", "logs", "--output", "out", "app"],
  ["docker", "logs", "-Hprod", "app"],
  ["docker", "ps", "--config=alternate"],
  ["docker", "ps", "--", "--host=prod"],
  ["docker", "images", "--remote"],
  ["docker", "ps", "-ac"],
  ["docker", "ps", "--format", "name"],
  ["docker", "ps", "~/socket"],
  ["rg", "needle", "--pre", "executable"],
  ["rg", "needle", "--pre-glob", "*.txt"],
  ["rg", "needle", "--files"],
  ["rg", "needle", "-in"],
  ["rg", "needle", "-A5"],
  ["rg", "needle", "-A", "5"],
  ["rg", "needle", "-A", "5.5"],
  ["rg", "needle", "--glob", "*.rs"],
  ["rg", "needle", "--glob=*.rs"],
  ["rg", "needle", "--help=true"],
  ["rg", "needle", "--", "--pre", "literal"],
  ["rg", "-e", "two words", "file"],
  ["rg", "-e12", "file"],
  ["rg", "-ehello", "file"],
  ["grep", "-n", "needle", "file"],
  ["grep", "--regexp", "two words", "file"],
  ["grep", "--unknown", "needle"],
  ["grep", "$literal", "file"],
  ["git", "diff", "--output=out"],
  ["git", "diff", "--ext-diff"],
  ["git", "diff", "--no-ext-diff"],
  ["git", "log", "--format=oneline"],
  ["git", "log", "--format=%G?"],
  ["git", "log", "--format=%H"],
  ["git", "log", "--unknown"],
  ["git", "log", "-10", "--oneline"],
  ["git", "branch", "--list", "feature/*"],
  ["git", "branch", "--delete", "main"],
  ["git", "ls-files", "--stage"],
  ["git", "ls-files", "--with-tree=HEAD"],
  ["git", "ls-remote", "origin"],
  ["git", "ls-remote", "--heads"],
  ["git", "remote", "show", "-n", "origin"],
  ["git", "config", "--get", "user.name"],
  ["git", "config", "--get", "--file", "alternate", "user.name"],
  ["sort", "-n", "input"], ["sort", "--output", "out", "input"],
  ["date", "-u", "+%F"], ["date", "20300101"],
  ["tree", "-L", "2"], ["tree", "-o", "out"],
  ["sha256sum", "file"], ["base64", "--decode", "file"],
  ["xargs", "-I", "{}", "echo", "{}"], ["xargs", "rm"],
  ["xargs", "--", "head", "-n", "5"],
  ["man", "-k", "printf"], ["man", "-s", "3", "printf"], ["man", "/tmp/untrusted"],
  ["help", "printf"], ["help", "../untrusted"],
  ["hostname"], ["hostname", "new-name"],
  ["ps", "-ef"], ["ps", "e"], ["ps", "-e", "--forest", "-L"],
  ["lsof", "-i", "@127.0.0.1:80"], ["lsof", "-i", "@remote.example:80"],
  ["ss", "-lnt", "sport", "=", ":80"], ["ss", "dst", "remote.example"],
  ["tput", "cols"], ["tput", "clear"],
  ["fd", "--hidden", "needle"], ["fd", "--exec", "program"],
  ["test", "-f", "file"], ["test", "4", "-gt", "2"], ["test", "value[expr]", "-eq", "2"],
  ["sed", "-n", "p", "file.txt"], ["sed", "-n", "1,5p", "file"], ["sed", "s/a/b/"], ["sed", "s/a/b/", "file"], ["sed", "-i", "s/a/b/", "file"],
];
for (const [name, config] of Object.entries(context.tables)) {
  const command = name.split(" ");
  tokens.push(command, [...command, "--lingxi-unknown-flag"]);
  for (const [flag, kind] of Object.entries(config.safeFlags)) {
    if (flag === "--") continue;
    if (kind === "none") {
      tokens.push([...command, flag], [...command, `${flag}=invalid`]);
    } else {
      const value = kind === "number" ? "7" : "value";
      tokens.push([...command, flag, value], [...command, `${flag}=${value}`], [...command, flag]);
      if (kind === "number") tokens.push([...command, `${flag}=7.5`]);
    }
  }
}
const cases = tokens.map((words) => {
  // Tokenization is deliberately supplied, not attributed to this oracle.
  // The public Rust test also checks a quoted spelling for every token list.
  context.words = words;
  vm.runInContext("globalThis.$C=()=>words", context);
  const raw = words.join(" ");
  const nativeTable = context.pVt;
  // GH's table exists in the artifact, but native c0o does not select it.
  // Probe its finite flag semantics separately, then restore the real table.
  if (words[0] === "gh") context.pVt = { ...nativeTable, ...context.M$a };
  const expected = words[0] === "docker" && ["ps", "images"].includes(words[1])
    ? context.native.E0o(words) : context.native.g0o(raw);
  context.pVt = nativeTable;
  const readOnlyExpected = words[0] === "docker" && ["ps", "images"].includes(words[1])
    ? context.native.E0o(words) : context.native.g0o(raw);
  const quoted = words.map((word) => "'" + word.replaceAll("'", "'\\''") + "'").join(" ");
  const quotedExpected = words[0] === "docker" && ["ps", "images"].includes(words[1])
    ? context.native.E0o(words) : context.native.g0o(quoted);
  return { words, expected, ...(readOnlyExpected !== expected ? { readOnlyExpected } : {}),
    ...(quotedExpected !== readOnlyExpected ? { quotedExpected } : {}) };
});
// Exercise vat's public glob gate as well as the private flag tables. The
// simple AST/argv is supplied explicitly; the scanner and gate are Native.
const publicGlobInputs = [
  ["sort *", ["sort", "*"]], ["file ?", ["file", "?"]],
  ["sed -n p [ab]", ["sed", "-n", "p", "[ab]"]],
  ["git log *", ["git", "log", "*"]], ["rg needle *", ["rg", "needle", "*"]],
  ["cat *", ["cat", "*"]], ["sha256sum *.txt", ["sha256sum", "*.txt"]],
  ["sort '*'", ["sort", "*"]], ["sort \\*", ["sort", "*"]],
  ['sort "*"', ["sort", "*"]], ["sed -n p '[ab]'", ["sed", "-n", "p", "[ab]"]],
];
const publicGlobCases = publicGlobInputs.map(([command, words]) => {
  context.words = words;
  const publicContext = vm.createContext({
    fx: 10000, ly: () => ({ parse: () => ({}) }),
    _$: () => ({ kind: "simple", commands: [{ argv: words, text: command, redirects: [], envVars: [] }], bareAssignmentNames: [] }),
    fVt: { getStore: () => false }, SVt: () => false, bVt: () => false,
    jSe: () => false, fI: () => false, oZ: () => false, FT: () => false,
    eZ: (argv) => argv, I0o: (text) => text, I2e: () => false, AJt: () => false, P: () => "macos",
    g0o: (text) => context.native.g0o(text), E0o: (argv) => context.native.E0o(argv),
  });
  const code = range(268669115, "function C0o(") +
    range(binary.indexOf("function R0o(", 268669115), "var yVt=") +
    range(268675702, "function rZ(");
  const result = vm.runInContext(`${code};vat({command:${JSON.stringify(command)}},false,{offlineEstimate:true})`, publicContext);
  return { command, expected: result.behavior === "allow" };
});
const configs = Object.fromEntries(Object.entries(context.tables).map(([name, config]) => [name, { safeFlags: config.safeFlags,
  ...(config.respectsDoubleDash === false ? { respectsDoubleDash: false } : {}) }]));
const tableText = "{\n" + Object.entries(configs).map(([name, config]) => "  " + JSON.stringify(name) + ":" + JSON.stringify(config)).join(",\n") + "\n}\n";
const fixture = JSON.stringify({ version: "2.1.318", sha256, tablesRange: ranges.tables,
  scope: "Native getopt/callbacks with injected shell words, POSIX (macOS/Linux share these predicates), default-on safety flags. GH table validation is separate from native c0o dispatch: its unused GH table does not grant read-only calls. Sed preflight is injected accepted for these syntactically simple quoted fixtures; its expression evaluator is replayed. Full shell parser and shell execution excluded." }, null, 2).slice(0, -2) +
  ',\n  "publicGlobCases": ' + JSON.stringify(publicGlobCases) +
  ',\n  "cases": [\n' + cases.map((item) => "    " + JSON.stringify(item)).join(",\n") + '\n  ]\n}\n';
const outputs = [
  [new URL("../../crates/permission/src/bundled/read_only_318_flags.json", import.meta.url), tableText],
  [new URL("../../crates/permission/tests/fixtures/read_only_2_1_318.json", import.meta.url), fixture],
];
for (const [path, expected] of outputs) {
  if (process.argv.includes("--write")) writeFileSync(path, expected);
  else if (readFileSync(path, "utf8") !== expected) throw Error(`Oracle output changed: ${path}`);
}
console.log(`Claude Code 2.1.318 read-only: ${Object.keys(configs).length} configs and ${cases.length} cases match`);
