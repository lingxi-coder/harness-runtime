// Extracted source closure for 2.1.295; source module src_188159463.js sha256 3e358ce5fa2861c3c7936b4a7172bd6a7b9909b7202feac3c5f1d2ac773f642f
import { c } from "./src_186460102.mjs";
import { G, E, As, rt } from "./src_186056234.mjs";
import { tG, vHt, hut, Pl, dY, nOs } from "./src_186715191.mjs";
import { basename as _e, basename as Ae, sep as Ue } from "path";
import { O } from "./src_186477637.mjs";
import { fo } from "./src_187670201.mjs";
import { a } from "./src_186481843.mjs";
import { vb, St, t, Xi, ah } from "./src_186240709.mjs";
import { Xe } from "./src_186066580.mjs";
import { ZRr } from "./src_188108105.mjs";
import { homedir as $e } from "os";
var crn="<user-code>";
function Be(){try{return $e()}catch{return""}}
var oe="cli";
var ie=new Set(["cli.js",oe,"sdk.mjs","browser-sdk.js","agentSdk.js"]);
var se=/^chunk-[0-9a-z]+\.js$/;
function ce(e){return e[0]==="/"&&se.test(e.slice(1))}
function de(e){return ce(e)||e[0]==="/"&&ie.has(e.slice(1))}
var He=/^(?:\/(?:\$bunfs|~BUN)|\/?[A-Za-z]:[\\/](?:\$bunfs|~BUN))[\\/]/;
function ue(e){let n=e.startsWith("file://")?e.slice(7):e;return He.test(n)}
function We(e){let n=e;if(n.startsWith("file://"))n=n.slice(7);if(ue(n))return ARr(n);if(n.startsWith("/snapshot/"))n=n.slice(10);let r=Be();if(r&&n.startsWith(r+Ue))n="~"+n.slice(r.length);return n}
var Ve=["src/","packages/"];
function ARr(e){let n=Math.max(e.lastIndexOf("/"),e.lastIndexOf("\\"));return n===-1?e:e.slice(n+1)}
function RWn(e){return de(e.file)||Ve.some((n)=>e.file.startsWith(n))}
var je=["node:","bun:","internal:"];
var Ke=new Set(["native","unknown","ws","undici","node-fetch","isomorphic-fetch","vercel_fetch","utf-8-validate"]);
function ae(e){return Ke.has(e)||je.some((n)=>e.startsWith(n))}
function lTo(e){return ae(e.file)}
function Je(e){let n=e.split(/[/\\]/),r=-1;for(let u=n.length-1;u>=0;u--)if(n[u]==="node_modules"){r=u;break}if(r===-1||r>=n.length-1)return null;let o=n[r+1],i;if(o.startsWith("@")){if(r+2>=n.length)return null;i=`${o}/${n[r+2]}`}else i=o;let s=n.at(-1);if(!s)return null;return`node_modules/${i}/${s}`}
function Ye(e){let n=We(e),r=ARr(n),o=Je(n);if(ie.has(r)&&(o===null||o.startsWith("node_modules/@anthropic-ai/"))||ue(e)&&se.test(r))return"/"+r;let i=null;if(i)return i;if(ae(n))return n;return crn}
function ze(e){let n=e.trim();if(!n.startsWith("at "))return null;let r=n.slice(3),o=r.indexOf(" ("),i,s;if(o!==-1&&r.endsWith(")"))i=r.slice(0,o).trim(),s=r.slice(o+2,-1);else s=r.trim();if(i){if(i=i.replace(/^async\s+/,"").replace(/^new\s+/,""),i=i.replace(/\s*\[as\s+[^\]]+\]$/,""),!i)i=void 0}let u=s.match(/^(.*):(\d+):(\d+)$/);if(!u)return null;let[,d,p,f]=u,_=Ye(d);return{file:_,line:Number(p),column:Number(f),function:_===crn&&i?crn:i}}
var le=50;
function drn(e,n=le){let r=[];for(let o of gHt(e).split(`
`)){let i=ze(o);if(i){if(r.push(i),r.length>=n)break}}return r}
function gHt(e){let n=typeof e.stack==="string"?e.stack:"",r=typeof e.message==="string"?e.message:"",o=qe(n,e.name,r);return o===-1?n:n.slice(o+1)}
function qe(e,n,r){let o=(s)=>s!==""&&e.startsWith(`${s}
`);if(r!==""){let s=typeof n==="string"&&n!==""?`${n}: ${r}`:"";if(s!==""&&o(s))return s.length;let u=`: ${r}`,d=e.indexOf(`${u}
`);return d>0&&!e.slice(0,d).includes(`
`)?d+u.length:n===""&&o(r)?r.length:-1}let i=n===void 0?"Error":typeof n==="string"?n:typeof n==="number"||typeof n==="boolean"||typeof n==="bigint"||n===null?String(n):"";return o(i)?i.length:-1}
function fe(e,n){try{let r=typeof e.stack==="string"?e.stack:"",o=gHt(e),s=o.length<r.length&&r.endsWith(o)?o.replace(/\n$/,"").split(`
`):[],u=s.every((d)=>d.trimStart().startsWith("at "))?s:[];e.stack=[`${e.name}: ${n}`,...u].join(`
`),e.message=n}catch{}return e}
var Cn=Object.freeze(["-c","core.hooksPath=/dev/null","-c","core.fsmonitor="]);
var C=Object.freeze({GIT_LFS_SKIP_SMUDGE:"1"});
var pe=["'alias.remote-none=version'","'alias.remote-none.command=version'","'alias.remote-/.command=version'","'fetch.bundleURI='","'help.autocorrect=0'"];
var ge=pe.join(" ");
var Qe=Object.freeze({GIT_ALLOW_PROTOCOL:"/",GIT_CONFIG_PARAMETERS:ge,GIT_NO_LAZY_FETCH:"1",GIT_NO_REPLACE_OBJECTS:"1",GIT_TERMINAL_PROMPT:"0",...C});
var et=Object.freeze({GIT_SSH_COMMAND:"false"});
var PWn=["GIT_DIR","GIT_WORK_TREE","GIT_COMMON_DIR","GIT_INDEX_FILE","GIT_CEILING_DIRECTORIES","GIT_DISCOVERY_ACROSS_FILESYSTEM","GIT_OBJECT_DIRECTORY","GIT_ALTERNATE_OBJECT_DIRECTORIES","GIT_SHALLOW_FILE","GIT_NAMESPACE","GIT_IMPLICIT_WORK_TREE","GIT_REFERENCE_BACKEND"];
var xUe=[...PWn,"GIT_CONFIG_COUNT","GIT_CONFIG_PARAMETERS","GIT_CONFIG_GLOBAL","GIT_CONFIG_SYSTEM","GIT_CONFIG"];
var tt=/^GIT_CONFIG_(KEY|VALUE)_\d+$/;
var urn=new RegExp(tt.source,"i");
var nt=new Set(xUe);
function ks(e,n={}){let r=e?.GIT_ALLOW_PROTOCOL?.split(":").includes("ssh")??!1;return ct(me({...Qe,...!r&&et},e,n))}
function me(e,n,{base:r=process.env,repository:o}){let i=it({...e,...o!==void 0&&{GIT_DIR:o.gitDir,GIT_COMMON_DIR:o.commonDir},...o!==void 0&&(o.workTree!==null?{GIT_WORK_TREE:o.workTree}:{GIT_IMPLICIT_WORK_TREE:"0"}),...n}),s={};for(let d of xUe)s[d]=void 0;let u=new Set(Object.keys(i).map((d)=>d.toUpperCase()));for(let d of Object.keys(r)){let p=d.toUpperCase();if(nt.has(p)||urn.test(d)||u.has(p)&&!(d in i))s[d]=void 0}return{...r,...s,...i}}
function it(e){let n=e.GIT_ALLOW_PROTOCOL;if(n===void 0)return{...e};let r=n.split(":").filter((d)=>d!=="/");if(r.find((d)=>!/^[A-Za-z0-9-]*$/.test(d))!==void 0)throw Error('GIT_ALLOW_PROTOCOL may list only transport names made of letters, digits and "-"');let i=[...new Set([...pe,...r.flatMap((d)=>[`'alias.remote-${d}=version'`,`'alias.remote-${d}.command=version'`])])],s=e.GIT_CONFIG_PARAMETERS;return{...e,GIT_CONFIG_PARAMETERS:[...s===void 0||s===""||s===ge?[]:[s],...i].join(" ")}}
function ct(e){let o=(d)=>d,i=(d)=>d.startsWith("/"),s=(d)=>{if(d===void 0)return;let p=d.split(":").map(o).filter((f)=>f!==""&&i(f));return p.length>0?p.join(":"):"/dev/null"},u=Object.keys(e).filter((d)=>d==="PATH");return{...e,...Object.fromEntries(u.map((d)=>[d,s(e[d])]))}}
function xRr(){return O()==="windows"?"\\\\.\\NUL":"/dev/null"}
var pt=/^core\.hooksPath=(?:\/dev\/null|\\\\\.\\nul)$/i;
var gt=["applypatch-msg","pre-applypatch","post-applypatch","pre-commit","pre-merge-commit","prepare-commit-msg","commit-msg","post-commit","pre-rebase","post-checkout","post-merge","pre-push","pre-receive","update","proc-receive","post-receive","post-update","reference-transaction","push-to-checkout","pre-auto-gc","post-rewrite","sendemail-validate","fsmonitor-watchman","p4-changelist","p4-prepare-changelist","p4-post-changelist","p4-pre-submit","post-index-change"];
var PRr=gt.map((e)=>[`hook.${e}.enabled`,"false"]);
function PUe(){return[["core.fsmonitor",""],["core.hooksPath",xRr()],["core.askPass",""],["protocol.ext.allow","never"],["submodule.recurse","false"],["log.showSignature","false"],["format.pretty","medium"],["gc.auto","0"],["maintenance.auto","false"],...PRr]}
var mt=new Set(["core.hooksPath","core.askPass","submodule.recurse",...PRr.map(([e])=>e)]);
var IRr={GIT_NO_LAZY_FETCH:"1",GIT_NO_REPLACE_OBJECTS:"1",GIT_TERMINAL_PROMPT:"0",...C};
function cTo(e,{allowGitTerminalPrompt:n=!1}={}){return fo(IRr,(r,o)=>!(n&&o==="GIT_TERMINAL_PROMPT")&&!Tt(e,o))}
function w9e(e,n,r,o=r.GIT_CONFIG_COUNT,{allowRepoGitHooks:i=!1,allowGitTerminalPrompt:s=!1}={}){let u=_e(e).toLowerCase();if(!/^git(\.exe|\.cmd|\.bat|\.com)?$/.test(u))return null;let d=i?PUe().filter(([f])=>!mt.has(f)):PUe(),p=Ee(n).map((f)=>n[f]);return{args:d.filter(([f])=>!p.some((_)=>_.startsWith(`${f}=`))).flatMap(([f,_])=>["-c",`${f}=${_}`]).concat(Et(n)),env:{...r,...cTo(r,{allowGitTerminalPrompt:s}),...GB(o,d),GIT_PROXY_COMMAND:r.GIT_PROXY_COMMAND??""}}}
var _t=new Set(["-C","--git-dir","--work-tree","--namespace","--super-prefix","--config-env","--attr-source","--exec-path"]);
function Ee(e){let n=[],r=0;while(r<e.length){let o=e[r];if(o==="-c"&&r+1<e.length)n.push(r+1),r+=2;else if(_t.has(o)&&r+1<e.length)r+=2;else if(o.startsWith("-"))r+=1;else break}return n}
function Et(e){let n=[...e];for(let r of Ee(n))if(pt.test(n[r]))n[r]=`core.hooksPath=${xRr()}`;return n}
function N(e,n){let r={};for(let o of Object.keys(e))if(n(o.toUpperCase(),o))r[o]=void 0;return r}
function Tt(e,n){if(Object.hasOwn(e,n))return!0;return O()==="windows"&&Object.keys(e).some((r)=>r.toUpperCase()===n)}
function GB(e,n,r={}){let o=Number(e),i=Number.isInteger(o)&&o>0?o:0,s={GIT_CONFIG_COUNT:String(i+n.length)};n.forEach(([d,p],f)=>{s[`GIT_CONFIG_KEY_${i+f}`]=d,s[`GIT_CONFIG_VALUE_${i+f}`]=p});let u=new Set(Object.keys(s));return{...N(r,(d,p)=>u.has(d)&&!u.has(p)),...s}}
var bt=1000;
var Lt=60;
function Dxe(e,{abortSignal:n,timeout:r=10*Lt*bt,input:o,stdio:i=["ignore","pipe","pipe"],useToolMemoryCgroup:s}={}){n?.throwIfAborted();using u=ah`exec: ${e.slice(0,200)}`;try{let d=nOs(e,{env:process.env,maxBuffer:1e6,timeout:r,cwd:ZRr(),stdio:i,reject:!1,input:o,useToolMemoryCgroup:s});if(!d.stdout)return null;return d.stdout.trim()||null}catch{return null}}
var V=1000;
var j=60;
function st(e,n,r={timeout:10*j*V,preserveOutputOnError:!0,useCwd:!0}){return we(()=>lt(e,n,{allowRepoGitHooks:r.allowRepoGitHooks,allowGitTerminalPrompt:r.allowGitTerminalPrompt,abortSignal:r.abortSignal,timeout:r.timeout,preserveOutputOnError:r.preserveOutputOnError,cwd:r.cwd??(r.useCwd?ZRr():void 0),env:r.env,extendEnv:r.extendEnv,stdin:r.stdin,input:r.input,stdout:r.stdout,stderr:r.stderr,maxBuffer:r.maxBuffer,stripFinalNewline:r.stripFinalNewline,useToolMemoryCgroup:r.useToolMemoryCgroup,toolCgroupClass:r.toolCgroupClass,waitForExitAfterKill:r.waitForExitAfterKill}))}
async function we(e){try{return await e()}catch(n){return c(n),{stdout:"",stderr:"",code:1,error:G(n).message}}}
function lt(e,n,r){return we(()=>r===void 0?ye(e,n):ye(e,n,r))}
function Ne(e){if(e<=1)return;try{process.kill(-e,"SIGTERM")}catch{}}
var Ft=6000;
var kt=5000;
function Mt(e,n,r,o,i){let s,u=!1,d=()=>{if(e.exitCode!==null||e.signalCode!==null)return;Promise.resolve().then(()=>o(n)).catch(()=>!1).then((p)=>{if(u)return;if(!p){i.abort(r.reason);return}s=setTimeout((f,_,{stdout:m,stderr:y})=>{f.abort(_),m?.destroy(),y?.destroy()},kt,i,r.reason,e)})};if(r.aborted)d();else r.addEventListener("abort",d,{once:!0});return()=>{u=!0,r.removeEventListener("abort",d),clearTimeout(s)}}
async function Dt(e,n){if(e.pid===void 0||e.exitCode!==null||e.signalCode!==null)return;await Xe(new Promise((r)=>e.once("exit",()=>r())),n)}
function Ie(e){return E(e)==="ERR_CHILD_PROCESS_STDIO_MAXBUFFER"||e?.isMaxBuffer===!0||e?.name==="MaxBufferError"}
var R={cwd:"execFileNoThrow spawn rejected: null byte in cwd",file:"execFileNoThrow spawn rejected: null byte in executable path",argv:"execFileNoThrow spawn rejected: null byte in argv",envName:"execFileNoThrow spawn rejected: null byte in env name",envValue:"execFileNoThrow spawn rejected: null byte in env value"};
function $t(e,n,r,o,i,s){let u=`the runtime rejected ${Pe(Ae(e))} because`;if(typeof r==="string"&&r.includes("\x00"))return{telemetry:R.cwd,local:`${u} the working directory contains a null byte (\\0)`};if(e.includes("\x00"))return{telemetry:R.file,local:`${u} the executable path contains a null byte (\\0)`};let d=n.findIndex((m)=>typeof m==="string"&&m.includes("\x00"));if(d!==-1)return{telemetry:R.argv,local:`${u} the argument at index ${d} (zero-based) contains a null byte (\\0)`};let p=Object.entries({...i,...o}).filter(([,m])=>m!==void 0),f=p.find(([m])=>m.includes("\x00"));if(f!==void 0)return{telemetry:R.envName,local:`${u} the name of environment variable ${xe(f[0])} contains a null byte (\\0)${Ce(f,s)}`};let _=p.find(([,m])=>typeof m==="string"&&m.includes("\x00"));return _&&{telemetry:R.envValue,local:`${u} the value of environment variable ${xe(_[0])} contains a null byte (\\0)${Ce(_,s)}`}}
function Ce([e,n],r){return r[e]===n?"; every program this process starts will fail the same way until that variable is removed from its environment":" (in the env passed for this command)"}
var Re=200;
function Ut(e){return e===1?"1 character":`${e} characters`}
function Pe(e){return Xi(e).slice(1,-1).replace(/[^\x20-\x7e]/g,(n)=>`\\u${n.charCodeAt(0).toString(16).padStart(4,"0")}`)}
function xe(e){let n=e.indexOf("="),r=n===-1?e:e.slice(0,n),o=r.length>Re,i=`"${Pe(o?r.slice(0,Re):r)}"${o?"\u2026":""}`;return n===-1?i:`${i} (then '=' and ${Ut(e.length-n-1)}, not shown)`}
var Bt=new Set(["EPIPE","ECONNRESET","ENOTCONN","EOF"]);
function Ht(e){let n={},r;try{r=e.stdin}catch{return n}if(!r)return n;return r.removeAllListeners("error"),r.on("error",(o)=>{let i=E(o)??"unknown";if(Bt.has(i))return;n.truncatedBy??=i,e.kill()}),n}
function Wt(e,n){if(e.shortMessage)return e.shortMessage;if(typeof e.signal==="string")return e.signal;return String(n)}
function Vt(e,n){if(!As(n)||n.code===void 0||typeof n.syscall!=="string"||!n.syscall.startsWith("spawn")||e.exitCode!==void 0||e.timedOut||e.isCanceled||E(e)===n.code)return;return`Command failed with ${n.code}: ${e.command}
${n.message}`}
function P(e){return e instanceof Uint8Array?Buffer.from(e).toString("latin1"):typeof e==="string"?e:""}
async function ye(e,n,{abortSignal:r,timeout:o=10*j*V,preserveOutputOnError:i=!0,cwd:s,env:u,extendEnv:d,maxBuffer:p,shell:f,stdin:_,input:m,stdout:y,stderr:K,stripFinalNewline:J,encoding:Ge,allowRepoGitHooks:ve,allowGitTerminalPrompt:be,useToolMemoryCgroup:Le=!0,toolCgroupClass:Fe="helper",waitForExitAfterKill:ke=!1,onStdoutData:Y,withoutControllingTerminal:Me=!1,tellBeforeKill:v}={timeout:10*j*V,preserveOutputOnError:!0,maxBuffer:1e6}){if(tG())return{stdout:"",stderr:vHt,code:126,error:vHt};let b=e,z=process.env,Z=w9e(b,n,u===void 0?z:d===!1?u:{...z,...u},u&&"GIT_CONFIG_COUNT"in u?u.GIT_CONFIG_COUNT:a.GIT_CONFIG_COUNT,{allowRepoGitHooks:ve,allowGitTerminalPrompt:be}),De=Z?.args??n,L=Z?.env??u,F=Me&&O()!=="windows";if(F&&vb()){let l=`${Ae(e)} was not started because Claude Code was exiting; try again after it restarts`;return{stdout:"",stderr:l,code:1,error:l}}let g,I,X,q,k=v!==void 0&&r!==void 0?new AbortController:void 0,M,Q=O()==="linux"?{noChildLeft:!0}:{},A=Q,ee;try{let l=hut(b,De,{signal:k?.signal??r,timeout:o,cwd:s,env:L,shell:f,stdin:_,...p===void 0?{}:{maxBuffer:p},...J===void 0?{}:{stripFinalNewline:J},...Ge==="latin1"&&{encoding:"latin1"},...y===void 0?{}:{stdout:y},...K===void 0?{}:{stderr:K},...d===void 0?{}:{extendEnv:d},...Le?Pl(Fe):{},...F&&{detached:!0},reject:!1});if(A=l.pid===void 0?Q:{},v!==void 0&&r!==void 0&&k!==void 0&&l.pid!==void 0)q=Mt(l,l.pid,r,v,k);if(I=F?l.pid:void 0,I!==void 0){let T=I,S=()=>Ne(T),h=St(S),w=dY(S,{alwaysLast:!0});X=()=>{h(),w()}}if(l.once("error",(T)=>{ee=T}),M=m===void 0?void 0:Ht(l),m!==void 0&&l.pid!==void 0)l.stdin?.end(m);if(Y&&l.pid!==void 0)l.stdout?.on("data",(T)=>{try{Y(T.toString())}catch(S){c(S)}});if(g=await l,ke&&g.exitCode===void 0&&(r?.aborted===!0||g.timedOut))await Dt(l,Ft)}catch(l){let T=l.message;if(As(l))return t(`execFileNoThrow spawn failed: ${E(l)} ${T}`,{level:"error"}),{stdout:"",stderr:"",code:1,...A};else if(Ie(l))return t(`execFileNoThrow maxBuffer exceeded: ${T}`,{level:"error"}),{stdout:"",stderr:"",code:1,maxBufferExceeded:!0};else{let S=process.env,h=E(l)==="ERR_INVALID_ARG_VALUE"?$t(b,n,s,L,d===!1&&L!==void 0?void 0:S,S):void 0;if(h)t(`execFileNoThrow spawn refused: ${h.local}`,{level:"error"});let w=G(l);c(rt(h?fe(w,h.telemetry):w,h?.telemetry??"execFileNoThrow unexpected rejection"))}return{stdout:"",stderr:"",code:1}}finally{if(q?.(),X?.(),I!==void 0&&(g===void 0||g.exitCode===void 0))Ne(I)}if(M?.truncatedBy!==void 0){let l=`stdin write failed: ${M.truncatedBy}`;return t(`execFileNoThrow ${l}`,{level:"error"}),{stdout:"",stderr:"",code:1,error:l}}if(!g.failed)return{stdout:P(g.stdout),stderr:P(g.stderr),code:0,exitCode:0};let te=Ie(g)?{maxBufferExceeded:!0}:{},ne=g.timedOut?{timedOut:!0}:{};if(!i)return{stdout:"",stderr:"",code:g.exitCode??1,exitCode:g.exitCode,...te,...ne,...A};let re=g.exitCode??1;return{stdout:P(g.stdout),stderr:P(g.stderr),code:re,error:Vt(g,ee)??Wt(g,re),exitCode:g.exitCode,...te,...ne,...A}}
export { st as st };
export { Dxe as Dxe };
export { Cn as Cn };
export { ks as ks };
export { lt as lt };
export { gHt as gHt };
export { drn as drn };
export { lTo as lTo };
export { RWn as RWn };
export { ARr as ARr };
export { crn as crn };
