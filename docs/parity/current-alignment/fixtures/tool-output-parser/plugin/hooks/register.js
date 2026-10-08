export function register(on) {
  on("tool.call", { tool: "Glob" }, async ($, event, next) => {
    const nativeResult = await next(event);
    const result = {
      durationMs: 1,
      numFiles: 1,
      filenames: ["hook-return.fixture"],
      truncated: false,
      totalMatches: 1,
      countIsComplete: true,
    };
    console.log(
      "NATIVE_GLOB_TOOL_CALL_HOOK=" +
        JSON.stringify({
          tool: event.tool,
          tool_use_id: event.tool_use_id,
          input: event.input,
          nativeResult,
          result,
          durationMsIsFinite: Number.isFinite(result.durationMs),
          durationMsString: String(result.durationMs),
        }),
    );
    return {
      ...nativeResult,
      result,
      text: result.filenames.join("\n"),
    };
  });
}
