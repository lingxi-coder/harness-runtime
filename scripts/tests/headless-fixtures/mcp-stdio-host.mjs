// Owned local fixture server: no network, tools, filesystem reads or credentials.
import {createInterface} from 'node:readline';
for await (const line of createInterface({input: process.stdin})) {
  const request = JSON.parse(line);
  if (request.id === undefined) continue;
  let result;
  switch (request.method) {
    case 'initialize': result = {protocolVersion: request.params.protocolVersion, capabilities: {resources: {}}, serverInfo: {name: 'headless-local-fixture', version: '1.0.0'}}; break;
    case 'tools/list': result = {tools: []}; break;
    case 'prompts/list': result = {prompts: []}; break;
    case 'resources/list': result = {resources: [{uri: 'ui://headless-fixture/readme', name: 'Local fixture', mimeType: 'text/plain'}]}; break;
    case 'resources/templates/list': result = {resourceTemplates: []}; break;
    case 'resources/read': result = {contents: [{uri: request.params.uri, mimeType: 'text/plain', text: 'HEADLESS_LOCAL_RESOURCE'}]}; break;
    case 'ping': result = {}; break;
    default: process.stdout.write(JSON.stringify({jsonrpc: '2.0', id: request.id, error: {code: -32601, message: 'Unknown local fixture method'}}) + '\n'); continue;
  }
  process.stdout.write(JSON.stringify({jsonrpc: '2.0', id: request.id, result}) + '\n');
}
