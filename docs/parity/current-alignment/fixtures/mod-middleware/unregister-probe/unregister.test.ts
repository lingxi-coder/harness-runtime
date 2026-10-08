import { test } from "claude-code/testing";

test("probe on() subscription disposer policy", async ($) => {
  await $.session.append({
    message: { type: "user", content: [{ type: "text", text: "fixture" }] },
    door: "user",
    origin: { kind: "core" },
    uuid: "unregister",
  });
});
