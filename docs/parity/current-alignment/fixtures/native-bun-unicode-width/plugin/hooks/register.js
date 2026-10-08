import * as claudeCode from "claude-code";

const samples = [
  ["lone-high", String.fromCharCode(0xd800)],
  ["lone-low", String.fromCharCode(0xdc00)],
  ["lone-high-combining-acute", String.fromCharCode(0xd800, 0x0301)],
  ["replacement-combining-acute", String.fromCharCode(0xfffd, 0x0301)],
  ["lone-high-zwj-grinning", String.fromCharCode(0xd800, 0x200d, 0xd83d, 0xde00)],
  ["woman-technologist-zwj", "👩‍💻"],
];

function codeUnits(value) {
  return Array.from({ length: value.length }, (_, index) => value.charCodeAt(index));
}

function segment(value) {
  try {
    const segmenter = new Intl.Segmenter("en", { granularity: "grapheme" });
    return {
      values: Array.from(segmenter.segment(value), (item) => ({
        index: item.index,
        text: item.segment,
        codeUnits: codeUnits(item.segment),
      })),
    };
  } catch (error) {
    return { error: String(error) };
  }
}

export async function register(_on) {
  const bun = typeof Bun === "undefined" ? null : Bun;
  let widthFunction = bun && typeof bun.stringWidth === "function" ? bun.stringWidth : null;
  let widthProvider = widthFunction ? "Bun.stringWidth" : null;
  if (!widthFunction && typeof claudeCode.stringWidth === "function") {
    widthFunction = claudeCode.stringWidth;
    widthProvider = "claude-code.stringWidth";
  }
  if (!widthFunction && typeof globalThis.stringWidth === "function") {
    widthFunction = globalThis.stringWidth;
    widthProvider = "globalThis.stringWidth";
  }
  const widthImportError = widthFunction ? null : "No Bun.stringWidth or globalThis.stringWidth in plugin module scope";

  const measureWidth = (value) => {
    if (!widthFunction) return { error: widthImportError ?? "width function unavailable" };
    try {
      return { value: widthFunction(value) };
    } catch (error) {
      return { error: String(error) };
    }
  };
  const result = {
    bunVersion: bun?.version ?? null,
    processBunVersion: typeof process === "undefined" ? null : process.versions?.bun ?? null,
    processNodeVersion: typeof process === "undefined" ? null : process.versions?.node ?? null,
    processGlobalAvailable: typeof process !== "undefined",
    bunGlobalAvailable: typeof Bun !== "undefined",
    claudeCodeExports: Object.keys(claudeCode).sort(),
    widthProvider,
    widthImportError,
    stringWidthAvailable: typeof widthFunction === "function",
    segmenterAvailable: typeof Intl.Segmenter === "function",
    samples: samples.map(([name, text]) => ({
      name,
      text,
      codeUnits: codeUnits(text),
      width: measureWidth(text),
      graphemes: segment(text),
    })),
  };
  console.error("NATIVE_BUN_UNICODE_ORACLE=" + JSON.stringify(result));
}
