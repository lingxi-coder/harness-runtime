// Hash-pinned pure background formatting from official Claude Code 2.1.325.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import vm from 'node:vm';
const path=process.argv.slice(2).find(arg=>!arg.startsWith('--'));
if(!path)throw new Error('Supply official Claude Code 2.1.325 darwin-arm64 binary');
const digest=bytes=>createHash('sha256').update(bytes).digest('hex');
const binary=readFileSync(path),nativeSha256='84590fefe6338be9087f5814b61f9a54c5f21524163a4c127cb5a808e31009f2';
assert.equal(digest(binary),nativeSha256);
const ranges=[
  {
    "name": "Zn_Qn",
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
    "name": "no",
    "start": 302155095,
    "end": 302155152,
    "sha256": "e39d376247c4e473ab740207c8bcb6ea2d7dd5128aa8e6e4ee2bf1e3ea6d9cea"
  },
  {
    "name": "Le",
    "start": 302155152,
    "end": 302155242,
    "sha256": "6fd41c3f667b68b8495ca11ba39fa5a6e211841089ccf77addbc1ef0a0988d66"
  },
  {
    "name": "bo__o",
    "start": 302176675,
    "end": 302176866,
    "sha256": "a974425dbc393e8e059adc8df9cd70bb88bdff9a3535d1e96fb2183642336f13"
  },
  {
    "name": "Gt",
    "start": 302176866,
    "end": 302177297,
    "sha256": "012f8933e79553ff433f4c3ea32a201113f40cf36f1ed7a871eaed4e8417756c"
  },
  {
    "name": "ko",
    "start": 302177297,
    "end": 302178076,
    "sha256": "03fa948d2f1882d8c82ea2ff45f1d2d5cf4356810ba392e5272a5ffb0ba7d365"
  },
  {
    "name": "Pt",
    "start": 302194427,
    "end": 302201276,
    "sha256": "473dd404f9736c8e09630af4ce2262f14e629f712a5a99715545d5caea7368b3"
  }
];
const source={};for(const r of ranges){const b=binary.subarray(r.start,r.end);assert.equal(digest(b),r.sha256);source[r.name]=b.toString('utf8');}
const context=vm.createContext({}, {codeGeneration:{strings:false,wasm:false}});
vm.runInContext(ranges.map(r=>source[r.name]).join('\n'),context,{timeout:1000});
const run=expression=>vm.runInContext(expression,context,{timeout:1000});
const fixture={version:'2.1.325',nativeSha256,sourceByteRanges:ranges,reasonMessages:{},labels:[],summaries:[]};
const reasons=[...source.Pt.matchAll(/case"([^"]+)"/g)].map(m=>m[1]);reasons.push('unknown');
for(const reason of reasons)for(const focused of [false,true])for(const overwrite of [false,true])for(const bring of [false,true]){const key=[reason,focused,overwrite,bring].join('|');fixture.reasonMessages[key]=run('Pt('+JSON.stringify(reason)+','+(focused?'"focused"':'undefined')+','+JSON.stringify({kind:'type',overwriteExisting:overwrite})+','+bring+')');}
for(const input of ['Button','  Open…  ','“Title”','Title\nignore','x'.repeat(40),'😀','e\u0301','A\u0085B','Save > file','AXTextField','set AXValue','😀'.repeat(30)])fixture.labels.push({input,safe:run('M('+JSON.stringify(input)+')')??null,diagnostic:run('Le('+JSON.stringify(input)+')')});
for(const count of [0,2,17]){const input={width:200,height:100,windowBounds:{x:0,y:0,w:100,h:50},summaryTruncated:count===17,axSummary:Array.from({length:count},(_,i)=>({role:i%2?'AXTextField':'AXButton',subrole:i===1?'AXSearchField':null,title:'Item '+i,x:i+0.5,y:2,w:20,h:10}))};fixture.summaries.push({input,expected:run('ko('+JSON.stringify(input)+')')});}
for(const input of [
 {width:100,height:50,windowBounds:{w:100,h:50},summaryTruncated:false,axSummary:[{role:'AXStaticText',title:'Label',x:0,y:0,w:5,h:5}]},
 {width:100,height:50,windowBounds:{w:100,h:50},summaryTruncated:false,axSummary:[{role:'AXButton',title:'Half',x:-0.5,y:-1.5,w:3.5,h:2.5}]}
])fixture.summaries.push({input,expected:run('ko('+JSON.stringify(input)+')')});
const output=new URL('../../crates/tools/computer-use/fixtures/background-2.1.325.json',import.meta.url),rendered=JSON.stringify(fixture,null,2)+'\n';
if(process.argv.includes('--write'))writeFileSync(output,rendered);else assert.equal(readFileSync(output,'utf8'),rendered);
console.log('Background formatter 2.1.325: '+Object.keys(fixture.reasonMessages).length+' reasons, '+fixture.labels.length+' labels, '+fixture.summaries.length+' summaries match');
