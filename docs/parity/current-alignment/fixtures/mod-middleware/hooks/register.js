export function register(on) {
  console.log("NATIVE_MOD_REGISTERED");
  on("session.append", async ($, event, next) => {
    const fixedHarnessCases = {
      "00000000-0000-4000-8000-000000000001": "rewrite",
      "00000000-0000-4000-8000-000000000002": "direct",
      "00000000-0000-4000-8000-000000000003": "cancel",
      "00000000-0000-4000-8000-000000000004": "throw",
    };
    const mode = fixedHarnessCases[event.uuid] ?? event.uuid;
    if (mode === "rewrite") {
      return await next({
        ...event,
        message: {
          ...event.message,
          content: event.message.content.map(block => ({ ...block, text: `native-prefix:${block.text}` })),
        },
      });
    }
    if (mode === "direct") {
      return {
        uuid: event.uuid,
        message: { type: "user", content: [{ type: "text", text: "native-direct-return" }] },
      };
    }
    if (mode === "cancel") {
      return { deny: "native-middleware-fixture-cancel" };
    }
    if (mode === "throw") {
      throw new Error("native-middleware-fixture-error");
    }
    return await next(event);
  });
}
