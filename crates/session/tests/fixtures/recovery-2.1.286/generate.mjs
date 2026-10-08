// Execute bounded recovery functions extracted from the official 2.1.286 binary.
import fs from 'node:fs';
import vm from 'node:vm';
import path from 'node:path';
import { createHash } from 'node:crypto';
const source = fs.readFileSync(path.join(process.argv[2], 'src_183727495.js'), 'utf8');
if (createHash('sha256').update(source).digest('hex') !== '5f2e79dc208fa1829ae1b7e8862ea500c7b40b8a9bb26b51b9706852f7f79f92') {
  throw Error('Recovery oracle source differs from the pinned 2.1.286 chunk');
}
const context = vm.createContext({
  // Side-effect/feature boundaries; default production gates are enabled.
  i() {}, d() {}, bj() {}, wKt() {}, Gp: () => true, Y2o: () => true,
  L: v => v !== null && typeof v === 'object' && !Array.isArray(v),
  hx: (content,type) => Array.isArray(content) && content.some(b => b?.type === type),
  // Fixture rows are already-valid, unredacted JSONL, without compaction.
  MKt: () => ({admit: () => true, finish() {}}), Wan: () => true,
  hze: () => false, uI: row => ['user','assistant','system','attachment'].includes(row.type),
  xIe() {}, qi: row => row.type === 'system' && row.subtype === 'compact_boundary',
  qmr: row => row.type === 'attachment' && row.attachment?.type === 'fork_briefing',
  _mr: () => undefined,
});
for (const [start,end] of [
  ['function Jie(', 'var w2o='],
  ['function jD(', 'function gkr('],
  ['function kln(', 'function Mvo('],
  ['function yan(', 'function aes('],
]) {
  const at = source.indexOf(start), until = source.indexOf(end,at);
  if (at < 0 || until < 0) throw Error(`missing official anchors ${start} ${end}`);
  vm.runInContext(source.slice(at,until),context);
}
const row = (uuid,type,parentUuid,second,content,extra={}) => ({
  uuid,type,parentUuid,sessionId:'fixture-session',isSidechain:false,
  timestamp:`2026-09-30T00:00:${String(second).padStart(2,'0')}.000Z`,
  message:{role:type,content},...extra,
});
const user=(id,parent,time,text=id)=>row(id,'user',parent,time,text);
const assistant=(id,parent,time,msg,call)=>row(id,'assistant',parent,time,
  call?[{type:'tool_use',id:call,name:'Read',input:{}}]:[{type:'text',text:id}],{message:{role:'assistant',id:msg,content:call?[{type:'tool_use',id:call,name:'Read',input:{}}]:[{type:'text',text:id}]}});
const result=(id,parent,time,call,extra={})=>row(id,'user',parent,time,[{type:'tool_result',tool_use_id:call,content:id}],extra);
const meta=(id,parent,time,extra={})=>row(id,'attachment',parent,time,undefined,{attachment:{type:'hook_success'},...extra});
const base=()=>[user('root',null,0),assistant('a1','root',1,'batch','call1'),assistant('a2','a1',2,'batch','call2'),result('r1','a1',3,'call1'),result('r2','a2',4,'call2')];
const cases=[];
const add=(name,entries)=>{const loader=context.jln(false);entries.forEach(row=>loader.processEntry(structuredClone(row)));const loaded=loader.finish();const tip=context.jD(loaded.messages.values(),row=>loaded.leafUuids.has(row.uuid));cases.push({name,entries,tip:tip?.uuid??null,expected:tip?context.nat(loaded.messages,tip).map(row=>row.uuid):[]});};
add('legacy_parallel_recovery_order',[...base(),assistant('answer','r1',5,'next')]);
add('crash_after_parallel_batch',[...base(),{type:'last-prompt',leafUuid:'r2'},user('followup','r1',5),assistant('answer','followup',6,'next')]);
add('checkpoint_with_newer_timestamp',[...base().map(row=>row.uuid==='r2'?{...row,timestamp:'2026-09-30T00:00:59.000Z'}:row),{type:'last-prompt',leafUuid:'r2'},user('followup','r1',5),assistant('answer','followup',6,'next')]);
add('source_order_ignores_clock_skew',[...base().map(row=>row.uuid==='a2'?{...row,timestamp:'2026-09-30T00:00:58.000Z'}:row),assistant('answer','r1',6,'next')]);
add('call_id_recovers_misparented_result',[...base().map(row=>row.uuid==='r2'?{...row,parentUuid:'root'}:row),assistant('answer','r1',6,'next')]);
add('call_id_does_not_cross_agents',[...base().map(row=>row.uuid==='r2'?{...row,parentUuid:'root',agentId:'other'}:row),assistant('answer','r1',6,'next')]);
add('orphan_metadata_tail',[...base(),meta('hook','r2',5),meta('tail','hook',6),assistant('answer','r1',7,'next')]);
add('ambiguous_metadata_tail',[...base(),meta('hook','r2',5),meta('tail1','hook',6),meta('tail2','hook',6),assistant('answer','r1',7,'next')]);
add('explicit_checkpoint_blocks_sibling_jump',[...base(),{type:'last-prompt',leafUuid:'r2',explicit:true},user('followup','r1',5),assistant('answer','followup',6,'next')]);
add('explicit_checkpoint_allows_direct_continuation',[...base(),{type:'last-prompt',leafUuid:'r2',explicit:true},user('followup','r2',5),assistant('answer','followup',6,'next')]);
add('checkpoint_after_newer_rows',[...base(),user('followup','r1',5),assistant('answer','followup',6,'next'),{type:'last-prompt',leafUuid:'r2',explicit:true}]);
add('clear_to_empty',[...base(),{type:'last-prompt',leafUuid:null,explicit:true}]);
add('clear_then_new_turn',[...base(),{type:'last-prompt',leafUuid:null,explicit:true},user('fresh',null,7)]);
add('checkpoint_metadata_in_batch',[...base(),meta('hook','r2',5),{type:'last-prompt',leafUuid:'hook'},user('followup','r1',6),assistant('answer','followup',7,'next')]);
add('metadata_after_tip',[...base(),assistant('answer','r1',6,'next'),meta('tail','answer',7)]);
add('duplicate_indirect_result_is_not_replayed',[...base(),result('duplicate','root',5,'call2'),assistant('answer','r1',6,'next')]);
add('ambiguous_call_id_is_not_recovered',[...base().map(row=>row.uuid==='r2'?{...row,parentUuid:'root'}:row),assistant('other','root',5,'other-batch','call2'),assistant('answer','r1',6,'next')]);
add('metadata_tail_does_not_cross_sidechains',[...base(),meta('foreign-hook','r2',5,{isSidechain:true}),assistant('answer','r1',6,'next')]);
const wholeBatch=assistant('a','root',1,'batch','call1');
wholeBatch.message.content.push({type:'tool_use',id:'call2',name:'Read',input:{}});
add('unsplit_parallel_batch',[user('root',null,0),wholeBatch,result('r1','a',3,'call1'),result('r2','a',4,'call2'),{type:'last-prompt',leafUuid:'r2'},user('followup','r1',5),assistant('answer','followup',6,'next')]);
add('tool_attachment_checkpoint',[user('root',null,0),wholeBatch,result('r1','a',3,'call1'),meta('tool-hook','a',4,{attachment:{type:'hook_success',toolUseID:'call2'}}),{type:'last-prompt',leafUuid:'tool-hook'},user('followup','r1',5),assistant('answer','followup',6,'next')]);
fs.writeFileSync(new URL('cases.json',import.meta.url),JSON.stringify(cases,null,2)+'\n');
