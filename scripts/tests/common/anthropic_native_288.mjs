import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
export function nativeBetaWorld(binaryPath) {
const binary=readFileSync(binaryPath),hash=bytes=>createHash('sha256').update(bytes).digest('hex'),ranges=[];
assert.equal(hash(binary),'bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750');
function take(first,last,offset,label){const start=binary.indexOf(first,offset),end=binary.indexOf(last,start+first.length);assert(start>=offset&&end>start&&end-start<4000,label);const bytes=binary.subarray(start,end);ranges.push({label,start,end,sha256:hash(bytes)});return bytes.toString();}
const identities=[['Vfe=w(', ',P8r=',180411700],['W2e=w(', ',nR=',180411200],['cEe=w(', ',r$=',180412700],['aY=w(', ',dL=',180411500],['uv=w(', ',n$=',180411800]].map(([a,b,c])=>take(a,b,c,'actual '+a.slice(0,3)+' beta identity'));
const pieces=[
 take('class xt extends Un','class Dd extends xt',178899273,'actual APIError construction and message bytes'),
 take('var EF="capability_rejected: ";','function ONo(',186225290,'actual capability boundaries'),
 take('function b4n(e)','var fie=',186225540,'actual context predicates'),
 take('function KFr(e)','function YFr(',186226000,'actual thinking signature copy'),
 take('function XT(e)','function Pne(',187976120,'actual signature guard'),
 take('function XO(e)','function DEt(',187975180,'actual prefix mismatch and declined guard'),
 take('function vA(e)','function tEt(',187958790,'actual context, credit, organization and output-filter guards'),
 take('var yeo=','function pMe(',187967900,'actual encrypted-content and advisor guards'),
 take('var xF=','function v4n(',186227850,'actual mid-system recognition'),
 take('function YT(e)','function Cne(',187972410,'actual mid-system guard'),
 take('function Yle(e)','function oFe(',188948210,'actual fast-mode guard'),
 take('function O(e){return typeof e===','function N(e)',181200700,'actual structured error envelope test'),
 take('var sve="dlp_request_denied";','var utt=',181197500,'actual DLP and policy guards'),
 take('function sRn(e)','var ye=',181198000,'actual error-code extraction'),
 take('function hne(e)','var RZn=',187954900,'actual disabled guard'),
 take('function H0(e,n=He())','function Fvt(',187955250,'actual disabled provider guard'),
 take('function lMe(e)','function SEt(',187967200,'actual first-line error extraction'),
 take('function hb(e,n)','function YT(',187972370,'actual beta header matching'),
 take('var TEt=','function CEt(',187969300,'actual display validation pattern'),
 take('var yMe=new Map','function bMe(',187969870,'actual kMe and SMe named beta policy'),
 take('WGt=[','async function*Kle(',188935300,'actual complete GGt composition'),
 take('E3e=(qn)=>','T3e=',189030900,'actual earlier first-party token-count repair'),
 take('T3e=(qn)=>','C3e=',189031440,'actual named beta callback'),
 take('let Gt=(qn,Go)=>',';await i5e(',188981200,'actual immediate conversation and model latch commit'),
];
const context=vm.createContext({});
vm.runInContext(`var Un=Error;function w(name,header){return {name,header};} var ${identities.join(';var ')};
function He(){return 'firstParty';}function mir(e){return e.status;}function t(){}function i(){}function c(e){throw e;}function $fe(x){return x;}function Et(x){return x;}function _A(){return 'fixture';}
var kw=[],Qe=[],qe={rejected:new Set()},Bt='fixture',V={model:'fixture'},modelRejected=new Map();
function sm(state,beta){state.rejected.add(beta.header);}function jso(model,beta){if(!modelRejected.has(model))modelRejected.set(model,new Set());modelRejected.get(model).add(beta.header);}
function C4e(){return false;}
${pieces.slice(0,20).join('\n')}
var ${pieces[20]};var ${pieces[21]} noop=0;var ${pieces[22]} noop2=0;
${pieces[23]};
`,context);
// E3e's Tne dependency is a separate native predicate, rather than a guessed flag.
const tokenPredicate=take('function Tne(e)','var yMe=',187969750,'actual invalid-beta predicate');vm.runInContext(tokenPredicate,context);
const betaHeaders=[context.Vfe,context.W2e,context.cEe,context.aY].map(beta=>beta.header),messages=[
 'unknown request failure','invalid beta flag',
 ...betaHeaders.flatMap(header=>[
  `Unexpected value(s) \`${header}\` for the \`anthropic-beta\` header`,
  `Unexpected value(s) \`${header}\`, for the \`anthropic_beta\` header`,
  `Unrecognized ${header} in anthropic-beta`,
  `capability_rejected: beta_header:${header}`,
  `capability_rejected: beta_header:${header}_suffix`,
  `${header} alone`,
 ]),
 `Unexpected value(s) \`${betaHeaders[2]}\`, \`${betaHeaders[0]}\` for the \`anthropic-beta\` header`,
 'thinking.adaptive.display: Input should be omitted','thinking.enabled.display: Input should be updates',
 'thinking.adaptive.display: input should be omitted','evict_on_complete requires a beta',
 'not created in this conversation; thinking.adaptive.display: Input should be omitted',
 'signature in thinking block; thinking.adaptive.display: Input should be omitted',
];
function error(status,body,declined=false){context.inputBody=body;context.inputStatus=status;const e=vm.runInContext('new xt(inputStatus,inputBody,undefined,undefined,inputBody?.error?.type)',context);e.prefixLockHealDeclined=declined;return e;}

return {binary,hash,ranges,take,context,betaHeaders,messages,error};
}
