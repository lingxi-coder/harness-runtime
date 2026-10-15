#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync, writeFileSync} from 'node:fs';
import vm from 'node:vm';

const binary = readFileSync(process.argv[2]);
const sha256 = value => createHash('sha256').update(value).digest('hex');
assert.equal(sha256(binary), 'bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750');
const sourceByteRanges = [];

function take(first, last, offset, label, maxBytes = 5000) {
  const start = binary.indexOf(first, offset);
  const end = binary.indexOf(last, start + first.length);
  assert(start >= offset && end > start && end - start < maxBytes, label);
  const bytes = binary.subarray(start, end);
  sourceByteRanges.push({label, start, end, sha256: sha256(bytes)});
  return bytes.toString();
}

const apiError = take(
  'class xt extends Un', 'class Dd extends xt', 178899273,
  'native APIError construction, status factory, and error message',
);
const badRequest = take(
  'class Xvt extends xt{}', 'class mMe extends xt', 178901700,
  'native BadRequestError subclass of APIError',
);
const betaIdentities = take(
  'uL=w("server_side_fallback"', ',XU=', 180412200,
  'native June and July server fallback beta identities',
);
const classifier = take(
  'function bMe(e)', 'function wMe(e)', 187970437,
  'native bMe error classifier',
);
const requestGate = take(
  'let pm=Tu&&', ';let nd=!1', 189012900,
  'native fallback parameter gate and a1 assignment',
);
const attemptState = take(
  'a1=!1,O3="none",D3=!1,oSe=!1', 'if(V.fallbackCreditCode', 189006490,
  'native query-local fallback parameter and one-shot initialization',
);
const retryHandler = take(
  'r3e=(qn,Go)=>', ',pF=(qn)=>', 189017800,
  'native shared server fallback HTTP repair handler', 5000,
);
const finalBodySpread = take(
  'let Fl={model:', '};Tye=u3', 189014000,
  'native final request body spread order', 3000,
);

const context = vm.createContext({Headers});
vm.runInContext(`
class Un extends Error {}
function w(name, header) { return {name, header}; }
var ${betaIdentities};
var XU = w('fallback_credit', 'fallback-credit-2026-06-01');
${apiError}
${badRequest}
${classifier}

var LR=false, QM, a1=false, O3='none', D3=false, oSe=false, dF=false;
var M6e=false, L6e=false, j3=false, A6e=false, F6e=false;
var V={querySource:'main', stickyBetas:void 0};
var qe={rejected:[]};
function sm(state,beta){state.rejected.push(beta.header);}
function jp(state,beta){return state.rejected.includes(beta.header);}
function AO(){}
function wMe(){return void 0;}
function t(){}
function i(){}
function d(value){return value;}
function is(value){return value;}
function _(value){return value;}
function es(value){return value;}
function ph(){}
function D2(){return false;} function YT(){return false;}
function YO(){return false;} function X0(){return false;}
function H0(){return false;} function hne(){return false;}
function Yle(){return false;} function Vle(){return void 0;}
function x4e(){return false;} function pF(){return false;}
function vye(){return void 0;} function Eye(){return void 0;}
function Gft(){return 'unknown';} function vne(){return false;}
function dMe(){return false;} function pMe(){return false;}
function kne(){return void 0;} function kMe(){return [];}
function ST(){return []; } function mMeGuard(){return false;}
${retryHandler}

function computeNativeA1(Tu, Ip, oc) {
  var a1=false, O3='none', ql=false, ia=[], $g, nw;
  function VY(){return false;}
  ${requestGate};
  return {present:a1, mode:O3, parameter:pm};
}

function evaluate(status, body, fallbackParameterAdded, repairAlreadyAttempted) {
  LR=false; QM=void 0; a1=fallbackParameterAdded; O3=fallbackParameterAdded?'explicit':'none';
  D3=false; oSe=repairAlreadyAttempted; dF=false; M6e=false; L6e=false;
  j3=false; A6e=false; F6e=false; qe={rejected:[]};
  let error;
  if (status===400) error=xt.generate(status, body, void 0, new Headers());
  else error=new xt(status, body, void 0, new Headers());
  const cause=bMe(error);
  const retry=r3e(error, 'stream');
  return {className:error.constructor.name, isApiError:error instanceof xt,
    cause:cause??null, retry:retry??null, retired:qe.rejected};
}
`, context);

const messages = [
  {name:'category_beta_header', body:{type:'error',error:{type:'invalid_request_error',message:'Invalid beta header `server-side-fallback-2026-07-01` for anthropic-beta'}}},
  {name:'default_unconfigured', body:{type:'error',error:{type:'invalid_request_error',message:'has no default fallback configuration'}}},
  {name:'generic_beta_header_precedes_default_unconfigured', body:{type:'error',error:{type:'invalid_request_error',message:'`server-side-fallback-2026-06-01` anthropic-beta has no default fallback configuration'}}},
  {name:'ordinary_400', body:{type:'error',error:{type:'invalid_request_error',message:'fixture rejection'}}},
];
const cases=[];
for (const status of [400,401,403,500]) {
  for (const message of messages) {
    for (const fallbackParameterAdded of [false,true]) {
      for (const repairAlreadyAttempted of [false,true]) {
        const actual=JSON.parse(JSON.stringify(context.evaluate(
          status,message.body,fallbackParameterAdded,repairAlreadyAttempted,
        )));
        cases.push({status,name:message.name,error_body:message.body,
          fallback_parameter_added:fallbackParameterAdded,
          repair_already_attempted:repairAlreadyAttempted,actual});
      }
    }
  }
}

const a1Cases=[];
for (const Tu of [false,true]) {
  for (const fallback of [undefined,'default',[{model:'fallback-model'}]]) {
    for (const sent of [[],['server-side-fallback-2026-06-01'],['server-side-fallback-2026-07-01'],['server-side-fallback-2026-06-01','server-side-fallback-2026-07-01']]) {
      const input={}; if(fallback!==undefined)input.fallbacks=fallback;
      const betaObjects=sent.map(value=>value==='server-side-fallback-2026-07-01'
        ? context.I_
        : value==='server-side-fallback-2026-06-01' ? context.uL : {header:value});
      const result=JSON.parse(JSON.stringify(context.computeNativeA1(Tu,betaObjects,input)));
      a1Cases.push({Tu,sent,fallback:input.fallbacks??null,actual:result});
    }
  }
}

// Anchor the native source evaluation on the retry conditions the SDK exposes.
const find=(status,name,a1,attempted)=>cases.find(c=>c.status===status&&c.name===name&&c.fallback_parameter_added===a1&&c.repair_already_attempted===attempted).actual;
assert.deepEqual(find(400,'category_beta_header',false,false),{
  className:'Xvt',isApiError:true,cause:'category_beta_header',
  retry:'retry:server-fallback-category-strip',retired:['server-side-fallback-2026-07-01'],
});
assert.equal(find(400,'default_unconfigured',false,false).cause,'default_unconfigured');
assert.equal(find(400,'default_unconfigured',false,false).retry,null);
assert.equal(find(400,'default_unconfigured',true,false).retry,'retry:server-fallback-category-strip');
assert.equal(find(400,'category_beta_header',false,true).retry,null);
assert.equal(find(401,'category_beta_header',true,false).cause,null);
assert(a1Cases.some(c=>c.Tu&&c.fallback==='default'&&c.sent.includes('server-side-fallback-2026-07-01')&&c.actual.present&&c.actual.mode==='default'));
assert(a1Cases.some(c=>c.Tu&&Array.isArray(c.fallback)&&c.sent.includes('server-side-fallback-2026-06-01')&&c.actual.present&&c.actual.mode==='explicit'));
assert(a1Cases.some(c=>!c.Tu&&c.fallback==='default'&&c.actual.present===false));

const data={
  version:'2.1.288',
  native_sha256:sha256(binary),
  sourceByteRanges,
  scope:'Actual extracted APIError/BadRequestError, bMe, a1 transport gate, and shared r3e executed in a VM. This covers classifier and retry guard semantics; provider wrapper transport was separately checked with localhost mocks. No live provider or signed cloud request.',
  cases,
  a1Cases,
};
const output=JSON.stringify(data,null,2)+'\n';
const fixture=new URL('../../deps/llm-client/tests/fixtures/server_fallback_repair_2_1_288.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(readFileSync(fixture,'utf8'),output);
else writeFileSync(fixture,output);
console.log(JSON.stringify({mode:process.argv.includes('--check')?'checked':'wrote',cases:cases.length,a1Cases:a1Cases.length,ranges:sourceByteRanges.length}));
