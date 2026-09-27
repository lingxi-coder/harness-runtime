// Regenerate with extracted official 2.1.270 source as the first argument.
import fs from 'node:fs';
import vm from 'node:vm';
const source = fs.readFileSync(process.argv[2], 'utf8');
const start = source.indexOf('function ARt(');
const end = source.indexOf('function BQn(', start);
if (start < 0 || end < 0) throw new Error('2.1.270 ARt anchor missing');
const context = vm.createContext({
  GYs: 200, npe: (value) => value, mE: () => '22222222-2222-4222-8222-222222222222', x: (count, word) => count === 1 ? word : word + 's',
  Date: class { toISOString() { return '2026-09-12T09:00:00.000Z'; } },
});
vm.runInContext(source.slice(start, end), context);
const common = {task:{id:'task-1',cron:'1 9 * * *',prompt:'/loop',kind:'loop'},uuid:'11111111-1111-4111-8111-111111111111',cronKind:'loop'};
const cases = [{...common, task:{id:"fixed-1",cron:"*/5 * * * *",prompt:"check"},cronKind:undefined}, common, {...common,noOpStreak:2,streakStartedAt:'2026-09-12T08:00:00.000Z',foldedUuids:['00000000-0000-4000-8000-000000000000']}];
const lines = cases.map((options) => JSON.stringify({parentUuid:null,isSidechain:false,
  ...context.ARt('fire',options),userType:'external',entrypoint:'cli',cwd:'/fixture',sessionId:'session-1',version:'0.12.0'}));
fs.writeFileSync(new URL('scheduled_fire.jsonl', import.meta.url), lines.join('\n')+'\n');

const teStart = source.indexOf('function Te(');
vm.runInContext(source.slice(teStart, source.indexOf('function OM(', teStart)), context);
const schedulerSource = fs.readFileSync(new URL('src_197155721.js', 'file://' + process.argv[2]), 'utf8');
const kStart = schedulerSource.indexOf('function K(');
vm.runInContext(schedulerSource.slice(kStart, schedulerSource.indexOf('function S(', kStart)), context);
const companion = JSON.stringify({parentUuid:null,isSidechain:false,...context.K(2),
  userType:'external',entrypoint:'cli',cwd:'/fixture',sessionId:'session-1',version:'0.12.0'});
fs.writeFileSync(new URL('turn_companion.jsonl', import.meta.url), companion+'\n');
const utility = fs.readFileSync(new URL('src_164782789.js', 'file://' + process.argv[2]), 'utf8');
const sanitizer = vm.createContext({ Buffer });
for (const [begin, end] of [['var w3=', 'function Nr('], ['function Nr(', 'function UI('], ['var De=', 'function eE('], ['function eE(', 'function tpe('], ['function ne(', 'function Ed('], ['function Oe(', 'function It('], ['function npe(', 'function hFt(']]) {
  const at = utility.indexOf(begin);
  vm.runInContext(utility.slice(at, utility.indexOf(end, at)), sanitizer);
}
const inputs = ['\x1b[\x1b[31m32mred','\x1b[٣١mred', '/loop', '\x1b[31mhello\x1b[0m\n\t world\u200b', 'a\ufeffb', 'a'.repeat(199)+'😀z', 'a'.repeat(198)+'😀z'];
fs.writeFileSync(new URL('prompt.json', import.meta.url), JSON.stringify(inputs.map(input => ({input,expected:sanitizer.npe(input,200)})),null,2)+'\n');
