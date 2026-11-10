// Replay exact bundled MCP envelope schemas and SSE parser, without networking.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync,mkdirSync} from 'node:fs';
import vm from 'node:vm';
import {loadNativeOAuthSchemas,BASELINE_SHA256} from './mcp_oauth_322_schema_oracle.mjs';
const args=process.argv.slice(2),binaryPath=args.find(arg=>!arg.startsWith('--'));
const binary=readFileSync(binaryPath),sha=data=>createHash('sha256').update(data).digest('hex');
assert.equal(sha(binary),BASELINE_SHA256);
const {dependencies}=loadNativeOAuthSchemas(binaryPath,binary),ranges=[];
function slice(name,start,end){const bytes=binary.subarray(start,end);ranges.push({name,start,end,sha256:sha(bytes)});return bytes.toString('utf8');}
const schemaStart=binary.indexOf('var At="2025-11-25";',304763858);
const schemaEnd=binary.indexOf(',Ac=j(',schemaStart);
const schemas=slice('native-mcp-envelope-schemas',schemaStart,schemaEnd)+';';
const guardStart=binary.indexOf('function $t(e)',schemaEnd),guardEnd=binary.indexOf(';var vi=',guardStart);
const guards=slice('native-mcp-envelope-parser-guards',guardStart,guardEnd)+';';
const decode=vm.runInNewContext(schemas+guards+';$t',{...dependencies},{timeout:3000});
const inputs=[
 'null','[]','1','true','"text"','{}','{"invalid":"frame"}',
 '{"jsonrpc":"2.0","id":7,"result":[]}',
 '{"jsonrpc":"1.0","id":7,"result":{}}',
 '{"jsonrpc":"2.0","id":null,"result":{}}',
 '{"jsonrpc":"2.0","id":1.25,"result":{}}',
 '{"jsonrpc":"2.0","id":9007199254740992,"result":{}}',
 '{"jsonrpc":"2.0","id":7,"method":1,"params":[]}',
 '{"jsonrpc":"2.0","id":7,"result":{"_meta":1}}',
 '{"jsonrpc":"2.0","id":7,"result":{},"extra":true}',
 '{"jsonrpc":"2.0","id":7,"result":{},"\\ud800":true}',
 '{"jsonrpc":"2.0","method":"ping","params":{"_meta":null}}',
 '{"jsonrpc":"2.0","method":"ping","params":{"_meta":{"progressToken":[]}}}',
 '{"jsonrpc":"2.0","method":"ping","params":{"_meta":{"io.modelcontextprotocol/related-task":{"taskId":1}}}}',
 '{"jsonrpc":"2.0","id":7,"error":{"code":1.25,"message":true}}',
 '{"jsonrpc":"2.0","id":7,"error":{"code":1,"message":"ok","data":"\\ud800","extra":true}}',
 '{"jsonrpc":"2.0","id":7,"result":{"text":"\\ud800","_meta":{"serverInfo":1}}}',
 '{"jsonrpc":"2.0","id":7,"method":"ping","params":{"_meta":{"progressToken":7}}}',
];
const envelopeCases=inputs.map(input=>{try{return{input,output:JSON.stringify(decode(JSON.parse(input)))}}catch(error){return{input,error:error.message,issues:JSON.stringify(error.issues)}}});
const parserStart=binary.indexOf('var I=10,_=13,A=32;function y(n){}function ZHt(',270300000);
const parserEnd=binary.indexOf('\nexport{',parserStart);
const parser=slice('native-eventsource-parser',parserStart,parserEnd);
const parse=vm.runInNewContext(parser+';ZHt',{O:Error},{timeout:1000});
const sseInputs=[
 ['retry: 13\n\n'],['id: cursor\n\ndata: one\n\n'],['data: one\n\nid:\ndata: two\n\n'],
 ['id: first\nevent: other\ndata: a\r\n\r\n','data: b\n\n'],
 ['retry: +2\nretry: -1\nretry: 7\n\ndata: ok\n\n'],
 ['data: café 😀\r','\n','data: Ω\r\n\r','\n'],['data: unfinished'],
 ['id: one\nid: bad\u0000id\ndata: value\n\n'],['data\n\n'],['data: x\r'],
];
const sseCases=sseInputs.map(chunks=>{const items=[];const target=parse({onEvent:event=>items.push({event:event.event??'',data:event.data,id:event.id??null}),onRetry:retry=>items.push({retry})});for(const chunk of chunks)target.feed(chunk);return{chunks,items};});
const budgetStart=binary.indexOf('var vt=jq,jr=4*vt,',305502000),budgetEnd=binary.indexOf('function Mt(',budgetStart);
const budgetSource=slice('native-http-body-budget',budgetStart,budgetEnd);
const limitStart=binary.indexOf('jq=16777216',266155000),messageLimit=Number(slice('native-mcp-message-byte-limit',limitStart,limitStart+'jq=16777216'.length).split('=')[1]);
const factoryEnd=binary.indexOf('var Kr=new Set(',budgetEnd),factorySource=slice('native-http-body-wrapper',budgetEnd,factoryEnd);
const guard=vm.runInNewContext(budgetSource+';Gr',{jq:messageLimit,TransformStream,performance,setImmediate},{timeout:1000});
const bodyBudgetCases=[];
for(const [eventLimit,totalLimit,chunks]of [[16,16,['1234567890123456']],[16,16,['1234567890123456','x']],[8,32,['123456789']],[8,32,['x: 123\n\n','x: 123\n\n']],[8,32,['x: 123\r','\n\r','\n','12345678']],[16,null,Array(20).fill('data: x\n\n')],[16,64,Array(20).fill('data: x\n\n')]]){
 const stream=new ReadableStream({start(controller){for(const chunk of chunks)controller.enqueue(new TextEncoder().encode(chunk));controller.close();}}).pipeThrough(guard(eventLimit,totalLimit??Infinity));
 let error=null;try{for await(const _chunk of stream){}}catch(failure){error=failure.message;}
 bodyBudgetCases.push({eventLimit,totalLimit,chunks,error});
}
const fixture={version:'2.1.322',native_sha256:BASELINE_SHA256,ranges,envelopeCases,sseCases,bodyBudgetCases,scope:'Exact shipped MCP envelope schemas with complete bundled Zod dependencies, source parser guards, eventsource-parser and body-budget transform. JSON.parse runs in Node for valid JSON inputs only; native Bun SyntaxError and 17MiB overflow lifecycle semantics are separately captured by actual native CLI loopback.'};
const output=args.find(arg=>arg.startsWith('--source-output='))?.slice('--source-output='.length);
if(output){mkdirSync(output,{recursive:true});for(const[name,source]of [['envelopes.js',schemas],['guards.js',guards],['sse-parser.js',parser],['body-budget.js',budgetSource],['body-wrapper.js',factorySource]])writeFileSync(output+'/'+name,source);writeFileSync(output+'/ranges.json',JSON.stringify({native_sha256:BASELINE_SHA256,ranges,derived_source_note:'Schema and guard files add a trailing semicolon; parser and body budget/wrapper are unchanged.'},null,2)+'\n');}
const target=new URL('../../crates/platforms/common/tests/fixtures/mcp_remote_gap_2_1_322.json',import.meta.url),rendered=JSON.stringify(fixture,null,2)+'\n';
if(args.includes('--write'))writeFileSync(target,rendered);else assert.equal(readFileSync(target,'utf8'),rendered);
console.log(`PASS native MCP gap replay: ${envelopeCases.length} envelope, ${sseCases.length} SSE framing and ${bodyBudgetCases.length} body budget cases`);
