export function register(on) {
  on("ui.render", async ($, event, next) => {
    const { Button, Text } = $.ui.resolve(event);

    if (event.requestId === "button-children") {
      return Button({
        key: "button-children",
        children: ["A", Text({ bold: true, children: ["B", "C"] })],
        onPress: () => {},
      });
    }

    if (event.requestId === "button-explicit-label") {
      return Button({
        key: "button-explicit-label",
        label: "Explicit",
        children: [Text({ children: "Child" })],
        onPress: () => {},
      });
    }

    if (event.requestId === "button-string-child") {
      return Button({
        key: "button-string-child",
        children: ["String child"],
        onPress: () => {},
      });
    }

    return await next(event);
  });

  on("session.append", async ($, event, next) => {
    let result;
    try {
      result = `resolved:${String(await $.ui.notify("native-safe-probe"))}`;
    } catch (error) {
      result = `rejected:${String(error?.message ?? error)}`;
    }
    return await next({
      ...event,
      message: {
        ...event.message,
        content: [{ type: "text", text: result }],
      },
    });
  });
}
