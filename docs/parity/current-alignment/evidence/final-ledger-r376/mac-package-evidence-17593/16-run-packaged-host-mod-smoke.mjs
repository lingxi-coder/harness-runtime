#!/usr/bin/env node

import assert from 'node:assert/strict';
import { createHash, randomUUID } from 'node:crypto';
import { execFileSync, spawn } from 'node:child_process';
import { createServer } from 'node:http';
import {
  cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync,
  renameSync, rmSync, statSync, writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, dirname, join, resolve } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath, pathToFileURL } from 'node:url';

const SCRIPT_DIR = dirname(fileURLToPath(import.meta.url));
const PRODUCT_ROOT = resolve(SCRIPT_DIR, '../../../lingxi-app');
const smokeHelpers = await import(pathToFileURL(join(
  PRODUCT_ROOT, 'apps/electron/scripts/packaged-app-smoke.mjs',
)).href);
const bunHelpers = await import(pathToFileURL(join(
  PRODUCT_ROOT, 'apps/electron/scripts/mod-bun.mjs',
)).href);

const APP_NAME = smokeHelpers.APP_NAME;
const runningAppCommands = smokeHelpers.runningAppCommands;
const PLUGIN_ID = 'product-packaged-mod-smoke@acceptance';
const PLUGIN_NAME = 'product-packaged-mod-smoke';
const PLUGIN_VERSION = '1.0.0';
const UI_PLUGIN_ID = 'packaged-host-client-ui-smoke@acceptance';
const UI_PLUGIN_NAME = 'packaged-host-client-ui-smoke';
const UI_PLUGIN_VERSION = '1.0.0';
const UI_CLIENT_KEY = 'host-client-form';
const UI_CLIENT_MODULE = 'hooks/client.tsx';
const MARKER = 'PACKAGED_MOD_PROBE';
const TOOL_PROBE_MARKER = 'PACKAGED_TOOL_PROBE';
const TOOL_READ_MARKER = 'PACKAGED_TOOL_READ_MARKER';
const TOOL_FINAL_MARKER = 'PACKAGED_TOOL_READ_COMPLETED';
const TOOL_SEARCH_CALL_ID = 'call_packaged_tool_search';
const TOOL_READ_CALL_ID = 'call_packaged_tool_read';
const ACTIVE_PREFIX = '[product-mod-active]';
const FIXTURE_ROOT = resolve(SCRIPT_DIR, '../packaged-mod-fixture');
const UI_FIXTURE_ROOT = resolve(SCRIPT_DIR, '../packaged-ui-fixture');
const SAFE_PATH = '/usr/bin:/bin:/usr/sbin:/sbin';
const PROVIDER_KEY = 'packaged-host-smoke-synthetic-key';
const APP_TIMEOUT_MS = 45_000;
const TURN_TIMEOUT_MS = 90_000;
const TOOL_TURN_TIMEOUT_MS = 60_000;

function parseArgs(argv) {
  const options = {
    app: undefined,
    resultsDir: join(SCRIPT_DIR, 'results'),
    keepRoot: false,
    guiHandoff: false,
    handoffTimeoutSeconds: 600,
    selfTest: false,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === '--help' || arg === '-h') {
      options.help = true;
    } else if (arg === '--app' || arg === '--results-dir') {
      const value = argv[index + 1];
      if (!value || value.startsWith('--')) throw new Error(arg + ' requires a path');
      options[arg === '--app' ? 'app' : 'resultsDir'] = resolve(value);
      index += 1;
    } else if (arg === '--keep-root') {
      options.keepRoot = true;
    } else if (arg === '--gui-handoff') {
      options.guiHandoff = true;
    } else if (arg === '--self-test') {
      options.selfTest = true;
    } else if (arg === '--handoff-timeout-seconds') {
      const value = Number(argv[index + 1]);
      if (!Number.isInteger(value) || value < 60 || value > 1800) {
        throw new Error('--handoff-timeout-seconds must be an integer from 60 to 1800');
      }
      options.handoffTimeoutSeconds = value;
      index += 1;
    } else {
      throw new Error('unknown argument: ' + arg);
    }
  }
  return options;
}

function printHelp() {
  process.stdout.write(
    'Usage: node run-packaged-host-mod-smoke.mjs --app /path/to/signed/LingXi\\ Code.app [--results-dir DIR] [--keep-root]\n'
    + '       [--gui-handoff] [--handoff-timeout-seconds 600]\n'
    + '       [--self-test]\n'
    + 'Runs an isolated signed-app Host test through public window.lingxi APIs.\n'
    + 'GUI handoff keeps the tested app/profile and loopback provider alive for a bounded time.\n'
    + 'It never clicks or manipulates renderer DOM and never uses a developer Bun from PATH.\n',
  );
}

function readPlist(appPath, key) {
  return execFileSync('/usr/libexec/PlistBuddy', [
    '-c', 'Print :' + key, join(appPath, 'Contents', 'Info.plist'),
  ], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim();
}

function verifySignedApp(appPath) {
  const canonical = realpathSync(appPath);
  assert.ok(canonical.endsWith('.app'), 'app path must name a .app bundle');
  assert.ok(statSync(canonical).isDirectory(), 'app bundle must be a directory');
  execFileSync('/usr/bin/codesign', ['--verify', '--deep', '--strict', canonical], {
    encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'],
  });
  const executableName = readPlist(canonical, 'CFBundleExecutable');
  const executable = join(canonical, 'Contents', 'MacOS', executableName);
  assert.ok(existsSync(executable), 'signed app executable is missing');
  return {
    appPath: canonical,
    executable,
    bundleId: readPlist(canonical, 'CFBundleIdentifier'),
    version: readPlist(canonical, 'CFBundleShortVersionString'),
    build: readPlist(canonical, 'CFBundleVersion'),
    executableName,
  };
}

function packagedBunIdentity(appPath) {
  const resources = join(appPath, 'Contents', 'Resources');
  const manifest = bunHelpers.verifyModBun(resources, 'darwin', process.arch);
  return {
    executable: join(resources, 'bin', 'bun'),
    version: manifest.version,
    revision: manifest.revision,
    sha256: manifest.sha256,
  };
}

function isolatedEnvironment(home, temporaryDirectory) {
  const names = [
    'HOME', 'TMPDIR', 'TMP', 'TEMP', 'LANG', 'LC_ALL', 'LC_CTYPE',
    'USER', 'LOGNAME', 'SHELL', 'SSL_CERT_FILE', 'SSL_CERT_DIR', 'NIX_SSL_CERT_FILE',
  ];
  const environment = {};
  for (const name of names) {
    if (process.env[name] !== undefined) environment[name] = process.env[name];
  }
  Object.assign(environment, {
    HOME: home,
    TMPDIR: temporaryDirectory,
    TMP: temporaryDirectory,
    TEMP: temporaryDirectory,
    PATH: SAFE_PATH,
  });
  // Product's packaged main process explicitly injects Contents/Resources/bin/bun
  // into the bridge environment. Remove developer overrides so no PATH fallback
  // can accidentally make this test pass with another Bun.
  delete environment.LINGXI_MOD_BUN_EXECUTABLE;
  delete environment.LINGXI_CONFIG_DIR;
  delete environment.LINGXI_PLUGIN_CACHE_DIR;
  return environment;
}

function writeJson(path, value) {
  mkdirSync(dirname(path), { recursive: true, mode: 0o700 });
  writeFileSync(path, JSON.stringify(value, null, 2) + '\n', { mode: 0o600 });
}

function atomicWriteJson(path, value) {
  const temporaryPath = path + '.tmp-' + process.pid + '-' + Date.now()
    + '-' + Math.random().toString(36).slice(2);
  writeJson(temporaryPath, value);
  renameSync(temporaryPath, path);
}

function createWorkspace(root) {
  const workspace = join(root, 'workspace');
  mkdirSync(workspace, { recursive: true, mode: 0o700 });
  writeFileSync(join(workspace, 'README.txt'), 'isolated packaged Host acceptance workspace\n', {
    mode: 0o600,
  });
  writeFileSync(join(workspace, 'tool-probe.txt'), TOOL_READ_MARKER + '\n', { mode: 0o600 });
  return workspace;
}

function installFixture(configHome) {
  const pluginRoot = join(
    configHome, 'plugins', 'cache', 'acceptance', PLUGIN_NAME, PLUGIN_VERSION,
  );
  mkdirSync(dirname(pluginRoot), { recursive: true, mode: 0o700 });
  cpSync(FIXTURE_ROOT, pluginRoot, { recursive: true, errorOnExist: true, force: false });
  const uiPluginRoot = join(
    configHome, 'plugins', 'cache', 'acceptance', UI_PLUGIN_NAME, UI_PLUGIN_VERSION,
  );
  mkdirSync(dirname(uiPluginRoot), { recursive: true, mode: 0o700 });
  cpSync(UI_FIXTURE_ROOT, uiPluginRoot, { recursive: true, errorOnExist: true, force: false });
  // Current loader selects exact installed records; cache presence alone is
  // not an installation. Both fixtures are installed in this isolated user scope.
  const installedAt = '2026-10-17T00:00:00.000Z';
  writeJson(join(configHome, 'plugins', 'installed_plugins.json'), {
    version: 2,
    plugins: {
      [PLUGIN_ID]: [{ scope: 'user', version: PLUGIN_VERSION,
        installPath: pluginRoot, installedAt, lastUpdated: installedAt }],
      [UI_PLUGIN_ID]: [{ scope: 'user', version: UI_PLUGIN_VERSION,
        installPath: uiPluginRoot, installedAt, lastUpdated: installedAt }],
    },
  });
  const settingsPath = join(configHome, 'settings.json');
  writeJson(settingsPath, {
    enabledPlugins: {
      [PLUGIN_ID]: true,
      [UI_PLUGIN_ID]: true,
    },
  });
  return { pluginRoot, uiPluginRoot, settingsPath };
}

function changePluginEnabled(settingsPath, enabled, pluginId = PLUGIN_ID) {
  const settings = JSON.parse(readFileSync(settingsPath, 'utf8'));
  settings.enabledPlugins = { ...(settings.enabledPlugins ?? {}), [pluginId]: enabled };
  atomicWriteJson(settingsPath, settings);
}

function reserveLoopbackPort() {
  return new Promise((resolvePromise, rejectPromise) => {
    const server = createServer();
    server.once('error', rejectPromise);
    server.listen(0, '127.0.0.1', () => {
      const address = server.address();
      if (!address || typeof address === 'string') {
        server.close();
        rejectPromise(new Error('could not reserve a loopback TCP port'));
        return;
      }
      server.close((error) => error ? rejectPromise(error) : resolvePromise(address.port));
    });
  });
}

function textFromContent(content) {
  if (typeof content === 'string') return content;
  if (!Array.isArray(content)) return '';
  return content.map((part) => {
    if (typeof part === 'string') return part;
    if (!part || typeof part !== 'object') return '';
    return typeof part.text === 'string' ? part.text : '';
  }).join('');
}

function lastUserText(body) {
  const messages = Array.isArray(body?.messages) ? body.messages : [];
  const message = [...messages].reverse().find((item) => item?.role === 'user');
  return message ? textFromContent(message.content) : '';
}

function advertisedFunctions(body) {
  return Array.isArray(body?.tools)
    ? body.tools.filter((tool) => tool?.type === 'function' && typeof tool.function?.name === 'string')
    : [];
}

function advertisedFunction(body, name) {
  return advertisedFunctions(body).find((tool) => tool.function.name === name)?.function;
}

function advertisedSchemaEvidence(name, definition) {
  const schema = definition?.parameters;
  assert.equal(schema?.type, 'object', name + ' must advertise an object input schema');
  return {
    name,
    required: Array.isArray(schema.required) ? schema.required : [],
    properties: Object.keys(schema.properties ?? {}).sort(),
  };
}

function argumentsFromAdvertisedSchema(name, definition, argumentName, value) {
  const evidence = advertisedSchemaEvidence(name, definition);
  const schema = definition.parameters;
  const property = schema.properties?.[argumentName];
  assert.equal(property?.type, 'string', name + ' schema must declare ' + argumentName + ' as a string');
  assert.ok(evidence.required.includes(argumentName), name + ' schema must require ' + argumentName);
  const args = { [argumentName]: value };
  const missingRequired = evidence.required.filter((key) => !Object.hasOwn(args, key));
  assert.deepEqual(missingRequired, [], name + ' schema has unsupported required arguments');
  for (const key of Object.keys(args)) {
    assert.ok(Object.hasOwn(schema.properties ?? {}, key), name + ' arguments must come from its advertised schema');
  }
  return { args, schema: evidence };
}

function toolReplyFor(messages, callId) {
  const matches = (Array.isArray(messages) ? messages : []).filter((message) =>
    message?.role === 'tool' && message.tool_call_id === callId);
  assert.equal(matches.length, 1, 'provider follow-up must contain exactly one tool reply for ' + callId);
  return textFromContent(matches[0].content);
}

function newToolProbeState() {
  return {
    phase: 'start',
    route: null,
    requestCount: 0,
    searchCallId: null,
    readCallId: null,
    toolSearchResultHadReadSchema: false,
    readResultContainsMarker: false,
  };
}

function isToolProbePrompt(text) {
  return text.includes(TOOL_PROBE_MARKER);
}

function isToolProbeBody(body, state) {
  return isToolProbePrompt(lastUserText(body))
    || (['await-read-schema', 'await-read-result'].includes(state?.phase)
      && Array.isArray(body?.messages) && body.messages.some(message =>
      message?.role === 'tool'
      && [TOOL_READ_CALL_ID, TOOL_SEARCH_CALL_ID].includes(message.tool_call_id)));
}

/** Pure request-to-tool selection: every emitted tool and argument is backed by this request's schema. */
function nextToolProbeAction(body, state, filePath) {
  state.requestCount += 1;
  assert.ok(state.requestCount <= 3, 'tool probe exceeded the three-request search/read/final cap');
  const messages = Array.isArray(body?.messages) ? body.messages : [];

  if (state.phase === 'start') {
    const read = advertisedFunction(body, 'Read');
    if (read) {
      state.route = 'direct-read';
      state.readCallId = TOOL_READ_CALL_ID;
      state.phase = 'await-read-result';
      const input = argumentsFromAdvertisedSchema('Read', read, 'file_path', filePath);
      return { type: 'tool_call', toolName: 'Read', callId: state.readCallId, ...input };
    }

    const search = advertisedFunction(body, 'ToolSearch');
    assert.ok(search, 'Read is not advertised and the request advertises no ToolSearch fallback');
    state.route = 'tool-search-read';
    state.searchCallId = TOOL_SEARCH_CALL_ID;
    state.phase = 'await-read-schema';
    const input = argumentsFromAdvertisedSchema('ToolSearch', search, 'query', 'select:Read');
    return { type: 'tool_call', toolName: 'ToolSearch', callId: state.searchCallId, ...input };
  }

  if (state.phase === 'await-read-schema') {
    const searchResult = toolReplyFor(messages, state.searchCallId);
    assert.ok(searchResult.includes('Read') && searchResult.includes('file_path'),
      'ToolSearch reply must return Read with its full file_path schema');
    state.toolSearchResultHadReadSchema = true;
    const read = advertisedFunction(body, 'Read');
    assert.ok(read, 'Read schema must be advertised after the actual ToolSearch result');
    state.readCallId = TOOL_READ_CALL_ID;
    state.phase = 'await-read-result';
    const input = argumentsFromAdvertisedSchema('Read', read, 'file_path', filePath);
    return { type: 'tool_call', toolName: 'Read', callId: state.readCallId, ...input };
  }

  if (state.phase === 'await-read-result') {
    const readResult = toolReplyFor(messages, state.readCallId);
    state.readResultContainsMarker = readResult.includes(TOOL_READ_MARKER);
    assert.ok(state.readResultContainsMarker, 'actual Read tool result must contain the isolated file marker');
    state.phase = 'complete';
    return { type: 'text', text: TOOL_FINAL_MARKER };
  }

  throw new Error('tool probe received an unexpected extra provider request after completion');
}

function runToolProbeContractSelfTest() {
  const toolSearchSchema = {
    type: 'object', properties: { query: { type: 'string' }, max_results: { type: 'number' } }, required: ['query'],
  };
  const readSchema = {
    type: 'object', properties: { file_path: { type: 'string' }, offset: { type: 'integer' } }, required: ['file_path'],
  };
  const functionTool = (name, parameters) => ({ type: 'function', function: { name, parameters } });
  const filePath = '/tmp/isolated-tool-probe/tool-probe.txt';

  const directState = newToolProbeState();
  const directCall = nextToolProbeAction({ tools: [functionTool('Read', readSchema)], messages: [] }, directState, filePath);
  assert.equal(directCall.toolName, 'Read');
  assert.deepEqual(directCall.args, { file_path: filePath });
  const directFinal = nextToolProbeAction({
    tools: [functionTool('Read', readSchema)],
    messages: [{ role: 'tool', tool_call_id: TOOL_READ_CALL_ID, content: TOOL_READ_MARKER }],
  }, directState, filePath);
  assert.equal(directFinal.text, TOOL_FINAL_MARKER);
  assert.equal(directState.readResultContainsMarker, true);

  const deferredState = newToolProbeState();
  const searchCall = nextToolProbeAction({ tools: [functionTool('ToolSearch', toolSearchSchema)], messages: [] }, deferredState, filePath);
  assert.equal(searchCall.toolName, 'ToolSearch');
  assert.deepEqual(searchCall.args, { query: 'select:Read' });
  const readCall = nextToolProbeAction({
    tools: [functionTool('ToolSearch', toolSearchSchema), functionTool('Read', readSchema)],
    messages: [{ role: 'tool', tool_call_id: TOOL_SEARCH_CALL_ID, content: 'Read schema: {"file_path":{"type":"string"}}' }],
  }, deferredState, filePath);
  assert.equal(readCall.toolName, 'Read');
  assert.deepEqual(readCall.args, { file_path: filePath });
  const deferredFinal = nextToolProbeAction({
    tools: [functionTool('Read', readSchema)],
    messages: [{ role: 'tool', tool_call_id: TOOL_READ_CALL_ID, content: TOOL_READ_MARKER }],
  }, deferredState, filePath);
  assert.equal(deferredFinal.text, TOOL_FINAL_MARKER);
  assert.equal(deferredState.toolSearchResultHadReadSchema, true);
  assert.equal(deferredState.readResultContainsMarker, true);
  assert.throws(
    () => nextToolProbeAction({ tools: [], messages: [] }, newToolProbeState(), filePath),
    /no ToolSearch fallback/,
    'missing advertised Read and ToolSearch must fail instead of inventing a tool',
  );
  process.stdout.write('tool probe direct Read and advertised ToolSearch -> Read contract checks passed\n');
}

async function readRequestBody(request) {
  const chunks = [];
  let bytes = 0;
  for await (const chunk of request) {
    bytes += chunk.byteLength;
    if (bytes > 8 * 1024 * 1024) throw new Error('provider request exceeded 8 MiB');
    chunks.push(chunk);
  }
  return Buffer.concat(chunks).toString('utf8');
}

function lastPhysicalLine(text) {
  return text.slice(text.lastIndexOf('\n') + 1);
}

function publicProviderRequest(request) {
  const retainedMarkerTexts = new Set([MARKER, ACTIVE_PREFIX + MARKER]);
  return {
    path: request.path,
    method: request.method,
    authorized: request.authorized,
    model: request.model,
    lastUserTextSha256: createHash('sha256').update(request.lastUserText).digest('hex'),
    lastUserMarkerLine: retainedMarkerTexts.has(lastPhysicalLine(request.lastUserText))
      ? lastPhysicalLine(request.lastUserText) : '[redacted non-marker line]',
    lastUserText: request.toolProbeRequest
      ? TOOL_PROBE_MARKER
      : retainedMarkerTexts.has(request.lastUserText)
        ? request.lastUserText
        : '[redacted non-marker prompt]',
    ...(request.toolProbeRequest ? {
      advertisedToolNames: request.advertisedToolNames,
      toolCallName: request.toolCallName,
      toolCallArgumentNames: request.toolCallArgumentNames,
      toolSchemaEvidence: request.toolSchemaEvidence,
      toolSearchResultHadReadSchema: request.toolSearchResultHadReadSchema,
      toolResultContainsMarker: request.toolResultContainsMarker,
    } : {}),
  };
}

function makeFakeProvider(capturePath, toolProbe) {
  const requests = [];
  const server = createServer(async (request, response) => {
    let isToolProbeRequest = false;
    try {
      if (request.method === 'GET' && request.url?.includes('/models')) {
        response.writeHead(200, { 'Content-Type': 'application/json' }).end(JSON.stringify({
          object: 'list',
          data: [{ id: 'smoke-model', object: 'model', owned_by: 'acceptance' }],
        }));
        return;
      }
      const rawBody = await readRequestBody(request);
      let body;
      try { body = JSON.parse(rawBody); } catch { body = null; }
      const authorized = request.headers.authorization === 'Bearer ' + PROVIDER_KEY;
      const record = {
        path: request.url ?? '',
        method: request.method ?? '',
        authorized,
        model: typeof body?.model === 'string' ? body.model : '',
        lastUserText: lastUserText(body),
      };
      isToolProbeRequest = isToolProbeBody(body, toolProbe?.state);
      if (isToolProbeRequest) {
        assert.ok(toolProbe, 'isolated Read tool probe was not configured');
        if (toolProbe.state.phase === 'start') {
        assert.ok(record.lastUserText.includes(toolProbe.relativePath), 'tool probe prompt must name the isolated marker file');
        }
        record.toolProbeRequest = true;
        record.advertisedToolNames = advertisedFunctions(body).map((tool) => tool.function.name);
      }
      requests.push(record);
      if (!authorized) {
        if (isToolProbeRequest && toolProbe) toolProbe.failure = 'tool probe request did not use the isolated synthetic credential';
        if (capturePath) atomicWriteJson(capturePath, requests.map(publicProviderRequest));
        response.writeHead(401, { 'Content-Type': 'application/json' })
          .end(JSON.stringify({ error: { message: 'synthetic credential rejected' } }));
        return;
      }
      if (!body || !Array.isArray(body.messages)) {
        if (isToolProbeRequest && toolProbe) toolProbe.failure = 'tool probe request had no chat messages';
        response.writeHead(400, { 'Content-Type': 'application/json' })
          .end(JSON.stringify({ error: { message: 'expected a chat completion body' } }));
        return;
      }
      if (isToolProbeRequest) {
        const action = nextToolProbeAction(body, toolProbe.state, toolProbe.filePath);
        record.toolSearchResultHadReadSchema = toolProbe.state.toolSearchResultHadReadSchema;
        if (action.type === 'tool_call') {
          record.toolCallName = action.toolName;
          record.toolCallArgumentNames = Object.keys(action.args).sort();
          record.toolSchemaEvidence = action.schema;
          toolProbe.calls.push({
            toolName: action.toolName,
            callId: action.callId,
            arguments: action.args,
            schema: action.schema,
          });
          if (capturePath) atomicWriteJson(capturePath, requests.map(publicProviderRequest));
          const completionId = 'chatcmpl-packaged-tool-probe';
          const created = Math.floor(Date.now() / 1000);
          if (body.stream === false) {
            response.writeHead(200, { 'Content-Type': 'application/json' }).end(JSON.stringify({
              id: completionId,
              object: 'chat.completion',
              created,
              model: 'smoke-model',
              choices: [{
                index: 0,
                message: {
                  role: 'assistant',
                  content: null,
                  tool_calls: [{
                    id: action.callId,
                    type: 'function',
                    function: { name: action.toolName, arguments: JSON.stringify(action.args) },
                  }],
                },
                finish_reason: 'tool_calls',
              }],
              usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
            }));
            return;
          }
          response.writeHead(200, {
            'Content-Type': 'text/event-stream',
            'Cache-Control': 'no-cache',
            Connection: 'keep-alive',
          });
          response.write('data: ' + JSON.stringify({
            id: completionId,
            object: 'chat.completion.chunk',
            created,
            model: 'smoke-model',
            choices: [{
              index: 0,
              delta: {
                role: 'assistant',
                tool_calls: [{
                  index: 0,
                  id: action.callId,
                  type: 'function',
                  function: { name: action.toolName, arguments: JSON.stringify(action.args) },
                }],
              },
              finish_reason: null,
            }],
          }) + '\n\n');
          response.write('data: ' + JSON.stringify({
            id: completionId,
            object: 'chat.completion.chunk',
            created,
            model: 'smoke-model',
            choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }],
            usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
          }) + '\n\n');
          response.end('data: [DONE]\n\n');
          return;
        }

        assert.equal(action.type, 'text');
        record.toolResultContainsMarker = toolProbe.state.readResultContainsMarker;
        if (capturePath) atomicWriteJson(capturePath, requests.map(publicProviderRequest));
        const completionId = 'chatcmpl-packaged-tool-probe-final';
        const created = Math.floor(Date.now() / 1000);
        if (body.stream === false) {
          response.writeHead(200, { 'Content-Type': 'application/json' }).end(JSON.stringify({
            id: completionId,
            object: 'chat.completion',
            created,
            model: 'smoke-model',
            choices: [{ index: 0, message: { role: 'assistant', content: action.text }, finish_reason: 'stop' }],
            usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
          }));
        } else {
          response.writeHead(200, {
            'Content-Type': 'text/event-stream',
            'Cache-Control': 'no-cache',
            Connection: 'keep-alive',
          });
          response.write('data: ' + JSON.stringify({
            id: completionId,
            object: 'chat.completion.chunk',
            created,
            model: 'smoke-model',
            choices: [{ index: 0, delta: { role: 'assistant', content: action.text }, finish_reason: null }],
          }) + '\n\n');
          response.write('data: ' + JSON.stringify({
            id: completionId,
            object: 'chat.completion.chunk',
            created,
            model: 'smoke-model',
            choices: [{ index: 0, delta: {}, finish_reason: 'stop' }],
            usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
          }) + '\n\n');
          response.end('data: [DONE]\n\n');
        }
        toolProbe.completed = true;
        return;
      }

      if (capturePath) atomicWriteJson(capturePath, requests.map(publicProviderRequest));
      const completion = {
        id: 'chatcmpl-packaged-mod-smoke',
        object: 'chat.completion',
        created: Math.floor(Date.now() / 1000),
        model: 'smoke-model',
        choices: [{
          index: 0,
          message: { role: 'assistant', content: 'PACKAGED_MOD_FAKE_REPLY' },
          finish_reason: 'stop',
        }],
        usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
      };
      if (body.stream === false) {
        response.writeHead(200, { 'Content-Type': 'application/json' }).end(JSON.stringify(completion));
        return;
      }
      response.writeHead(200, {
        'Content-Type': 'text/event-stream',
        'Cache-Control': 'no-cache',
        Connection: 'keep-alive',
      });
      const chunk = {
        id: completion.id,
        object: 'chat.completion.chunk',
        created: completion.created,
        model: completion.model,
        choices: [{ index: 0, delta: { role: 'assistant', content: 'PACKAGED_MOD_FAKE_REPLY' }, finish_reason: null }],
      };
      response.write('data: ' + JSON.stringify(chunk) + '\n\n');
      response.write('data: ' + JSON.stringify({
        id: completion.id,
        object: 'chat.completion.chunk',
        created: completion.created,
        model: completion.model,
        choices: [{ index: 0, delta: {}, finish_reason: 'stop' }],
        usage: completion.usage,
      }) + '\n\n');
      response.end('data: [DONE]\n\n');
    } catch (error) {
      if (isToolProbeRequest && toolProbe) {
        toolProbe.failure = error instanceof Error ? error.message : String(error);
        if (capturePath) atomicWriteJson(capturePath, requests.map(publicProviderRequest));
      }
      if (!response.headersSent) response.writeHead(isToolProbeRequest ? 400 : 500, { 'Content-Type': 'application/json' });
      response.end(JSON.stringify({ error: { message: isToolProbeRequest ? 'acceptance tool probe failed' : 'synthetic provider failure' } }));
    }
  });
  return {
    requests,
    toolProbe,
    async start() {
      await new Promise((resolvePromise, rejectPromise) => {
        server.once('error', rejectPromise);
        server.listen(0, '127.0.0.1', resolvePromise);
      });
      return 'http://127.0.0.1:' + server.address().port + '/v1';
    },
    async close() {
      await new Promise((resolvePromise) => {
        server.close(resolvePromise);
        server.closeAllConnections();
      });
    },
  };
}

async function waitFor(read, { timeoutMs = 20_000, intervalMs = 200, label = 'condition' } = {}) {
  const deadline = Date.now() + timeoutMs;
  let lastError;
  while (Date.now() < deadline) {
    try {
      const value = await read();
      if (value !== undefined && value !== null && value !== false) return value;
    } catch (error) {
      lastError = error;
    }
    await delay(intervalMs);
  }
  throw new Error('timed out waiting for ' + label + (lastError ? ': ' + lastError.message : ''));
}

class CdpConnection {
  constructor(socket) {
    this.socket = socket;
    this.nextId = 1;
    this.pending = new Map();
    socket.addEventListener('message', (event) => {
      let message;
      try { message = JSON.parse(String(event.data)); } catch { return; }
      if (!Number.isSafeInteger(message.id)) return;
      const pending = this.pending.get(message.id);
      if (!pending) return;
      this.pending.delete(message.id);
      clearTimeout(pending.timer);
      if (message.error) pending.reject(new Error(message.error.message ?? 'CDP request failed'));
      else pending.resolve(message.result);
    });
    socket.addEventListener('close', () => {
      for (const pending of this.pending.values()) {
        clearTimeout(pending.timer);
        pending.reject(new Error('CDP connection closed'));
      }
      this.pending.clear();
    });
  }

  send(method, params = {}) {
    const id = this.nextId++;
    return new Promise((resolvePromise, rejectPromise) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        rejectPromise(new Error('CDP request timed out: ' + method));
      }, 15_000);
      this.pending.set(id, { resolve: resolvePromise, reject: rejectPromise, timer });
      this.socket.send(JSON.stringify({ id, method, params }));
    });
  }

  async evaluate(expression) {
    const response = await this.send('Runtime.evaluate', {
      expression,
      awaitPromise: true,
      returnByValue: true,
      userGesture: true,
    });
    if (response.exceptionDetails) {
      const message = response.exceptionDetails.exception?.description
        ?? response.exceptionDetails.text
        ?? 'renderer evaluation failed';
      throw new Error(message);
    }
    return response.result && Object.hasOwn(response.result, 'value')
      ? response.result.value
      : undefined;
  }

  close() {
    if (this.socket.readyState === WebSocket.CLOSED) return Promise.resolve();
    return new Promise((resolvePromise) => {
      this.socket.addEventListener('close', resolvePromise, { once: true });
      this.socket.close();
      setTimeout(resolvePromise, 1_000).unref?.();
    });
  }
}

async function connectToPackagedPage(port, child) {
  if (typeof WebSocket !== 'function') {
    throw new Error('the Node runtime used for this driver must provide global WebSocket');
  }
  const target = await waitFor(async () => {
    if (child.exitCode !== null || child.signalCode !== null) {
      throw new Error('signed app exited before exposing its loopback DevTools endpoint');
    }
    const response = await fetch('http://127.0.0.1:' + port + '/json/list', {
      signal: AbortSignal.timeout(2_000),
    });
    if (!response.ok) return undefined;
    const targets = await response.json();
    return targets.find((item) => item.type === 'page'
      && typeof item.webSocketDebuggerUrl === 'string'
      && item.url?.startsWith('file://'));
  }, { timeoutMs: APP_TIMEOUT_MS, label: 'packaged renderer DevTools target' });
  const socketUrl = new URL(target.webSocketDebuggerUrl);
  assert.ok(
    ['127.0.0.1', 'localhost', '[::1]', '::1'].includes(socketUrl.hostname),
    'DevTools websocket must remain loopback-only',
  );
  socketUrl.hostname = '127.0.0.1';
  const socket = new WebSocket(socketUrl);
  try {
    await new Promise((resolvePromise, rejectPromise) => {
      let settled = false;
      const finish = (error) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        socket.removeEventListener('open', onOpen);
        socket.removeEventListener('error', onError);
        socket.removeEventListener('close', onClose);
        if (error) rejectPromise(error);
        else resolvePromise();
      };
      const onOpen = () => finish();
      const onError = () => finish(new Error('cannot connect to local DevTools'));
      const onClose = () => finish(new Error('local DevTools websocket closed before opening'));
      const timer = setTimeout(
        () => finish(new Error('timed out opening local DevTools websocket')),
        APP_TIMEOUT_MS,
      );
      socket.addEventListener('open', onOpen, { once: true });
      socket.addEventListener('error', onError, { once: true });
      socket.addEventListener('close', onClose, { once: true });
      if (socket.readyState === WebSocket.OPEN) finish();
    });
  } catch (error) {
    try { socket.close(); } catch {}
    throw error;
  }
  const cdp = new CdpConnection(socket);
  try {
    await cdp.send('Page.enable');
    await cdp.send('Runtime.enable');
    return cdp;
  } catch (error) {
    await cdp.close().catch(() => {});
    throw error;
  }
}

function launchPackagedApp(app, environment, userDataDir, cdpPort) {
  const executable = app.executable;
  const child = spawn(executable, [
    '--user-data-dir=' + userDataDir,
    '--remote-debugging-address=127.0.0.1',
    '--remote-debugging-port=' + cdpPort,
  ], {
    cwd: dirname(app.appPath),
    env: environment,
    detached: true,
    stdio: 'ignore',
  });
  return child;
}

function childHasExited(child) {
  return child.exitCode !== null || child.signalCode !== null;
}

function assertOwnedProcessGroupId(pgid) {
  if (!Number.isSafeInteger(pgid) || pgid <= 1) {
    throw new Error('refusing to signal an invalid driver-owned process group');
  }
}

function ownedProcessGroupExists(pgid) {
  assertOwnedProcessGroupId(pgid);
  try {
    process.kill(-pgid, 0);
    return true;
  } catch (error) {
    if (error?.code === 'ESRCH') return false;
    if (error?.code === 'EPERM') return true;
    throw error;
  }
}

function signalOwnedProcessGroup(pgid, signal) {
  if (!ownedProcessGroupExists(pgid)) return false;
  try {
    process.kill(-pgid, signal);
    return true;
  } catch (error) {
    if (error?.code === 'ESRCH') return false;
    throw error;
  }
}

function waitForChildExit(child, timeoutMs) {
  if (childHasExited(child)) return Promise.resolve(true);
  return new Promise((resolvePromise) => {
    let settled = false;
    const finish = (exited) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      child.removeListener('exit', onExit);
      resolvePromise(exited);
    };
    const onExit = () => finish(true);
    const timer = setTimeout(() => finish(false), timeoutMs);
    child.once('exit', onExit);
    if (childHasExited(child)) finish(true);
  });
}

async function waitForOwnedProcessGroupExit(pgid, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (ownedProcessGroupExists(pgid)) {
    if (Date.now() >= deadline) return false;
    await delay(Math.min(100, deadline - Date.now()));
  }
  return true;
}

async function terminateOwnedApp(child, cdp) {
  if (!child) {
    await cdp?.close().catch(() => {});
    return;
  }
  try {
    try { await cdp?.send('Browser.close'); } catch {}

    const pgid = child.pid;
    if (!Number.isSafeInteger(pgid) || pgid <= 1) {
      if (!childHasExited(child)) {
        throw new Error('cannot clean up the driver-owned app without its process group id');
      }
      return;
    }

    if (!childHasExited(child)) await waitForChildExit(child, 10_000);

    // The app leader may have exited while detached helper processes remain.
    // Always inspect and reap only the process group created by this driver.
    if (!ownedProcessGroupExists(pgid)) return;
    signalOwnedProcessGroup(pgid, 'SIGTERM');
    if (await waitForOwnedProcessGroupExit(pgid, 5_000)) return;

    signalOwnedProcessGroup(pgid, 'SIGKILL');
    if (!await waitForOwnedProcessGroupExit(pgid, 3_000)) {
      throw new Error('driver-owned app process group remained after bounded SIGKILL cleanup');
    }
  } finally {
    await cdp?.close().catch(() => {});
  }
}

function waitForGuiHandoffStop(stopPath, timeoutMs) {
  return new Promise((resolvePromise) => {
    let settled = false;
    const finish = (reason) => {
      if (settled) return;
      settled = true;
      clearInterval(poll);
      clearTimeout(timeout);
      process.removeListener('SIGINT', onSigint);
      process.removeListener('SIGTERM', onSigterm);
      resolvePromise(reason);
    };
    const onSigint = () => finish('SIGINT');
    const onSigterm = () => finish('SIGTERM');
    const poll = setInterval(() => {
      if (existsSync(stopPath)) finish('control-file');
    }, 300);
    const timeout = setTimeout(() => finish('timeout'), timeoutMs);
    process.once('SIGINT', onSigint);
    process.once('SIGTERM', onSigterm);
  });
}

function expressionCall(method, ...args) {
  return 'window.lingxi.' + method + '(' + args.map((arg) => JSON.stringify(arg)).join(',') + ')';
}

async function bootstrapReady(cdp) {
  await waitFor(async () => {
    const value = await cdp.evaluate('window.lingxi?.bootstrap()');
    return value?.connection?.status === 'connected' && value?.workspace?.trusted === true
      ? value
      : undefined;
  }, { timeoutMs: APP_TIMEOUT_MS, label: 'trusted packaged bridge bootstrap' });
}

async function installEventCollector(cdp, sessionId) {
  const sessionLiteral = JSON.stringify(sessionId);
  await cdp.evaluate(
    '(() => {'
    + ' window.__packagedHostProbe?.unsubscribe?.();'
    + ' const state = {events: [], foreignEvents: [], unsubscribe: null};'
    + ' state.unsubscribe = window.lingxi.onEvent(envelope => {'
    + '   if (!envelope?.event) return;'
    + '   if (envelope.sessionId === ' + sessionLiteral + ') state.events.push(envelope.event);'
    + '   else state.foreignEvents.push({sessionId: envelope.sessionId, type: envelope.event.type});'
    + ' });'
    + ' window.__packagedHostProbe = state;'
    + ' return true;'
    + '})()',
  );
}

async function clearCollectedEvents(cdp) {
  await cdp.evaluate('window.__packagedHostProbe.events.length = 0');
}

async function waitForTurn(cdp, timeoutMs = TURN_TIMEOUT_MS) {
  const events = await waitFor(
    () => cdp.evaluate('window.__packagedHostProbe.events.some(event => event.type === "turn_ended" || event.type === "error"'
      + ' || (event.type === "system_notice" && event.is_error === true))'
      + ' ? window.__packagedHostProbe.events : undefined'),
    { timeoutMs, label: 'packaged Host prompt turn completion' },
  );
  assert.equal(
    events.some((event) => event.type === 'error'
      || (event.type === 'system_notice' && event.is_error === true)),
    false,
    'marker prompt reported a runtime error',
  );
  const ended = events.find((event) => event.type === 'turn_ended');
  assert.ok(ended, 'marker turn must emit turn_ended');
  if (ended.outcome?.type !== undefined) {
    assert.equal(ended.outcome.type, 'end_turn', 'marker turn must finish normally');
  }
}

async function ensureCustomProvider(cdp, probe, sessionId, restore = false, onCredentialStored = () => {}) {
  await installEventCollector(cdp, sessionId);
  if (!restore) {
    const patch = { providers: { [probe.providerId]: {
      type: 'openai',
      baseUrl: probe.baseUrl,
      models: [{ id: 'smoke-model' }],
    } } };
    await cdp.evaluate(expressionCall('command', sessionId, {
      type: 'update_settings',
      destination: 'user',
      patch_json: JSON.stringify(patch),
    }));
  }
  await cdp.evaluate(expressionCall('command', sessionId, {
    type: 'refresh_listings',
    which: [{ type: 'settings' }],
  }));
  await waitFor(
    () => cdp.evaluate(
      'window.__packagedHostProbe.events.some(event => event.type === "settings_snapshot"'
      + ' && JSON.parse(event.effective_json).providers?.[' + JSON.stringify(probe.providerId) + '])',
    ),
    { label: 'isolated custom provider settings snapshot' },
  );
  if (!restore) {
    onCredentialStored();
    const saved = await cdp.evaluate(expressionCall('setProviderCredential', probe.providerId, PROVIDER_KEY));
    assert.equal(saved?.credential?.configured, true, 'synthetic provider credential should be stored');
    assert.equal(saved?.credential?.encryptionAvailable, true, 'credential storage must stay encrypted');
  }
  const metadata = await cdp.evaluate(expressionCall('providerCredentials', probe.providerId));
  const credential = metadata.find((entry) => entry.providerId === probe.providerId);
  assert.equal(credential?.configured, true, 'provider credential must survive the app restart');
  assert.equal(credential?.encryptionAvailable, true);
  assert.equal(JSON.stringify(metadata).includes(PROVIDER_KEY), false);
  // Provider routes and the trusted credential handoff are launch inputs.
  // Saving settings/credentials is not proof that an existing runtime has
  // rebuilt its provider catalog. Use the public runtime restart before this
  // Mods acceptance flow selects the newly configured provider.
  if (!restore) {
    await cdp.evaluate(expressionCall('restartBridge', sessionId));
    await installEventCollector(cdp, sessionId);
  }
  await cdp.evaluate(expressionCall('command', sessionId, { type: 'list_models' }));
  await waitFor(
    () => cdp.evaluate(
      'window.__packagedHostProbe.events.some(event => event.type === "model_list"'
      + ' && event.models.includes(' + JSON.stringify(probe.providerId + '/smoke-model') + '))',
    ),
    { label: 'boot-loaded custom provider model route' },
  );
  await cdp.evaluate(expressionCall('command', sessionId, {
    type: 'set_model',
    model: probe.providerId + '/smoke-model',
  }));
}

async function sendMarkerTurn(cdp, provider, sessionId, expectedText, stage) {
  await clearCollectedEvents(cdp);
  const before = provider.requests.length;
  await cdp.evaluate(expressionCall('sendPrompt', sessionId, MARKER));
  const request = await waitFor(
    () => provider.requests.slice(before).find((item) => item.path.includes('/chat/completions')),
    { timeoutMs: TURN_TIMEOUT_MS, label: stage + ' fake-provider request' },
  );
  assert.equal(request.authorized, true, stage + ' request must use the synthetic credential');
  assert.equal(request.model, 'smoke-model');
  assert.equal(lastPhysicalLine(request.lastUserText), expectedText,
    stage + ' must expose the exact terminal user marker line, preserving surrounding context');
  await waitForTurn(cdp);
  return {
    stage,
    requestPath: request.path,
    authorized: request.authorized,
    model: request.model,
    lastUserMarkerLine: lastPhysicalLine(request.lastUserText),
    lastUserTextSha256: createHash('sha256').update(request.lastUserText).digest('hex'),
    comparison: 'exact terminal physical line; functional middleware check, not full request byte parity',
  };
}

async function clearSyntheticProviderCredential(cdp, providerId) {
  assert.ok(cdp, 'the packaged Host must remain connected while clearing its Keychain credential');
  const cleared = await cdp.evaluate(expressionCall('clearProviderCredential', providerId));
  assert.equal(cleared?.configured, false, 'public clearProviderCredential must report the synthetic credential cleared');
  const metadata = await cdp.evaluate(expressionCall('providerCredentials', providerId));
  const credential = metadata.find((entry) => entry.providerId === providerId);
  assert.ok(credential, 'cleared provider credential metadata must remain inspectable');
  assert.equal(credential.configured, false, 'shared Keychain must no longer contain the synthetic provider credential');
}

async function setSessionPermissionMode(cdp, sessionId, mode) {
  await clearCollectedEvents(cdp);
  await cdp.evaluate(expressionCall('command', sessionId, {
    type: 'set_permission_mode',
    mode,
  }));
  await waitFor(
    () => cdp.evaluate('window.__packagedHostProbe.events.some(event => event.type === "permission_mode_changed"'
      + ' && event.mode === ' + JSON.stringify(mode) + ')'),
    { timeoutMs: 20_000, label: 'isolated session permission mode ' + mode },
  );
}

async function sendReadToolTurn(cdp, provider, sessionId, workspace) {
  const toolProbe = provider.toolProbe;
  assert.ok(toolProbe, 'fake provider must own a configured tool probe');
  const foreignBefore = await cdp.evaluate('window.__packagedHostProbe.foreignEvents.length');
  await setSessionPermissionMode(cdp, sessionId, 'bypassPermissions');
  try {
    await clearCollectedEvents(cdp);
    const firstRequest = provider.requests.length;
    const prompt = TOOL_PROBE_MARKER
      + '\nUse the Read tool to read ' + toolProbe.relativePath
      + ' from this trusted workspace, then return its exact marker.';
    await cdp.evaluate(expressionCall('sendPrompt', sessionId, prompt));
    await waitFor(() => {
      if (toolProbe.failure) throw new Error('fake-provider tool probe failed: ' + toolProbe.failure);
      return toolProbe.completed ? true : undefined;
    }, { timeoutMs: TOOL_TURN_TIMEOUT_MS, label: 'bounded fake-provider Read tool round trip' });
    await waitForTurn(cdp, TOOL_TURN_TIMEOUT_MS);

    const events = await cdp.evaluate('window.__packagedHostProbe.events');
    const started = events.filter((event) => event.type === 'tool_use_started');
    const results = events.filter((event) => event.type === 'tool_use_result');
    const expectedToolNames = toolProbe.calls.map((call) => call.toolName);
    assert.ok(expectedToolNames.includes('Read'), 'fake provider must eventually call advertised Read');
    assert.deepEqual(started.map((event) => event.tool), expectedToolNames,
      'Host must dispatch exactly the actual fake-provider tool call sequence');
    assert.deepEqual(results.map((event) => event.tool), expectedToolNames,
      'Host must return a result for every dispatched tool');
    assert.deepEqual(started.map((event) => event.id), toolProbe.calls.map((call) => call.callId),
      'Host tool start IDs must match the fake-provider call IDs in order');
    assert.deepEqual(results.map((event) => event.id), toolProbe.calls.map((call) => call.callId),
      'Host tool result IDs must match the fake-provider call IDs in order');
    assert.ok(results.every((event) => event.is_error === false), 'Read/search tool calls must succeed');

    const readCall = toolProbe.calls.find((call) => call.toolName === 'Read');
    const readStarted = started.find((event) => event.tool === 'Read');
    const readResult = results.find((event) => event.tool === 'Read');
    assert.ok(readCall && readStarted && readResult, 'Host must expose the actual Read call and result');
    assert.deepEqual(JSON.parse(readStarted.input_json), readCall.arguments,
      'Host must dispatch the exact schema-derived Read arguments');
    assert.equal(readCall.arguments.file_path, toolProbe.filePath,
      'Read may target only the disposable workspace marker file');
    assert.ok(readResult.result_json.includes(TOOL_READ_MARKER),
      'Host tool_use_result must contain the real marker file contents');

    const finalText = events.filter((event) => event.type === 'text_delta')
      .map((event) => event.text ?? '').join('');
    assert.ok(finalText.includes(TOOL_FINAL_MARKER), 'fake provider must finish with normal text after the tool result');
    const ended = events.find((event) => event.type === 'turn_ended');
    assert.ok(ended, 'tool probe must end through the normal Host turn event');
    assert.equal(ended.outcome?.type, 'end_turn', 'tool probe must finish normally');
    const foreignAfter = await cdp.evaluate('window.__packagedHostProbe.foreignEvents.length');
    assert.equal(foreignAfter, foreignBefore, 'tool activity must not emit events into another session');

    const activeSessionId = (await cdp.evaluate('window.lingxi.bootstrap()'))?.activeSession?.sessionId;
    assert.equal(activeSessionId, sessionId, 'tool activity must remain in the same parent session');
    const catalog = await waitFor(
      () => cdp.evaluate(expressionCall('listProjectSessions', workspace)).then(value => {
        assert.equal(value?.error, undefined, 'session catalog must not report an error');
        return value?.sessions?.some(row => row.uuid === sessionId) ? value : undefined;
      }),
      { timeoutMs: APP_TIMEOUT_MS, label: 'persisted session visible after bounded catalog cache expiry' },
    );
    assert.deepEqual(catalog?.sessions?.map((row) => row.uuid), [sessionId],
      'tool activity must not create or contaminate another session in the isolated workspace');

    const toolRequests = provider.requests.slice(firstRequest).filter((request) => request.toolProbeRequest);
    assert.equal(toolRequests.length, expectedToolNames.length + 1,
      'provider must observe each tool round and one final text request');
    assert.ok(toolRequests.every((request) => request.authorized && request.model === 'smoke-model'));
    assert.equal(toolRequests.at(-1).toolResultContainsMarker, true,
      'the provider follow-up request must contain the actual Read result');
    if (toolProbe.state.route === 'tool-search-read') {
      assert.equal(toolProbe.state.toolSearchResultHadReadSchema, true,
        'deferred Read must be discovered through the advertised ToolSearch schema');
      assert.ok(toolProbe.calls.some((call) => call.toolName === 'ToolSearch' && call.arguments.query === 'select:Read'));
    }
    return {
      stage: 'packaged-read-tool-host-roundtrip',
      file: toolProbe.relativePath,
      marker: TOOL_READ_MARKER,
      finalText,
      route: toolProbe.state.route,
      toolCalls: toolProbe.calls.map((call) => ({
        name: call.toolName,
        argumentNames: Object.keys(call.arguments).sort(),
        advertisedSchema: call.schema,
      })),
      hostToolUseEvents: started.length,
      successfulToolResultEvents: results.length,
      readResultContainsMarker: true,
      providerFollowUpContainsMarker: true,
      parentSessionId: sessionId,
      foreignSessionEvents: foreignAfter - foreignBefore,
      isolatedSessionCount: catalog.sessions.length,
      permissionModeRestored: 'default',
    };
  } finally {
    await setSessionPermissionMode(cdp, sessionId, 'default');
  }
}

async function reloadPlugins(cdp, sessionId, stage) {
  // BetaDesktop's real /reload-plugins action uses this Host operation.
  // A raw engine slash request bypasses the Desktop command dispatcher.
  await cdp.evaluate(expressionCall('restartBridge', sessionId));
  await installEventCollector(cdp, sessionId);
  const state = await cdp.evaluate('window.lingxi.bootstrap()');
  assert.equal(state?.activeSession?.sessionId, sessionId,
    stage + ' Desktop reload must restore the same owned session');
}

async function createOrRestoreSession(cdp, workspace, expectedSessionId) {
  await bootstrapReady(cdp);
  if (!expectedSessionId) {
    const result = await cdp.evaluate(
      'window.lingxi.bootstrap().then(state => window.lingxi.newSession(state.workspace.path))',
    );
    const sessionId = result?.activeSession?.sessionId;
    assert.equal(typeof sessionId, 'string', 'public newSession must create an active session');
    return sessionId;
  }
  const catalog = await waitFor(
    () => cdp.evaluate(expressionCall('listProjectSessions', workspace))
      .then((value) => value?.sessions?.some((row) => row.uuid === expectedSessionId) ? value : undefined),
    { timeoutMs: APP_TIMEOUT_MS, label: 'persisted project session catalog' },
  );
  assert.ok(catalog.sessions.some((row) => row.uuid === expectedSessionId));
  const opened = await cdp.evaluate(expressionCall('openSession', workspace, expectedSessionId));
  const activeId = opened?.activeSession?.sessionId
    ?? (await cdp.evaluate('window.lingxi.bootstrap()'))?.activeSession?.sessionId;
  assert.equal(activeId, expectedSessionId, 'public openSession must restore the same session');
  return expectedSessionId;
}

function findUiClient(tree) {
  if (Array.isArray(tree)) {
    for (const item of tree) {
      const found = findUiClient(item);
      if (found) return found;
    }
    return undefined;
  }
  if (!tree || typeof tree !== 'object') return undefined;
  if (tree.type === 'Client' && tree.props?.key === UI_CLIENT_KEY) return tree;
  return findUiClient(tree.children);
}

function assertPublicPressRefs(tree, expectedPlugin) {
  if (Array.isArray(tree)) {
    for (const item of tree) assertPublicPressRefs(item, expectedPlugin);
    return;
  }
  if (!tree || typeof tree !== 'object') return;
  assert.equal(Object.hasOwn(tree, 'held'), false, 'public Client frames must not expose worker-held callbacks');
  if (tree.press !== undefined) {
    assert.deepEqual(Object.keys(tree.press).sort(), ['handle', 'plugin'], 'public press refs contain only Native fields');
    assert.equal(tree.press.plugin, expectedPlugin, 'press refs must retain their owning plugin');
    assert.ok(Number.isSafeInteger(tree.press.handle) && tree.press.handle > 0, 'press handle must be positive');
  }
  for (const value of Object.values(tree)) assertPublicPressRefs(value, expectedPlugin);
}

function findPressControl(tree, elementType, elementKey, expectedPlugin) {
  if (Array.isArray(tree)) {
    for (const item of tree) {
      const found = findPressControl(item, elementType, elementKey, expectedPlugin);
      if (found !== undefined) return found;
    }
    return undefined;
  }
  if (!tree || typeof tree !== 'object') return undefined;
  if (tree.type === elementType && tree.props?.key === elementKey) {
    assert.ok(tree.press && typeof tree.press === 'object', `${elementType} must expose its Native press ref`);
    assert.deepEqual(Object.keys(tree.press).sort(), ['handle', 'plugin']);
    assert.equal(tree.press.plugin, expectedPlugin);
    assert.ok(Number.isSafeInteger(tree.press.handle) && tree.press.handle > 0);
    return tree.press.handle;
  }
  return findPressControl(tree.children, elementType, elementKey, expectedPlugin);
}

async function installUiFrameCollector(cdp, sessionId) {
  const sessionLiteral = JSON.stringify(sessionId);
  await cdp.evaluate(
    '(() => {'
    + ' window.__packagedModUiProbe?.unsubscribe?.();'
    + ' const state = {frames: [], unsubscribe: null};'
    + ' state.unsubscribe = window.lingxi.modUi.onFrame(event => {'
    + '   if (event?.sessionId === ' + sessionLiteral + ') state.frames.push(event);'
    + ' });'
    + ' window.__packagedModUiProbe = state;'
    + ' return true;'
    + '})()',
  );
}

async function waitForUiFrameText(cdp, runtimeId, revision, expectedText, label) {
  const expression = '(() => {'
    + ' const flatten = node => Array.isArray(node) ? node.map(flatten).join("")'
    + '   : typeof node === "string" ? node'
    + '   : node && typeof node === "object" ? flatten(node.children ?? []) : "";'
    + ' const event = window.__packagedModUiProbe.frames.findLast(item =>'
    + '   item.runtimeId === ' + JSON.stringify(runtimeId)
    + '   && item.frame?.renderRevision === ' + JSON.stringify(revision)
    + '   && flatten(item.frame.tree).includes(' + JSON.stringify(expectedText) + '));'
    + ' return event?.frame;'
    + '})()';
  return waitFor(
    () => cdp.evaluate(expression),
    { timeoutMs: 20_000, label },
  );
}

function uiFrameText(tree) {
  if (Array.isArray(tree)) return tree.map(uiFrameText).join('');
  if (typeof tree === 'string') return tree;
  if (!tree || typeof tree !== 'object') return '';
  return uiFrameText(tree.children);
}

async function runModUiHostApiProbe(cdp, sessionId) {
  await installUiFrameCollector(cdp, sessionId);
  const instanceId = 'packaged-host-ui-api-' + sessionId.replace(/[^A-Za-z0-9._-]/g, '-');
  const renderResult = await cdp.evaluate(expressionCall('modUi.control', sessionId, {
    subtype: 'ui_render',
    surface: 'desktop',
    component: 'AbovePrompt',
    instance_id: instanceId,
    props: {},
    viewport: { columns: 100, rows: 28 },
  }));
  assert.equal(renderResult?.response?.hooked, true, 'Host must dispatch the installed ui.render hook');
  assert.equal(renderResult?.response?.rewritten, true, 'the plugin must contribute a real Client node');
  const client = findUiClient(renderResult?.response?.tree);
  assert.ok(client, 'real ui.render hook must return the installed Client node');
  assert.equal(client.props?.module, UI_CLIENT_MODULE, 'Client module identity must use its prepared plugin-relative path');
  assert.equal(typeof client.client?.plugin, 'string', 'ui.resolve must stamp the Client owner plugin');
  assert.equal(client.client.plugin, UI_PLUGIN_NAME);
  const moduleHash = renderResult.response.client_modules?.[client.client.plugin];
  assert.equal(typeof moduleHash, 'string', 'Host render must report the prepared Client module hash');
  const revision = renderResult.metadata?.renderRevision;
  assert.ok(Number.isSafeInteger(revision) && revision > 0, 'Host render must return a revision');
  const runtimeEpoch = renderResult.metadata?.clientRuntimeEpochs?.[client.client.plugin];
  assert.ok(Number.isSafeInteger(runtimeEpoch) && runtimeEpoch > 0, 'Host render must report the plugin runtime epoch');

  const identity = {
    plugin: client.client.plugin,
    key: client.props.key,
    module: client.props.module,
  };
  const committed = await cdp.evaluate(expressionCall('modUi.operation', sessionId, {
    type: 'draw_commit',
    surface: 'desktop',
    component: 'AbovePrompt',
    instance_id: instanceId,
    render_revision: revision,
    clients: [identity],
  }));
  assert.equal(committed?.handled, true, 'Host must commit the exact returned Client draw');
  assert.equal(committed.renderRevision, revision);

  const mounted = await cdp.evaluate(expressionCall('modUi.operation', sessionId, {
    type: 'mount',
    surface: 'desktop',
    component: 'AbovePrompt',
    instance_id: instanceId,
    plugin: identity.plugin,
    client: identity.key,
    module: identity.module,
    render_revision: revision,
    columns: 100,
    rows: 28,
  }));
  const runtimeId = mounted?.runtimeId;
  assert.equal(typeof runtimeId, 'string', 'Host must mount the prepared Client in its actual worker');
  assert.equal(mounted.renderRevision, revision);
  assert.ok(Number.isSafeInteger(mounted.frameSequence) && mounted.frameSequence > 0);
  assertPublicPressRefs(mounted.tree, identity.plugin);
  const inputHandle = findPressControl(mounted.tree, 'Input', 'message-input', identity.plugin);
  const initialButtonHandle = findPressControl(mounted.tree, 'Button', 'submit', identity.plugin);
  assert.ok(Number.isSafeInteger(inputHandle), 'mounted frame must expose its Native Input press handle');
  assert.ok(Number.isSafeInteger(initialButtonHandle), 'mounted frame must expose its Native Button press handle');
  assert.ok(uiFrameText(mounted.tree).includes('Waiting for a value.'),
    'initial Host Client frame must be returned by mount; subsequent updates use frame events');

  const inputValue = 'packaged-host-ui-value';
  const inputControl = await cdp.evaluate(expressionCall('modUi.control', sessionId, {
    subtype: 'ui_client_press',
    plugin: identity.plugin,
    component: 'AbovePrompt',
    instance_id: instanceId,
    client: identity.key,
    module: identity.module,
    element: 'message-input',
    event: { type: 'input', kind: 'change', value: inputValue },
  }));
  const inputReached = inputControl?.response?.reached;
  assert.equal(inputControl?.response?.handled, true, 'Host must admit the current Input callback address');
  assert.equal(inputReached?.element, 'message-input');
  assert.equal(inputReached?.value, inputValue, 'ui.input hook chain must return the submitted value');
  const inputRun = await cdp.evaluate(expressionCall('modUi.operation', sessionId, {
    type: 'runHeld',
    runtimeId,
    render_revision: revision,
    event: { kind: 'change', value: inputReached.value },
    handle: inputHandle,
  }));
  assert.equal(inputRun?.renderRevision, revision, 'Host must run the Native Input callback at the drawn revision');
  const inputFrame = await waitForUiFrameText(
    cdp, runtimeId, revision, 'Input callback received: ' + inputValue,
    'ui.message result from real Input callback',
  );
  assert.ok(Number.isSafeInteger(inputFrame.frameSequence) && inputFrame.frameSequence > mounted.frameSequence);
  assertPublicPressRefs(inputFrame.tree, identity.plugin);
  const inputSubmitControl = await cdp.evaluate(expressionCall('modUi.control', sessionId, {
    subtype: 'ui_client_press',
    plugin: identity.plugin,
    component: 'AbovePrompt',
    instance_id: instanceId,
    client: identity.key,
    module: identity.module,
    element: 'message-input',
    event: { type: 'input', kind: 'submit', value: inputValue },
  }));
  const inputSubmitReached = inputSubmitControl?.response?.reached;
  assert.equal(inputSubmitControl?.response?.handled, true, 'Host must admit the current Input submit address');
  assert.equal(inputSubmitReached?.element, 'message-input');
  assert.equal(inputSubmitReached?.value, inputValue, 'ui.input hook chain must return the submitted value');
  assert.ok(uiFrameText(inputRun.tree).includes('Input callback received: ' + inputValue),
    'settled runHeld frame must include its synchronous post result');
  assertPublicPressRefs(inputRun.tree, identity.plugin);
  const inputSubmitHandle = findPressControl(inputRun.tree, 'Input', 'message-input', identity.plugin);
  assert.ok(Number.isSafeInteger(inputSubmitHandle));
  assert.notEqual(inputSubmitHandle, inputHandle, 'props update must replace the previous Input press handle');
  const inputSubmitRun = await cdp.evaluate(expressionCall('modUi.operation', sessionId, {
    type: 'runHeld',
    runtimeId,
    render_revision: revision,
    event: { kind: 'submit', value: inputSubmitReached.value },
    handle: inputSubmitHandle,
  }));
  assert.equal(inputSubmitRun?.renderRevision, revision, 'Host must run the Native Input submit callback at the drawn revision');
  const inputSubmitFrame = await waitForUiFrameText(
    cdp, runtimeId, revision, 'Input submit accepted: ' + inputValue,
    'ui.message result from real Input submit callback',
  );
  assert.ok(inputSubmitFrame.frameSequence > inputFrame.frameSequence);
  assertPublicPressRefs(inputSubmitFrame.tree, identity.plugin);
  assert.ok(uiFrameText(inputSubmitRun.tree).includes('Input submit accepted: ' + inputValue),
    'settled Input submit frame must include its synchronous post result');
  assertPublicPressRefs(inputSubmitRun.tree, identity.plugin);
  const buttonHandle = findPressControl(inputSubmitRun.tree, 'Button', 'submit', identity.plugin);
  assert.ok(Number.isSafeInteger(buttonHandle), 'updated frame must expose the current Native Button press handle');
  assert.notEqual(buttonHandle, initialButtonHandle, 'props update must replace the previous Button press handle');

  const buttonControl = await cdp.evaluate(expressionCall('modUi.control', sessionId, {
    subtype: 'ui_client_press',
    plugin: identity.plugin,
    component: 'AbovePrompt',
    instance_id: instanceId,
    client: identity.key,
    module: identity.module,
    element: 'submit',
    event: { type: 'press' },
  }));
  assert.equal(buttonControl?.response?.handled, true, 'Host must admit the current Button callback address');
  assert.equal(buttonControl.response.reached?.element, 'submit');
  const buttonRun = await cdp.evaluate(expressionCall('modUi.operation', sessionId, {
    type: 'runHeld',
    runtimeId,
    render_revision: revision,
    handle: buttonHandle,
  }));
  assert.equal(buttonRun?.renderRevision, revision, 'Host must run the Native Button callback at the drawn revision');
  const resultFrame = await waitForUiFrameText(
    cdp, runtimeId, revision, 'Submit callback accepted: ' + inputValue,
    'ui.message result from real Button callback',
  );
  assert.ok(resultFrame.frameSequence > inputSubmitFrame.frameSequence);
  assertPublicPressRefs(resultFrame.tree, identity.plugin);
  const resultText = uiFrameText(resultFrame.tree);

  const unmounted = await cdp.evaluate(expressionCall('modUi.operation', sessionId, {
    type: 'unmount',
    runtimeId,
    render_revision: revision,
  }));
  assert.equal(unmounted?.handled, true, 'Host must dispose the mounted Client runtime');
  const drawUnmounted = await cdp.evaluate(expressionCall('modUi.operation', sessionId, {
    type: 'draw_unmount',
    surface: 'desktop',
    component: 'AbovePrompt',
    instance_id: instanceId,
    render_revision: revision,
  }));
  assert.equal(drawUnmounted?.handled, true, 'Host must release the parent draw and Client address');
  const staleControl = await cdp.evaluate(expressionCall('modUi.control', sessionId, {
    subtype: 'ui_client_press',
    plugin: identity.plugin,
    component: 'AbovePrompt',
    instance_id: instanceId,
    client: identity.key,
    module: identity.module,
    element: 'submit',
    event: { type: 'press' },
  }));
  assert.equal(staleControl?.response?.handled, false, 'Host must reject callback controls after unmount');
  const staleRun = await cdp.evaluate(expressionCall('modUi.operation', sessionId, {
    type: 'runHeld',
    runtimeId,
    render_revision: revision,
    handle: buttonHandle,
  }));
  assert.equal(staleRun?.handled, false, 'Host must reject the old press handle after unmount');
  assert.equal(staleRun?.renderRevision, revision);
  const frameCountAfterUnmount = await cdp.evaluate(
    'window.__packagedModUiProbe.frames.filter(event => event.runtimeId === ' + JSON.stringify(runtimeId) + ').length',
  );
  await delay(350);
  const frameCountAfterStaleRun = await cdp.evaluate(
    'window.__packagedModUiProbe.frames.filter(event => event.runtimeId === ' + JSON.stringify(runtimeId) + ').length',
  );
  assert.equal(frameCountAfterStaleRun, frameCountAfterUnmount, 'rejected stale handles must not emit another Client frame');
  await cdp.evaluate('window.__packagedModUiProbe.unsubscribe?.()');

  return {
    stage: 'mod-ui-host-api',
    pluginId: UI_PLUGIN_ID,
    module: identity.module,
    moduleHash,
    instanceId,
    renderRevision: revision,
    clientRuntimeEpoch: runtimeEpoch,
    runtimeId,
    mountFrameSequence: mounted.frameSequence,
    initialButtonHandle,
    currentButtonHandle: buttonHandle,
    inputFrameSequence: inputFrame.frameSequence,
    resultFrameSequence: resultFrame.frameSequence,
    inputResult: 'Input callback received: ' + inputValue,
    buttonResult: resultText,
    unmountHandled: unmounted.handled,
    drawUnmountHandled: drawUnmounted.handled,
    staleControlHandled: staleControl.response.handled,
    staleRunHandled: staleRun.handled,
    staleRunEmittedFrame: frameCountAfterStaleRun !== frameCountAfterUnmount,
    visibleCuaExpectation: {
      component: 'AbovePrompt',
      title: 'Packaged Host Client UI',
      inputLabel: 'Message',
      inputPlaceholder: 'Type a value',
      buttonLabel: 'Submit',
      initialResult: 'Waiting for a value.',
      suggestedInputValue: 'packaged-host-cua-value',
      afterInputResult: 'Input callback received: packaged-host-cua-value',
      afterSubmitResult: 'Submit callback accepted: packaged-host-cua-value',
    },
  };
}

async function seedSidebarGestureSessions(cdp, probe, workspace, alpha) {
  await cdp.evaluate(expressionCall('renameSession', workspace, alpha, 'Sidebar Alpha'));
  async function create(title) {
    const state = await cdp.evaluate(expressionCall('newSession', workspace, probe.providerId + '/smoke-model'));
    const id = state?.activeSession?.sessionId;
    assert.equal(typeof id, 'string');
    assert.notEqual(id, alpha, 'a persisted original session must not be reused as a draft');
    await installEventCollector(cdp, id);
    await sendMarkerTurn(cdp, { requests: probe.requests }, id, ACTIVE_PREFIX + MARKER, 'sidebar-seed-' + title);
    await waitFor(
      () => cdp.evaluate(expressionCall('listProjectSessions', workspace)).then(value => {
        assert.equal(value?.error, undefined, 'sidebar fixture catalog must not report an error');
        return value?.sessions?.some(row => row.uuid === id) ? value : undefined;
      }),
      { timeoutMs: APP_TIMEOUT_MS, label: 'persisted sidebar fixture visible before rename: ' + title },
    );
    await cdp.evaluate(expressionCall('renameSession', workspace, id, title));
    return id;
  }
  const gamma = await create('Sidebar Gamma');
  const beta = await create('Sidebar Beta');
  assert.notEqual(gamma, beta);
  await cdp.evaluate(expressionCall('updateSettings', {
    sidebar: { organization: 'project', chatSort: 'updated', manualSessionOrder: {} },
  }));
  for (const id of [gamma, beta, alpha]) {
    await cdp.evaluate(expressionCall('touchSession', workspace, id));
    await delay(30);
  }
  const catalog = await cdp.evaluate(expressionCall('listProjectSessions', workspace));
  assert.equal(catalog?.error, undefined, 'sidebar fixture catalog must not report an error');
  assert.equal(catalog?.sessions?.length, 3, 'sidebar fixture must contain exactly the three seeded sessions');
  for (const [id, title] of [[alpha, 'Sidebar Alpha'], [beta, 'Sidebar Beta'], [gamma, 'Sidebar Gamma']]) {
    assert.ok(catalog.sessions.some(row => row.uuid === id && row.title === title));
  }
  await cdp.evaluate(expressionCall('openSession', workspace, beta));
  return { alpha, beta, gamma, expectedInitialOrder: [alpha, beta, gamma],
    initialOrderVerification: 'pending-visible-CUA', selected: beta,
    note: 'public API state setup only; actual click/drag/hold require CUA and persisted readback' };
}

async function waitForNoRunningApp() {
  await waitFor(
    () => runningAppCommands().length === 0,
    { timeoutMs: 15_000, label: 'signed app process exit' },
  );
}

async function run(options) {
  if (process.platform !== 'darwin') {
    throw new Error('packaged Host acceptance requires macOS');
  }
  if (!options.app) throw new Error('--app /path/to/signed/App.app is required');
  if (runningAppCommands().length > 0) {
    throw new Error('refusing to start while another ' + APP_NAME + ' process is running');
  }
  assert.ok(existsSync(FIXTURE_ROOT), 'external packaged append Mod fixture is missing');
  assert.ok(existsSync(UI_FIXTURE_ROOT), 'external packaged Mod UI fixture is missing');
  const app = verifySignedApp(options.app);
  const bun = packagedBunIdentity(app.appPath);

  const root = mkdtempSync(join(tmpdir(), 'lingxi-packaged-host-mod-'));
  const home = join(root, 'home');
  const temporaryDirectory = join(root, 'tmp');
  const configHome = join(home, '.lingxi');
  const providerCapturePath = join(root, 'provider-capture.json');
  const stages = [];
  let currentStage = 'profile-setup';
  let workspace;
  let runtimePaths;
  let fixture;
  let environment;
  let provider;
  let baseUrl;
  let probe;
  let cdpPort;
  let child;
  let cdp;
  let providerCredentialNeedsCleanup = false;
  let providerCredentialCleanup = 'not-needed';
  let sessionId;
  let toolReadEvidence;
  let uiHostEvidence;
  let keepRoot = options.keepRoot || options.guiHandoff;
  let errorText;
  let runFailure;
  let handoffInfo;
  let handoffStopReason;
  try {
    workspace = createWorkspace(root);
    mkdirSync(home, { recursive: true, mode: 0o700 });
    mkdirSync(temporaryDirectory, { recursive: true, mode: 0o700 });
    mkdirSync(configHome, { recursive: true, mode: 0o700 });
    runtimePaths = smokeHelpers.runtimePathsForHome(home);
    writeJson(runtimePaths.settingsPath, smokeHelpers.createPackagedSettings({ workspace }));
    fixture = installFixture(configHome);
    environment = isolatedEnvironment(home, temporaryDirectory);
    const toolProbe = {
      filePath: join(workspace, 'tool-probe.txt'),
      relativePath: 'tool-probe.txt',
      state: newToolProbeState(),
      calls: [],
      completed: false,
      failure: null,
    };
    provider = makeFakeProvider(providerCapturePath, toolProbe);
    baseUrl = await provider.start();
    const providerId = 'packaged-mod-' + randomUUID();
    probe = { providerId, baseUrl, requests: provider.requests };

    currentStage = 'first-launch';
    cdpPort = await reserveLoopbackPort();
    child = launchPackagedApp(app, environment, runtimePaths.userDataDir, cdpPort);
    cdp = await connectToPackagedPage(cdpPort, child);
    await bootstrapReady(cdp);
    sessionId = await createOrRestoreSession(cdp, workspace);
    await ensureCustomProvider(cdp, probe, sessionId, false, () => {
      providerCredentialNeedsCleanup = true;
      providerCredentialCleanup = 'pending';
    });
    stages.push(await sendMarkerTurn(cdp, provider, sessionId, ACTIVE_PREFIX + MARKER, 'enabled-startup'));
    currentStage = 'packaged-read-tool';
    toolReadEvidence = await sendReadToolTurn(cdp, provider, sessionId, workspace);
    stages.push(toolReadEvidence);

    currentStage = 'disable-and-reload';
    changePluginEnabled(fixture.settingsPath, false);
    await reloadPlugins(cdp, sessionId, 'disable');
    stages.push(await sendMarkerTurn(cdp, provider, sessionId, MARKER, 'disabled-after-reload'));

    currentStage = 'enable-and-reload';
    changePluginEnabled(fixture.settingsPath, true);
    await reloadPlugins(cdp, sessionId, 'enable');
    stages.push(await sendMarkerTurn(cdp, provider, sessionId, ACTIVE_PREFIX + MARKER, 'enabled-after-reload'));

    currentStage = 'restart-and-session-recovery';
    await terminateOwnedApp(child, cdp);
    child = undefined;
    cdp = undefined;
    await waitForNoRunningApp();
    cdpPort = await reserveLoopbackPort();
    child = launchPackagedApp(app, environment, runtimePaths.userDataDir, cdpPort);
    cdp = await connectToPackagedPage(cdpPort, child);
    await bootstrapReady(cdp);
    sessionId = await createOrRestoreSession(cdp, workspace, sessionId);
    await ensureCustomProvider(cdp, probe, sessionId, true);
    stages.push(await sendMarkerTurn(cdp, provider, sessionId, ACTIVE_PREFIX + MARKER, 'enabled-after-restart'));
    uiHostEvidence = await runModUiHostApiProbe(cdp, sessionId);
    stages.push(uiHostEvidence);
    if (options.guiHandoff) {
      currentStage = 'gui-handoff';
      const sidebarGestureFixtures = await seedSidebarGestureSessions(cdp, probe, workspace, sessionId);
      const stopPath = join(root, 'STOP-HANDOFF');
      const handoffTimestamp = new Date().toISOString().replace(/[:.]/g, '-');
      handoffInfo = {
        id: 'packaged-host-' + process.pid + '-' + handoffTimestamp,
        root,
        appPid: child.pid,
        cdpPort,
        workspace,
        userDataDir: runtimePaths.userDataDir,
        configHome,
        pluginRoot: fixture.pluginRoot,
        uiPluginRoot: fixture.uiPluginRoot,
        uiPluginId: UI_PLUGIN_ID,
        settingsPath: fixture.settingsPath,
        providerBaseUrl: baseUrl,
        sessionId,
        toolReadHostEvidence: toolReadEvidence,
        modUiHostEvidence: uiHostEvidence,
        visibleUiExpected: uiHostEvidence.visibleCuaExpectation,
        sidebarGestureFixtures,
        desktopSettingsPath: join(runtimePaths.userDataDir, 'settings.v1.json'),
        pendingCuaChecks: [
          'Click Sidebar Alpha, drag Sidebar Gamma before Alpha, then inspect persisted order; no synthetic gesture claim.',
          'Use CUA to confirm the real AbovePrompt form, Message input, Submit button and initial result.',
          'Type packaged-host-cua-value and confirm the visible Input callback result updates.',
          'Click Submit and confirm the visible Submit callback result updates.',
        ],
        stopFile: stopPath,
        timeoutSeconds: options.handoffTimeoutSeconds,
      };
      writeJson(join(root, 'handoff.json'), handoffInfo);
      process.stdout.write('GUI_HANDOFF ' + JSON.stringify(handoffInfo) + '\n');
      handoffStopReason = await waitForGuiHandoffStop(
        stopPath, options.handoffTimeoutSeconds * 1000,
      );
    }
    currentStage = 'complete';
  } catch (error) {
    runFailure = error;
    errorText = error?.stack ?? String(error);
    keepRoot = true;
  } finally {
    const cleanupFailures = [];
    if (providerCredentialNeedsCleanup) {
      try {
        await clearSyntheticProviderCredential(cdp, probe?.providerId);
        providerCredentialNeedsCleanup = false;
        providerCredentialCleanup = 'cleared';
      } catch (error) {
        providerCredentialCleanup = 'failed';
        cleanupFailures.push(error);
      }
    }
    try {
      await terminateOwnedApp(child, cdp);
    } catch (error) {
      cleanupFailures.push(error);
    }
    try {
      await waitForNoRunningApp();
    } catch (error) {
      cleanupFailures.push(error);
    }
    try {
      await provider?.close();
    } catch (error) {
      cleanupFailures.push(error);
    }
    if (cleanupFailures.length > 0) {
      keepRoot = true;
      runFailure ??= cleanupFailures[0];
      const cleanupText = cleanupFailures
        .map((error) => error?.stack ?? String(error))
        .join('\n');
      errorText = errorText
        ? errorText + '\nCleanup failure(s):\n' + cleanupText
        : 'Cleanup failure(s):\n' + cleanupText;
    }
    mkdirSync(options.resultsDir, { recursive: true, mode: 0o700 });
    const timestamp = new Date().toISOString().replace(/[:.]/g, '-');
    const resultPath = join(options.resultsDir, 'packaged-host-mod-' + timestamp + '.json');
    const result = {
      outcome: errorText ? 'failed' : 'passed',
      stage: currentStage,
      app: {
        bundleName: basename(app.appPath),
        bundleId: app.bundleId,
        version: app.version,
        build: app.build,
      },
      packagedBun: { version: bun.version, revision: bun.revision, sha256: bun.sha256 },
      fixture: { pluginId: PLUGIN_ID, pluginVersion: PLUGIN_VERSION },
      uiFixture: {
        pluginId: UI_PLUGIN_ID,
        pluginVersion: UI_PLUGIN_VERSION,
        clientKey: UI_CLIENT_KEY,
        module: UI_CLIENT_MODULE,
      },
      sessionId: sessionId ?? null,
      providerCredentialCleanup,
      stages,
      toolReadHostEvidence: toolReadEvidence,
      modUiHostEvidence: uiHostEvidence,
      providerRequestCount: provider?.requests.length ?? 0,
      providerRequests: (provider?.requests ?? []).map((request) => ({
        ...publicProviderRequest(request),
      })),
      retainedIsolatedRoot: keepRoot ? root : null,
      handoff: handoffInfo ? { ...handoffInfo, stopReason: handoffStopReason ?? null } : undefined,
      providerCapturePath: keepRoot ? providerCapturePath : undefined,
      error: errorText,
    };
    writeJson(resultPath, result);
    if (!keepRoot) rmSync(root, { recursive: true, force: true });
    process.stdout.write(JSON.stringify({ resultPath, outcome: result.outcome, stage: result.stage }) + '\n');
  }
  if (runFailure) throw runFailure;
}

const options = parseArgs(process.argv.slice(2));
if (options.help) {
  printHelp();
} else if (options.selfTest) {
  runToolProbeContractSelfTest();
  assert.throws(() => nextToolProbeAction({ messages: [
    { role: 'tool', tool_call_id: TOOL_READ_CALL_ID, content: 'wrong read content' },
  ] }, { ...newToolProbeState(), phase: 'await-read-result', readCallId: TOOL_READ_CALL_ID }, '/isolated/file'), /actual Read tool result/);
  assert.equal(isToolProbeBody({ messages: [
    { role: 'tool', tool_call_id: TOOL_READ_CALL_ID, content: TOOL_READ_MARKER },
    { role: 'user', content: '<system-reminder>post-tool context</system-reminder>' },
  ] }, { phase: 'await-read-result' }), true);
  assert.equal(isToolProbeBody({ messages: [
    { role: 'tool', tool_call_id: 'foreign-tool-call', content: TOOL_READ_MARKER },
    { role: 'user', content: '<system-reminder>post-tool context</system-reminder>' },
  ] }, { phase: 'await-read-result' }), false);
  assert.equal(isToolProbeBody({ messages: [
    { role: 'tool', tool_call_id: TOOL_READ_CALL_ID, content: TOOL_READ_MARKER },
    { role: 'user', content: MARKER },
  ] }, { phase: 'complete' }), false);
  assert.equal(lastPhysicalLine('<system-reminder>context</system-reminder>\n\n' + ACTIVE_PREFIX + MARKER), ACTIVE_PREFIX + MARKER);
  assert.equal(lastPhysicalLine('context\n' + MARKER), MARKER);
  assert.notEqual(lastPhysicalLine('context\n' + ACTIVE_PREFIX + MARKER), MARKER);
  assert.notEqual(lastPhysicalLine('context\n' + ACTIVE_PREFIX + MARKER + ' '), ACTIVE_PREFIX + MARKER);
  process.stdout.write('exact marker-line checks preserve enabled/disabled and trailing-space distinctions\n');
} else {
  await run(options);
}
