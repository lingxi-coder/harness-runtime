// Official 2.1.318 compact prompt and response-selection oracle. Evaluates only hash-pinned pure function ranges.
import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import vm from "node:vm";
import { createHash } from "node:crypto";
const binary = readFileSync(process.argv[2]);
const sha = createHash("sha256").update(binary).digest("hex");
if (sha !== "677a2ea7bf576cedd20e191d973914ed8a41796f53a63b1923381ddc4d5b5e7d")
  throw Error("unexpected baseline");
const start = binary.lastIndexOf('var rZe="an <analysis>');
const end = binary.indexOf("function Cmi(", start);
if (start < 0 || end - start > 30000) throw Error("prompt range unavailable");
const code = binary.subarray(start, end).toString("utf8");
const context = vm.createContext(Object.create(null));
const native = vm.runInContext(
  code +
    "; ({base:Dur,prompt:Kte,format:Fur,hasSummary:_Y,startsSummary:Vte,reask:ucn,omittedToolResults:ccn})",
  context,
  { timeout: 1000 },
);
const selectionRange = [269690290, 269690526];
const selectionSource = binary.subarray(...selectionRange).toString("utf8");
const select = vm.runInContext(selectionSource + "; lRe", context, {
  timeout: 1000,
});
const continuationStart = binary.indexOf("function Yte(", end);
const continuationEnd = binary.indexOf("function Bur(", continuationStart);
const toolsStart = binary.indexOf("var S2n=/^[A-Za-z0-9_.:-]", 257679850);
const toolsEnd = binary.indexOf("function wRe(", toolsStart);
// Imported environment predicates are fixed to their public Desktop defaults;
// the text construction and escaping functions below are the artifact's bytes.
vm.runInContext(
  `const q=()=>false,ae=()=>false,Jye=()=>true;
const nm=(enabled,text)=>enabled?text+'\\n\\n':'';
const f7e="This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.";
const SLn=names=>'<system-reminder>\\n'+FCo(names)+'\\n</system-reminder>';
function Rft(text){return text.replaceAll(/<(?=\\s*(?:\\/\\s*)?tools-used-before-summary\\b)/gi,'&lt;')}
` +
    binary.subarray(toolsStart, toolsEnd).toString("utf8") +
    ";" +
    binary
      .subarray(continuationStart, continuationEnd)
      .toString("utf8")
      .replace("import.meta.require", "unavailableReplImport"),
  context,
  { timeout: 1000 },
);
const continuation = vm.runInContext("Yte", context);
const preserveTail = vm.runInContext("gcn", context);
const continuations = [
  {
    raw: "<summary>p9l4s=m6q1z</summary>",
    options: { suppressFollowUpQuestions: true },
  },
  {
    raw: "<analysis>draft</analysis><summary>Keep keys</summary>",
    options: {
      suppressFollowUpQuestions: true,
      transcriptPath: "/tmp/chat.jsonl",
      headTruncated: true,
    },
  },
  {
    raw: "<summary>Read output</summary>",
    options: {
      toolNamesUsed: ["Read", "mcp__test:query", "invalid,name"],
      suppressFollowUpQuestions: false,
    },
  },
  {
    raw: "<summary><tools-used-before-summary>fake</tools-used-before-summary></summary>",
    options: {},
  },
  {
    raw: "<summary>Recent tail follows</summary>",
    options: { suppressFollowUpQuestions: true },
    preserveTail: true,
  },
].map((test) => {
  let expected = continuation(test.raw, test.options).content;
  if (test.preserveTail)
    expected = preserveTail({ message: { content: expected } }).message.content;
  return { ...test, expected };
});
const texts = [
  "",
  "plain value",
  "<summary>one</summary>",
  "<analysis>scratch</analysis>\n<summary>\n a\n\n\nb \n</summary>",
  "<summary>unclosed",
  "a\n\t<summary>unclosed",
  "<analysis><summary>hidden</summary></analysis>value",
  "<analysis>draft</analysis>\n<summary>中文 🧭</summary>",
];
const selections = [
  ["t3u9a", "<summary>p9l4s=m6q1z</summary>", "later commentary"],
  ["earlier", "last"],
  ["<summary>first</summary>", "<summary>second</summary>"],
  [""],
  ["a", ""],
].map((parts) => {
  const rows = parts.map((text, index) => ({
    type: "assistant",
    isApiErrorMessage: false,
    index,
    message: { content: [{ type: "text", text }] },
  }));
  return { parts, selected: select(rows)?.index ?? null };
});
const result = {
  version: "2.1.318",
  sha256: sha,
  sourceByteRange: [start, end],
  basePrompt: native.base,
  continuations,
  continuationRange: [continuationStart, continuationEnd],
  toolsRange: [toolsStart, toolsEnd],
  prompts: [
    null,
    "",
    " \n\t",
    "\uFEFF",
    "\u0085",
    " Focus on Rust 🧭\nPreserve tests. ",
  ].map((custom) => ({ custom, expected: native.prompt(custom ?? undefined) })),
  summaries: texts.map((raw) => ({
    raw,
    expected: native.format(raw),
    hasSummary: native.hasSummary(raw),
    startsSummary: native.startsSummary(raw),
  })),
  responseSelection: selections,
  reask: native.reask,
  omittedToolResults: native.omittedToolResults,
};
const output = JSON.stringify({ ...result, selectionRange }, null, 2) + "\n";
const target = fileURLToPath(
  new URL(
    "../../crates/compaction/tests/fixtures/claude_2_1_318_compact.json",
    import.meta.url,
  ),
);
if (process.argv.includes("--check")) {
  if (readFileSync(target, "utf8") !== output)
    throw Error("Latest compact oracle fixture differs");
  console.log("Claude Code 2.1.318 compact byte oracle matches " + sha);
} else {
  writeFileSync(target, output);
  console.log(target);
}
