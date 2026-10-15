#!/usr/bin/env node
// Source-pinned Claude Code 2.1.289 managed MCP tool.describe routing probe.
// Usage: node scripts/tests/mods_managed_mcp_describe_289_probe.mjs [native-binary]
// The binary is read only. Pure helpers execute with controlled registry/next
// callbacks; this is not a Native app or plugin-dispatch launch.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';

const binaryPath = process.argv[2] ?? '/tmp/harness-claude-code-2.1.289-darwin-arm64';
const binary = readFileSync(binaryPath);
const sha256 = value => createHash('sha256').update(value).digest('hex');
assert.equal(sha256(binary), '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69',
  'unexpected Claude Code 2.1.289 executable');

const ranges = [];
function take(label, start, end, expectedHash, startText, endText) {
  assert(start >= 0 && end > start, `${label}: invalid byte range`);
  if (startText !== undefined) {
    assert.equal(binary.subarray(start, start + Buffer.byteLength(startText)).toString(),
      startText, `${label}: start marker moved`);
  }
  if (endText !== undefined) {
    assert.equal(binary.subarray(end, end + Buffer.byteLength(endText)).toString(),
      endText, `${label}: end marker moved`);
  }
  const bytes = binary.subarray(start, end);
  const digest = sha256(bytes);
  assert.equal(digest, expectedHash, `${label}: source hash changed`);
  ranges.push({label, start, end, sha256: digest});
  return bytes.toString('utf8');
}

const scopeClassifierSource = take(
  'MCP provider scopes and hQ', 187631796, 187632304,
  'ab6d301969e0561b317a782857d37ff63080cbacddade53791010f349530e4f4',
  'var $st=Object.freeze(["enterprise","managed"]);', 'function ZLn(e){',
);
const pluginOwnerLookupSource = take(
  'installed plugin MCP owner lookup zLo', 187689722, 187689937,
  '9087feb66f578a56a77b8a4cbe7a1292d2a818e534657b8d20e9bb3339191380',
  'function zLo(e){', 'var pit=',
);
const toolDescribeProviderSource = take(
  'tool.describe MCP provider resolver sFn', 187700969, 187701489,
  '8a3f3a585106e02d5299a74dd529a08b62bb5479d86d125448e242a48a0309ae',
  'async function sFn(e){', 'var Nit=',
);
const toolDescribeInvocationSource = take(
  'tool.describe hook invocation and change measurement', 187701489, 187702322,
  '7223a00d2215b01f84a8781171710dd19c1452a08071aa7b13ebbfa878e4af2b',
  'var Nit=(e)=>Cc("tool.describe"', 'rls(lFn);',
);
const secureTierRouterSource = take(
  'sec-default tier allowlist and description router d', 213012004, 213012125,
  '2099f21dce425f816c22308841153667fe2b6ac9ba291f8d25d941f1b87ba20a',
  'var S=Object.freeze(["user","builtin","core"]);function d(e,r){', 'function F(e,r=Date.now){',
);
const tierFloorSource = take(
  'Native hook tier order and next.to floor helpers', 182598362, 182599150,
  'cde7a69c3c700a68abe5bee413b0a15033254d88da634250524584815bd19540',
  'var mc=["prepend","user","append","builtin","core"];', 'function mt(',
);
const nextToFloorBuilderSource = take(
  'Native next.to validation and floor builder Fn', 182766661, 182767293,
  '203c737b352a7c57c8dc478c1642278766e944a8f38df5369c5e391557f443b1',
  'function Fn(e,t,o){', 'function Io(',
);
const nextToFloorApplicationSource = take(
  'Native next.to applies the computed floor below the hook', 182769578, 182770499,
  '1590dbaa7f986d05d3cb7337d32acbce847670f8a4b3d94c7a3773090c6d5008',
  'function By({handler:e,below:t,site:o,e:r,budget:n,downstreamSignal:s,state:i,run:p,floors:a,tier:f}){', 'var V4r=',
);
const nextToFloorMatchSource = take(
  'Native hook-chain tier-floor bypass matcher Mn', 182763890, 182764378,
  '5ebc572353233a62b8dfef90b82db03316ac9a8e41535dfb08b4a437719ac9b8',
  'function Mn({handler:e,tier:t,index:o,site:r,e:n,descent:s}){', 'import{isProxy as sy}',
);
const secureDescriptionRegistrationSource = take(
  'sec-default tool.describe handler registration', 213013622, 213013657,
  '1535cc5b3906e8d91f807c1f31c81793450358f34d1eebfa6f4d7338dff2606e',
  'e("tool.describe",(n,o,t)=>d(o,t))', 'e("command.describe"',
);
const secureToolRegistrationSource = take(
  'separate sec-default tool.register handler', 213013761, 213013957,
  '53401a0fd27d6d048891f0253480bff984f22ffe5b17337a95ee55893c696744',
  'e("tool.register",async(n,o,t)=>{', 'e("tool.list"',
);
const secureDefaultSeatSource = take(
  'sec-default seat description', 213014666, 213015201,
  '8f495167f46b52193c2cdeb1188a09df05915b317e1bf263a4bd5a15b270a7e0',
  'var ge=()=>uS({name:nXe,description:"Security default:', 'export{E as MOD_FOLDER',
);
const toolDescribeContractSource = take(
  'tool.describe provider pin and description schema', 182736910, 182737832,
  '8cc850d27cf57ed5cb62a3cfc64776e009adccd68dd0684e0c488bb4c86ede02',
  'var ua={event:"tool.describe"', ';var ctr=',
);
const authenticateStubSource = take(
  'MCP authenticate OAuth placeholder builder', 205914815, 205919979,
  'dd944f83c9c8029eec952619239d17b79970aea407e0888d3d2c4db002b24b6a',
  'function G(e,r,n){', 'function V(e,r){',
);
const completeAuthenticationStubSource = take(
  'MCP complete-authentication OAuth placeholder builder', 205919979, 205923097,
  '0845d93d147661dca2ec7964ad5b494809b4edc53f0b65d82ba65ef559e1566e',
  'function V(e,r){', 'function zKe(n){',
);
const serverScopeSource = take(
  'MCP configured-source projection Tj', 187741909, 187742266,
  '247a6d92cf8a6689d960ae6ceea5b1beb31797af70f1addf032d71ae4778244e',
  'function Tj(e,n){if(n.type==="sdk"', 'function glt(',
);
const pluginSourcePredicateSource = take(
  'MCP pluginSource predicate ML', 179781389, 179781436,
  '6b27b1085141d98c0cfe644f4ccceb7c8b3ea248ffcbfeedbcc6a7c7cd84d8b8',
  'function ML(e){return e?.pluginSource!==void 0}', 'function ntt(',
);
const nativeModulesFilterSource = take(
  'sec-default excluded from user-hook change measurement', 187121157, 187121353,
  '99549cd0da8db075981557fc1e708d9d94099cf67d4b6d9febe81005aa3fa9b6',
  'var Gue=(o)=>o.link!==void 0;var r8=', 'export{Gue,r8};',
);

// Execute Tj with exact Native code for ordinary stdio configs. A false
// MOr stub is unreachable for these non-SDK inputs; Xj is a controlled empty
// plugin-owner registry. ML and mlt are part of the pinned source.
const sourceContext = vm.createContext({
  Xj: {ownerOfKey: () => undefined},
  MOr: () => false,
});
vm.runInContext(`${pluginSourcePredicateSource}\n${serverScopeSource}\nthis.nativeSource=Tj;`, sourceContext);
const nativeSource = config => sourceContext.nativeSource('acme', config);

// Execute actual Native hQ+sFn. Only the plugin-record reader is controlled:
// null means no installed-plugin record; rejected models the unreadable-record
// fail-closed path. No filesystem, settings, MCP server, or auth API is called.
const providerContext = vm.createContext({
  f3: () => undefined,
  $r: () => '',
  aF: () => [],
  Xj: {ownerOfKey: () => undefined},
  qFe: plugin => Object.freeze({plugin, tier: 'user'}),
  zLo: async () => null,
  as: () => null,
  u2e: Object.freeze({plugin: 'engine', tier: 'core'}),
  t: () => {},
  l: value => String(value),
});
vm.runInContext(`${scopeClassifierSource}\n${toolDescribeProviderSource}\nthis.nativeProvider=sFn;`, providerContext);

const providerFor = async source => providerContext.nativeProvider({
  isMcp: true,
  name: 'mcp__acme__authenticate',
  mcpInfo: {serverName: 'acme', toolName: 'authenticate', isAuthStub: true, source},
});
const providers = {};
for (const scope of ['managed', 'enterprise', 'user', 'project', 'dynamic']) {
  providers[scope] = await providerFor(nativeSource({type: 'stdio', scope}));
}
providerContext.zLo = async () => { throw new Error('controlled unreadable plugin record'); };
providers.unreadablePluginRecord = await providerFor('user');
providerContext.zLo = async () => null;

assert.deepEqual(JSON.parse(JSON.stringify(providers.managed)), {plugin: 'mcp:acme', tier: 'prepend'});
assert.deepEqual(JSON.parse(JSON.stringify(providers.enterprise)), {plugin: 'mcp:acme', tier: 'prepend'});
assert.deepEqual(JSON.parse(JSON.stringify(providers.user)), {plugin: 'mcp:acme', tier: 'user'});
assert.deepEqual(JSON.parse(JSON.stringify(providers.project)), {plugin: 'mcp:acme', tier: 'user'});
assert.deepEqual(JSON.parse(JSON.stringify(providers.dynamic)), {plugin: 'mcp:acme', tier: 'user'});
assert.deepEqual(JSON.parse(JSON.stringify(providers.unreadablePluginRecord)), {plugin: 'mcp:acme', tier: 'prepend'});

// `tool.describe` itself routes by the immutable provider tier. Run the exact
// Native d helper with a recording next callback; next implementations are
// controlled and only reveal whether Native asks for normal continuation or
// the append tier.
const tierContext = vm.createContext({});
vm.runInContext(`${secureTierRouterSource}\nthis.nativeRoute=d;`, tierContext);
function routeFor(tier) {
  const calls = [];
  const next = event => {
    calls.push('next');
    return {description: `rewritten:${event.description}`};
  };
  next.to = (_event, target) => {
    calls.push(`to:${target}`);
    return {description: 'append-layer'};
  };
  const result = tierContext.nativeRoute(
    {description: 'base', provider: {tier}}, next,
  );
  return {calls, description: result.description};
}
const routes = Object.fromEntries(
  ['prepend', 'user', 'builtin', 'core', 'append'].map(tier => [tier, routeFor(tier)]),
);
assert.deepEqual(routes, {
  prepend: {calls: ['to:append'], description: 'append-layer'},
  user: {calls: ['next'], description: 'rewritten:base'},
  builtin: {calls: ['next'], description: 'rewritten:base'},
  core: {calls: ['next'], description: 'rewritten:base'},
  append: {calls: ['to:append'], description: 'append-layer'},
});
const floorContext = vm.createContext({Ie: Error});
vm.runInContext(`${tierFloorSource}\n${nextToFloorBuilderSource}` +
  '\nthis.nativeFloors=Fn;this.nativeFloorOwner=Nas;', floorContext);
const prependToAppendFloor = floorContext.nativeFloors([], ['append'], {
  plugin: 'cc-plugin-sec-default', tier: 'prepend',
});
assert.deepEqual(JSON.parse(JSON.stringify(prependToAppendFloor)), [
  {from: 'prepend', to: 'append', plugin: 'cc-plugin-sec-default'},
]);
assert.equal(floorContext.nativeFloorOwner(prependToAppendFloor, 'user'), 'cc-plugin-sec-default');
assert.equal(floorContext.nativeFloorOwner(prependToAppendFloor, 'append'), undefined);
assert.match(nextToFloorApplicationSource, /to:async\(y,u\)=>m\(c\(y\),void 0,d\(u\)\)/);
assert.match(nextToFloorMatchSource, /Nas\(p,x\)/);

// Static checks that the two auth tools are Native-generated MCP tools, carry
// the config-derived source as mcpInfo.source, and set isAuthStub. The large
// constructors are intentionally not executed: they depend on OAuth/session
// services outside this controlled probe.
for (const source of [authenticateStubSource, completeAuthenticationStubSource]) {
  assert.match(source, /isMcp:!0/);
  assert.match(source, /source:Tj\(e,r\)/);
  assert.match(source, /isAuthStub:!0/);
  assert.match(source, /async description\(\)/);
}
assert.match(authenticateStubSource, /toolName:e1t/);
assert.match(completeAuthenticationStubSource, /toolName:t1t/);

assert.match(toolDescribeContractSource, /restoreArgument:\(e,t\)=>nL\(\["provider"\],e,t\)/);
assert.match(toolDescribeContractSource, /Bn\(e\.provider\)!==Bn\(t\.provider\)/);
assert.match(toolDescribeContractSource, /typeof e\.description==="string"/);
assert.match(secureDescriptionRegistrationSource, /d\(o,t\)/);
assert.match(secureDefaultSeatSource, /seated outermost/);
assert.match(secureDefaultSeatSource, /out of reach of the plugins a person installs/);
assert.match(toolDescribeInvocationSource, /ya\("tool\.describe"/);
assert.match(secureToolRegistrationSource, /tool\.register/);

const result = {
  nativeBinary: {path: binaryPath, sha256: sha256(binary)},
  sourceRanges: ranges,
  managedScopes: {
    managed: nativeSource({type: 'stdio', scope: 'managed'}),
    enterprise: nativeSource({type: 'stdio', scope: 'enterprise'}),
    user: nativeSource({type: 'stdio', scope: 'user'}),
    dynamic: nativeSource({type: 'stdio', scope: 'dynamic'}),
  },
  generatedAuthStubProviderResults: providers,
  secDefaultToolDescribeRoutes: routes,
  secDefaultPrependToAppendFloor: {
    floor: JSON.parse(JSON.stringify(prependToAppendFloor)),
    bypassedTierOwner: floorContext.nativeFloorOwner(prependToAppendFloor, 'user'),
    appendTierOwner: floorContext.nativeFloorOwner(prependToAppendFloor, 'append') ?? null,
  },
  sourceOnlyChecks: {
    authPlaceholderBuildersExecuted: false,
    nativePluginDispatcherExecuted: false,
    hasToolAddHookLiteral: binary.includes(Buffer.from('tool.add')),
    hasToolDefineHookLiteral: binary.includes(Buffer.from('tool.define')),
    hasToolRegisterHookLiteral: binary.includes(Buffer.from('tool.register')),
    hasToolDescribeHookLiteral: binary.includes(Buffer.from('tool.describe')),
  },
};
process.stdout.write(`${JSON.stringify(result, null, 2)}\n`);
