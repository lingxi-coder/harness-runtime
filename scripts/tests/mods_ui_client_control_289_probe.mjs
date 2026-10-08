#!/usr/bin/env node
// Source-pinned Native 2.1.289 Client control API, JSX transform, and worker
// differential probe. Native selected functions execute from the pinned binary;
// actual Mods worker RPC covers ui.press.preflight only. The SDK router, real
// Bun.Transpiler, plugin filesystem, and Electron frame do not execute here.
import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {createHash} from 'node:crypto';
import {readFileSync} from 'node:fs';
import path from 'node:path';
import {createInterface} from 'node:readline';
import {fileURLToPath, pathToFileURL} from 'node:url';
import vm from 'node:vm';

const binaryPath = process.argv[2];
assert(binaryPath, 'usage: node scripts/tests/mods_ui_client_control_289_probe.mjs <2.1.289-binary>');
const sha256 = value => createHash('sha256').update(value).digest('hex');
const binary = readFileSync(binaryPath);
assert.equal(
  sha256(binary),
  '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69',
  'unexpected Claude Code 2.1.289 macOS arm64 binary SHA-256',
);

const pins = {
  requestSchemas: {
    start: 181089372,
    end: 181102570,
    sha256: 'ee256e6e77748b2d75827de4228f70c55bd7d11c0a3102842c51da28d27123f8',
    marker: 'GAn=f(()=>',
  },
  nativeSurfaceTreeSchema: {
    start: 181078531,
    end: 181085162,
    sha256: 'e519b2155e05d32163c9dc03dff0fe53ed2fad1e6a64b1b97bb88671345558b5',
    marker: 'Fw=f(()=>Fe([u({type:j(["Box","Text","div","span","b"])',
  },
  parentPressSchema: {
    start: 181093166,
    end: 181094724,
    sha256: '55d745248e067f85323bfd06f622554e9ef7d6f40b52590ae29a1dda06aa55e8',
    marker: 'zAn=f(()=>u({subtype:R("ui_press")',
  },
  parentPressResult: {
    start: 181094724,
    end: 181095163,
    sha256: 'da9d11b05c398ac8642c4c4bb0facc9885a06915f35a63a9cfdccae90a8739df',
    marker: 'jW=f(()=>u({handled:H()',
  },
  parentInputSchema: {
    start: 181102570,
    end: 181104852,
    sha256: '5832c72decd5420b82894ffe47bab65f138a5927ba84050eb9f471b1ff283d24',
    marker: 'XAn=f(()=>u({subtype:R("ui_input")',
  },
  parentInputResult: {
    start: 181104852,
    end: 181105390,
    sha256: '536d95c2ea5190bdfbbfe9123a1987b300f911f71a0d246a10271d50b8df9a07',
    marker: 'FW=f(()=>u({handled:H()',
  },
  parentSelectSchema: {
    start: 181105390,
    end: 181107323,
    sha256: '553d952e0ce2270c2fac969f50934ab7edf51f8713e0aeaecad294e06817d8f1',
    marker: 'JAn=f(()=>u({subtype:R("ui_select")',
  },
  parentSelectResult: {
    start: 181107323,
    end: 181107912,
    sha256: '97be028937461d373e47b7410da7c4e06b9578dec3903bafb543cd05a509c8d4',
    marker: 'YW=f(()=>u({handled:H()',
  },
  zodObjectParser: {
    start: 181659962,
    end: 181660990,
    sha256: '62e4a9c6cb6a5408889593eed2edc7bf73484506a79e0ad805a55994a935530d',
    marker: 'if(!(this._def.catchall instanceof j&&this._def.unknownKeys==="strip"))',
  },
  zodDefaultObjectFactory: {
    start: 181662427,
    end: 181662752,
    sha256: 'e3bc6f469229f61fa07c0d718083e3cf1e881420372d81faaf82089b069d072d',
    marker: 'y.create=(e,t)=>new y({shape:()=>e,unknownKeys:"strip"',
  },
  benchSchema: {
    start: 181085162,
    end: 181085412,
    sha256: '0969b917e76001dd59adb16afa7b9c9854cf3715e6e8bb225741302b917823a0',
    marker: 'Yw=f(()=>u({seq:C().int(),t0:C()})',
  },
  viewportSchema: {
    start: 181085412,
    end: 181086214,
    sha256: 'a077be97c98c92bf42677da89b74dc35abe602ef27ae03b17b9c116fe911d9f1',
    marker: 'Ww=f(()=>u({columns:C().int().positive(),rows:C().int().positive()',
  },
  onScreenSchema: {
    start: 181086214,
    end: 181086681,
    sha256: '72555e350f6041534fd6d8d00d6b9dfb8f720674c659922a6415a1327b5a58b3',
    marker: 'DW=f(()=>u({first:C().int().nonnegative()',
  },
  keyedRowSchema: {
    start: 181108151,
    end: 181108560,
    sha256: '26e2f407f90c9979aef15200edbb51075cd22098b01ac3540d9a9457dda3b4a1',
    marker: 'Qw=f(()=>u({plugin:o().max(Y),key:o().max(Y)',
  },
  clientIdSchema: {
    start: 181087148,
    end: 181087466,
    sha256: '419a1afa4af1297e8a07ec88b3fa78292eb89da370abcaa7ee09d523aabc987f',
    marker: 'xW=/^[A-Za-z0-9._-]{1,64}$/',
  },
  parentPressRoute: {
    start: 206550676,
    end: 206551251,
    sha256: 'dd1eb4814d86bbc63137aa0cbe664ebfdcbda649d855707a69c4219fd318de9f',
    marker: 'I.request.subtype==="ui_press"',
  },
  parentInputRoute: {
    start: 206551251,
    end: 206552029,
    sha256: '7bebcf4c22d021c1153b722a9ce60b7517c65f29c5d4f8a94b6d8b752f0502fe',
    marker: 'else if(I.request.subtype==="ui_input")',
  },
  parentSelectRoute: {
    start: 206552747,
    end: 206553490,
    sha256: '5edfc8bac420d8d7faf4c1fc94c4ef3faaa75d26941bd1955beed970461dc300',
    marker: 'else if(I.request.subtype==="ui_select")',
  },
  parentHeldHookProjection: {
    start: 186016324,
    end: 186021168,
    sha256: '3dc778e49d120ae893754570eafd3c7c7d0e2996bcf022bc96b09b2e95c7a85c',
    marker: 'var Vm=(e,n,r)=>',
  },
  parentHrefPathNormalizer: {
    start: 185185005,
    end: 185185446,
    sha256: '7ecb73ed5a8b42081a9e2bf22bd0541774dacaa30bad766d9a8e15f71b252c4d',
    marker: 'function Gpn(e){',
  },
  parentHrefMatcher: {
    start: 186019330,
    end: 186019519,
    sha256: 'b89b60722610c6b6186053e70bdcadf17a11a1c369fa743b5ffb8f508e6e0a0f',
    marker: 'function sS(e){',
  },
  parentMarkdownPressGate: {
    start: 186021168,
    end: 186021896,
    sha256: '59ec2f29a2fc34eb4d1bda5d44f52de7dd18e57de5cab9c9806d63332bed1714',
    marker: 'async function Fmn({plugin:e,handle:n,element:r,surface:s,link:h})',
  },
  parentStoredPressRun: {
    start: 182882595,
    end: 182882944,
    sha256: '27f77af7d3436701e808088be8093879b28b24d051b432813be1d44442e39091',
    marker: 'press(s,u,m){',
  },
  parentPathGuardModule: {
    start: 179194648,
    end: 179217677,
    sha256: '5c40ce72bce681eebf3a96ef40fb8d8b5611db83b5d9ef3dd5dd3a2473bbf5a0',
    marker: '// @bun @bytecode',
  },
  parentPathRegexModule: {
    start: 179170213,
    end: 179177253,
    sha256: 'b275a7b1c9df4336655bcb2ea6b89656036f9dc76378245c8e94a7684f6886ec',
    marker: '// @bun @bytecode',
  },
  parentFileUrlSafetyModule: {
    start: 185137529,
    end: 185139203,
    sha256: '9acf18b281f9841bc4d2e44daed753268b0d21a375e8b92e857a256feb2ab736',
    marker: '// @bun @bytecode',
  },
  parentGpnProducerModule: {
    start: 185141352,
    end: 185186008,
    sha256: '5dff2c86ca15a3339ef1b49c35d5870b8e4285923f662e392769cb5eddd4ecf3',
    marker: '// @bun @bytecode',
  },
  parentPathCwdModule: {
    start: 181139543,
    end: 181140726,
    sha256: '75c27cadae4b9280b7fbb579536af17ee824d50b87ed98f7e8ff11cac9331ae2',
    marker: '// @bun @bytecode',
  },
  parentProjectCwdAccessor: {
    start: 179268614,
    end: 179268680,
    sha256: '7a68c31835916f8c66924b2adc340f2207ef6cb33f743bfa115fd18cf706053a',
    marker: 'function As(e){',
  },
  parentOriginalCwdAccessor: {
    start: 179294415,
    end: 179294474,
    sha256: '233960687c5e5d1973bfa0feef8c303435d9ed1805a64aa5777088debd4d7c57',
    marker: 'function Ee(){',
  },
  parentCurrentCwdAccessor: {
    start: 179295134,
    end: 179295167,
    sha256: '683bc920c73a7905c3f8b95229f88808060e15deecceeba078611802621c562a',
    marker: 'function _P(){',
  },
  parentCwdStoreGetter: {
    start: 181140514,
    end: 181140560,
    sha256: '02bac10c928f2f7479a89727d71ef2622aede388a82500c83faa2043046477ba',
    marker: 'function BKt(){',
  },
  parentCwdAccessor: {
    start: 181140641,
    end: 181140692,
    sha256: 'f3c3c2c45bc59b05d4b7d218225f49322765d385aeb1842b0950f5ae74576350',
    marker: 'function oe(){',
  },
  parentHookElementValidator: {
    start: 182717731,
    end: 182718287,
    sha256: '07917ae03b9cd4192832b9bce73a9d027af3fdcceee32bf971eb40fd6781db31',
    marker: 'function Ge(e,t){',
  },
  parentHookInputValidator: {
    start: 182718287,
    end: 182718475,
    sha256: 'aaefca5da1cf00d92d9e0474acc3a666ba861ce45353d259525c9de7f5321a21',
    marker: 'function Kr(e,t){',
  },
  parentHookSelectValidator: {
    start: 182724883,
    end: 182724999,
    sha256: '5ab3fa1b41ecbe1c2ac907c34a0f044be62d5d474320c7f8328980107598e73a',
    marker: 'function mn(e,t){',
  },
  parentHookSiteBindings: {
    start: 182724999,
    end: 182725631,
    sha256: '137629b4b649e8206fb7b086a65be49b7bbb7609b8f359d9ff6a309aae479858',
    marker: 'var Hi=Pt("ui.input",Kr);',
  },
  parentHookNextArgumentCheck: {
    start: 182766211,
    end: 182766661,
    sha256: 'f008611818b5a06f09bce6fb421e937c46ad427f344a9a652fa616bb7c93de1b',
    marker: 'function lt({handler:e,site:t,e:o},r){',
  },
  parentHookNextGenericArgumentCheck: {
    start: 180147250,
    end: 180147395,
    sha256: '06c05fd7a76fee84d7139947a944ab469112dd6d0ddcad4b6fa52a3b93e9f254',
    marker: 'function iGt(r,o){',
  },
  parentHookNextCall: {
    start: 182769578,
    end: 182770499,
    sha256: '1590dbaa7f986d05d3cb7337d32acbce847670f8a4b3d94c7a3773090c6d5008',
    marker: 'function By({handler:e,below:t,site:o,e:r,budget:n,downstreamSignal:s,state:i,run:p,floors:a,tier:f})',
  },
  hoverAllowlist: {
    start: 182640494,
    end: 182642346,
    sha256: '189e9b120f42998993db2274664433b3c59f4bbf56ada3e2596cd11152957952',
    marker: 'var X4r={Box:new Set(',
  },
  hoverValidator: {
    start: 182642346,
    end: 182644002,
    sha256: '1a54a31afaa03d776bf0cb0e090fba91942fcf6a625dbe8ed18617886d48490c',
    marker: 'function Y6t(e,t){',
  },
  hoverScopeValidator: {
    start: 186030557,
    end: 186032757,
    sha256: 'f8bc9932d329ae4b5e759920bff183cae556a641d960a8a6cda9d551c29a8a9c',
    marker: 'function pBr(e,n,r){',
  },
  parentResolverHoverGroup: {
    start: 182871916,
    end: 182874446,
    sha256: '69db5968532d68a604a7536f2813d1192d16062c52d17f62650276c45a6a2201',
    marker: 'var Re=(e,r)=>({...e,group:{plugin:r}});',
  },
  uiInvalidateSchema: {
    start: 180875737,
    end: 180877101,
    sha256: '01a251bab24d3f9bee03259064fadf0abb3b2f7719e5edde016164a7b1d60751',
    marker: 'FF=f(()=>u({type:R("system"),subtype:R("ui_invalidate")',
  },
  uiRenderHandler: {
    start: 206548795,
    end: 206556461,
    sha256: '6ebc028a516fa23815de8e7cd50f27a2a4726e8a46cfd62b3926cd8800ea4d2e',
    marker: 'else if(I.request.subtype==="ui_render")',
  },
  clientModuleHandler: {
    start: 206556461,
    end: 206556839,
    sha256: 'f502c86eca39daf528578150aeadb2a5e83e12e0523b2cfaad7644ec892dacaf',
    marker: 'else if(I.request.subtype==="ui_client_module")',
  },
  clientPressHandler: {
    start: 206556839,
    end: 206557467,
    sha256: '5c87b6306cc1bca41e39f3600fe8b98d112f4f8099b0b34ba10206fdf59d8299',
    marker: 'else if(I.request.subtype==="ui_client_press")',
  },
  uiMessageHandler: {
    start: 206557467,
    end: 206557994,
    sha256: '40474620d0196a0cf2528da405e729b56dae47fa6862c8787b6dc458bd25e57b',
    marker: 'else if(I.request.subtype==="ui_message")',
  },
  clientFaultHandler: {
    start: 206557994,
    end: 206558132,
    sha256: '251e25e6115f445e9d3c9101815ddb15ab8ece4dfc97f56c7b89f095aa3605dc',
    marker: 'else if(I.request.subtype==="ui_client_fault")',
  },
  sdkInvalidateProjection: {
    start: 206503857,
    end: 206504073,
    sha256: '0bc14b246d52f0a3c47b50f917363e1c5123b9492234a14219219ab791cef789',
    marker: 'let Db=E1e.onInvalidate',
  },
  surfaceManifestBuilder: {
    start: 186093994,
    end: 186094471,
    sha256: '6d103388f4b0a54aceb896f749dd961dad9b7ee75579dd95bcae51c1c01c40ba',
    marker: 'function EO(e,n){',
  },
  manifestStringifierImport: {
    start: 185589940,
    end: 185590050,
    sha256: '6adda6c5e4b6908282d1751f362accb17eefaf87ca83c89bcb3e60dfa0369b62',
    marker: 'import{ah,Wp,_t,S,LM,J,Zo,Bh,n4t,cPn,Rf,PV,bo,JL,lX,Ro,jh,se,OGe,s4t,IV,t}from"/$bunfs/root/chunk-gsc8spc7.js"',
  },
  manifestStringifierResource: {
    start: 179403354,
    end: 179403502,
    sha256: '18061067ea59a23be3a5f2af2ef5ee59ebf5e44442240537411a40f1ce580cd6',
    marker: 'var Pr={[Symbol.dispose](){}};function Lr(){return Pr}var lg=Lr;function S(e,n,r){',
  },
  manifestStringifierRe: {
    start: 179404002,
    end: 179404055,
    sha256: 'a8b72d4c36d4edc7eccdab218fa91a3ef4e972104fc632afefd2c13f2114d9c2',
    marker: 'function Re(e){',
  },
  nativeJsxFactory: {
    start: 182678123,
    end: 182696349,
    sha256: '5f9aca064bd0d9eec51d9f34f8e1be3399178d302144914bc1e19c282407e14c',
    marker: 'var Ger=String.raw`',
  },
  nativeSurfaceInstaller: {
    start: 186080973,
    end: 186093793,
    sha256: 'fecdc1d1e8159a8552f15ef162b15a6c17722fb0a2c3f383efef47aef50f98d4',
    marker: 'var g8n=String.raw`((host, limits) => {',
  },
  nativeSurfaceRuntimeTemplate: {
    start: 186093794,
    end: 186093912,
    sha256: '2eda803a1675d3d8e27f5b072fcbdce99ef7cc8dd6e7dace7851ea4e2db411df',
    marker: 'var wO=`const jsx = ${Ger}',
  },
  nativeHooksTypesBinding: {
    start: 182588479,
    end: 182588546,
    sha256: '9ee49de85c75b3f595ea6f89e8272e6791e1edc3e3c9d90d80699a9f24ab32d1',
    marker: 'var et=Me("/$bunfs/root/state-library.js-wy8y4843.txt");var Etr=et;',
  },
  nativeBorderStyles: {
    start: 182629470,
    end: 182631974,
    sha256: '183bfbe7ae76be0fcf3a9beb170aa64b1b774e34c000cc55f49a3e80cf39451f',
    marker: '// @bun @bytecode',
  },
  remoteLinkActivation: {
    start: 186053374,
    end: 186053878,
    sha256: 'a89f41285ea1021e52049c0d157cdd9d962082cca36d5d867717d307114ca534',
    marker: 'function OBr(e){',
  },
  stringifierSourceModule: {
    start: 179356903,
    end: 179425602,
    sha256: '516a298c4608054e2528cc45014932c3dff3c62d796b37722cf279ba5b2fe2f8',
    marker: '// @bun @bytecode',
  },
  transpilerLoaderMap: {
    start: 179786018,
    end: 179786212,
    sha256: '40c1754c66b68f01a068205a89a15ae60e5a42eb633790c66b77d060cab6c73e',
    marker: 'var eir={".ts":"ts",".tsx":"tsx",".jsx":"jsx",".js":"js",".mjs":"js",".cjs":"js",".mts":"ts",".cts":"ts"}',
  },
  jsxPragmas: {
    start: 182562071,
    end: 182562147,
    sha256: '4bc252b29c904a0f72caca03b45aa8a8cff10b2b4f75627f6bde64d5ae89f751',
    marker: 'var Xo=`/** @jsxRuntime classic */',
  },
  transpilerLoaderSelector: {
    start: 182562147,
    end: 182562203,
    sha256: 'eb80da9cf73d870e4cd26a268f587f012b476c2044ff32bf0ff1f4002db14c92',
    marker: 'var go=(e)=>eir[itt.find((o)=>e.endsWith(o))??""]??"js";',
  },
  hookTranspiler: {
    start: 182562693,
    end: 182562890,
    sha256: '832df48bf320790ff8b7dd6591c7fae4e2560989b942fd24621ee56d14cf4579',
    marker: 'function qEn(e,o){',
  },
  surfaceExportSelector: {
    start: 182571149,
    end: 182571461,
    sha256: 'dd7657efa37b1e3daee66d83e0a1c8224ecb295eef254f154e56136bb28fa81f',
    marker: 'function $as(e){',
  },
  terminalPointerProducer: {
    start: 203330134,
    end: 203331241,
    sha256: 'f15a05c3988c1d0df49d2bb0c9292a73229a69a9381101358732d09784042508',
    marker: 'function no({host:o,instance:i,snapshot:p})',
  },
  terminalKeyMapper: {
    start: 203329524,
    end: 203329721,
    sha256: '088ac1efb801586aa1cc9de3a944d4802663f1f818ae3c4ff28e4030cfddf04a',
    marker: 'function ro(o,i){',
  },
  terminalKeyProducer: {
    start: 203329721,
    end: 203330134,
    sha256: '18716f93d1c975d3bc6fa99583255823c4c64acab849b2ac71308149e4a3e535',
    marker: 'function oo(o){',
  },
  nativeClientEventConstructors: {
    start: 186019030,
    end: 186021168,
    sha256: 'ec65c59273f5d262fa9764cfe083ea851b021ff3d025b11902f33f435b49c121',
    marker: 'var LP=({held:e,surface:n,kind:r,value:s})=>',
  },
  nativeClientMessagePath: {
    start: 186094985,
    end: 186095785,
    sha256: 'd2a8de7676aab7a442912ac28233657f918eb165c0f1729036be5a0ab1d36ba0',
    marker: 'var tgn=(e,n,r)=>',
  },
  nativeClientPressPath: {
    start: 186095785,
    end: 186096798,
    sha256: '1f52950cadf0e2be3155ab65c80dff5a70cdfa7460e626f91bd994da5e4ec412',
    marker: 'async function I3(e,n=is.isDrawn)',
  },
  nativeClientDrawnIdentity: {
    start: 186075822,
    end: 186077339,
    sha256: '0c2387b6318b5e81fb4b9d7a16c768524c487074f955469d65d504869416f762',
    marker: 'var JC=(e)=>({plugin:e.plugin,element:e.element,component:e.component,requestId:e.requestId,surface:"desktop"})',
  },
  nativeHookDispatch: {
    start: 182771204,
    end: 182771783,
    sha256: '3b44a868600fb9a171d87e2252ea0030d8031993c92e71571c390738cbb98f09',
    marker: 'async function qT({e,handlers:t,site:o,signal:r=new AbortController().signal',
  },
  nativeHookHandler: {
    start: 182774738,
    end: 182777607,
    sha256: 'b4beb925b40fc51afa13d415d61892c2cf6ea8203154a2c4b2f9649beaa487d0',
    marker: 'var eg=({handler:e,index:t,below:o,site:r,budgetMs:n,cutAt:s,origin:i,nothingBelow:p,answersForEngine:a})=>',
  },
  surfaceVmMount: {
    start: 197823671,
    end: 197827134,
    sha256: 'd861064afd254ecc64170b9e605048f9e746c515a78ccee9a2861bb91c2147da',
    marker: 'async function Go(r,i={})',
  },
  nativeSurfaceInstanceUnmount: {
    start: 197825881,
    end: 197825987,
    sha256: '7b2545389f1f9d12c39d928d2e424888e64bc2dc75b489e643a02c68d4baeb30',
    marker: 'function z(o){if(!k.delete(o.id))return;',
  },
  nativeClientSurfaceManager: {
    start: 197828478,
    end: 197835868,
    sha256: '3f6f220aa7176322cf61eff70e8db40567f62ddec80d0e9abb07cbc8ea96e8ad',
    marker: 'function zbr(r={})',
  },
  nativeClientFaultCleanup: {
    start: 197828673,
    end: 197829074,
    sha256: 'c8f2ffcb5ae25621a4420856935e3716bad7b4cd15dcd5cb1febc26ee5c2b941',
    marker: 'function v(e){',
  },
  nativeClientFaultTransition: {
    start: 197829242,
    end: 197829538,
    sha256: '55d8c5a1c3d6c4040c4fc4f91fc5ca5140055cf91dac93230437b1a15e0bf048',
    marker: 'function S(e,o,n){',
  },
  nativeClientRunCallbackGuard: {
    start: 197829587,
    end: 197829670,
    sha256: 'fe358904dd23ed7048708c1e4fcc91da244d97a7117eba52a8ec224e77159a3b',
    marker: 'function y(e,o){',
  },
  nativeClientScheduleGuard: {
    start: 197831192,
    end: 197831568,
    sha256: '80daddb8c360fd3de4f869763fee83c80fd7689fa03ad8c61e583481e8cbe10a',
    marker: 'function me(e){',
  },
  nativeClientResizeGate: {
    start: 197833372,
    end: 197833792,
    sha256: '951156b87b9f2b83c57e09d415a5d8d0f1bd64dd553ae386e11d3a0f2b236c8b',
    marker: 'function Z(e,o){',
  },
  nativeClientTerminalFrameFingerprint: {
    start: 197822808,
    end: 197822896,
    sha256: 'cbe8ff162996b7b908496d0f2b7b6b773c4fdc691658769058d2a36f21444d7d',
    marker: 'function L(){',
  },
  nativeTerminalClientSubtree: {
    start: 203330134,
    end: 203331237,
    sha256: 'eddca432f4f56400c999fb198e7846bf880081701220a23c3ed0299fb3491b80',
    marker: 'function no({host:o,instance:i,snapshot:p}){',
  },
  nativeTerminalClientAcquireWrapper: {
    start: 203331819,
    end: 203332137,
    sha256: '1ac9c55ef4fa67625339ac609df05783a6a000aab9b6f2c9c268598efb727b02',
    marker: 'function ao(o){',
  },
  nativeTerminalClientBoundaryWrapper: {
    start: 203332137,
    end: 203332525,
    sha256: '4150c0fadfbacefc374977ca6c38da022760f7617f01706b4604eae2704c0d27',
    marker: 'function lo(o){',
  },
  nativeClientErrorBoundary: {
    start: 203306028,
    end: 203306890,
    sha256: '2c771ae011478024c83d21d2694b6c692802edae6b602316588843b950b83711',
    marker: 'class BO extends mjr{',
  },
  nativeClientConstructor: {
    start: 182690001,
    end: 182691156,
    sha256: '2186ea7d75b18bf2f3bee33cf85526c4488ddeb2c0017c2b9d7fef3fffa2fc75',
    marker: '  function client(props, children) {',
  },
  nativeClientPathLiteralPredicate: {
    start: 182563597,
    end: 182563669,
    sha256: 'ffc36282ee4ef68f905efdbf702204fd5735f943e968624635ae62499ebb9b96',
    marker: 'var _5r=',
  },
  nativeClientPathExtensionPredicate: {
    start: 182562890,
    end: 182562931,
    sha256: '4a419caa35fb3fb865df25bbef729fdcdcfd7035d6ed7d722c50b645924f78f1',
    marker: 'var y5r=(e)=>',
  },
  nativeClientSourcePathResolver: {
    start: 182571549,
    end: 182571984,
    sha256: 'fe3b7949be319500ada3f0de551047f3d0080a87840286b803ec6ee0dfcd26d6',
    marker: 'function So(e){',
  },
  nativeClientSourceRewrite: {
    start: 182572082,
    end: 182572323,
    sha256: '6aeb259b0b326d4a400d0daa8666608d26cb814bbeb24b09341aa15be4bfe9f2',
    marker: 'function Uas(e,o,r){',
  },
  nativeClientScanKeySet: {
    start: 182572323,
    end: 182572608,
    sha256: '67d98b189290dbddbc2ebb67394e657378640025282349b0fef58d44d64c04f4',
    marker: 'function Or(e){',
  },
  nativeHookSourceModuleLoader: {
    start: 182790916,
    end: 182792417,
    sha256: '9eb587278408ea1f2755207fb6bcf0f435516c9d3a7cc171c1a551fdc298c0bc',
    marker: 'function u5r(e){',
  },
  nativeHookModuleRewriteBoundary: {
    start: 182791876,
    end: 182792131,
    sha256: 'd2a67d21a470166ef79c9ea083b9c1e1bd01ef5993c7677e2805e58d86d5f444',
    marker: 'function H(S,v){',
  },
  nativePreparedSurfaceModuleBuilder: {
    start: 188332817,
    end: 188333254,
    sha256: '84709940a4a88f09b18f50c81881ce869c42a2149308f27eaf300ffd36978eb8',
    marker: 'async function WEt(e,n,r){',
  },
  nativePluginSurfaceModulesInstall: {
    start: 188333488,
    end: 188334058,
    sha256: 'b5479aa2ac203a20235cbc0d19b5634b82b9752c21965fb52be8e096ff38d930',
    marker: 'async function qEt(e,n){',
  },
  nativeSurfaceModuleExactLookup: {
    start: 197825987,
    end: 197826377,
    sha256: 'f879cb5af6c3b70eb95c2462147447bdcef3b0edb8680e922cee133521a24934',
    marker: 'function oe(o){let n=D.get(o);',
  },
};

function source(name) {
  const pin = pins[name];
  const bytes = binary.subarray(pin.start, pin.end);
  assert.equal(bytes.length, pin.end - pin.start, `${name}: source length changed`);
  assert.equal(sha256(bytes), pin.sha256, `${name}: source hash changed`);
  const text = bytes.toString('utf8');
  assert(text.startsWith(pin.marker), `${name}: source marker moved`);
  return text;
}

function sourceBetween(label, text, startMarker, endMarker) {
  const start = text.indexOf(startMarker);
  assert(start >= 0, `${label}: missing start marker ${startMarker}`);
  const end = text.indexOf(endMarker, start + startMarker.length);
  assert(end >= 0, `${label}: missing end marker ${endMarker}`);
  return text.slice(start, end);
}

const requestSchemas = source('requestSchemas');
const nativeSurfaceTreeSchema = source('nativeSurfaceTreeSchema');
const parentPressSchema = source('parentPressSchema');
const parentPressResult = source('parentPressResult');
const parentInputSchema = source('parentInputSchema');
const parentInputResult = source('parentInputResult');
const parentSelectSchema = source('parentSelectSchema');
const parentSelectResult = source('parentSelectResult');
const zodObjectParser = source('zodObjectParser');
const zodDefaultObjectFactory = source('zodDefaultObjectFactory');
const benchSchema = source('benchSchema');
const viewportSchema = source('viewportSchema');
const onScreenSchema = source('onScreenSchema');
const keyedRowSchema = source('keyedRowSchema');
const clientIdSchema = source('clientIdSchema');
const parentPressRoute = source('parentPressRoute');
const parentInputRoute = source('parentInputRoute');
const parentSelectRoute = source('parentSelectRoute');
const parentHeldHookProjection = source('parentHeldHookProjection');
const parentHrefPathNormalizer = source('parentHrefPathNormalizer');
const parentHrefMatcher = source('parentHrefMatcher');
const parentMarkdownPressGate = source('parentMarkdownPressGate');
const parentStoredPressRun = source('parentStoredPressRun');
const parentPathGuardModule = source('parentPathGuardModule');
const parentPathRegexModule = source('parentPathRegexModule');
const parentFileUrlSafetyModule = source('parentFileUrlSafetyModule');
const parentGpnProducerModule = source('parentGpnProducerModule');
const parentPathCwdModule = source('parentPathCwdModule');
const parentProjectCwdAccessor = source('parentProjectCwdAccessor');
const parentOriginalCwdAccessor = source('parentOriginalCwdAccessor');
const parentCurrentCwdAccessor = source('parentCurrentCwdAccessor');
const parentCwdStoreGetter = source('parentCwdStoreGetter');
const parentCwdAccessor = source('parentCwdAccessor');
const parentHookElementValidator = source('parentHookElementValidator');
const parentHookInputValidator = source('parentHookInputValidator');
const parentHookSelectValidator = source('parentHookSelectValidator');
const parentHookSiteBindings = source('parentHookSiteBindings');
const parentHookNextArgumentCheck = source('parentHookNextArgumentCheck');
const parentHookNextGenericArgumentCheck = source('parentHookNextGenericArgumentCheck');
const parentHookNextCall = source('parentHookNextCall');
const hoverAllowlist = source('hoverAllowlist');
const hoverValidator = source('hoverValidator');
const hoverScopeValidator = source('hoverScopeValidator');
const parentResolverHoverGroup = source('parentResolverHoverGroup');
const invalidateSchema = source('uiInvalidateSchema');
const uiRender = source('uiRenderHandler');
const clientModule = source('clientModuleHandler');
const clientPress = source('clientPressHandler');
const uiMessage = source('uiMessageHandler');
const clientFault = source('clientFaultHandler');
const invalidationProjection = source('sdkInvalidateProjection');
const manifestBuilder = source('surfaceManifestBuilder');
const nativeClientConstructor = source('nativeClientConstructor');
const nativeClientPathLiteralPredicate = source('nativeClientPathLiteralPredicate');
const nativeClientPathExtensionPredicate = source('nativeClientPathExtensionPredicate');
const nativeClientSourcePathResolver = source('nativeClientSourcePathResolver');
const nativeClientSourceRewrite = source('nativeClientSourceRewrite');
const nativeClientScanKeySet = source('nativeClientScanKeySet');
const nativeHookSourceModuleLoader = source('nativeHookSourceModuleLoader');
const nativeHookModuleRewriteBoundary = source('nativeHookModuleRewriteBoundary');
const nativePreparedSurfaceModuleBuilder = source('nativePreparedSurfaceModuleBuilder');
const nativePluginSurfaceModulesInstall = source('nativePluginSurfaceModulesInstall');
const nativeSurfaceModuleExactLookup = source('nativeSurfaceModuleExactLookup');
const manifestStringifierImport = source('manifestStringifierImport');
const manifestStringifierResource = source('manifestStringifierResource');
const manifestStringifierRe = source('manifestStringifierRe');
const nativeJsxFactory = source('nativeJsxFactory');
const nativeSurfaceInstaller = source('nativeSurfaceInstaller');
const nativeSurfaceRuntimeTemplate = source('nativeSurfaceRuntimeTemplate');
const nativeHooksTypesBinding = source('nativeHooksTypesBinding');
const nativeBorderStyles = source('nativeBorderStyles');
const remoteLinkActivation = source('remoteLinkActivation');
const stringifierSourceModule = source('stringifierSourceModule');
const jsxPragmas = source('jsxPragmas');
const hookTranspiler = source('hookTranspiler');
const transpilerLoaderMap = source('transpilerLoaderMap');
const transpilerLoaderSelector = source('transpilerLoaderSelector');
const exportSelectorSource = source('surfaceExportSelector');
const terminalPointer = source('terminalPointerProducer');
const terminalKeyMap = source('terminalKeyMapper');
const terminalKeyProducer = source('terminalKeyProducer');
const nativeClientEventConstructors = source('nativeClientEventConstructors');
const nativeClientMessagePath = source('nativeClientMessagePath');
const nativeClientPressPath = source('nativeClientPressPath');
const nativeClientDrawnIdentity = source('nativeClientDrawnIdentity');
const nativeHookDispatch = source('nativeHookDispatch');
const nativeHookHandler = source('nativeHookHandler');
const surfaceVmMount = source('surfaceVmMount');
const nativeSurfaceInstanceUnmount = source('nativeSurfaceInstanceUnmount');
const nativeClientSurfaceManager = source('nativeClientSurfaceManager');
const nativeClientFaultCleanup = source('nativeClientFaultCleanup');
const nativeClientFaultTransition = source('nativeClientFaultTransition');
const nativeClientRunCallbackGuard = source('nativeClientRunCallbackGuard');
const nativeClientScheduleGuard = source('nativeClientScheduleGuard');
const nativeClientResizeGate = source('nativeClientResizeGate');
const nativeClientTerminalFrameFingerprint = source('nativeClientTerminalFrameFingerprint');
const nativeTerminalClientSubtree = source('nativeTerminalClientSubtree');
const nativeTerminalClientAcquireWrapper = source('nativeTerminalClientAcquireWrapper');
const nativeTerminalClientBoundaryWrapper = source('nativeTerminalClientBoundaryWrapper');
const nativeClientErrorBoundary = source('nativeClientErrorBoundary');

// ui_render is an SDK request/response, and the response projects Native
// camelCase module hashes to the SDK's client_modules field.
assert.match(requestSchemas, /GAn=f\(\(\)=>u\(\{subtype:R\("ui_render"\)/);
assert.match(requestSchemas, /client_id:G\(\)\.optional\(\)/);
assert.match(requestSchemas, /on_screen:DW\(\)\.nullable\(\)\.optional\(\)/);
assert.match(requestSchemas, /keyed:T\(mE\(\(\)=>Qw\(\)\)\)\.max\(Jw\)\.optional\(\)/);
assert.match(requestSchemas, /client_modules:ue\(o\(\),o\(\)\)\.optional\(\)/);
assert.match(uiRender, /GAn\(\)\.safeParse\(I\.request\)/);
assert.match(uiRender, /Xe\(ce,\{tree:at\.tree,props:at\.props,rewritten:at\.rewritten,hooked:pt\.hasRenderHooks\(V\.component\)/);
assert.match(uiRender, /client_modules:at\.clientModules/);
assert.match(uiRender, /V\.bench!==void 0&&\{bench:V\.bench\}/);
assert.match(zodObjectParser, /this\._def\.unknownKeys==="strip"/);
assert.match(zodDefaultObjectFactory, /unknownKeys:"strip"/);
assert.match(requestSchemas, /viewport:Ww\(\)\.optional\(\),on_screen:DW\(\)\.nullable\(\)\.optional\(\)/);
assert.match(requestSchemas, /keyed:T\(mE\(\(\)=>Qw\(\)\)\)\.max\(Jw\)\.optional\(\)[\s\S]*?bench:Yw\(\)\.optional\(\)/);
assert.match(viewportSchema, /^Ww=f\(\(\)=>u\(\{/);
assert.match(onScreenSchema, /^DW=f\(\(\)=>u\(\{/);
assert.match(benchSchema, /^Yw=f\(\(\)=>u\(\{/);
assert.match(keyedRowSchema, /^Qw=f\(\(\)=>u\(\{/);
assert.doesNotMatch(requestSchemas, /\.strict\(\)|\.passthrough\(\)/);

// These outer host controls are distinct from ui_client_press. Ordinary
// strings carry only the shown max/regex constraints; integer handles have no
// sign, range, or safe-integer refinement.
assert.match(parentPressSchema, /plugin:o\(\)\.describe/);
assert.match(parentPressSchema, /handle:C\(\)\.int\(\)/);
assert.match(parentPressSchema, /key:o\(\)\.optional\(\)/);
assert.match(parentPressSchema, /surface:W\(\)\.default\("desktop"\)/);
assert.match(parentPressSchema, /href:o\(\)\.max\(2048\)\.optional\(\)/);
assert.match(parentPressSchema, /client_id:G\(\)\.optional\(\)/);
assert.match(clientIdSchema, /xW=\/\^\[A-Za-z0-9\._-\]\{1,64\}\$\/,G=f\(\(\)=>o\(\)\.regex\(xW\)/);
assert.doesNotMatch(parentPressSchema, /plugin:o\(\)\.(?:max|min|regex)/);
assert.doesNotMatch(parentPressSchema, /handle:C\(\)\.int\(\)\.(?:positive|min|max|safe)/);
assert.match(parentPressResult, /element:o\(\)\.optional\(\)/);
assert.doesNotMatch(parentPressResult, /element:o\(\)\.max\(/);
assert.match(parentInputSchema, /plugin:o\(\)\.max\(Y\)/);
assert.match(parentInputSchema, /handle:C\(\)\.int\(\)/);
assert.match(parentInputSchema, /kind:j\(\["change","submit"\]\)/);
assert.match(parentInputSchema, /value:o\(\)\.max\(ir\)/);
assert.match(parentInputSchema, /key:o\(\)\.optional\(\)/);
assert.match(parentInputSchema, /component:lr\(\)\.optional\(\)/);
assert.match(parentInputSchema, /instance_id:o\(\)\.max\(Y\)\.optional\(\)/);
assert.match(parentInputSchema, /surface:W\(\)\.default\("desktop"\)/);
assert.match(parentInputSchema, /client_id:G\(\)\.optional\(\)/);
assert.match(parentInputResult, /element:o\(\)\.optional\(\)[\s\S]*?value:o\(\)\.optional\(\)/);
assert.doesNotMatch(parentInputResult, /element:o\(\)\.max\(|value:o\(\)\.max\(/);
assert.match(parentSelectSchema, /plugin:o\(\)\.max\(Y\)/);
assert.match(parentSelectSchema, /value:o\(\)\.max\(ir\)/);
assert.doesNotMatch(parentSelectSchema, /kind:/);
assert.match(parentSelectResult, /element:o\(\)\.optional\(\)[\s\S]*?value:o\(\)\.optional\(\)/);

assert.match(parentPressRoute, /E1e\.press\(\{plugin:V\.plugin,handle:V\.handle,\.\.\.V\.key!==void 0&&\{element:V\.key\},surface:V\.surface,link:V\.href===void 0\?void 0:\{href:V\.href\}\}\)/);
assert.match(parentInputRoute, /E1e\.input\(\{plugin:V\.plugin,handle:V\.handle,surface:V\.surface,kind:V\.kind,value:V\.value,\.\.\.V\.key!==void 0&&\{element:V\.key\},\.\.\.V\.component!==void 0&&\{component:V\.component\},\.\.\.V\.instance_id!==void 0&&\{requestId:V\.instance_id\}\}\)/);
assert.match(parentSelectRoute, /E1e\.select\(\{plugin:V\.plugin,handle:V\.handle,surface:V\.surface,value:V\.value,\.\.\.V\.key!==void 0&&\{element:V\.key\},\.\.\.V\.component!==void 0&&\{component:V\.component\},\.\.\.V\.instance_id!==void 0&&\{requestId:V\.instance_id\}\}\)/);
assert.doesNotMatch(parentPressRoute, /client_id:/);
assert.doesNotMatch(parentInputRoute, /client_id:/);
assert.doesNotMatch(parentSelectRoute, /client_id:/);
assert.match(parentHeldHookProjection, /var LP=\(\{held:e,surface:n,kind:r,value:s\}\)=>\(\{plugin:e\.plugin,element:e\.element,component:e\.component,requestId:e\.requestId,surface:n,kind:r,value:s\}\)/);
assert.match(parentHeldHookProjection, /var BP=\(\{held:e,surface:n,value:r\}\)=>\(\{plugin:e\.plugin,element:e\.element,component:e\.component,requestId:e\.requestId,surface:n,value:r\}\)/);
assert.match(parentHeldHookProjection, /await h\.press\(e\.handle,s\)/);
assert.match(parentHrefPathNormalizer, /let n=\/\^file:\/i\.test\(e\);if\(!n\)\{if\(!\(e\.startsWith\("\/"\)&&!Fn\(e\)\)\)return e\}/);
assert(parentHrefPathNormalizer.includes('let s=r.search(/[#?]/)'));
assert(parentHrefMatcher.includes('let r=/^www\\./i.test(e)?`http://${e}`:Gpn(e)??e;try{return new URL(r).href}catch{return r}'));
assert(parentHrefMatcher.includes('return n.some((s)=>sS(s)===r)'));
assert.match(parentMarkdownPressGate, /let g=T3\.heldPress\(e,n,r\),b=h===void 0\?"Button":"Markdown"/);
assert.match(parentMarkdownPressGate, /if\(h!==void 0&&!j9n\(h\.href,g\.pressableLinks\)\)/);
const hrefGateOffset = parentMarkdownPressGate.indexOf('!j9n(h.href,g.pressableLinks)');
const pressHookOffset = parentMarkdownPressGate.indexOf('await qT({e:A,handlers:');
assert(hrefGateOffset >= 0 && pressHookOffset > hrefGateOffset,
  'Native Markdown href admission occurs before ui.press hooks');
assert.equal((parentMarkdownPressGate.match(/j9n\(/g) ?? []).length, 1,
  'Native does not recheck href after hooks rewrite the event');
assert.match(parentStoredPressRun, /c\.invoke\(g,\[c\.cloneIn\(m\)\]\)/);
assert.match(parentResolverHoverGroup, /let t=e,\{press:o,onPress:n,onEvent:i\}=e,a=t\.type==="Button"\?n:i/);
assert.match(parentResolverHoverGroup, /return r\.take\(d,a\),Oe\(t,r\.plugin,d\)/);
assert.match(nativeJsxFactory, /onEvent: e => onLinkPress\(e\.link, e\)/);
assert.match(nativeJsxFactory, /press: \{ plugin: '', handle: \+\+pressCounter \},\s+onPress,/);

// Execute exact Native Gpn/RD/path-guard source with a controlled async cwd.
// The lexical safety helpers are extracted from the pinned BunFS modules and
// run against Node's darwin-arm64 path/pathToFileURL implementation; no file
// reads or existence checks occur in RD. The Windows helper branch is tested
// separately because this pinned executable is the Darwin build.
function between(label, text, startMarker, endMarker) {
  const start = text.indexOf(startMarker);
  assert(start >= 0, `${label}: missing ${startMarker}`);
  const end = text.indexOf(endMarker, start + startMarker.length);
  assert(end >= 0, `${label}: missing ${endMarker}`);
  return text.slice(start, end);
}
const parentPathRegexMatch = parentPathRegexModule.match(/var qee=.*?,j4=(\/[^;]+\/);/);
assert(parentPathRegexMatch, 'Native path helper j4 regex source missing');
const nativeJ4 = vm.runInNewContext(parentPathRegexMatch[1]);
const controlledWorkingDirectory = '/Users/luolingfeng/lingxi/harness-runtime';
const nativePathFunctionsSource = [
  between('path guard', parentPathGuardModule, 'function Fn(t){', 'function QPn('),
  between('path guard', parentPathGuardModule, 'function Xi(t){', 'function DB(t)'),
  between('path guard', parentPathGuardModule, 'function Ol(t){', 'function SCt('),
  between('path guard', parentPathGuardModule, 'function B_(t){', 'function Xnt('),
  between('path guard', parentPathGuardModule, 'function PR(t){', 'function HA('),
  between('path guard', parentPathGuardModule, 'function KM(t){', 'export{'),
  between('file URL safety', parentFileUrlSafetyModule, 'var Qyn=', 'export{'),
  between('Gpn producer', parentGpnProducerModule, 'function RD(e){', 'export{'),
].join('\n');
const nativeGpnFunctions = vm.runInNewContext(
  `${nativePathFunctionsSource}\n${parentHrefMatcher}\n` +
    '({Gpn,RD,Fn,Ol,PR,s7e,G6r,Of,kn,sS,j9n})',
  {
    path,
    pathToFileURL,
    URL,
    TextDecoder,
    Uint8Array,
    oe: () => controlledWorkingDirectory,
    j4: nativeJ4,
    tt: path,
    je: path.isAbsolute,
    $e: path.resolve,
    hn: pathToFileURL,
    a: path.posix,
  },
);
const {Gpn: nativeGpn, sS: normalizeParentHref, j9n: parentHrefMatches} = nativeGpnFunctions;
assert.equal(nativeGpn('/tmp/a.md'), 'file:///tmp/a.md');
assert.equal(nativeGpn('file:///tmp/a.md'), 'file:///tmp/a.md');
assert.equal(parentHrefMatches('/tmp/a.md', ['file:///tmp/a.md']), true,
  'absolute local paths and equivalent file URLs compare equal');
assert.equal(nativeGpn('/tmp/a.md?Q=A#Frag'), 'file:///tmp/a.md?Q=A#Frag');
assert.equal(nativeGpn('file:///tmp/a.md?Q=A#Frag'), 'file:///tmp/a.md?Q=A#Frag');
assert.equal(parentHrefMatches('/tmp/a.md?Q=A#Frag', ['file:///tmp/a.md?Q=A#Frag']), true);
assert.equal(
  nativeGpn('file:relative/a.md'),
  'file:///Users/luolingfeng/lingxi/harness-runtime/relative/a.md',
  'file:relative paths resolve against the active contextual cwd',
);
assert.equal(nativeGpn('file://localhost/tmp/a.md'), 'file:///tmp/a.md');
assert.equal(
  nativeGpn('file://LOCALHOST/tmp/a.md'),
  'file:///Users/luolingfeng/lingxi/harness-runtime/LOCALHOST/tmp/a.md',
  'localhost handling is case-sensitive in Gpn',
);
assert.equal(
  nativeGpn('file://server/tmp/a.md'),
  'file:///Users/luolingfeng/lingxi/harness-runtime/server/tmp/a.md',
  'unrecognized file URL host text becomes a cwd-relative path segment',
);
assert.equal(nativeGpn('C:\\tmp\\a.md'), 'C:\\tmp\\a.md',
  'a bare Windows path is not Gpn-converted on the pinned Darwin build');
assert.equal(nativeGpn('file:///C:/tmp/a.md'), 'file:///C:/tmp/a.md');
assert.equal(nativeGpnFunctions.kn('/C:/tmp/a.md', path.win32.isAbsolute), 'C:/tmp/a.md');
assert.equal(nativeGpnFunctions.kn('/C:/tmp/a.md', path.posix.isAbsolute), '/C:/tmp/a.md');
for (const unsafePath of ['/net/server/a', '/.file/a', '/Network/Servers/server/share']) {
  assert.equal(nativeGpn(unsafePath), null, `Native RD rejects unsafe absolute path ${unsafePath}`);
}
assert.equal(nativeGpnFunctions.Ol('/net/server/a'), true);
assert.equal(nativeGpnFunctions.Ol('/.file/a'), true);
assert.equal(nativeGpnFunctions.G6r('file:///Network/Servers/server/share'), true);

// Native next(e) validates rewrite arguments through Ge/Kr/mn. These exact
// source functions let hooks change element/component/link and string values;
// the original closure handle remains fixed in Vm and is not selected by the
// rewritten element.
const nativeArgumentValidators = vm.runInNewContext(
  `${parentHookElementValidator}\n${parentHookInputValidator}\n${parentHookSelectValidator}\n` +
    `${parentHookNextGenericArgumentCheck}\n${parentHookNextArgumentCheck}\n({Ge,Kr,mn,lt})`,
  {
    QIe: surface => surface === 'desktop' || surface === 'terminal',
    N: value => typeof value === 'object' && value !== null,
    se: () => false,
    Po: value => structuredClone(value),
    Oa: value => value,
    Aa: () => {},
    p2e: () => {},
    Ie: class extends Error {},
  },
);
const {Ge: nativePressValidator, Kr: nativeInputValidator, mn: nativeSelectValidator, lt: nativeNextArgumentCheck} = nativeArgumentValidators;
const pressOriginal = {plugin: 'plug', element: 'before', component: 'Button', requestId: 'request', surface: 'desktop'};
const pressRewritten = {...pressOriginal, element: '', component: 'Other'};
assert.equal(nativePressValidator(pressRewritten, pressOriginal), undefined);
assert.equal(nativePressValidator({...pressRewritten, element: 'x'.repeat(257)}, pressOriginal), undefined,
  'Native hook event element is an unbounded string');
assert.notEqual(nativePressValidator({...pressOriginal, element: undefined}, pressOriginal), undefined,
  'Native event element is required, though it has no length bound');
assert.notEqual(nativePressValidator({...pressOriginal, plugin: 'other'}, pressOriginal), undefined);
assert.notEqual(nativePressValidator({...pressOriginal, requestId: 'other'}, pressOriginal), undefined);
const pressLinkOriginal = {...pressOriginal, link: {href: '/tmp/a.md'}};
const pressLinkRewritten = {...pressLinkOriginal, link: {href: 'file:///tmp/a.md'}};
assert.equal(nativePressValidator(pressLinkRewritten, pressLinkOriginal), undefined,
  'Native next(e) does not recheck rewritten Markdown href against pressableLinks');
assert.equal(nativePressValidator(
  {...pressLinkOriginal, link: {href: 'x'.repeat(20_001)}},
  pressLinkOriginal,
), undefined, 'Native rewritten href is an unbounded string');

const oversized = 'v'.repeat(16_385);
const inputOriginal = {...pressOriginal, element: 'input-key', component: 'Input', kind: 'change', value: 'small'};
const inputRewritten = {...inputOriginal, element: 'renamed', component: 'Other', value: oversized};
assert.equal(nativeInputValidator(inputRewritten, inputOriginal), undefined);
const selectOriginal = {...pressOriginal, element: 'select-key', component: 'Select', value: 'small'};
const selectRewritten = {...selectOriginal, element: '', component: 'Other', value: oversized};
assert.equal(nativeSelectValidator(selectRewritten, selectOriginal), undefined);
assert.notEqual(nativeInputValidator({...inputRewritten, kind: 'submit'}, inputOriginal), undefined);
const acceptedPressRewrite = nativeNextArgumentCheck(
  {handler: {name: 'controlled-hook'}, site: {checkArgument: nativePressValidator}, e: pressOriginal},
  pressRewritten,
);
assert.deepEqual(JSON.parse(JSON.stringify(acceptedPressRewrite)), pressRewritten,
  'the exact Native next(e) gate returns the rewritten event');
const oversizedHrefRewrite = {...pressLinkOriginal, link: {href: 'h'.repeat(20_001)}};
assert.deepEqual(JSON.parse(JSON.stringify(nativeNextArgumentCheck(
  {handler: {name: 'controlled-hook'}, site: {checkArgument: nativePressValidator}, e: pressLinkOriginal},
  oversizedHrefRewrite,
))), oversizedHrefRewrite, 'exact Native next(e) accepts an oversized rewritten href');
assert.deepEqual(JSON.parse(JSON.stringify(nativeNextArgumentCheck(
  {handler: {name: 'controlled-hook'}, site: {checkArgument: nativeInputValidator}, e: inputOriginal},
  inputRewritten,
))), inputRewritten, 'exact Native next(e) allows oversized rewritten input strings');
assert.deepEqual(JSON.parse(JSON.stringify(nativeNextArgumentCheck(
  {handler: {name: 'controlled-hook'}, site: {checkArgument: nativeSelectValidator}, e: selectOriginal},
  selectRewritten,
))), selectRewritten, 'exact Native next(e) allows oversized rewritten select strings');
assert.equal(normalizeParentHref('WWW.Example.com/a'), 'http://www.example.com/a');
assert.equal(
  normalizeParentHref('www.Example.com:443/a/../b'),
  'http://www.example.com:443/b',
  'the native www-prefix path retains 443 because it constructs an HTTP URL',
);
assert.equal(
  normalizeParentHref('WWW.Example.com:80/a/../b'),
  'http://www.example.com/b',
  'the native www-prefix path canonicalizes case, dot segments, and default port 80',
);
assert.equal(
  normalizeParentHref('www.Example.com/a/../b'),
  'http://www.example.com/b',
  'the native www-prefix path canonicalizes host case and dot segments without a port',
);
assert.equal(
  parentHrefMatches('www.Example.com:443/a/../b', ['http://www.example.com:443/b']),
  true,
  'www:443 is positively admitted against its canonical HTTP counterpart',
);
assert.equal(
  parentHrefMatches('www.Example.com:80/a/../b', ['http://www.example.com/b']),
  true,
  'www:80 is positively admitted against canonical HTTP default-port removal',
);
assert.equal(
  parentHrefMatches('www.Example.com/a/../b', ['http://www.example.com/b']),
  true,
  'www without a port is positively admitted against its canonical HTTP counterpart',
);
assert.equal(
  parentHrefMatches('HTTP://EXAMPLE.com:80/a/../b?Q=A#Frag', ['http://example.com/b?Q=A#Frag']),
  true,
);
assert.equal(parentHrefMatches('https://example.com/p?q=A#Frag', ['https://example.com/p?q=a#Frag']), false);
assert.equal(parentHrefMatches('https://example.com/p?q=A#Frag', ['https://example.com/p?q=A#frag']), false);
assert.equal(parentHrefMatches('relative/path', ['relative/path']), true);

// Compare the exact Native matcher with the real Mods worker's private
// ui.press.preflight RPC. The worker API returns only admission, so a pairwise
// link corpus compares its normalization equivalence relation without adding
// a test-only raw-normalized-value response to production code.
const workerPath = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  '../../crates/hooks/src/mods_worker.mjs',
);
const workerSourceShaBefore = sha256(readFileSync(workerPath));
const actualWorker = spawn(process.execPath, ['--experimental-vm-modules', workerPath], {
  stdio: ['pipe', 'pipe', 'pipe'],
});
const workerExit = new Promise(resolve => actualWorker.once('exit', (code, signal) => resolve({code, signal})));
const workerStderr = [];
actualWorker.stderr.setEncoding('utf8');
actualWorker.stderr.on('data', chunk => workerStderr.push(chunk));
const workerPending = new Map();
const workerLines = createInterface({input: actualWorker.stdout, crlfDelay: Infinity});
workerLines.on('line', line => {
  const frame = JSON.parse(line);
  if (frame.kind === 'api.callers') {
    actualWorker.stdin.write(`${JSON.stringify({id: frame.id, kind: 'api.callers.result', callId: frame.callId})}\n`);
    return;
  }
  if (frame.id !== undefined && frame.kind !== 'progress') {
    const settle = workerPending.get(frame.id);
    workerPending.delete(frame.id);
    settle?.(frame);
  }
});
actualWorker.once('exit', () => {
  for (const settle of workerPending.values()) settle({kind: 'worker.exit'});
  workerPending.clear();
});
let workerRequestId = 0;
function actualWorkerRequest(message) {
  const id = ++workerRequestId;
  return new Promise((resolve, reject) => {
    const timeout = setTimeout(() => {
      workerPending.delete(id);
      reject(new Error(`actual Mods worker RPC ${id} timed out`));
    }, 5_000);
    workerPending.set(id, frame => {
      clearTimeout(timeout);
      resolve(frame);
    });
    actualWorker.stdin.write(`${JSON.stringify({id, ...message})}\n`);
  });
}
async function workerAdmission(href, pressableLinks, apiContextTicket) {
  const message = {
    kind: 'ui.press.preflight',
    href,
    cwd: controlledWorkingDirectory,
    ...(pressableLinks === undefined ? {} : {pressableLinks}),
    ...(apiContextTicket === undefined ? {} : {apiContextTicket}),
  };
  const frame = await actualWorkerRequest(message);
  assert.equal(frame.kind, 'ui.press.preflight.result');
  assert.equal(typeof frame.admitted, 'boolean');
  return frame.admitted;
}

const hrefEquivalenceCorpus = [
  'www.Example.com/a',
  'http://www.example.com/a',
  'www.Example.com:443/a/../b',
  'http://www.example.com:443/b',
  'www.Example.com:80/a/../b',
  'WWW.Example.com:80/a/../b',
  'www.Example.com/a/../b',
  'http://www.example.com/b',
  'HTTPS://EXAMPLE.com:443/a/../b?Q=A#Frag',
  'https://example.com/b?Q=A#Frag',
  'https://example.com/b?q=A#Frag',
  '/tmp/a.md',
  'file:///tmp/a.md',
  '/tmp/a.md?Q=A#Frag',
  'file:///tmp/a.md?Q=A#Frag',
  'file:relative/a.md',
  'file:///Users/luolingfeng/lingxi/harness-runtime/relative/a.md',
  'file://localhost/tmp/a.md',
  'file://LOCALHOST/foo.md',
  'file:///Users/luolingfeng/lingxi/harness-runtime/LOCALHOST/foo.md',
  'file://server/foo.md',
  'file:///Users/luolingfeng/lingxi/harness-runtime/server/foo.md',
  'relative/path',
  'bad%ZZ.md',
  '/net/server/a.md',
  'file:///net/server/a.md',
  '/.file/secret.md',
  'file:///.file/secret.md',
  '/Network/Servers/server/a.md',
  'file:///Network/Servers/server/a.md',
  '//server/share/a.md',
  '',
  'file:',
];
const nativeHrefNormalizationRecords = hrefEquivalenceCorpus.map(raw => [raw, normalizeParentHref(raw)]);
let workerPairComparisons = 0;
let workerTicketComparisons = 0;
try {
  const positiveWwwPairs = [
    ['www.Example.com:443/a/../b', 'http://www.example.com:443/b'],
    ['www.Example.com:80/a/../b', 'http://www.example.com/b'],
    ['WWW.Example.com:80/a/../b', 'http://www.example.com/b'],
    ['www.Example.com/a/../b', 'http://www.example.com/b'],
  ];
  for (const [href, link] of positiveWwwPairs) {
    assert.equal(parentHrefMatches(href, [link]), true,
      `Native expected the www canonical pair to match: ${href} => ${link}`);
    assert.equal(await workerAdmission(href, [link]), true,
      `actual worker should positively admit Native www pair: ${href} => ${link}`);
  }
  const transportTicket = 'opaque-context-ticket-probe-1';
  assert.equal(parentHrefMatches(
    'www.Example.com:443/a/../b', ['http://www.example.com:443/b'],
  ), true, 'the href comparison remains a Native j9n positive case');
  assert.equal(await workerAdmission(
    'www.Example.com:443/a/../b',
    ['http://www.example.com:443/b'],
    transportTicket,
  ), true, 'the actual worker accepts a Native-positive href with transport ticket');
  workerTicketComparisons += 1;
  for (const href of hrefEquivalenceCorpus) {
    for (const link of hrefEquivalenceCorpus) {
      const nativeAdmitted = parentHrefMatches(href, [link]);
      const actualAdmitted = await workerAdmission(href, [link]);
      assert.equal(actualAdmitted, nativeAdmitted,
        `Native/worker href normalization mismatch for href=${JSON.stringify(href)} link=${JSON.stringify(link)}`);
      workerPairComparisons += 1;
    }
  }
  for (const href of ['', '/net/server/a.md', 'https://example.com/a']) {
    assert.equal(parentHrefMatches(href, undefined), true);
    assert.equal(await workerAdmission(href, undefined), true,
      `an absent pressableLinks list admits ${JSON.stringify(href)}`);
  }
  assert.equal(parentHrefMatches('empty-list.md', []), false);
  assert.equal(await workerAdmission('empty-list.md', []), false,
    'an empty pressableLinks list rejects every candidate');
  assert.equal(parentHrefMatches('', ['']), true);
  assert.equal(await workerAdmission('', ['']), true, 'empty href and empty allowlist entry match');
} finally {
  workerLines.close();
  actualWorker.kill('SIGTERM');
  await Promise.race([workerExit, new Promise(resolve => setTimeout(resolve, 1_000))]);
}
const workerSourceShaAfter = sha256(readFileSync(workerPath));
assert.equal(workerSourceShaAfter, workerSourceShaBefore,
  'mods_worker.mjs changed while its RPC differential probe was running');
assert.equal(parentHrefMatches('./relative/path', ['relative/path']), false);
assert.equal(parentHrefMatches('', ['']), true);
assert.equal(parentHrefMatches('http://[bad', ['http://[bad']), true);
assert.equal(parentHrefMatches('http://[bad', ['http://[BAD']), false);
assert.equal(parentHrefMatches('javascript:alert(1)', ['javascript:alert(1)']), true);
assert.equal(parentHrefMatches('anything', undefined), true);
assert(parentHeldHookProjection.includes('admits:(r)=>r.options===void 0||r.options.includes(e)'));
const selectAdmissionOffset = parentHeldHookProjection.indexOf('if(e.admits!==void 0&&!e.admits(D))');
const selectHookDispatchOffset = parentHeldHookProjection.indexOf('V=await qT({e:K,handlers:');
assert(selectAdmissionOffset >= 0 && selectHookDispatchOffset > selectAdmissionOffset,
  'Native ui.select rejects non-held option values before hook dispatch');
assert.match(nativeJsxFactory, /onEvent: e =>\s*e\.kind === 'submit'\s*\? onSubmit\(e\.value, e\)\s*:\s*onInput === undefined\s*\? undefined\s*:\s*onInput\(e\.value, e\)/);
assert.match(nativeJsxFactory, /onEvent: e => onSelect\(e\.value, e\)/);

assert.match(nativeSurfaceTreeSchema, /hover:jw\(\)\.optional\(\),group:u\(\{plugin:o\(\)\}\)\.optional\(\)/);
assert.match(hoverAllowlist, /Box:new Set\(\["borderStyle","borderColor","borderDimColor","backgroundColor","display","top","left","right","bottom"\]\),Text:new Set\(\["color","backgroundColor","dimColor","bold","italic","underline","strikethrough","inverse"\]\)/);
assert.match(hoverScopeValidator, /if\(s\.scope!==void 0\)\{let M=h==="Button"\?e\.press:e\.group,D=pBr\(s\.scope,M,n\)/);
assert.match(hoverScopeValidator, /if\(M==="scope"\)continue/);
assert.match(hoverScopeValidator, /typeof e==="string"&&e!==""&&e\.length<=V9n/);
assert.match(hoverScopeValidator, /hover has no Box with a key around it/);
assert.match(hoverScopeValidator, /hover is scoped to a keyed Box drawn display "none"/);
assert.match(hoverScopeValidator, /takes no hover; Box, Text and Button do/);
assert.match(hoverValidator, /Number\.isInteger\(t\)&&Math\.abs\(t\)<=lQe/);
assert.match(parentResolverHoverGroup, /function Ee\(e,r\)\{let t=mSt\(e\.hover\);if\(t===void 0\)return e/);
assert.match(parentResolverHoverGroup, /o===""\|\|o===void 0\|\|o===r\.plugin\)return Re\(e,r\.plugin\)/);
assert.match(parentResolverHoverGroup, /e\.groups\.set\(o,n-1\)/);
assert.match(parentResolverHoverGroup, /if\(!K\(e\)\)\{let A=xe\(e\)\?Ee\(e,r\):e/);
assert.match(parentResolverHoverGroup, /case"Button":\{let n=\{type:"Button",props:e\.props,press:o\}/);

// The desktop asks for a module manifest only after ui_render announced a
// plugin hash. The raw manifest is returned on the same control request.
assert.match(requestSchemas, /VAn=f\(\(\)=>u\(\{subtype:R\("ui_client_module"\),plugin:o\(\)\.describe/);
assert.match(requestSchemas, /GW=f\(\(\)=>u\(\{plugin:o\(\),hash:o\(\)\.describe/);
assert.match(requestSchemas, /modules:T\(u\(\{module:o\(\)\.describe/);
assert.match(requestSchemas, /entry:o\(\)\.describe.*component:o\(\)\.describe/);
assert.match(requestSchemas, /runtime:o\(\)\.describe.*limits:u\(\{nodes:C\(\)\.int\(\),depth:C\(\)\.int\(\),chars:C\(\)\.int\(\),values:C\(\)\.int\(\),dataDepth:C\(\)\.int\(\)\}\).*files:T\(u\(\{key:o\(\),source:o\(\)\}\)/);
assert.match(manifestBuilder, /hash:i3\("sha256"\)\.update\(S\(\{files:h,modules:g,limits:r\}\)\)\.digest\("hex"\)/);
assert.match(manifestBuilder, /modules:g,runtime:Ih,limits:r,files:h/);
assert.match(manifestStringifierImport, /from"\/\$bunfs\/root\/chunk-gsc8spc7\.js"/);
assert.match(stringifierSourceModule, /function S\(e,n,r\)\{using s=lg`JSON\.stringify\(\$\{Re\(e\)\}\)`;return JSON\.stringify\(e,n,r\)\}/);
assert.match(stringifierSourceModule, /export\{[^}]*,S,LM,pa,lPn,J,Zo,/);
assert.match(clientModule, /VAn\(\)\.safeParse\(I\.request\)/);
assert.match(clientModule, /E1e\.clientModule\(B\.data\.plugin\)/);
assert.match(clientModule, /Xe\(I,ce\)/);

// Native Client interactions use a closed press/input/select union. The
// Desktop callback receives the parent ui_render instance id and Client key.
assert.match(requestSchemas, /qAn=f\(\(\)=>Oa\(\)\.extend\(\{subtype:R\("ui_client_press"\),element:o\(\)\.max\(Y\)/);
assert.match(requestSchemas, /event:qo\("type",\[u\(\{type:R\("press"\)\}\),u\(\{type:R\("input"\),kind:j\(\["change","submit"\]\),value:o\(\)\.max\(ir\)\}/);
assert.match(requestSchemas, /u\(\{type:R\("select"\),value:o\(\)\.max\(ir\)\}\)\]\)/);
assert.match(requestSchemas, /BW=f\(\(\)=>u\(\{handled:H\(\)\.describe.*reached:ue\(o\(\),ie\(\)\)\.optional\(\)/);
assert.match(clientPress, /qAn\(\)\.safeParse\(I\.request\)/);
assert.match(clientPress, /E1e\.clientPress\(\{plugin:V\.plugin,component:V\.component,requestId:V\.instance_id,key:V\.client,module:V\.module,element:V\.element,event:V\.event\}\)/);
assert.match(clientPress, /reached:ue\.reached/);

assert.match(requestSchemas, /KAn=f\(\(\)=>Oa\(\)\.extend\(\{subtype:R\("ui_message"\),data:ie\(\)\.describe/);
assert.match(requestSchemas, /qW=f\(\(\)=>u\(\{handled:H\(\)\.describe.*props:ie\(\)\.optional\(\)/);
assert.match(uiMessage, /E1e\.clientMessage\(\{plugin:V\.plugin,component:V\.component,requestId:V\.instance_id,key:V\.client,module:V\.module,data:V\.data\}\)/);
assert.match(uiMessage, /props:ue\.props/);
assert.match(requestSchemas, /YAn=f\(\(\)=>Oa\(\)\.extend\(\{subtype:R\("ui_client_fault"\),phase:j\(\["load","render","run"\]\)/);
assert.match(requestSchemas, /reason:o\(\)\.max\(HW\)/);
assert.match(requestSchemas, /KW=f\(\(\)=>u\(\{handled:H\(\)\.describe/);
assert.match(clientFault, /_v\(B\.request\)/);
assert.match(invalidateSchema, /instances:T\(u\(\{surface:o\(\)\.describe.*component:o\(\)\.describe.*instance_id:o\(\)\.describe.*\}\)\)\.optional\(\)/);
assert.match(invalidationProjection, /Vi\(\{type:"system",subtype:"ui_invalidate",event:"ui\.render"/);
assert.match(invalidationProjection, /instance_id:x\.requestId/);

// The Desktop Client control response does not run its held callback. The SDK
// runs matching ui.* hooks first; only the appended native bottom projects a
// reached element/value for the external Client runtime to dispatch.
assert.match(nativeClientDrawnIdentity, /Ph=\(e\)=>\[e\.plugin,e\.module,e\.key\]\.join/);
assert.match(nativeClientDrawnIdentity, /isDrawn\(\{component:s,requestId:h,\.\.\.g\}\)/);
assert.match(nativeClientPressPath, /case"press":return x\(\)/);
assert.match(nativeClientPressPath, /handlers:qP\("ui\.press"\),site:iS,bottom:\(V\)=>\(w=V,Promise\.resolve\(\{element:V\.element\}\)\)/);
assert.match(nativeClientPressPath, /rS\(h\.plugin\)\(\(\)=>A\(oS,\{\.\.\.g,kind:s\.kind,value:s\.value\}\)\)/);
assert.match(nativeClientPressPath, /lS\(h\.plugin\)\(\(\)=>A\(aS,\{\.\.\.g,value:s\.value\}\)\)/);
assert.match(nativeClientPressPath, /let Z=\{element:Q\.element,value:Q\.value\}/);
assert.match(nativeClientEventConstructors, /var LP=\(\{held:e,surface:n,kind:r,value:s\}\)=>\(\{plugin:e\.plugin,element:e\.element,component:e\.component,requestId:e\.requestId,surface:n,kind:r,value:s\}\)/);
assert.match(nativeClientEventConstructors, /var BP=\(\{held:e,surface:n,value:r\}\)=>\(\{plugin:e\.plugin,element:e\.element,component:e\.component,requestId:e\.requestId,surface:n,value:r\}\)/);
assert.match(nativeClientMessagePath, /var tgn=\(e,n,r\)=>\(\{plugin:e\.plugin,message:\{surface:n,component:e\.component,requestId:e\.requestId,element:e\.key,module:e\.module,data:r\}\}\)/);
assert.match(nativeClientMessagePath, /handlers:\[\.\.\.qP\("ui\.message",void 0,\{only:e\}\),\{name:"core",isCore:!0,budgetMs:0,run:\(\)=>Promise\.resolve\(\{\}\)\}\]/);
assert.match(nativeClientMessagePath, /async function O3\(e,n=is\.isDrawn\)/);
assert.match(nativeHookDispatch, /m\.reduceRight\(/);
assert.match(nativeHookHandler, /e\.run\(V,Me,\{call:U,floors:y,cutAt:s\}\)/);
assert.match(nativeHookHandler, /runBelow:Qe/);

// Native manager faults unmount the VM before publishing a failed snapshot.
// Ordinary setProps/resize/schedule paths cannot produce another frame while
// that instance has no surface; recovery is through a gated remount path.
assert.match(nativeClientSurfaceManager, /function S\(e,o,n\)\{[\s\S]*?v\(e\),d\(e,\{status:"failed",text:c\}\)/);
assert.match(nativeClientSurfaceManager, /e\.surface\?\.unmount\(\),e\.surface=void 0/);
assert(nativeClientFaultCleanup.includes('e.mountGeneration+=1'));
assert(nativeClientFaultCleanup.includes('clearImmediate(e.renderTimer)'));
assert(nativeClientFaultCleanup.includes('clearImmediate(e.selfDrawnTimer)'));
assert(nativeClientFaultCleanup.includes('C(e),e.surface?.unmount(),e.surface=void 0,f.cancel(e.id)'));
assert(nativeClientSurfaceManager.includes('function C(e){k(e,e.held),e.held=[]}'));
assert(nativeClientSurfaceManager.includes('function k(e,o){xYe.release(e.plugin,o.map((n)=>n.surfaceHandle)),e.surface?.dropHeld(o.map((n)=>n.environmentHandle))}'));
assert(nativeClientFaultTransition.includes('let s=l(o),c=O(e,s,"the instance unmounted")'));
assert(nativeClientFaultTransition.includes('d(e,{status:"failed",text:c})'));
assert(nativeClientFaultTransition.includes('reason:s'));
assert.match(nativeClientSurfaceManager, /function M\(e\)\{let\{surface:o\}=e;if\(!o\|\|e\.isDestroyed\)return/);
assert.match(nativeClientSurfaceManager, /d\(e,\{status:"drawn",tree:ze\}\)/);
assert(nativeClientRunCallbackGuard.includes('function y(e,o){if(!e.surface||e.isDestroyed)return;try{o()}catch(n){S(e,n,"run")}'));
assert(nativeClientScheduleGuard.includes('function me(e){if(e.isDirty||e.isDestroyed||!e.surface)return'));
assert(nativeClientResizeGate.includes('if(e.surface)e.surface.resize(o.columns,o.rows),M(e)'));
assert(nativeSurfaceInstanceUnmount.includes('for(let n of o.timers.values())n();if(o.timers.clear(),!a)Y(o.id)'));
assert.match(nativeClientSurfaceManager, /if\(e\.surface\)e\.surface\.resize\(o\.columns,o\.rows\),M\(e\)/);
assert.match(nativeClientSurfaceManager, /threw:\(n\)=>\{if\(e\.surface===void 0\|\|e\.isDestroyed\)return;S\(e,n,"render"\),e\.thrownAt=P\(e,e\.renderedAt\)\}/);
assert.match(nativeClientSurfaceManager, /drawnAgain:\(\)=>e\.thrownAt!==void 0&&e\.thrownAt!==P\(e,L\(\)\)&&e\.holders>0\?B\(e\):void 0/);
assert(nativeClientSurfaceManager.includes('let P=(e,o)=>`${o} ${Bn({props:e.handedProps})}`'));
assert(nativeClientTerminalFrameFingerprint.includes('function L(){let r=d3.get();return[r?.columns,r?.rows,r?.conversationColumns].join("x")}'));
assert.match(nativeClientSurfaceManager, /function oe\(e\)\{for\(let o of p\.values\(\)\)\{if\(o\.plugin!==e\|\|o\.isDestroyed\)continue;if\(v\(o\),o\.toldLast=void 0,d\(o,\{status:"loading"\}\),o\.holders>0\)B\(o\)\}\}/);
assert.match(nativeClientSurfaceManager, /onDisposed\(oe\),Omt\.onFreed\(fe\)/);
assert.match(nativeTerminalClientSubtree, /x\(\(\)=>\{i\.drawnAgain\(\)\},\[i,o\.drawn\]\)/);
assert.match(nativeTerminalClientSubtree, /i\.resize\(R,T\)/);
assert.match(nativeTerminalClientSubtree, /p\.status==="failed"&&jl\(n,\{dimColor:!0,wrap:"truncate-end"\},p\.text\)/);
assert(nativeTerminalClientAcquireWrapper.includes('surface:"terminal"'));
assert.match(nativeTerminalClientBoundaryWrapper, /resetKey:p,[\s\S]*?onError:i\.threw/);

// Execute Native's exact error-boundary class: its reset key is snapshot
// identity, not a parent revision field. A changed snapshot clears hasError
// even when the test snapshots carry the same revision label.
const NativeClientErrorBoundary = vm.runInNewContext(
  `(()=>{class mjr{constructor(props){this.props=props}};${nativeClientErrorBoundary};return BO})()`,
);
const sameRevisionFailure = {status: 'failed', revision: 'same-parent-revision'};
let boundaryState = {
  hasError: true,
  resetKey: sameRevisionFailure,
};
assert.equal(
  NativeClientErrorBoundary.getDerivedStateFromProps(
    {resetKey: sameRevisionFailure}, boundaryState,
  ),
  null,
  'the same snapshot object does not reset the boundary',
);
const sameRevisionSuccess = {status: 'drawn', revision: 'same-parent-revision'};
const boundaryReset = NativeClientErrorBoundary.getDerivedStateFromProps(
  {resetKey: sameRevisionSuccess}, boundaryState,
);
assert.equal(boundaryReset.hasError, false);
assert.equal(boundaryReset.resetKey, sameRevisionSuccess,
  'a changed snapshot identity resets regardless of the parent revision label');

// `pointer`, `key`, and runHeld have a terminal producer, while no matching
// Desktop SDK control subtype exists in the inspected handler source chunk.
const sdkControlSource = binary.subarray(206056762, 206637682);
const sdkControlChunk = sdkControlSource.toString('utf8');
assert.equal(
  sha256(sdkControlSource),
  'ca03716821e6b76b1769143dcb5b95654d56a7d1c41036c31e0ed3c1c421c937',
  'SDK control handler chunk changed',
);
assert.doesNotMatch(sdkControlChunk, /subtype===\"ui_client_(?:pointer|key)\"/);
assert.match(terminalPointer, /P\.localCol,y:P\.localRow/);
assert.match(terminalPointer, /i\.pointer\(\{type:P\.type,x:P\.localCol,y:P\.localRow/);
assert.match(terminalKeyMap, /function ro\(o,i\)/);
assert.match(terminalKeyMap, /key:p/);
assert.match(terminalKeyProducer, /return o\.key\(k\),!0/);
assert.match(surfaceVmMount, /runHeld:\(h,I\)=>\{H\(g,"held",\{handle:h,event:I\}\)\}/);

// The aliased Native S helper executes JSON.stringify(e, replacer, space). The
// manifest supplies only one argument, so insertion order and literal Unicode
// become part of the UTF-8 SHA-256 preimage; this is not a canonical sorter.
const nativeS = vm.runInNewContext(
  `${manifestStringifierResource};${manifestStringifierRe};S`,
);

// Native Client routing: the constructor keeps its input string, but hook
// source compilation rewrites that literal with the same importer-relative
// So result used by scan.clients and the prepared surfaceModules list.
assert.match(nativeClientConstructor, /const clientProps = \{ key, module \}/);
assert.match(nativeClientSourceRewrite, /let p=So\(\{spelled:s\.spelled,file:o,root:r\}\);if\("module"in p\)n=n\.slice\(0,s\.at\.start\)\+S\(p\.module\)\+n\.slice\(s\.at\.end\)/);
assert.match(nativeHookModuleRewriteBoundary, /let P=Uas\(qEn\(S,v\),S,a\).*new Jn\.SourceTextModule\(P/);
assert(nativeClientScanKeySet.includes('o.add(s.module)}return[...o].sort()'));
assert.match(nativePreparedSurfaceModuleBuilder, /return\{module:e,modulePath:s,component:w\.component/);
assert.match(nativePluginSurfaceModulesInstall, /fHr\(s\.scan\.clients,e\.path,e\.name\)/);
assert(manifestBuilder.includes('g=n.map((b)=>({module:b.module,entry:al(b.modulePath,e),component:b.component}))'));
assert.match(nativeSurfaceModuleExactLookup, /let n=D\.get\(o\)/);

const nativeClientPathResolver = vm.runInNewContext(
  `${nativeClientPathLiteralPredicate};${nativeClientPathExtensionPredicate};` +
    `${nativeClientSourcePathResolver};So`,
  {
    itt: vm.runInNewContext(`${transpilerLoaderMap};itt`),
    Ss: path.dirname,
    ws: path.isAbsolute,
    vs: path.relative,
    vr: path.resolve,
    Tr: path.sep,
  },
);
assert.deepEqual(
  JSON.parse(JSON.stringify(nativeClientPathResolver({
    spelled: './card.tsx', file: '/plugin/register.tsx', root: '/plugin',
  }))),
  {module: 'card.tsx'},
);
assert.deepEqual(
  JSON.parse(JSON.stringify(nativeClientPathResolver({
    spelled: './card.tsx', file: '/plugin/hooks/register.tsx', root: '/plugin',
  }))),
  {module: 'hooks/card.tsx'},
  'the same literal resolves relative to its own importer file',
);

let currentClientLiteralNodes = [];
const nativeRewriteClientSource = vm.runInNewContext(
  `${nativeClientSourceRewrite};Uas`,
  {
    oe: 'Client',
    Nr: () => ({}),
    bo: () => currentClientLiteralNodes,
    So: nativeClientPathResolver,
    S: nativeS,
  },
);
function clientSourceFor(modulePath) {
  const literal = JSON.stringify('./card.tsx');
  const sourceText = `globalThis.tree = Client({ module: ${literal}, key: "main" }, []);`;
  const at = sourceText.indexOf(literal);
  currentClientLiteralNodes = [{spelled: './card.tsx', at: {start: at, end: at + literal.length}}];
  return sourceText;
}
const nativeClientFactory = vm.runInNewContext(`${nativeClientConstructor};client`);
assert.equal(
  nativeClientFactory({module: './card.tsx', key: 'main'}, []).props.module,
  './card.tsx',
  'the exact Client constructor preserves whatever module string reaches it',
);
const rootClientSource = clientSourceFor('/plugin/register.tsx');
const nestedClientSource = clientSourceFor('/plugin/hooks/register.tsx');
const rewrittenRootClientSource = nativeRewriteClientSource(
  rootClientSource, '/plugin/register.tsx', '/plugin',
);
const rewrittenNestedClientSource = nativeRewriteClientSource(
  nestedClientSource, '/plugin/hooks/register.tsx', '/plugin',
);
assert(rewrittenRootClientSource.includes('module: "card.tsx"'));
assert(rewrittenNestedClientSource.includes('module: "hooks/card.tsx"'));
const rootClientContext = {Client: nativeClientFactory};
const nestedClientContext = {Client: nativeClientFactory};
vm.runInNewContext(rewrittenRootClientSource, rootClientContext);
vm.runInNewContext(rewrittenNestedClientSource, nestedClientContext);
assert.equal(rootClientContext.tree.props.module, 'card.tsx');
assert.equal(nestedClientContext.tree.props.module, 'hooks/card.tsx');
const nativeClientScanKeys = vm.runInNewContext(
  `${nativeClientScanKeySet};Or`,
  {bo: nodes => nodes, So: nativeClientPathResolver},
);
const scannedClientKeys = nativeClientScanKeys({
  root: '/plugin',
  files: new Map([
    ['/plugin/register.tsx', {scanned: {program: currentClientLiteralNodes, refuse() {}}}],
    ['/plugin/hooks/register.tsx', {scanned: {program: [{
      spelled: './card.tsx', at: {start: 0},
    }], refuse() {}}}],
  ]),
});
assert.deepEqual(JSON.parse(JSON.stringify(scannedClientKeys)), ['card.tsx', 'hooks/card.tsx']);

const manifestPreimage = {
  files: [{key: 'surface:///cards/é-界.tsx', source: 'const label = "café 🧭";\n'}],
  modules: [{module: 'cards/é-界.tsx', entry: 'surface:///cards/é-界.tsx', component: 'default'}],
  limits: {nodes: 20_000, depth: 32, chars: 100_000, values: 20_000, dataDepth: 32},
};
const manifestPreimageText = nativeS(manifestPreimage);
assert.equal(manifestPreimageText, JSON.stringify(manifestPreimage));
assert(manifestPreimageText.includes('é-界') && manifestPreimageText.includes('café 🧭'));
assert(!manifestPreimageText.includes('\\u00e9'));
const manifestDigest = sha256(Buffer.from(manifestPreimageText, 'utf8'));
const reorderedManifestPreimage = {
  modules: manifestPreimage.modules,
  files: manifestPreimage.files,
  limits: manifestPreimage.limits,
};
assert.notEqual(nativeS(reorderedManifestPreimage), manifestPreimageText,
  'Native S preserves the object key order used at the hash call site');
assert.notEqual(
  sha256(Buffer.from(nativeS(reorderedManifestPreimage), 'utf8')),
  manifestDigest,
  'changing the top-level insertion order changes the manifest digest',
);

// Execute the actual bundled builtin strings. The separate BunFS hooks-types
// asset is extracted and behavior-probed by mods_ui_client_asset_289_probe.mjs.
const nativeGer = vm.runInNewContext(`${nativeJsxFactory};Ger`);
const nativeGerApi = vm.runInNewContext(nativeGer);
const nativeG8n = vm.runInNewContext(`${nativeSurfaceInstaller};g8n`);
const nativeSurfaceRuntime = vm.runInNewContext(
  `${nativeSurfaceRuntimeTemplate};wO`,
  {Ger: nativeGer, g8n: nativeG8n},
);
assert.equal(nativeGer.length, 18_206);
assert.equal(sha256(Buffer.from(nativeGer, 'utf8')),
  'f11fc9d75055432ee9fe0b2706934f4c84f8dab28ae5b53cd06e2c34f35390b9');
assert.equal(nativeG8n.length, 12_800);
assert.equal(sha256(Buffer.from(nativeG8n, 'utf8')),
  'b2fe354b44f09b97f4b3320771ad9e9b58aa32a2e14c001a89d084c350b465f5');
assert.equal(nativeSurfaceRuntime.length, 31_103);
assert.equal(sha256(Buffer.from(nativeSurfaceRuntime, 'utf8')),
  '833ab9dd15983d1fbdad634bf41a7ea633f092ac806539c7e927100f0e89e1b7');
assert.match(nativeHooksTypesBinding,
  /Me\("\/\$bunfs\/root\/state-library\.js-wy8y4843\.txt"\);var Etr=et/);
assert.match(nativeBorderStyles, /singleDouble.*doubleSingle.*classic.*arrow/);
assert.match(nativeBorderStyles, /dashed:.*quote:/);
assert.match(remoteLinkActivation,
  /n==="https:"\|\|n==="http:"&&r==="localhost"/);
assert.match(remoteLinkActivation, /carries credentials|username/);

// Execute the exact parent select dispatcher/wrapper with a controlled hook
// chain: invalid incoming values are rejected before qT; a valid value can be
// rewritten outside the held option set and still reaches Ger's onSelect.
const parentQmStart = parentHeldHookProjection.indexOf('async function qm(e){');
const parentQmEnd = parentHeldHookProjection.indexOf('var HP=', parentQmStart);
const selectHelpersStart = parentHeldHookProjection.indexOf('var BP=');
const selectHelpersEnd = parentHeldHookProjection.indexOf('var Ym=', selectHelpersStart);
assert(parentQmStart >= 0 && parentQmEnd > parentQmStart);
assert(selectHelpersStart >= 0 && selectHelpersEnd > selectHelpersStart);
const nativeParentQmSource = parentHeldHookProjection.slice(parentQmStart, parentQmEnd);
const nativeSelectHelpersSource = parentHeldHookProjection.slice(selectHelpersStart, selectHelpersEnd);
const heldSelect = {
  tag: 'Select', plugin: 'plugin-a', handle: 4, element: 'picker',
  component: 'AbovePrompt', requestId: 'render-1', options: ['allowed'],
};
const selectHookInputs = [];
const selectHeldEvents = [];
const selectCallbackCalls = [];
let selectRewrite = 'outside-options';
const nativeSelectNode = nativeGerApi.h('Select', {
  key: 'picker',
  options: [{value: 'allowed'}],
  onSelect: (value, event) => selectCallbackCalls.push({
    value,
    event: JSON.parse(JSON.stringify(event)),
  }),
});
const selectVmContext = vm.createContext({
  T3: {
    heldPress: (plugin, handle, element) =>
      plugin === heldSelect.plugin && handle === heldSelect.handle
        && (element === undefined || element === heldSelect.element)
        ? heldSelect : undefined,
    heldByKeyOf: () => undefined,
  },
  qP: () => [],
  Vm: (held, _event, bottom) => ({
    name: held.plugin,
    async run(event) {
      selectHeldEvents.push(JSON.parse(JSON.stringify(event)));
      nativeSelectNode.onEvent(event);
      return bottom(event);
    },
  }),
  qT: async ({e, handlers}) => {
    selectHookInputs.push(JSON.parse(JSON.stringify(e)));
    return handlers.at(-1).run({...e, value: selectRewrite});
  },
  t: () => {},
  performance: {now: () => 1},
  Od: {'ui.select': 'ui.select'},
});
const nativeParentSelect = vm.runInContext(
  `${nativeParentQmSource};${nativeSelectHelpersSource};({qm,$P})`,
  selectVmContext,
);
const selectAddress = {
  plugin: heldSelect.plugin,
  handle: heldSelect.handle,
  element: heldSelect.element,
  component: heldSelect.component,
  requestId: heldSelect.requestId,
  surface: 'desktop',
};
const rejectedBeforeHooks = await nativeParentSelect.$P({...selectAddress, value: 'not-held'});
assert.equal(rejectedBeforeHooks, undefined);
assert.equal(selectHookInputs.length, 0);
assert.equal(selectCallbackCalls.length, 0);
const rewrittenSelectResult = await nativeParentSelect.$P({...selectAddress, value: 'allowed'});
assert.deepEqual(JSON.parse(JSON.stringify(rewrittenSelectResult)), {
  element: 'picker', value: 'outside-options',
});
assert.equal(selectHookInputs.length, 1);
assert.equal(selectHookInputs[0].value, 'allowed');
assert.equal(selectHeldEvents.length, 1);
assert.equal(selectHeldEvents[0].value, 'outside-options');
assert.equal(selectCallbackCalls.length, 1);
assert.equal(selectCallbackCalls[0].value, 'outside-options');
assert.equal(selectCallbackCalls[0].event.value, 'outside-options');

// Execute the exact Native qEn and $as bodies with controlled dependencies.
// This tests pragma and export-selection semantics without pretending to run
// Bun's transpiler, its extension map, or the real filesystem scanner.
const transpilerCalls = [];
class ControlledBunTranspiler {
  constructor(options) { this.options = options; }
  transformSync(text) {
    transpilerCalls.push({options: this.options, text});
    return text;
  }
}
const transpilerContext = vm.createContext({
  Bun: {Transpiler: ControlledBunTranspiler},
  Xo: jsxPragmas.slice(jsxPragmas.indexOf('`') + 1, jsxPragmas.lastIndexOf('`')),
  go: filename => filename.endsWith('.tsx') ? 'tsx'
    : filename.endsWith('.jsx') ? 'jsx'
      : filename.endsWith('.ts') ? 'ts' : 'js',
});
const nativeQEn = vm.runInContext(`${hookTranspiler};qEn`, transpilerContext);
const exactNativeGo = vm.runInNewContext(`${transpilerLoaderMap};${transpilerLoaderSelector};go`);
transpilerContext.go = exactNativeGo;
const tsxText = nativeQEn('/plugin/card.tsx', 'export const Card = () => <Text />');
assert.equal(transpilerCalls.at(-1).options.loader, 'tsx');
assert.equal(transpilerCalls.at(-1).options.macro, false);
assert.equal(transpilerCalls.at(-1).text,
  `${transpilerContext.Xo}export const Card = () => <Text />`);
assert.equal(tsxText, transpilerCalls.at(-1).text);
const tsText = nativeQEn('/plugin/types.ts', 'export type CardProps = {};');
assert.equal(transpilerCalls.at(-1).options.loader, 'ts');
assert.equal(transpilerCalls.at(-1).text, 'export type CardProps = {};',
  'Native .ts compilation adds no JSX pragma');
const jsText = 'export const Card = (props) => props;';
assert.equal(nativeQEn('/plugin/card.js', jsText), jsText);
for (const suffix of ['.mts', '.cts']) {
  nativeQEn(`/plugin/types${suffix}`, 'export type CardProps = {};');
  assert.equal(transpilerCalls.at(-1).options.loader, 'ts');
  assert.equal(transpilerCalls.at(-1).text, 'export type CardProps = {};');
}
for (const suffix of ['.mjs', '.cjs']) {
  const original = `export const Card = ${JSON.stringify(suffix)};`;
  assert.equal(nativeQEn(`/plugin/card${suffix}`, original), original);
}
const jsxText = nativeQEn('/plugin/card.jsx', 'const Card = () => <Text />');
assert.equal(transpilerCalls.at(-1).options.loader, 'jsx');
assert.equal(transpilerCalls.at(-1).text,
  `${transpilerContext.Xo}const Card = () => <Text />`);
assert.equal(jsxText, transpilerCalls.at(-1).text);
assert.deepEqual(
  ['.ts', '.tsx', '.jsx', '.js', '.mjs', '.cjs', '.mts', '.cts'].map(ext => [ext, exactNativeGo(`file${ext}`)]),
  [['.ts', 'ts'], ['.tsx', 'tsx'], ['.jsx', 'jsx'], ['.js', 'js'], ['.mjs', 'js'], ['.cjs', 'js'], ['.mts', 'ts'], ['.cts', 'ts']],
);
assert.equal(transpilerCalls.length, 5, 'js, mjs, and cjs bypass Bun.Transpiler');

const nativeExportSelector = vm.runInNewContext(`${exportSelectorSource};$as`, {
  Sr: value => value,
  xe: 'default',
  wr: name => /^[A-Z]/.test(name),
});
assert.deepEqual(JSON.parse(JSON.stringify(nativeExportSelector(['default', 'Board']))), {component: 'default'});
assert.deepEqual(JSON.parse(JSON.stringify(nativeExportSelector(['Board', 'helper']))), {component: 'Board'});
assert.match(nativeExportSelector(['Board', 'Card']).problem, /single capitalized/);
assert.match(nativeExportSelector(['helper']).problem, /no component/);

console.log('PASS: 2.1.289 parent and Client control schemas/handlers, Gpn path/file-URL equivalence, unbounded next(e) rewrites, unknown-key stripping, hover scope validation/stamping, ui.press URL admission and callback arity, ui.select pre-hook gate and rewritten-value callback, hook event projection, manifest hash serializer, invalidation, exact extension loader map/JSX transform target, export selection, and terminal-only pointer/key producer pins');
console.log(`NATIVE S GOLDEN: ${manifestDigest} (UTF-8 JSON.stringify of controlled files/modules/limits fixture)`);
console.log('NATIVE CLIENT MODULE ROUTE: the constructor preserves its argument; exact So/Uas/Or execution normalizes ./card.tsx by importer before tree creation; sourceprep and Go lookup use that same key (AST parser was injected)');
console.log(`NATIVE SURFACE RUNTIME GOLDEN: ${sha256(Buffer.from(nativeSurfaceRuntime, 'utf8'))} (resolved wO from Ger + g8n)`);
console.log('NATIVE CLIENT FAULT LIFECYCLE: failed surface unmounts; schedule/resize cannot render without it; exact React boundary reset-key helper is snapshot identity (not parent revision)');
console.log(`ACTUAL MODS WORKER RPC DIFFERENTIAL: ${workerPairComparisons} href/link pairs + absent/empty-list cases + ${workerTicketComparisons} opaque apiContextTicket positive; native-normalized corpus SHA ${sha256(Buffer.from(JSON.stringify(nativeHrefNormalizationRecords), 'utf8'))}; worker SHA-256 before/after ${workerSourceShaBefore}/${workerSourceShaAfter}`);
console.log('LIMIT: Native Bun transpilation, SDK parent-control transport/Desktop frame IPC, hooks event-chain execution beyond preflight, and Product renderer were not executed');
