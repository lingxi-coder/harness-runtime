const units = (text) => Array.from({ length: text.length }, (_, index) => text.charCodeAt(index));

export function register(on) {
  on("session.append", async ($, event, next) => {
    if (event.message?.type !== "assistant") return await next(event);
    const source = event.message.content.find(
      (block) => block.type === "text" && block.text === "Native seed answer",
    );
    if (!source) return await next(event);

    const rewritten =
      "accepted Mod text " +
      String.fromCharCode(0xd800) +
      "Ω" +
      String.fromCharCode(0xdc00);
    const forwarded = {
      ...event,
      message: {
        ...event.message,
        content: event.message.content.map((block) =>
          block === source ? { ...block, text: rewritten } : block,
        ),
      },
    };
    const result = await next(forwarded);
    console.log(
      "NATIVE_COLD_RESUME_APPEND=" +
        JSON.stringify({
          type: event.message.type,
          uuid: event.uuid,
          source_units: units(source.text),
          forwarded_units: units(rewritten),
          result_units: units(result.message.content.find((block) => block.type === "text").text),
        }),
    );
    return result;
  });

  on("turn.complete", async ($, event, next) => {
    const summary = await $.session.messages();
    const api = await $.session.messages({ as: "api" });
    const summaryRows = summary
      .filter((row) => row.role === "assistant" && row.text.startsWith("accepted Mod text "))
      .map((row) => units(row.text));
    const apiBlocks = api
      .flatMap((row) => row.content ?? [])
      .filter((block) => block.type === "text" && block.text.startsWith("accepted Mod text "))
      .map((block) => units(block.text));
    console.log(
      "NATIVE_COLD_RESUME_SESSION_MESSAGES=" +
        JSON.stringify({
          answer_units: typeof event.answer === "string" ? units(event.answer) : null,
          summary_units: summaryRows,
          api_units: apiBlocks,
        }),
    );
    return next(event);
  });
}
