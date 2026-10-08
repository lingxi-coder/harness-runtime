export function register(on) {
  on("session.start", async ($, event, next) => {
    await $.tool.register({
      name: "probe",
      description: "Fixed Native Unicode preview-boundary fixture",
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
          "a".repeat(1999) + "😀" + "TAIL😀" + "z".repeat(48000),
      };
      const units = (index) => result.result.charCodeAt(index);
      console.log(
        "NATIVE_TOOL_RESULT_META=" +
          JSON.stringify({
            tool: event.tool,
            tool_use_id: event.tool_use_id,
            result_units: result.result.length,
            unit_at_1998: units(1998),
            unit_at_1999: units(1999),
            unit_at_2000: units(2000),
            unit_at_2005: units(2005),
            unit_at_2006: units(2006),
          }),
      );
      return result;
    },
  );
}
