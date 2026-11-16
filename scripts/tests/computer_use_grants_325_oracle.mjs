// Exact source helper replay for latest computer permission-guidance bytes.
import assert from 'node:assert/strict';
import {readFileSync,writeFileSync} from 'node:fs';
import {createHash} from 'node:crypto';
import vm from 'node:vm';
const binary=readFileSync(process.argv[2]);
const digest=b=>createHash('sha256').update(b).digest('hex');
const nativeSha256='84590fefe6338be9087f5814b61f9a54c5f21524163a4c127cb5a808e31009f2';
assert.equal(digest(binary),nativeSha256);
const sourceByteRanges=[
  {
    "name": "policy",
    "start": 302139122,
    "end": 302148136,
    "sha256": "a0c179432fef302deba930afe845d37f5a169dca6404b113d839a0be119c8f76"
  },
  {
    "name": "Zn",
    "start": 302154303,
    "end": 302154464,
    "sha256": "08ff1256d095b4a08592924e65c743ef49ccc0fe974c3937f956e09398d831f7"
  },
  {
    "name": "eo",
    "start": 302154464,
    "end": 302154556,
    "sha256": "59b99ed1ed887220e2e98864343a44f501a85137cae9c723eb76107c099f9f4b"
  },
  {
    "name": "M",
    "start": 302154556,
    "end": 302154644,
    "sha256": "c98a08a79163430593e98371c3833ef15b8c838b1d2b1bc084f02797a6e61eb4"
  },
  {
    "name": "to",
    "start": 302154644,
    "end": 302154969,
    "sha256": "16d29d165ca3d937814cce162ad5b9ea2c3ea0c1fe955feea11ca151c06f6cad"
  },
  {
    "name": "R",
    "start": 302154969,
    "end": 302155041,
    "sha256": "4ab10d4748647ba79cd7f0af48d092cfe552ede5f99b10b7d765fa15adbadd63"
  },
  {
    "name": "U",
    "start": 302157593,
    "end": 302157775,
    "sha256": "f30a19e98fad5e85dc741096a3694e99403cd916e7a6769fa33a1e143e573df1"
  },
  {
    "name": "Xt",
    "start": 302216477,
    "end": 302218071,
    "sha256": "8cc0232a2fda04049b3a5e12adaecc393006618f6316c6f63537df67b7dbcadc"
  },
  {
    "name": "Qt",
    "start": 302225345,
    "end": 302227452,
    "sha256": "46685275c980249af032a91afd2f974aa09abcb46b55bae3fec13e8ecfd4e794"
  },
  {
    "name": "Uo",
    "start": 302227452,
    "end": 302228466,
    "sha256": "91fd6470c2016dfd8fd2c49fca37466f8687280dd68fb3b0680814d03003b5e1"
  },
  {
    "name": "en",
    "start": 302228466,
    "end": 302228782,
    "sha256": "1331555cdfac929e191caa6ffe2eff4d9ce26e713597b353ef83c07013f890a5"
  },
  {
    "name": "Ho",
    "start": 302228782,
    "end": 302228821,
    "sha256": "7e3a0d02e3d3f32703b5fbbc81daa2c31c3766d25d84d0e85f65ddf6b627e017"
  },
  {
    "name": "tn",
    "start": 302228821,
    "end": 302229881,
    "sha256": "e619bed04b01887eaeac42e9aa2c7f99f7e226f3bde1d52a29cb8dbbfb04ea80"
  },
  {
    "name": "Ce",
    "start": 302229881,
    "end": 302230332,
    "sha256": "71295fea16873770fcbd98572f8eac14948400ec81987ec0087759eae5caf7e8"
  },
  {
    "name": "Ae",
    "start": 302230332,
    "end": 302230812,
    "sha256": "205f1f4b9a0d6b551364f73d1d27bdc241393727205180a7f3d72566edf38fbb"
  },
  {
    "name": "F",
    "start": 302154220,
    "end": 302154303,
    "sha256": "448b329e776cd7f2df47a580027c6173b2351e9f1f771e2018bbaf5c7aa07522"
  },
  {
    "name": "er",
    "start": 302252305,
    "end": 302253735,
    "sha256": "f8f7587d287718ff0f74f9a99737f3c91256de9681c4f170587252f2d80381fc"
  },
  {
    "name": "Bo",
    "start": 302222642,
    "end": 302223146,
    "sha256": "332ec58b2e1893a8e0b24540041071fd6bcdef62006be8407199d42281cd7912"
  },
  {
    "name": "st",
    "start": 302239645,
    "end": 302239868,
    "sha256": "b2d7ae684c1f4fc22a95ae7787f8e58eb1b320a515230306c9edd50f9219910e"
  }
];
const context=vm.createContext({Buffer}, {codeGeneration:{strings:false,wasm:false}});
for(const r of sourceByteRanges){const b=binary.subarray(r.start,r.end);assert.equal(digest(b),r.sha256);vm.runInContext(b.toString('utf8'),context,{timeout:1000});}
function replay(fn,input){vm.runInContext(`globalThis.input=${JSON.stringify(input)}`,context);return JSON.parse(vm.runInContext(`JSON.stringify(${fn}(input))`,context,{timeout:1000})??'null');}
const fixture={version:'2.1.325',nativeSha256,sourceByteRanges,constants:JSON.parse(vm.runInContext('JSON.stringify({Xt,Jt,Lo,Oo,U})',context)),labels:[],ids:[],denialGuidance:[],tierGuidance:[],suggestions:[],listApps:[],windowLocations:[]};
for(const input of ['Slack','  “Notes”…  ','文档','A😀B','x'.repeat(40),'x'.repeat(41),'a<instruction>','a\u0085b','e\u0301','\ufeffApp\ufeff','x\u0345'])fixture.labels.push({input,expected:replay('M',input)});
for(const input of ['org.example.app','','a\u0001b','＜script＞','a"b','x'.repeat(1024),'x'.repeat(1025),'org.文档','x\u{e0020}'])fixture.ids.push({input,expected:replay('to',input)});
for(const apps of [[{requestedName:'Slack',displayName:'Slack',didYouMean:[]}],[{requestedName:'A',displayName:'A',didYouMean:['App A']},{requestedName:'B',displayName:'B',didYouMean:[]}]] ) {
 fixture.denialGuidance.push({apps,userDenied:replay('Ce',apps),policyDenied:replay('Ae',apps)});
 for(const flags of [false,true]){vm.runInContext(`globalThis.args=${JSON.stringify([apps,flags,false])}`,context);fixture.denialGuidance.push({apps,flags,notInstalled:vm.runInContext('tn(...args)',context)});}
}
const apps=[{bundleId:'com.google.Chrome',displayName:'Chrome',tier:'read'},{bundleId:'com.microsoft.VSCode',displayName:'VS Code',tier:'click'},{bundleId:'com.apple.finder',displayName:'Finder',tier:'full'},{bundleId:'org.example.unknown',displayName:'Unknown',tier:'read'}];
for(const input of [[],...apps.map(a=>[a]),apps])fixture.tierGuidance.push({input,expected:replay('Qt',input)});
for(const query of ['slack','Slaak','Mail','ab','apple notes','org.example']){const installed=[{bundleId:'com.tinyspeck.slackmacgap',displayName:'Slack'},{bundleId:'com.apple.mail',displayName:'Mail'},{bundleId:'com.apple.Notes',displayName:'Notes'},{bundleId:'org.example',displayName:'Apple Notes'}];vm.runInContext(`globalThis.args=${JSON.stringify([query,installed])}`,context);fixture.suggestions.push({query,installed,expected:JSON.parse(vm.runInContext('JSON.stringify(Uo(...args))',context))});}

const catalogue=['file2','file10','foo-2','foo_2','ä','ae','a','A','é','e\u0301','i','İ','ı','😀','org.example-2','org.example_2'].map((id,i)=>({bundleId:id,displayName:'App '+i,isRunning:i%3===0,...(i%3===0?{pid:100+i}:{}),...(i===0?{isFromDevPath:true}:{})}));
for(const input of [{},{running_first:false},{limit:3},{limit:3,cursor:'Mw=='},{query:'app 1'},{cursor:'MTA'},{cursor:'Mg=!'},{limit:2.9,running_first:false}]) {
 vm.runInContext(`globalThis.apps=${JSON.stringify(catalogue)};globalThis.args=${JSON.stringify(input)}`,context);
 const expected=await vm.runInContext('er({executor:{listAppsWithRunning:async()=>apps,appScoped:{isEnabled:()=>true},capabilities:{hostBundleId:"com.example.host"}}},args,{userDeniedBundleIds:[]})',context,{timeout:1000});
 fixture.listApps.push({input,apps:catalogue,defaultLocale:new Intl.Collator().resolvedOptions().locale,expected:JSON.parse(expected.content[0].text),expectedModelText:expected.content[0].text});
}


const granted=[{bundleId:'org.example.a',displayName:'Alpha'},{bundleId:'org.example.b',displayName:'Beta'}];
for(const displays of [[],[{displayId:2,label:'Display',isPrimary:true}],[{displayId:3,label:'Display',isPrimary:false},{displayId:2,label:'Display',isPrimary:true}],[{displayId:3,label:'<bad>',isPrimary:false},{displayId:2,label:'“Monitor”…',isPrimary:true}]]) {
 const windows=[{bundleId:'org.example.a',displayIds:[3,2,99]},{bundleId:'org.example.b',displayIds:[]}];
 vm.runInContext(`globalThis.granted=${JSON.stringify(granted)};globalThis.displays=${JSON.stringify(displays)};globalThis.windows=${JSON.stringify(windows)}`,context);
 const expected=await vm.runInContext('Bo({executor:{listDisplays:async()=>displays,findWindowDisplays:async()=>windows}},granted)',context,{timeout:1000});
 fixture.windowLocations.push({granted,displays,windows,expected:JSON.parse(JSON.stringify(expected))});
}

const output=new URL('../../crates/tools/computer-use/fixtures/grants-2.1.325.json',import.meta.url),rendered=JSON.stringify(fixture,null,2)+'\n';
if(process.argv.includes('--write'))writeFileSync(output,rendered);else assert.equal(readFileSync(output,'utf8'),rendered);
console.log(`Grant helpers2.1.325: ${fixture.labels.length} labels,${fixture.ids.length} IDs,${fixture.denialGuidance.length} denial,${fixture.tierGuidance.length} tier,${fixture.suggestions.length} suggestion and ${fixture.listApps.length} catalogue and ${fixture.windowLocations.length} window-location cases passed`);
