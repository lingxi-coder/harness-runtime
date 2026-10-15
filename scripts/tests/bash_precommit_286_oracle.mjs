// Execute the exact precommit functions from the hash-pinned native reference.
// Usage: node scripts/tests/bash_precommit_286_oracle.mjs /path/to/2.1.286 [--check]
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
function range(offset, startMarker, endMarker) {
  const chunkEnd = binary.indexOf('// @bun @bytecode\n', offset + 1);
  const bytes = binary.subarray(offset, chunkEnd < 0 ? binary.length : chunkEnd);
  const start = bytes.indexOf(startMarker);
  const end = bytes.indexOf(endMarker, start + startMarker.length);
  if (start < 0 || end < start) throw new Error(`Missing source range: ${startMarker}`);
  sourceByteRanges.push({ signature: startMarker, start: offset + start, end: offset + end });
  return bytes.subarray(start, end).toString('utf8');
}
const offset = 183727495;
const source = [
  range(offset, 'function Lqe(', 'var xHs='),
  range(offset, 'function qzn(', 'function TM('),
  range(offset, 'function Dft(', 'function SJ('),
  range(offset, 'function Nqe(', 'var Qzn='),
  range(offset, 'async function Jzn(', 'async function tqn('),
  range(182118681, 'function RX(', 'function XAo('),
].join('\n');
const harness = vm.runInNewContext(`(()=>{
  let state={},settings={},gate=true,git=true,flagCalls=0;
  const w7='verify',DNt='simplify',YB='code-review',MNt='commit',LNt='pr';
  const Qw='TaskCreate',AT='TodoWrite',St='Agent',Be='Bash',_J='unused';
  const i=()=>{},c=value=>value,Je=()=>settings,ko=()=>state,sFo=()=>false;
  const vo=()=>{flagCalls++;return gate},QPt=()=>git,TM=()=>false,ET=()=>true;
  const Oft=async()=>({commit:'',pr:''}),$ft=()=>'';
  ${source}
  return {reset:()=>{state={};flagCalls=0},
    allowlist:(commands,names)=>RX(commands,names??undefined).map(command=>command.name),
    step:async(input)=>{
      if(input.new_session){state={};flagCalls=0}
      settings={includeCodeReviewSuggestion:input.include_code_review_suggestion};
      gate=input.flag??true;git=input.git??true;
      const commands=input.skill_tool===false?[]:input.commands;
      const skills=Lqe(commands);
      const full=await Jzn(commands),lean=await eqn(commands);
      const suggestion=await _It(skills,'bash',state.bashPromptCarriesPreShip===true);
      return {skills,suggestion,latched:state.bashPromptCarriesPreShip??null,flag_calls:flagCalls,
        verbose_boundary:suggestion?full.slice(full.indexOf(suggestion)-13,full.indexOf(suggestion)+suggestion.length+26):null,
        concise_boundary:suggestion?lean.slice(lean.indexOf(suggestion)-3):null};
    }};
})()`, {}, { timeout: 2_000 });
const command = (name, loadedFrom='skills') => ({ name, loadedFrom });
const cases = [];
async function add(name, steps) {
  harness.reset();
  const outputs=[];
  for (const input of steps) outputs.push(await harness.step(input));
  cases.push({name,steps,outputs});
}
await add('no_skills', [{commands:[]}]);
for (const origin of ['skills','commands_DEPRECATED','builtin','plugin','bundled','managed','syncedSkills']) {
  await add(`verify_${origin}`, [{commands:[command('verify',origin)]}]);
  await add(`simplify_${origin}`, [{commands:[command('simplify',origin)]}]);
}
await add('verify_and_simplify', [{commands:[command('verify'),command('simplify')]}]);
await add('all_three', [{commands:[command('verify'),command('simplify'),command('code-review','builtin')],include_code_review_suggestion:true}]);
for (const setting of [null,false,true]) {
  await add(`code_review_${setting}`, [{commands:[command('code-review','plugin')],include_code_review_suggestion:setting}]);
}
await add('first_matching_command_wins', [{commands:[command('verify','builtin'),command('verify')]}]);
await add('qualified_name_is_not_verify', [{commands:[command('plugin:verify','skills')]}]);
await add('skill_tool_absent', [{commands:[command('verify')],skill_tool:false}]);
await add('gate_enabled_stays_enabled', [{commands:[command('verify')],flag:true},{commands:[command('simplify')],flag:false}]);
await add('gate_disabled_stays_disabled', [{commands:[command('verify')],flag:false},{commands:[command('simplify')],flag:true}]);
await add('git_disabled_defers_latch', [{commands:[command('verify')],git:false,flag:false},{commands:[command('verify')],flag:true}]);
await add('no_skills_still_latches', [{commands:[],flag:false},{commands:[command('verify')],flag:true}]);
await add('code_review_setting_remains_live', [{commands:[command('code-review')],include_code_review_suggestion:false},{commands:[command('code-review')],include_code_review_suggestion:true}]);
await add('session_activation_resets_latch', [{commands:[command('verify')],flag:false},{commands:[command('verify')],flag:true,new_session:true}]);
const grantCommands=[
  {...command('verify'),type:'prompt',source:'settings'},
  {...command('plugin:simplify','plugin'),type:'prompt',source:'plugin'},
  {...command('synced:check','syncedSkills'),type:'prompt',source:'settings',aliases:['verify']},
  {...command('ordinary:check','plugin'),type:'prompt',source:'plugin',aliases:['verify']},
  {...command('mcp:verify','mcp'),type:'prompt',source:'mcp'},
  {...command('synced:verify','syncedSkills'),type:'prompt',source:'settings'},
];
const allowlistCases=[null,[],['verify'],['simplify'],['mcp:verify'],['ordinary:check']].map(allowlist=>({
  commands:grantCommands,allowlist,selected:harness.allowlist(grantCommands,allowlist),
}));
const output=JSON.stringify({version:'2.1.286',binary_sha256:sha256,source_byte_ranges:sourceByteRanges,cases,allowlist_cases:allowlistCases},null,2)+'\n';
const fixture=fileURLToPath(new URL('../../crates/tools/shell/tests/fixtures/bash_precommit_286.json',import.meta.url));
if (process.argv.includes('--check')) {
  if (readFileSync(fixture,'utf8')!==output) throw new Error('Bash precommit fixture differs from the native reference');
} else writeFileSync(fixture,output);
console.log(`Verified ${cases.length} Bash precommit cases from Claude Code 2.1.286 (${sha256})`);
