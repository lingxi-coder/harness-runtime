import { test, expect, mock } from "claude-code/testing";
const input = (uuid, text) => ({message:{type:"user",content:[{type:"text",text}]},door:"user",origin:{kind:"core"},uuid});
const systemInput = (uuid, text) => ({message:{type:"system",content:[{type:"text",text}]},door:"notice",origin:{kind:"engine"},uuid});
test("session.append preserves lone UTF-16 units", async ($) => {
  const e=input("source", "\ud800x\udc00"); const r=await $.session.append(e);
  expect(JSON.stringify(r)).toBe(JSON.stringify({message:e.message,uuid:e.uuid}));
  console.log("NATIVE_APPEND_RESULT="+JSON.stringify({case:"source",input:e,result:r}));
});
test("session.append returns middleware rewritten content", async ($) => {
  const e=input("rewrite", "\ud800"); const r=await $.session.append(e);
  expect(r.message.content[0].text).toBe("prefix:"+e.message.content[0].text);
  console.log("NATIVE_APPEND_RESULT="+JSON.stringify({case:"rewrite",input:e,result:r}));
});
test("session.append middleware rewrites system content with a lone UTF-16 unit", async ($) => {
  const e=systemInput("00000000-0000-4000-8000-000000000015", "system-before-\ud800-after");
  const r=await $.session.append(e);
  expect(r.message.type).toBe("system");
  expect(r.message.content[0].text).toBe("system-prefix:"+e.message.content[0].text);
  console.log("NATIVE_APPEND_SYSTEM_RESULT="+JSON.stringify({case:"system-lone",input:e,result:r}));
});
test("mock.session retains successful rewritten rows and skips denied rows", async () => {
  let hook; const state=mock.session((name,handler)=>{expect(name).toBe("session.append");hook=handler;});
  const first=input("record", "\ud800"); const stored={message:{...first.message,content:[{type:"text",text:"\udc00"}]},uuid:first.uuid};
  expect(await hook({},first,async()=>stored)).toBe(stored);
  expect(JSON.stringify(state.appended())).toBe(JSON.stringify([{...first,message:stored.message}]));
  const denied={deny:"no"};expect(await hook({},input("deny","x"),async()=>denied)).toBe(denied);
  expect(state.appended().length).toBe(1);
  console.log("NATIVE_MOCK_SESSION_RESULT="+JSON.stringify(state.appended()));
});
