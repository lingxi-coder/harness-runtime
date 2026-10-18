import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { openSync, readSync, closeSync } from "node:fs";

const artifactPath = "/Users/luolingfeng/.local/share/claude/versions/2.1.295";
const artifactSha256 =
  "0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70";
const range = { offset: 186325155, bytes: 7419 };
const expectedRangeSha256 =
  "a66f6f6d6f121ab2a8a9b5acf8ad8fa5d95ce3c6b55175dd7bbf5463b3567f0b";

const fd = openSync(artifactPath, "r");
const rangeBuffer = Buffer.alloc(range.bytes);
try {
  const count = readSync(fd, rangeBuffer, 0, range.bytes, range.offset);
  assert.equal(count, range.bytes, "complete pinned JavaScript source range");
} finally {
  closeSync(fd);
}

const rangeSha256 = createHash("sha256").update(rangeBuffer).digest("hex");
assert.equal(rangeSha256, expectedRangeSha256, "pinned source range SHA-256");
const source = rangeBuffer.toString("utf8");

function extract(startMarker, endMarker) {
  const start = source.indexOf(startMarker);
  assert.notEqual(start, -1, `source marker present: ${startMarker}`);
  const end = source.indexOf(endMarker, start + startMarker.length);
  assert.notEqual(end, -1, `end marker present: ${endMarker}`);
  return source.slice(start, end);
}

const matcher = new Function(
  [
    extract("function P(e)", 'var A="mcp__";'),
    extract("function h(e)", "function bnn("),
    extract("function LTr(e)", "function y(e)"),
    "return LTr;",
  ].join("\n"),
)();

const cases = [
  { id: "exact-equality", pattern: "plain", name: "plain", expected: true },
  { id: "exact-mismatch", pattern: "plain", name: "other", expected: false },
  { id: "wildcard-ordinary", pattern: "pos_a*b", name: "pos_ab", expected: true },
  { id: "wildcard-internal-lf", pattern: "lf_a*b", name: "lf_a\nb", expected: true },
  { id: "wildcard-internal-cr", pattern: "cr_a*b", name: "cr_a\rb", expected: true },
  {
    id: "wildcard-internal-line-separator",
    pattern: "ls_a*b",
    name: "ls_a\u2028b",
    expected: true,
  },
  {
    id: "wildcard-internal-paragraph-separator",
    pattern: "ps_a*b",
    name: "ps_a\u2029b",
    expected: true,
  },
  {
    id: "terminal-lf-is-not-ignored",
    pattern: "suffix_a*b",
    name: "suffix_ab\n",
    expected: false,
  },
  {
    id: "wildcard-can-split-a-surrogate-pair",
    pattern: "unit_\ud83d*\ude00",
    name: "unit_\ud83d\ude00",
    expected: true,
  },
];

const utf16Units = (value) =>
  Array.from({ length: value.length }, (_, index) =>
    value.charCodeAt(index).toString(16).padStart(4, "0"),
  );

const results = cases.map(({ id, pattern, name, expected }) => {
  const observed = matcher(pattern)(name);
  assert.equal(observed, expected, id);
  return {
    id,
    pattern_utf16_units: utf16Units(pattern),
    name_utf16_units: utf16Units(name),
    expected,
    observed,
  };
});

process.stdout.write(
  `${JSON.stringify(
    {
      schema: "claude-2.1.295-ltr-source-probe-v1",
      execution: "extracted-pure-javascript-functions-only",
      native_process_started: false,
      artifact: { path: artifactPath, sha256: artifactSha256 },
      source_range: {
        ...range,
        source_sha256: rangeSha256,
        functions: ["P", "h", "LTr"],
      },
      node_version: process.versions.node,
      cases: results,
    },
    null,
    2,
  )}\n`,
);
