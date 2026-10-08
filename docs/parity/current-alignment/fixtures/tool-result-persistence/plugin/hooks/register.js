export function register(on) {
  on("session.start", async ($, event, next) => {
    await $.tool.register({
      name: "probe",
      description: "Fixed Native tool-result persistence fixture",
      inputSchema: { type: "object", properties: {}, additionalProperties: false },
      isDeferred: false,
    });
    return await next(event);
  });

  on(
    "tool.call",
    { tool: "mcp__native-tool-result-persist-fixture__probe" },
    async ($, event) => {
      const result = {
        result:
          "begin" +
          String.fromCharCode(0xd800) +
          "z".repeat(50001),
      };
      console.log(
        "NATIVE_TOOL_RESULT_META=" +
          JSON.stringify({
            tool: event.tool,
            tool_use_id: event.tool_use_id,
            result_units: result.result.length,
            unit_at_5: result.result.charCodeAt(5),
          }),
      );
      return result;
    },
  );
}
