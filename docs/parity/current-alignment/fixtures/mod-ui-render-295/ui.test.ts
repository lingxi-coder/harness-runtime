import { expect, test } from "claude-code/testing";

const props = {
  hasSurvey: false,
  isWorking: false,
  maxRows: 5,
  bodyColumns: 80,
  scroll: { offset: 0, bodyRows: 5 },
  view: {},
};

function renderEvent(requestId) {
  return {
    surface: "terminal",
    component: "AbovePrompt",
    requestId,
    props,
  };
}

test("Button keeps Text and string children and derives its public label", async ($) => {
  const result = await $.ui.render(renderEvent("button-children"));
  console.log(`NATIVE_MOD_UI_BUTTON=${JSON.stringify(result)}`);
  expect(result.type).toBe("Button");
  expect(result.props.label).toBe("ABC");
  expect(result.children.length).toBe(2);
  expect(result.children[0]).toBe("A");
  expect(result.children[1].type).toBe("Text");
  expect(result.children[1].props.bold).toBe(true);
  expect(result.children[1].children).toEqual(["B", "C"]);
});

test("Button keeps children when an explicit label is supplied", async ($) => {
  const result = await $.ui.render(renderEvent("button-explicit-label"));
  console.log(`NATIVE_MOD_UI_BUTTON=${JSON.stringify(result)}`);
  expect(result.props.label).toBe("Explicit");
  expect(result.children[0].type).toBe("Text");
  expect(result.children[0].children).toEqual(["Child"]);
});

test("a lone string child becomes a label without a children field", async ($) => {
  const result = await $.ui.render(renderEvent("button-string-child"));
  console.log(`NATIVE_MOD_UI_BUTTON=${JSON.stringify(result)}`);
  expect(result.props.label).toBe("String child");
  expect(Object.hasOwn(result, "children")).toBe(false);
});

test("ui.notify accepts text but plugin-test has no delivery implementation", async ($) => {
  const event = {
    message: { type: "user", content: [{ type: "text", text: "probe" }] },
    door: "user",
    origin: { kind: "core" },
    uuid: "native-ui-notify-probe",
  };
  const result = await $.session.append(event);
  const text = result.message.content[0].text;
  console.log(`NATIVE_MOD_UI_NOTIFY=${text}`);
  expect(text).toBe("rejected:no implementation for ui.notify");
});
