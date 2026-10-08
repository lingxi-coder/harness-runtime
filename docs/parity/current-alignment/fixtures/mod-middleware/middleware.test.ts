import { test, expect } from "claude-code/testing";

const input = (uuid, text) => ({
  message: { type: "user", content: [{ type: "text", text }] },
  door: "user",
  origin: { kind: "core" },
  uuid,
});

test("session.append middleware next rewrite returns raw rewritten event", async ($) => {
  const event = input("rewrite", "fixture-input");
  const result = await $.session.append(event);
  console.log(`NATIVE_MOD_RAW_EVENT=${JSON.stringify({ case: "rewrite", input: event, result })}`);
  expect(result.message.content[0].text).toBe("native-prefix:fixture-input");
});

test("session.append middleware direct return is skipped and the original row is kept", async ($) => {
  const event = input("direct", "fixture-input");
  const result = await $.session.append(event);
  console.log(`NATIVE_MOD_RAW_EVENT=${JSON.stringify({ case: "direct", input: event, result })}`);
  expect(result.message.content[0].text).toBe("fixture-input");
});

test("session.append middleware can return a denial without next", async ($) => {
  const event = input("cancel", "fixture-input");
  let outcome;
  try {
    outcome = { returned: await $.session.append(event) };
  } catch (error) {
    outcome = { threw: String(error), message: String(error?.message ?? "") };
  }
  console.log(`NATIVE_MOD_RAW_EVENT=${JSON.stringify({ case: "cancel", input: event, outcome })}`);
  expect(outcome.returned?.message.content[0].text ?? "").toBe("fixture-input");
});

test("session.append middleware exception is surfaced to the caller", async ($) => {
  const event = input("throw", "fixture-input");
  let outcome;
  try {
    outcome = { returned: await $.session.append(event) };
  } catch (error) {
    outcome = { threw: String(error), message: String(error?.message ?? "") };
  }
  console.log(`NATIVE_MOD_RAW_EVENT=${JSON.stringify({ case: "throw", input: event, outcome })}`);
  expect(outcome.returned?.message.content[0].text ?? "").toBe("fixture-input");
});
