// Controlled replay of exact Claude Code 2.1.325 display handlers. Native
// effects and permission/target gates are explicit stubs; no GUI is touched.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import vm from 'node:vm';
const path = process.argv.slice(2).find(arg => !arg.startsWith('--'));
if (!path) throw new Error('Supply official Claude Code 2.1.325 darwin-arm64 binary');
const digest = value => createHash('sha256').update(value).digest('hex');
const binary = readFileSync(path);
const binarySha256 = digest(binary);
assert.equal(binarySha256, '84590fefe6338be9087f5814b61f9a54c5f21524163a4c127cb5a808e31009f2');
const ranges = [
  ['scale',302086147,302087380,'35d6198bf3f51f36295e6f397589ee3d1248455e4c0e58706bbbf888f4487e55'],
  ['keys',302088399,302090484,'057ef814a6a51cdc2f7874207b2d4804cda25272a471139c44a99da3ccf2031c'],
  ['valuesCoordinates',302154067,302157442,'a141ef4ccaa6d3d9fb77e97d70196340f1c2dd54602d67118ea958e40a946170'],
  ['focusRestriction',302157593,302157775,'f30a19e98fad5e85dc741096a3694e99403cd916e7a6769fa33a1e143e573df1'],
  ['focusDelivery',302161149,302161657,'000728628c3b1fe041eb9fe300a9ff03ff9aacb152bad98120d65210527b3066'],
  ['scaleValidator',302213824,302213913,'e5d6df7cf486db683c55e3ae8f02dd5f2bf17661995b84b80bb715cb8a134013'],
  ['graphemes',302213913,302214222,'05df0610b4859f22e823bb77b53af207ee7f78280c98c2a41a9fe6e6ca0f500f'],
  ['mouseState',302214222,302214331,'1e9b09bcbe9dc411c77d1d730facfee25d1314d2c791503e013cdb51be8ee569'],
  ['displayNames',302239645,302239868,'b2d7ae684c1f4fc22a95ae7787f8e58eb1b320a515230306c9edd50f9219910e'],
  ['handlers',302243026,302260388,'94cfd169ba2fe2e7d0e13fae63ca47e7bfb092929539ba0be4b756f97edea1a4'],
  ['dispatch',302260458,302261465,'336699ca9edd502d9b7503e73018f21428fd9b6cfa7f3b96e2e3114cb01c01d1'],
];
const source = Object.fromEntries(ranges.map(([name,start,end,hash]) => {
  const bytes=binary.subarray(start,end);assert.equal(digest(bytes),hash,name);return[name,bytes.toString('utf8')];
}));
function context() {
  const ctx=vm.createContext({}, {codeGeneration:{strings:false,wasm:false}});
  for(const [name] of ranges){
    // Minified identifiers belong to different original modules. In particular
    // key-order H must never collide with display-held-mouse H.
    const code=name==='keys'?`var keyPolicy=(()=>{${source[name]};return {fen,gUi}})();var fen=keyPolicy.fen,gUi=keyPolicy.gUi;`:source[name];
    vm.runInContext(code,ctx,{timeout:1000});
  }
  vm.runInContext(`
    var events=[];
    async function me(){};
    async function te(){return {approvedFrontmost:{bundleId:'test.app',displayName:'Test'}}};
    async function le(){};async function uo(){};async function Ue(){};
    var env={logger:{debug(){},warn(){},error(){}},executor:{
      capabilities:{platform:'darwin'},
      async getFrontmostApp(){return {bundleId:'test.app',displayName:'Test'}},
      async getDisplaySize(){return {width:2,height:2,originX:-100,originY:0,scaleFactor:2}},
      async getCursorPosition(){return {x:-99,y:1.5}},
      async moveMouse(x,y){events.push(['move',x,y])},
      async click(x,y,button,count,modifiers){events.push(['click',x,y,button,count,modifiers??[]])},
      async type(value,options){events.push(['type',value,options])},
      async key(value){events.push(['key',value])},
      async scroll(x,y,dx,dy){events.push(['scroll',x,y,dx,dy])},
      async drag(from,to){events.push(['drag',from??null,to])},
      async holdKey(keys,duration){events.push(['hold',keys,duration])},
      async mouseDown(){events.push(['down'])},async mouseUp(){events.push(['up'])},
      async readClipboard(){return 'controlled clipboard'},async writeClipboard(value){events.push(['clipboard',value])},
      async listDisplays(){return [{displayId:1,label:'Only monitor'}]},
      async openApp(value){events.push(['open',value])}
    }};
    var state={allowedApps:[{bundleId:'test.app',displayName:'Test',tier:'full',grantedAt:17}],
      grants:{isEmpty(){return false},wantsHideBeforeAction:false,lookup(id){return{granted:id==='test.app'}},explicitGrants:[{bundleId:'test.app',displayName:'Test',tier:'full'}]},
      grantFlags:{clipboardRead:false,clipboardWrite:false,systemKeyCombos:false},coordinateMode:'pixels',
      userDeniedBundleIds:[],selectedDisplayId:1,isAborted(){return false},onDisplayPinned(id){events.push(['display',id??null])}
    };
    var gates={hideBeforeAction:false,pixelValidation:false,autoTargetDisplay:false,clipboardGuard:false,clipboardPasteMultiline:false};
  `,ctx,{timeout:1000});
  return ctx;
}
const cases=[];
async function capture(action,input,productionDifferential=true){
  const ctx=context();
  vm.runInContext(`var input=${JSON.stringify(input)};`,ctx,{timeout:1000});
  const result=await vm.runInContext(`lt(${JSON.stringify(action)},input,env,state,gates)`,ctx,{timeout:1000});
  const {screenshot,telemetry,...output}=JSON.parse(JSON.stringify(result));
  cases.push({action,input,productionDifferential,output,effects:JSON.parse(vm.runInContext('JSON.stringify(events)',ctx))});
}
for(const action of ['left_click','double_click','triple_click','right_click','middle_click','mouse_move']){
  await capture(action,{coordinate:[1.25,0.4]},!['triple_click','middle_click'].includes(action));
  await capture(action,{});
  await capture(action,{coordinate:[1]});
  await capture(action,{coordinate:[-1,0]});
}
await capture('left_click',{coordinate:[1,1],text:42});
await capture('left_click',{coordinate:[1,1],text:'cmd+q'});
await capture('type',{text:'Ae\u0301😀\n\t'});
await capture('type',{text:''});await capture('type',{});
for(const input of [{text:'a'},{text:'a',repeat:3},{text:'a',repeat:0},{text:'a',repeat:1.5},{text:'a',repeat:101},{}])await capture('key',input);
for(const input of [{duration:0},{duration:1e-7},{duration:1e-6},{duration:1.234e-7},{duration:Number.MIN_VALUE},{duration:-1},{duration:101},{duration:'1'},{}])await capture('wait',input);
for(const input of [{text:'space',duration:0},{text:'space',duration:-1},{duration:0}])await capture('hold_key',input);
for(const input of [{coordinate:[1,1],scroll_direction:'down',scroll_amount:2},{coordinate:[1,1],scroll_direction:'wrong',scroll_amount:2},{coordinate:[1,1],scroll_direction:'down',scroll_amount:-1},{coordinate:[1,1],scroll_direction:'down',scroll_amount:101}])await capture('scroll',input);
await capture('left_click_drag',{start_coordinate:[0,0],coordinate:[1.5,1]});
await capture('left_click_drag',{start_coordinate:[1],coordinate:[1,1]});
await capture('left_mouse_down',{});await capture('left_mouse_up',{});
await capture('cursor_position',{});
await capture('read_clipboard',{});await capture('write_clipboard',{text:'hello'});await capture('write_clipboard',{});await capture('write_clipboard',{text:7});
await capture('open_application',{});await capture('open_application',{app:'test.app'},false);
await capture('switch_display',{display:'auto'});await capture('switch_display',{display:'missing'},false);await capture('switch_display',{});
for(const input of [{},{region:[0,0,0,1]},{region:[0,0,1,0]},{region:[0,-1,1,2]},{region:[0,0,1,1]},{scale:2},{region:[0,0,1,1],scale:2},{region:[0,0,0,1],scale:2}])await capture('zoom',input);
for(const input of [{},{actions:[null]},{actions:[{}]},{actions:[{action:'request_access'}]},{actions:[{action:'wait',duration:0},{action:'key',text:'b'},{action:'type',text:'Hi'}]},{actions:[{action:'wait',duration:0},{action:'key',repeat:0,text:'b'},{action:'type',text:'never'}]}])await capture('computer_batch',input);
const ctx=context();
const scaleCases=[];
for(const dimensions of [[4,4],[2880,1800],[3000,500],[500,3000],[1568,1568],[10000,1],[1,10000]]){
  vm.runInContext(`var dims=${JSON.stringify(dimensions)}`,ctx);
  scaleCases.push({input:dimensions,output:JSON.parse(vm.runInContext('JSON.stringify(men(dims[0],dims[1],sLn))',ctx))});
}
const focusCases=[];
for(const expected of [null,{bundleId:'test.app',displayName:'Test'},{bundleId:'other.app',displayName:'Other'}]){
  const ctx=context();
  const input={expected,current:{bundleId:'test.app',displayName:'Test'},prefix:'Typing aborted after 1 of 3 graphemes'};
  vm.runInContext(`var focusInput=${JSON.stringify(input)}`,ctx);
  const result=await vm.runInContext('ue(env,focusInput.expected,focusInput.prefix)',ctx,{timeout:1000});
  let output=null;
  if(result){const {telemetry,...publicResult}=JSON.parse(JSON.stringify(result));output=publicResult;}
  focusCases.push({input,output});
}
const numberCases=[];
for(const input of [-0,1e-7,1e-6,1.234e-7,1e20,1e21,Number.MIN_VALUE,Number.MAX_VALUE,1.0000000000000002]){
  vm.runInContext(`var numberInput=${Object.is(input,-0)?'-0':JSON.stringify(input)}`,ctx);
  numberCases.push({input,output:vm.runInContext('String(numberInput)',ctx)});
}
const zoomCases=[];
for(const region of [[0.25,0.5,100.75,55.25],[0,0,1568,980],[1.23456789,2.3456789,444.555555,666.777777]]){
 const ctx=context();const frame={frameWidth:1568,frameHeight:980,width:1568,height:980,displayWidth:1440,displayHeight:900,displayId:7};
 vm.runInContext(`state.grants.captureAllowedBundleIds=async()=>['test.app'];state.lastScreenshot=${JSON.stringify(frame)};var zoomInput=${JSON.stringify({region})};env.executor.zoom=async function(rect){events.push(['zoom',rect]);return{base64:''}};`,ctx);
 await vm.runInContext('jo(env,zoomInput,state)',ctx,{timeout:1000});
 zoomCases.push({input:{region,captureWidth:2880,captureHeight:1800,scale:2,frameWidth:1568,frameHeight:980},output:JSON.parse(vm.runInContext("JSON.stringify(events.find(event=>event[0]==='zoom')[1])",ctx))});
}
const fixture={version:'2.1.325',binarySha256,sourceByteRanges:ranges.map(([name,start,end,sha256])=>({name,start,end,sha256})),
  scope:'Source-replayed field validation, coordinate conversion, focus delivery continuity, success text and batch content with controlled permission/target/native-effect seams; numberCases capture ECMAScript interpolation spelling. Production tests compare marked public wire outputs and assert selected actual backend effects. No native screenshot bytes, live GUI, OS grants or native event implementation claim.',cases,scaleCases,focusCases,numberCases,zoomCases};
const output=JSON.stringify(fixture,null,2)+'\n';
const destination=new URL('../../crates/tools/computer-use/fixtures/display-2.1.325.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(readFileSync(destination,'utf8'),output,'Display fixture bytes differ');else writeFileSync(destination,output);
console.log(JSON.stringify({status:'passed',fixtureSha256:digest(output),cases:cases.length,productionCases:cases.filter(x=>x.productionDifferential).length,scaleCases:scaleCases.length,focusCases:focusCases.length,numberCases:numberCases.length,zoomCases:zoomCases.length}));
