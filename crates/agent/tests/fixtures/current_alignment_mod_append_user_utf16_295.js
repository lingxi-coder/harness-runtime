// Source-derived candidate for the Native 2.1.295 user rewrite test.
// Native's fixture selects uuid "rewrite"; this copy maps that same
// `prefix:` + source-text operation to a stable UUID so AgentTranscriptWriter
// can exercise its typed MessageId path. This is not a captured Native event.
const USER_REWRITE_UUID = "00000000-0000-4000-8000-000000000016";

export function register(on) {
  on("session.append", async ($, event, next) => {
    if (event.uuid === USER_REWRITE_UUID) {
      return next({
        ...event,
        message: {
          ...event.message,
          content: event.message.content.map(block => ({
            ...block,
            text: `prefix:${block.text}`,
          })),
        },
      });
    }
    return next(event);
  });
}
