// Raw HTTP capture, including header spelling/order and SSE bytes. This keeps
// wire evidence that an ordinary parsed node:http request cannot reconstruct.
import {createServer} from 'node:net';
import {mkdir, writeFile} from 'node:fs/promises';
import path from 'node:path';

export const dummyApiKey = 'sk-ant-headless-local-fixture-only';
const sse = (event, value) => `event: ${event}\ndata: ${JSON.stringify(value)}\n\n`;
export function providerBody(body, response, index) {
  if (response.httpError) return {status: response.httpError.status, contentType: 'application/json', bytes: Buffer.from(JSON.stringify(response.httpError.body))};
  const model = body.model ?? 'claude-sonnet-5-5';
  const usage = response.usage ?? {input_tokens: 1, output_tokens: 2};
  const stop = response.stopReason ?? (response.tool ? 'tool_use' : 'end_turn');
  const content = response.tool
    ? [{type: 'tool_use', id: `tool_headless_${index}`, name: response.tool.name, input: response.tool.input}]
    : [{type: 'text', text: response.text ?? ''}];
  const message = {id: `msg_headless_${index}`, type: 'message', role: 'assistant', model, content, stop_reason: stop, stop_sequence: null, usage};
  if (!body.stream) return {contentType: 'application/json', bytes: Buffer.from(JSON.stringify(message))};
  const events = [sse('message_start', {type: 'message_start', message: {...message, content: [], stop_reason: null, usage: {input_tokens: usage.input_tokens, output_tokens: 0}}})];
  const block = content[0];
  events.push(sse('content_block_start', {type: 'content_block_start', index: 0, content_block: response.tool ? {...block, input: {}} : {type: 'text', text: ''}}));
  if (response.tool) events.push(sse('content_block_delta', {type: 'content_block_delta', index: 0, delta: {type: 'input_json_delta', partial_json: JSON.stringify(block.input)}}));
  else for (const text of response.chunks ?? [block.text]) events.push(sse('content_block_delta', {type: 'content_block_delta', index: 0, delta: {type: 'text_delta', text}}));
  events.push(sse('content_block_stop', {type: 'content_block_stop', index: 0}));
  events.push(sse('message_delta', {type: 'message_delta', delta: {stop_reason: stop, stop_sequence: null}, usage: {output_tokens: usage.output_tokens}}));
  events.push(sse('message_stop', {type: 'message_stop'}));
  return {contentType: 'text/event-stream', bytes: Buffer.from(events.join(''))};
}

// Research routes are selected from the actual last user message, so parallel
// parent/child request scheduling cannot change the fixture response assignment.
export function selectResponse(fixture, body, ordinal) {
  if (!fixture.responseRoutes) return {response: fixture.responses[ordinal], index: ordinal};
  const user = body.messages?.findLast(message => message.role === 'user');
  const blocks = typeof user?.content === 'string' ? [{type: 'text', text: user.content}] : user?.content ?? [];
  const text = blocks.filter(block => block.type === 'text').map(block => block.text).join('\n');
  const matches = fixture.responses.flatMap((response, index) => {
    const match = response.match;
    if (!match || Object.keys(match).length !== 1) throw new Error('Invalid explicit provider response route');
    const matches = typeof match.lastUserTextIncludes === 'string' ? text.includes(match.lastUserTextIncludes)
      : match.lastUserHasToolResult === true ? blocks.some(block => block.type === 'tool_result') : false;
    return matches ? [{response, index}] : [];
  });
  if (ordinal >= fixture.maxProviderRequests || matches.length !== 1) return {response: undefined, index: -1};
  return matches[0];
}

export async function startProvider() {
  const sockets = new Set();
  const pending = new Set();
  let active;
  const server = createServer(socket => {
    sockets.add(socket);
    socket.on('close', () => sockets.delete(socket));
    socket.on('error', error => { if (active) active.socketErrors.push(error.code ?? String(error)); });
    let buffer = Buffer.alloc(0);
    socket.on('data', chunk => {
      buffer = Buffer.concat([buffer, chunk]);
      for (;;) {
        const headerEnd = buffer.indexOf('\r\n\r\n');
        if (headerEnd < 0) break;
        const headersBytes = buffer.subarray(0, headerEnd + 4);
        const lines = headersBytes.toString('latin1').split('\r\n');
        const [method, url, protocol] = lines.shift().split(' ');
        const headers = lines.filter(Boolean).map(line => { const split = line.indexOf(':'); return [line.slice(0, split), line.slice(split + 1).trim()]; });
        const lookup = name => headers.find(([key]) => key.toLowerCase() === name)?.[1];
        const length = Number(lookup('content-length') ?? 0);
        if (!Number.isSafeInteger(length) || length < 0 || lookup('transfer-encoding')) { socket.destroy(new Error('unsupported request framing')); break; }
        if (buffer.length < headerEnd + 4 + length) break;
        const raw = Buffer.from(buffer.subarray(0, headerEnd + 4 + length));
        const requestBody = raw.subarray(headerEnd + 4);
        buffer = buffer.subarray(raw.length);
        const state = active;
        if (!state) { socket.destroy(new Error('provider has no active fixture')); break; }
        const task = (async () => {
          const ordinal = state.requests.length;
          const record = {ordinal, method, url, protocol, headers, process: state.processIndex, fixtureResponse: null};
          state.requests.push(record);
          const prefix = path.join(state.captureDir, 'provider', String(ordinal).padStart(3, '0'));
          await mkdir(path.dirname(prefix), {recursive: true});
          await Promise.all([writeFile(`${prefix}.request.bin`, raw), writeFile(`${prefix}.request-headers.bin`, headersBytes), writeFile(`${prefix}.request-body.bin`, requestBody)]);
          let status = 200;
          let result = {contentType: 'application/json', bytes: Buffer.alloc(0)};
          let responseFixture;
          if (socket.remoteAddress !== '127.0.0.1') { status = 403; state.unexpected.push('non-loopback-peer'); }
          else if (method === 'HEAD' && url === '/api/hello') { /* local health */ }
          else if (method === 'POST' && (url === '/v1/messages?beta=true' || url === '/v1/messages')) {
            if (lookup('x-api-key') !== dummyApiKey) { status = 401; state.unexpected.push('non-dummy-api-key'); }
            else {
              let body;
              try { body = JSON.parse(requestBody); } catch { status = 400; state.unexpected.push('invalid-json-body'); }
              if (body) {
                const selected = selectResponse(state.fixture, body, state.messageCount);
                responseFixture = selected.response;
                record.fixtureResponse = selected.index;
                record.requestWallMs = Date.now();
                state.messageCount++;
                if (!responseFixture) { status = 429; state.unexpected.push('fixture-response-limit'); }
                else { result = providerBody(body, responseFixture, state.fixture.responseRoutes ? record.fixtureResponse + 1 : state.messageCount); status = result.status ?? status; state.onMessageRequest?.(record); }
              }
            }
          } else { status = 404; state.unexpected.push(`unexpected-route:${method}:${url}`); }
          const header = Buffer.from(`HTTP/1.1 ${status} ${status === 200 ? 'OK' : 'Fixture Error'}\r\nContent-Type: ${result.contentType}\r\nContent-Length: ${result.bytes.length}\r\nConnection: keep-alive\r\n\r\n`, 'latin1');
          const wire = Buffer.concat([header, result.bytes]);
          await writeFile(`${prefix}.offered-response.bin`, wire);
          const delay = responseFixture?.delayMs ?? state.fixture.providerDelayMs ?? 0;
          if (delay) await new Promise(resolve => setTimeout(resolve, delay));
          record.responseWriteWallMs = Date.now();
          record.responseWriteAttempted = !socket.destroyed;
          if (record.responseWriteAttempted) socket.write(wire);
          state.onResponseWrite?.(record);
          // An interrupted request can close before a response is written.
          // Preserve the offered fixture separately; response.bin contains
          // only bytes actually handed to the socket, or zero bytes if closed.
          await Promise.all([writeFile(`${prefix}.response.bin`, record.responseWriteAttempted ? wire : Buffer.alloc(0)), writeFile(`${prefix}.response-body.bin`, record.responseWriteAttempted ? result.bytes : Buffer.alloc(0)), writeFile(`${prefix}.meta.json`, JSON.stringify(record, null, 2) + '\n')]);
        })();
        pending.add(task);
        task.catch(error => { state.unexpected.push(`provider-error:${String(error)}`); socket.destroy(); }).finally(() => pending.delete(task));
      }
    });
  });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
  return {
    baseUrl: `http://127.0.0.1:${server.address().port}`,
    begin(fixture, captureDir) { active = {fixture, captureDir, requests: [], unexpected: [], socketErrors: [], messageCount: 0, processIndex: 0}; return active; },
    async settle() { await Promise.all([...pending]); },
    async close() { for (const socket of sockets) socket.destroy(); await Promise.all([...pending]); await new Promise(resolve => server.close(resolve)); },
  };
}
