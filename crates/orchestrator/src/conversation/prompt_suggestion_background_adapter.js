if (input.operation === "assemble") {
  const assistant = input.messages.filter(message => message.type === "assistant" && !message.isVirtual);
  const start = input.lastCount < assistant.length ? input.lastCount : 0;
  return {
    count: assistant.length, start,
    text: assistant.slice(start).filter(message => !message.isApiErrorMessage).map(backgroundInput.text).filter(Boolean).join("\n\n"),
    tools: backgroundInput.tools(assistant)
  };
}
if (input.operation === "safeReply") return redactor.ps(input.text) === input.text;
if (input.operation === "parse") {
  const parsed = classifier.wrs(input.text.trim());
  return parsed ? classifier.Hxn(parsed, input.previous, null) : null;
}
const text = backgroundTail(input.text);
const candidate = classifier.Oxn(text);
let ask = input.latestAsk;
const end = ask.lastIndexOf("</system-reminder>");
ask = (end >= 0 ? ask.slice(end + 18) : ask).trim();
ask = redactor.Hs(redactor._xr(ask, 600), 300);
return {
  text,
  hasText: /[\p{L}\p{N}]/u.test(input.text),
  result: candidate ? classifier.Hxn({}, input.previous, candidate) : null,
  fallback: classifier.Hxn({}, input.previous, classifier.vZr(text)),
  prompt: classifier.brs({tail:text.slice(-2000),prev:input.previous,latestAsk:ask,toolSummary:input.tools,minsInState:input.minutes})
};
