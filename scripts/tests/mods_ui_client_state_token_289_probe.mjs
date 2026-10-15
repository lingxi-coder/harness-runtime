#!/usr/bin/env node
// Static source-pinned audit of Native 2.1.289 Client render-version inputs.
// This reads the pinned executable as bytes; it does not execute Claude Code.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync} from 'node:fs';

const binaryPath = process.argv[2];
assert(binaryPath, 'usage: node scripts/tests/mods_ui_client_state_token_289_probe.mjs <2.1.289-binary>');
const sha256 = value => createHash('sha256').update(value).digest('hex');
const binary = readFileSync(binaryPath);
const binarySha256 = '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69';
assert.equal(sha256(binary), binarySha256, 'unexpected Claude Code 2.1.289 macOS arm64 binary SHA-256');

const sourcePins = {
  renderVersionsInit: [183434130, 183434270, '1e746e1c6ea71905e3193763daaf8affdf42704721cf65e4d92c50b42d0ea3bf'],
  uiRenderAndParticipants: [186021995, 186022276, '9b29270db93e83f16b29724811a4f32d62065c86b40f42ecbb72d1841e652057'],
  activeSitePace: [186022313, 186023106, 'ef1b2030c1d8fece3ef884d7d1f6c5ce604841ecc143671bb8623e5330289c07'],
  globalIncrementAndPluginPace: [186061240, 186062228, '2b0a4a4e5a053c37291c4c80c2083fc5e95298c0c050b6af7cbd078c5389ac18'],
  siteSignature: [186062228, 186063325, '818a8d15c7accdf4a9fa38d1e601af04f57c248552408b3a81743140edcb0d0f'],
  stateFold: [186063325, 186064944, 'c877749da950b7d0ec54d79c8ff715ae9ac08f143aaf0341f72ff9fa2435f715'],
  stateWriteReadCommit: [186064944, 186066294, '4244600ee4db9e46f14f4b072a7d6a3e90164e356a3356aa7d55d96d2919c481'],
  stateGetSet: [186067819, 186068250, '1c54837cc7b798763e4c4a20e4f262dc8139c9a0288d32b2cb9c172cbfe7f505'],
  renderDispatch: [186071887, 186073154, '74b94db6a3964c9377946bb48c275312ec47b099ae1c6a743a01b5a109c3db65'],
  composedSignature: [186073154, 186073490, 'c4ec4302355b4d7c23103b8f1826d441fb6ebda69fefa165ce4c38e3c6cb9232'],
  renderCacheHelpers: [186073490, 186074296, 'fdd80ca8e71ef804b387adcb6d5039eb157afe2094be29a2ba20fb54921e0c8b'],
  renderCache: [186074296, 186075934, 'b390f9f8b83d4c13b5b572426358a958a0a7ca358867e5a4e7c03a90032f7e23'],
  faultTokenRegistry: [186076108, 186077163, '02fe14d3b10c7698a6299396888f2217e58a64025df9a53bfd7085395daba342'],
  clientWrapper: [186098500, 186099268, 'bfe3b1722480f6203de10c29e765bf364284bbacb803d0439731d38c4f37ad8e'],
  invalidateAndSet: [197876875, 197877633, '9f22c811fb492a717b0a185369dbe6fb517aa3a2e30276255a547623c0cdf85c'],
  haeGlobalBump: [188916757, 188916837, '9dec22c0f6f9e9bc8a351d107dc4881cc6a1f818b214d4ea63278dad92406e0b'],
  hooksTableResolve: [188928006, 188928329, 'edefc78a7f904814f965bc3fe3a2821b53a77830a1e03fcb6b2d131c652f9615'],
  hooksSetCommit: [188928329, 188933129, '3e0ad960ad3e4c8be21ea782be73d859de46288a0caae4441f7fc6a397d44c4c'],
  hooksUnload: [188933129, 188933775, '4fa36b175ced8f8d083325a9a4d768ad5118bc618bf67d3427b37ad87f3012b1'],
  hooksReload: [188933775, 188939027, '653f36774275e1acba95dcba4df1997c971a2d5d7bbc35e378889e720143b211'],
  hooksRespawn: [188939027, 188941084, '3c236afca7001d683520db42db6e697d99cdbda47c6e15d122276c16d604ac3e'],
  viewportProducer: [209909018, 209909442, '76bea93643ad98d3130e2f92f7f9114eae31c59a7c82fb22349cd91bf2a57bb3'],
  cwdRehome: [215372065, 215375532, 'f4382e5acc5163da653fc48d54ee0c1b96a54fec238075346dda941c1d9117bf'],
};

const windows = {};
for (const [name, [start, end, expectedHash]] of Object.entries(sourcePins)) {
  const bytes = binary.subarray(start, end);
  assert.equal(bytes.length, end - start, `${name} source range is truncated`);
  assert.equal(sha256(bytes), expectedHash, `${name} source window changed`);
  windows[name] = bytes.toString('utf8');
}

function allOffsets(needle) {
  const bytes = Buffer.from(needle, 'utf8');
  const offsets = [];
  for (let from = 0; ; ) {
    const offset = binary.indexOf(bytes, from);
    if (offset < 0) return offsets;
    offsets.push(offset);
    from = offset + 1;
  }
}

const renderVersionsOffsets = [79124044, 183434161, 186061256];
assert.deepEqual(allOffsets('renderVersions'), renderVersionsOffsets,
  'unexpected renderVersions store/getter references');
const mapWriterOffsets = [186061286, 186063156];
assert.deepEqual(allOffsets('HD().setState'), mapWriterOffsets,
  'unexpected direct renderVersions store writers');
const globalBumpOffsets = [188916772, 188933566, 188941076, 215373556];
assert.deepEqual(allOffsets('kmn()'), globalBumpOffsets,
  'unexpected global Fv bump call sites');

const storeInit = windows.renderVersionsInit;
assert(storeInit.includes('renderVersions:Ds(new Map)'));
const globalCounter = windows.globalIncrementAndPluginPace;
assert(globalCounter.includes('var HD=()=>dt().renderVersions'));
assert(globalCounter.includes('r.set(e,(n.get(e)??0)+1)'));
assert(globalCounter.includes('function E1t(e,n="steady"){bh(e===void 0?Fv:wh(e,n))}'));
assert(globalCounter.includes('function jis(e=Date.now,n=I2)'));
assert(globalCounter.includes('var Cmt=100'));
assert(windows.uiRenderAndParticipants.includes('var Fv="ui.render"'));
assert(windows.uiRenderAndParticipants.includes('dt().loadedModules.filter'));

const signature = windows.siteSignature;
assert(signature.includes('var kmn=()=>E1t()'));
assert(signature.includes('var Tmn=`${Fv} viewport`'));
assert(signature.includes('let s=String(e.get(Fv)??0)'));
assert(signature.includes('e.get(wh(h,r))??0'));
assert(signature.includes('g!==Tmn&&!_Y(g)&&!HC(g)'));
assert(signature.includes('var MD=(e)=>`${e.surface}\\x00${e.component}\\x00${e.requestId}`'));
assert(signature.includes('function Qc(e,n){if(e.length===0)return;n.bumps+=1'));
assert(windows.activeSitePace.includes('var eh=34'));
assert(windows.activeSitePace.includes('function cS(e)'));
assert(windows.stateFold.includes('var RS={steady:Cmt,live:eh}'));
assert(windows.stateFold.includes('g.pending=setTimeout(ES,w,h,n)'));
assert(windows.stateGetSet.includes('versionsRead.set(r,g)'));
assert(windows.stateWriteReadCommit.includes('function H9n(e,n)'));
assert(windows.stateWriteReadCommit.includes('$C(e)'));
assert(windows.stateWriteReadCommit.includes('if(b)Zc([n])'));
assert(windows.invalidateAndSet.includes('E1t(r.plugin,"steady"),E1t(r.plugin,"live")'));
assert(windows.invalidateAndSet.includes('Amt("steady",s),Amt("live",s)'));

const render = windows.renderDispatch;
const faultRegistry = windows.faultTokenRegistry;
const wrapper = windows.clientWrapper;
assert(render.includes('if(e.requestId!==""&&!n.aborted)jC(g)'));
assert(windows.composedSignature.includes('var EUe=(e,n)=>DY(Rh(e,uUe(n),cS(n)),e,MD(n))'));
assert(windows.renderCache.includes('let w=MD(h),x=u8n(h,b),A=Lue(h)'));
assert(windows.renderCache.includes('X=FBr({input:h,signal:g,origin:b})'));
assert(windows.renderCache.includes('if(Q.settled=Z,!V)U2(B,Date.now()-D)'));
assert(windows.renderCache.includes('W2(B,Date.now()-D)'));
assert(faultRegistry.includes('var ZC=(e,n)=>()=>EUe(e,n)'));
assert(faultRegistry.includes('version:ZC(HD().getState(),id({component:s,requestId:h}))'));
assert(faultRegistry.includes('let x=g()'));
assert(wrapper.includes('let n=e.props,r=GBr(e)'));
assert(wrapper.includes('await Kre.uiRender(e'));
assert(wrapper.includes('if(r!==void 0)is.record(r,x)'));

const hookSet = windows.hooksSetCommit;
const unload = windows.hooksUnload;
const reload = windows.hooksReload;
const respawn = windows.hooksRespawn;
const cwd = windows.cwdRehome;
assert(hookSet.includes('Bt.length!==0||Pt.length!==0)Hae()'));
assert(windows.haeGlobalBump.includes('function Hae(){kmn(),'));
assert(windows.hooksTableResolve.includes('resolveTables'));
assert(unload.includes('$ae(e),Hae(),dse(s.loadedModules)'));
assert(unload.includes('await Kae(h),kmn()'));
assert(reload.includes('await Kae(Ge)'));
assert(reload.includes('Hae(),dse(r.loadedModules)'));
assert(respawn.includes('$ae(e),Hae(),dse(h),await Kae(h),kmn()'));
assert(cwd.includes('je.refreshConfig(),kmn(),i("tengu_cd_command"'));
assert(windows.viewportProducer.includes('setTimeout((Me,Ie)=>{Me.current=Ie,Wis()},Cmt'));

const report = {
  binarySha256,
  sourceWindowCount: Object.keys(sourcePins).length,
  renderVersionsReferences: renderVersionsOffsets,
  directVersionStoreWriters: mapWriterOffsets,
  globalFvBumpCallSites: globalBumpOffsets,
  lifecycleSites: {
    hooksSetCommit: 'J0t/xe calls Hae when old or new loaded list is nonempty',
    pluginUnload: 'Hae before Kae, then kmn after Kae',
    pluginReload: 'Hae after Kae and loaded-module replacement',
    hooksWorkerRespawn: 'Hae before Kae, then kmn after Kae',
    cwdRehome: 'kmn after settings/skills rehome and before tengu_cd_command',
  },
  timing: {
    globalFvWriter: 'immediate map increment per reached call; no debounce in kmn/E1t/bh',
    pluginInvalidation: {steadyMs: 100, liveMs: 34},
    stateReaderFold: {steadyMs: 100, liveMs: 34, stateSetFlush: 'both folds immediately'},
    terminalViewport: {debounceMs: 100, key: 'ui.render viewport; excluded from Rh global key'},
    slowPlugin: 'cached live evaluation duration >34ms; abort duration >16ms; both slow marks last 1s',
  },
  sampling: {
    start: 'jBr samples Lue(site) before FBr dispatch; Client registry begin is before uiRender',
    completed: 'FBr commits read set after non-aborted identified draw; Client registry records current signature after render',
    cacheReuse: 'checks fresh Lue(site) against captured composite signature',
  },
  result: 'all pinned source assertions passed; static binary audit only',
};
console.log(JSON.stringify(report, null, 2));
