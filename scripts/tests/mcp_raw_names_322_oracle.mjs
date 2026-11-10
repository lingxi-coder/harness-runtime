// Execute exact native normalization/provenance helpers on authored JSON keys.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import {homedir} from 'node:os';
import path from 'node:path';
import vm from 'node:vm';
const args=process.argv.slice(2);
const binary=readFileSync(args.find(arg=>!arg.startsWith('--'))??path.join(homedir(),'.local/share/claude/versions/2.1.322'));
const hash=bytes=>createHash('sha256').update(bytes).digest('hex');
const sha='a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450';assert.equal(hash(binary),sha);
const ranges=[{name:'normalize',start:260530248,end:260530385,sha256:'2a939978d12b8f9007235f0b22462fde69be38a1ec2b1016ef30aeb03ece18c4'},{name:'provenance',start:267958047,end:267958290,sha256:'d3b7bbc12d30ab386e668cd8398fb1d4462b517cba2877330e71ada3165dd3b4'}];
const catalogRanges=[{name:'wellformed',start:260462323,end:260462884,sha256:'b185fd1e8a6849ece80201f3443addd5daa90bf0363928bfd4b23b87ff43110e'},{name:'catalog-normalization',start:260471379,end:260472218,sha256:'00177a3c16f1ac0dec1d8b71ba2ab6486105a18d7d870d2ff416208e9016c658'},{name:'catalog-sanitize',start:260481569,end:260482231,sha256:'604d81337edb1b5023b055444efdeffe02be5f4983692140581b8cd60fbdfd66'}];
const source=ranges.map(range=>{const bytes=binary.subarray(range.start,range.end);assert.equal(hash(bytes),range.sha256);return bytes.toString('utf8')}).join('\n');
const native=vm.runInNewContext(source+';({normalize:Sn,provenance:QIr})',{QU:{ownerOfKey:()=>undefined}},{timeout:1000});
const catalogSource=catalogRanges.map(range=>{const bytes=binary.subarray(range.start,range.end);assert.equal(hash(bytes),range.sha256);return bytes.toString('utf8')}).join('\n');
const sanitize=vm.runInNewContext(catalogSource+';Ckt',{}, {timeout:1000});
const units=value=>Array.from({length:value.length},(_,index)=>value.charCodeAt(index));
const cases=[];
for(const name of ['', 'wire audit','a😀b','\ufffd','\ud800','\udfff','wire \ud800','wire \udfff','a\ud800\ud800b','__lingxi_mcp_utf16_d800','plugin:fixture:\ud800','claude.ai \ud800']){
 const rawConfig=JSON.stringify({mcpServers:{[name]:{command:'fake'}}});
 const authored=Object.keys(JSON.parse(rawConfig).mcpServers)[0];
 const provenance=native.provenance({serverName:authored,scope:'dynamic',serverType:'stdio'});
 cases.push({rawConfig,nameUnits:units(authored),normalized:native.normalize(authored),expectedProvenanceJson:JSON.stringify(provenance)});
}
const toolCases=['echo \ud800','echo \udfff','a\ud800\ud800b','echo 😀','ＦＵＬＬ','a\u200bb','a\ue000b','ordinary','\ufffd','e\u0301'].map(name=>{
 assert.ok(name.length<32,'These bounded cases use the unchanged native short-name NFKC path');
 const cleaned=sanitize(name);return {nameUnits:units(name),sanitizedUnits:units(cleaned),sanitized:cleaned,normalized:native.normalize(cleaned),expectedCallParamsJson:JSON.stringify({name:cleaned,arguments:{}}),directCallParamsJson:JSON.stringify({name,arguments:{}})};
});
const fixture={version:'2.1.322',native_sha256:sha,ranges,catalogRanges,scope:'Unchanged native helpers and actual JavaScript JSON.parse/Object.keys on source-keyed MCP records. Catalog-name cases execute native fg/NFKC/Lh/Ckt short-name paths; direct JSON serialization preserves caller string units.',cases,toolCases};
const target=new URL('../../crates/mcp/tests/fixtures/mcp_raw_names_2_1_322.json',import.meta.url);const rendered=JSON.stringify(fixture,null,2)+'\n';
if(args.includes('--write'))writeFileSync(target,rendered);else assert.equal(hash(readFileSync(target)),hash(rendered));
console.log(`Claude Code2.1.322 raw MCP names: ${cases.length} server cases and ${toolCases.length} catalog/direct-name cases verified`);
