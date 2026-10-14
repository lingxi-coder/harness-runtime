// Regenerate MCP negotiation fixtures by executing functions extracted from the
// hash-pinned native reference. No reference implementation is vendored.
// Usage: node scripts/tests/mcp_negotiation_286_oracle.mjs /path/to/2.1.286 [--check]
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

const binary = readFileSync(process.argv[2]);
const sha256 = createHash('sha256').update(binary).digest('hex');
if (sha256 !== '75e3016e9d2570767b08e43a7467d4817a4f149232c169ca295f2c95fef21433') {
  throw new Error(`Unexpected native oracle SHA-256: ${sha256}`);
}
const sourceByteRanges = [];
function chunk(offset) {
  const end = binary.indexOf('// @bun @bytecode\n', offset + 1);
  const contents = binary.subarray(offset, end < 0 ? binary.length : end);
  const exportStart = contents.lastIndexOf('export{');
  const nul = contents.indexOf(0, Math.max(exportStart, 0));
  return contents.subarray(0, nul < 0 ? contents.length : nul);
}
function range(offset, startMarker, endMarker) {
  const bytes = chunk(offset);
  const start = bytes.indexOf(startMarker);
  const end = bytes.indexOf(endMarker, start + startMarker.length);
  if (start < 0 || end < start) throw new Error(`Missing source range: ${startMarker}`);
  sourceByteRanges.push({ signature: startMarker, start: offset + start, end: offset + end });
  return bytes.subarray(start, end).toString('utf8');
}
// These three complete chunks are the reference's pure schema library. Evaluate
// their exact bundled exports rather than approximating validation in the test.
function pureModule(offset, imports = {}) {
  const bytes = chunk(offset);
  sourceByteRanges.push({ signature: 'pure schema dependency chunk', start: offset, end: offset + bytes.length });
  const source = bytes.toString('utf8')
    .replace(/import\{[^}]+\}from"[^"]+";/g, '')
    .replace(/export\{([^}]+)\};/, 'return {$1};');
  return vm.runInNewContext(`(()=>{${source}})()`, imports, { timeout: 2_000 });
}
const schemaCore = pureModule(176381936);
const schema = pureModule(176393579, schemaCore);
const schemaDates = pureModule(177781187, schema);
const sdkOffset = 209701831;
const source = [
  range(sdkOffset, 'var Hr=Symbol.for(', 'function xs('),
  range(sdkOffset, 'var Js="2026-07-28"', 'function Gs('),
  range(sdkOffset, 'function Ao(){', 'var Lo;'),
  range(sdkOffset, 'var Hs=function(', ',Fo=0') + ';',
  range(sdkOffset, 'function Vt(', 'var on='),
  range(sdkOffset, 'var Jc=-32022', 'function Za('),
  range(sdkOffset, 'function al(', 'function il('),
  range(sdkOffset, 'function ao(', 'function Mr('),
].join('\n');
const envelopeMethod = range(sdkOffset, 'outboundEnvelope(e){', ',validateEnvelopeMeta(');
const decodeMethod = range(sdkOffset, 'decodeResult(e,t){if(!Ht(t))', ',encodeResult(');
const headerMethod = range(sdkOffset, '_applyBodyDerivedHeaders(e,t){', '_isModernEnvelopedRequest(');
const requestSchemaSource = range(209672256, 'var a="2.0";', ',uZt=') + ';';
const nameHeaderSource = range(sdkOffset, 'var fa="=?base64?"', 'function Rn(');
const metadataMethods = [
  range(sdkOffset, 'async _connectNegotiated(', 'async _connectFromPrior('),
  range(sdkOffset, 'async discover(', 'async complete('),
].join('\n');
const harness = `
const VDe='io.modelcontextprotocol/protocolVersion',QRt='io.modelcontextprotocol/clientInfo',
 qDe='io.modelcontextprotocol/serverInfo',Ume='io.modelcontextprotocol/clientCapabilities',
 ZRt='io.modelcontextprotocol/logLevel',sZt='io.modelcontextprotocol/subscriptionId',Jbr='io.modelcontextprotocol/related-task',$r='2026-07-28';
${requestSchemaSource}
${nameHeaderSource}
const definitions=Ao();
const es={${envelopeMethod},validateResult:(method,value)=>Vt(definitions.dispatchResultSchemas[method],value)};
const Ht=value=>value!==null&&typeof value==='object'&&!Array.isArray(value);
const hn=()=>({'server/discover':definitions.DiscoverResultSchema});
const decoder={${decodeMethod}},headersTransport={${headerMethod}};
const ZZ=value=>dZt.safeParse(value).success;
const Me=()=>es,Ra={Implementation:value=>definitions.ImplementationSchema.safeParse(value).success};
const sT=class extends Error{},Bve=definitions.DiscoverResultSchema;
let exchanges=[],requests=[],lifecycle=[],modeResult;
const sl={open:async()=>({
 exchange:async (makeRequest,timeout)=>{requests.push({request:makeRequest('probe-'+requests.length),timeout});if(exchanges.length===0)throw Error('Missing fixture exchange');return exchanges.shift()},
 detach:()=>lifecycle.push('detach'),release:()=>lifecycle.push('release')
})};
const errorShape=error=>({name:error.name,code:error.code,message:error.message,...error.data!==undefined?{data:error.data}:{}});
const normalize=value=>JSON.parse(JSON.stringify(value,(_key,item)=>item instanceof Error?errorShape(item):item));
const materialize=value=>value.kind==='network-error'?{...value,error:value.typeError?new TypeError(value.message):new Error(value.message)}:value;
const context={clientModernVersions:['2026-07-28'],requestedVersion:'2026-07-28',fallbackAvailable:true,environment:'node',transportKind:'http'};
const runDiscovery=test=>normalize(Wc(test.result,{...context,...test.context}));
const runDecision=test=>normalize(Bc(materialize(test.event),{...context,...test.context}));
const runDecode=test=>normalize(decoder.decodeResult('server/discover',test.result));
const runHeaders=test=>{const headers=new Headers(test.initialHeaders);headersTransport._applyBodyDerivedHeaders(headers,test.request);return Object.fromEntries(headers)};
async function runProbe(test){
 exchanges=test.exchanges;requests=[];lifecycle=[];
 const transport={};
 try {const verdict=await _o(rl(test.negotiation??{mode:'auto'},test.supportedProtocolVersions),{
  transport,transportKind:test.transportKind??'http',disposableProbe:test.disposableProbe,
  environment:'node',defaultTimeoutMs:60000,clientInfo:{name:'oracle-client',version:'1'},capabilities:{roots:{listChanged:true}}
 });return normalize({verdict,requests,lifecycle,transport})}
 catch(error){return normalize({error,requests,lifecycle,transport})}
}
const Qa=()=> 'http',Za=()=> 'node',ro=()=>{},il=()=>undefined,Bt=60000;
class Base {async connect(){lifecycle.push('connect')}}
class Client extends Base {
 constructor(){super();this._clientInfo={name:'oracle-client',version:'1'};this._capabilities={};this._cache={setServerIdentity:value=>{this.identity=value}}}
 _resetConnectionState(){this._negotiatedProtocolVersion=undefined;this._serverCapabilities=undefined;this._serverVersion=undefined;this._instructions=undefined;this._discoverResult=undefined}
 _deriveServerIdentity(){return this._serverVersion?this._serverVersion.name+'@'+this._serverVersion.version:'anonymous'}
 _requestWithSchema(){return Promise.resolve(this.nextDiscovery)}
 ${metadataMethods}
}
async function runMetadata(test){
 const client=new Client(),transport={setProtocolVersion:version=>{transport.version=version}};
 exchanges=[{kind:'response',result:test.initial}];requests=[];lifecycle=[];
 await client._connectNegotiated(transport,rl({mode:'auto'}));
 const snapshot=()=>normalize({capabilities:client._serverCapabilities,serverInfo:client._serverVersion,instructions:client._instructions,discover:client._discoverResult,protocolVersion:client._negotiatedProtocolVersion,identity:client.identity});
 const connected=snapshot();client.nextDiscovery=test.next;
 const returned=await client.discover();
 return {connected,afterDiscover:snapshot(),returned};
}
({runDiscovery,runDecision,runProbe,runMetadata,runDecode,runHeaders,normalize});`;
const oracle = vm.runInNewContext(`${source}\n${harness}`, { ...schema, ...schemaDates, AbortController, Headers, TextEncoder, btoa }, { timeout: 2_000 });
const policySource = range(210493918, 'function So(e,n,r){', 'var da=');
const policyOracle = vm.runInNewContext(`
let a={},flags={},budget=60000,warnings=[];
const ps=5000,io=3000,ia=new Set(['http','claudeai-proxy','ccr-proxy','stdio']);
const Rl=()=>budget,R=(key,defaultValue)=>flags[key]??defaultValue;
const t=message=>warnings.push(message),fs=()=>false,iVr=()=>undefined;
${policySource}
test=>{a={MCP_PROTOCOL_NEGOTIATION:test.environment};flags=test.flags??{};budget=test.timeout??60000;warnings=[];
 return {policy:So(test.transport,test.server,test.denylisted),warnings}}
`, { URL }, { timeout: 1_000 });
const instructionSource = [
  range(183727495, 'function zvt(', 'var Aoo='),
  range(176497354, 'function ws(', 'function E7e('),
  range(176230173, 'function mi(', 'function F5r('),
  range(183727495, 'function ol(e){', 'function JZt('),
  range(183727495, 'function Yi(', 'function Afr('),
  range(183727495, 'var Jx=', 'function gon('),
  range(176210667, 'var kO=', 'function B2e('),
].join('\n');
const instructionRenderer = range(183727495, 'case"mcp_instructions_delta":', 'case"mcp_dropped_tools_delta":');
const instructionOracle = vm.runInNewContext(`
${instructionSource}
let cacheEnabled=true;
const ga=()=>cacheEnabled,i=()=>{};
const Re=({content,isMeta})=>({type:'user',isMeta,message:{content}});
function render(e){switch(e.type){${instructionRenderer}}}
test=>{cacheEnabled=test.cacheEnabled??true;
 const delta=zvt(test.clients,test.history??[],test.clientBlocks??[]);
 return {delta,rendered:delta?render({type:'mcp_instructions_delta',...delta}):[]}}
`, {}, { timeout: 1_000 });

const modern = { supportedVersions: ['2026-07-28'], capabilities: {} };
const withInfo = { ...modern, capabilities: { tools: { listChanged: true } }, instructions: 'initial instructions', _meta: { 'io.modelcontextprotocol/serverInfo': { name: 'initial-server', version: '1' } } };
const discoveryCases = [
  { name: 'minimal-modern', result: modern },
  { name: 'explicit-complete', result: { ...modern, resultType: 'complete' } },
  { name: 'partial-result-type-passthrough', result: { ...modern, resultType: 'partial' } },
  { name: 'numeric-result-type-passthrough', result: { ...modern, resultType: 42 } },
  { name: 'missing-supported-versions', result: { capabilities: {} } },
  { name: 'missing-capabilities', result: { supportedVersions: modern.supportedVersions } },
  { name: 'empty-versions-falls-back', result: { ...modern, supportedVersions: [] } },
  { name: 'mixed-version-types-rejected', result: { ...modern, supportedVersions: ['2026-07-28', 42] } },
  { name: 'null-capabilities-rejected', result: { ...modern, capabilities: null } },
  { name: 'array-capabilities-rejected', result: { ...modern, capabilities: [] } },
  { name: 'invalid-instructions-rejected', result: { ...modern, instructions: 42 } },
  { name: 'invalid-capability-rejected', result: { ...modern, capabilities: { tools: { listChanged: 'yes' } } } },
  { name: 'unknown-capabilities-stripped', result: { ...modern, capabilities: { tasks: { list: {} }, custom: {}, tools: { listChanged: true, custom: 3 }, extensions: { vendor: { enabled: true } } } } },
  { name: 'valid-cache-hints', result: { ...modern, ttlMs: 1200, cacheScope: 'public' } },
  { name: 'invalid-cache-hints-default', result: { ...modern, ttlMs: -1, cacheScope: 'bogus' } },
  { name: 'fractional-cache-hint-default', result: { ...modern, ttlMs: 1.5 } },
  { name: 'valid-server-info-meta', result: withInfo },
  { name: 'missing-server-info-version-dropped', result: { ...modern, _meta: { 'io.modelcontextprotocol/serverInfo': { name: 'broken' }, vendor: true } } },
  { name: 'null-server-info-dropped', result: { ...modern, _meta: { 'io.modelcontextprotocol/serverInfo': null } } },
  { name: 'null-meta-rejected', result: { ...modern, _meta: null } },
  { name: 'top-level-server-info-passthrough', result: { ...modern, serverInfo: { name: 'top-level', version: '1' } } },
  { name: 'unknown-result-field-preserved', result: { ...modern, custom: { value: 1 } } },
  { name: 'client-order-selects-version', result: { ...modern, supportedVersions: ['2026-07-28', '2027-01-01'] }, context: { clientModernVersions: ['2027-01-01', '2026-07-28'] } },
  { name: 'modern-no-overlap-falls-back', result: { ...modern, supportedVersions: ['2027-01-01'] } },
  { name: 'modern-no-overlap-without-fallback-errors', result: { ...modern, supportedVersions: ['2027-01-01'] }, context: { fallbackAvailable: false } },
];
const unsupported = supported => ({ kind: 'rpc-error', code: -32022, message: 'unsupported', data: { supported } });
const decisionCases = [
  { name: 'unsupported-mutual-modern-corrects', event: unsupported(['2026-07-28']) },
  { name: 'unsupported-modern-no-overlap-errors', event: unsupported(['2027-01-01']) },
  { name: 'unsupported-only-legacy-falls-back', event: unsupported(['2025-11-25']) },
  { name: 'unsupported-only-legacy-without-fallback-errors', event: unsupported(['2025-11-25']), context: { fallbackAvailable: false } },
  { name: 'unsupported-empty-list-falls-back', event: unsupported([]) },
  { name: 'unsupported-malformed-list-falls-back', event: unsupported([42]) },
  { name: 'unsupported-requested-value-adopted', event: { ...unsupported(['2027-01-01']), data: { supported: ['2027-01-01'], requested: 'server-reported' } } },
  ...[-32601, -32602, -32001, -32020, -32021, -32099].map(code => ({ name: `rpc-${code}-falls-back`, event: { kind: 'rpc-error', code, message: 'rejected' } })),
  { name: 'http-json-error-corrects', event: { kind: 'http-error', status: 400, body: JSON.stringify({ error: unsupported(['2026-07-28']) }) } },
  { name: 'http-invalid-json-falls-back', event: { kind: 'http-error', status: 500, body: 'oops' } },
  { name: 'http-timeout-errors', event: { kind: 'timeout', timeoutMs: 5000 } },
  { name: 'stdio-timeout-falls-back', event: { kind: 'timeout', timeoutMs: 3000 }, context: { transportKind: 'stdio' } },
  { name: 'http-close-errors', event: { kind: 'closed' } },
  { name: 'stdio-close-falls-back', event: { kind: 'closed' }, context: { transportKind: 'stdio' } },
  { name: 'node-network-error-terminal', event: { kind: 'network-error', message: 'offline' } },
  { name: 'browser-type-error-falls-back', event: { kind: 'network-error', typeError: true, message: 'fetch failed' }, context: { environment: 'browser' } },
  ...['id-mismatch', 'malformed', 'stream-end'].map(kind => ({ name: `${kind}-falls-back`, event: { kind } })),
];
const reply = result => ({ kind: 'response', result });
const reject = supported => ({ kind: 'response', error: { code: -32022, message: 'unsupported', data: { supported } } });
const probeCases = [
  { name: 'modern-single-probe', exchanges: [reply(modern)] },
  { name: 'timeout-retry-then-modern', negotiation: { mode: 'auto', probe: { timeoutMs: 123, maxRetries: 1 } }, exchanges: [{ kind: 'timeout' }, reply(modern)] },
  { name: 'http-timeout-exhausted-errors', negotiation: { mode: 'auto', probe: { timeoutMs: 123, maxRetries: 1 } }, exchanges: [{ kind: 'timeout' }, { kind: 'timeout' }] },
  { name: 'stdio-timeout-exhausted-falls-back', transportKind: 'stdio', exchanges: [{ kind: 'timeout' }] },
  { name: 'corrective-retry-only-once', exchanges: [reject(['2026-07-28']), reject(['2026-07-28'])] },
  { name: 'corrective-version-adopted', supportedProtocolVersions: ['2027-01-01', '2026-07-28', '2025-11-25'], exchanges: [reject(['2026-07-28']), reply(modern)] },
  { name: 'ordinary-rpc-error-not-retried', negotiation: { mode: 'auto', probe: { maxRetries: 2 } }, exchanges: [{ kind: 'response', error: { code: -32601, message: 'unknown' } }] },
  { name: 'malformed-pin-fails-without-initialize', negotiation: { mode: { pin: '2026-07-28' } }, exchanges: [{ kind: 'malformed' }] },
  { name: 'malformed-without-legacy-version-fails', supportedProtocolVersions: ['2026-07-28'], exchanges: [{ kind: 'malformed' }] },
  { name: 'stdio-in-place-close-fails', transportKind: 'stdio', exchanges: [{ kind: 'closed' }] },
  { name: 'stdio-disposable-close-falls-back', transportKind: 'stdio', disposableProbe: true, exchanges: [{ kind: 'closed' }] },
];
const policyCases = [
  ...['http', 'stdio', 'claudeai-proxy', 'ccr-proxy', 'sse', 'ws', 'ide', 'in-process', 'sdk-control'].map(transport => ({ name: `default-${transport}`, transport })),
  { name: 'http-disabled-by-flag', transport: 'http', flags: { tengu_mcp_protocol_negotiation_http: false } },
  { name: 'stdio-enabled-by-flag', transport: 'stdio', flags: { tengu_mcp_protocol_negotiation_stdio: true } },
  { name: 'override-auto-enables-stdio', transport: 'stdio', environment: 'auto' },
  { name: 'override-auto-cannot-enable-sse', transport: 'sse', environment: 'auto' },
  { name: 'override-legacy-disables-http', transport: 'http', environment: 'legacy' },
  { name: 'override-invalid-warns-and-keeps-default', transport: 'http', environment: 'invalid' },
  { name: 'http-timeout-budget-third', transport: 'http', timeout: 9001 },
  { name: 'stdio-timeout-budget-third', transport: 'stdio', environment: 'auto', timeout: 6000 },
  { name: 'denylist-forces-legacy', transport: 'http', server: { url: 'https://example.com/mcp' }, denylisted: true },
  { name: 'auto-override-bypasses-denylist', transport: 'http', environment: 'auto', server: { url: 'https://example.com/mcp' }, denylisted: true },
];
const metadataCases = [
  { name: 'post-discover-retains-connected-metadata', initial: withInfo, next: { ...modern, capabilities: { resources: { listChanged: true } }, instructions: 'updated instructions', _meta: { 'io.modelcontextprotocol/serverInfo': { name: 'updated-server', version: '2' } } } },
  { name: 'top-level-server-info-does-not-set-identity', initial: { ...modern, serverInfo: { name: 'ignored-top-level', version: '1' } }, next: modern },
];
const decodeCases = [
  { name: 'connected-modern-missing-result-type-rejected', result: modern },
  { name: 'connected-modern-complete-accepted-and-marker-removed', result: { ...modern, resultType: 'complete' } },
  { name: 'connected-modern-partial-rejected', result: { ...modern, resultType: 'partial' } },
  { name: 'connected-modern-numeric-result-type-rejected', result: { ...modern, resultType: 42 } },
  { name: 'connected-modern-task-dispatched-separately', result: { ...modern, resultType: 'task' } },
  { name: 'connected-modern-input-required-empty-rejected', result: { resultType: 'input_required' } },
  { name: 'connected-modern-input-required-state-accepted', result: { resultType: 'input_required', requestState: 'opaque' } },
];
const request = (method, params) => ({ jsonrpc: '2.0', id: 1, method, params });
const meta = { 'io.modelcontextprotocol/protocolVersion': '2026-07-28' };
const headerCases = [
  { name: 'initial-probe-protocol-and-method-headers', request: request('server/discover', { _meta: meta }) },
  { name: 'initial-legacy-request-no-derived-headers', request: request('initialize', { protocolVersion: '2025-11-25' }) },
  { name: 'body-protocol-overrides-common-header', initialHeaders: { 'mcp-protocol-version': '2025-11-25' }, request: request('server/discover', { _meta: meta }) },
  { name: 'tool-call-includes-name', request: request('tools/call', { name: 'test-tool', _meta: meta }) },
  { name: 'resource-read-includes-uri', request: request('resources/read', { uri: 'file:///test', _meta: meta }) },
  { name: 'unicode-name-base64-encoded', request: request('tools/call', { name: '工具', _meta: meta }) },
];
const client = (name, instructions, type = 'connected') => ({ name, instructions, type });
const announcement = (name, serverInstructions, withBaseline = true) => ({ type: 'attachment', attachment: {
  type: 'mcp_instructions_delta', addedNames: [name], addedBlocks: [`## ${name}\n${serverInstructions}`],
  ...(withBaseline ? { addedServerInstructions: [serverInstructions] } : {}), removedNames: [],
} });
const instructionCases = [
  { name: 'initial-instructions-ordered-by-server-name', clients: [client('zeta', 'last'), client('alpha', 'first')] },
  { name: 'cached-client-eligible-failed-client-omitted', clients: [client('cached', 'cached instructions', 'cached'), client('failed', 'stale instructions', 'failed')] },
  { name: 'unchanged-instructions-silent', clients: [client('alpha', 'same')], history: [announcement('alpha', 'same')] },
  { name: 'changed-instructions-reannounced-with-cache', clients: [client('alpha', 'new')], history: [announcement('alpha', 'old')] },
  { name: 'changed-instructions-silent-without-cache', cacheEnabled: false, clients: [client('alpha', 'new')], history: [announcement('alpha', 'old')] },
  { name: 'old-transcript-without-raw-baseline-suppresses-update', clients: [client('alpha', 'new')], history: [announcement('alpha', 'old', false)] },
  { name: 'clearing-instructions-while-connected-silent', clients: [client('alpha', '')], history: [announcement('alpha', 'old')] },
  { name: 'disconnect-emits-removal', clients: [], history: [announcement('alpha', 'old')] },
  { name: 'pending-client-counts-as-disconnected', clients: [client('alpha', 'old', 'pending')], history: [announcement('alpha', 'old')] },
  { name: 'client-side-block-appended-to-server-instructions', clients: [client('alpha', 'server')], clientBlocks: [{ serverName: 'alpha', block: 'client' }] },
  { name: 'cached-placeholder-can-receive-client-side-only-block', clients: [client('alpha', undefined, 'cached')], clientBlocks: [{ serverName: 'alpha', block: 'client' }, { serverName: 'missing', block: 'ignored' }] },
  { name: 'removal-then-reconnect-reannounces', clients: [client('alpha', 'same')], history: [announcement('alpha', 'same'), { type: 'attachment', attachment: { type: 'mcp_instructions_delta', addedNames: [], addedBlocks: [], removedNames: ['alpha'] } }] },
  { name: 'additions-and-removals-render-in-one-message', clients: [client('new', 'new instruction')], history: [announcement('old', 'old instruction')] },
  { name: 'initial-without-cache-omits-raw-baseline', cacheEnabled: false, clients: [client('alpha', 'first')] },
];
const fixture = {
  version: '2.1.286', sha256, sourceByteRanges,
  scope: 'Actual Wc/Bc/rl/al/ol/_o and Claude So functions, bundled Zod/Ao discovery schema, _connectNegotiated/discover methods, modern decodeResult, body-derived HTTP headers, and zvt/Gxe instruction delta producer/renderer. Exchange transport, timers, configuration lookup, cache enablement, telemetry and connection state storage are deterministic stubs. No live provider, process transport or network execution. Post-connect discovery request/validation is stubbed; only adoption behavior is captured. Instruction user-row construction omits nondeterministic transcript metadata.',
  discoveryCases: discoveryCases.map(test => ({ ...test, expected: oracle.runDiscovery(test) })),
  decisionCases: decisionCases.map(test => ({ ...test, expected: oracle.runDecision(test) })),
  probeCases: [], policyCases: policyCases.map(test => ({ ...test, expected: policyOracle(test) })), metadataCases: [],
  decodeCases: decodeCases.map(test => ({ ...test, expected: oracle.runDecode(test) })),
  headerCases: headerCases.map(test => ({ ...test, expected: oracle.runHeaders(test) })),
  instructionCases: instructionCases.map(test => ({ ...test, expected: instructionOracle(test) })),
};
for (const test of probeCases) fixture.probeCases.push({ ...test, expected: await oracle.runProbe(structuredClone(test)) });
for (const test of metadataCases) fixture.metadataCases.push({ ...test, expected: await oracle.runMetadata(structuredClone(test)) });
const output = `${JSON.stringify(fixture, null, 2)}\n`;
const outputPath = fileURLToPath(new URL('../../crates/platforms/posix/tests/fixtures/mcp_negotiation_286_oracle.json', import.meta.url));
if (process.argv.includes('--check')) {
  if (readFileSync(outputPath, 'utf8') !== output) throw new Error('MCP negotiation oracle fixture differs');
  console.log(`MCP negotiation fixtures match Claude Code ${fixture.version} (${sha256})`);
} else {
  writeFileSync(outputPath, output);
  console.log(outputPath);
}
