// Extracted source closure for 2.1.295; source module src_186893596.js sha256 8f57d1caa9f75046f56a537405a8bd1f19163ab39bdedc47f8fff891397087de
import { open as L, lstat as I, unlink as v, writeFile as oe, stat as Pe, rename as K } from "fs/promises";
import { constants as a } from "fs";
import { E, x } from "./src_186056234.mjs";
import { join as xe, basename as ve } from "path";
import { randomBytes as Te } from "crypto";
import { Q } from "./src_186066580.mjs";
var te=!1;
function ne(){return te}
function be(){return!1}
function KRr({served:e}){if(!ne())return!1;return e?!1:be()}
var q$=".cc-writes";
var fT=new Set(["EXDEV","EPERM","EEXIST","EBUSY"]);
function GWn(){return KRr({served:!1})}
function ETo(e,t){return`Could not rewrite ${e} in place: this file has other names on disk (it is hard-linked, ${t} names in all), and writing it in place would change the file under every one of those names. Remove the extra links, then try again.`}
function ae(e,t){return Object.assign(new x(ETo(e,t),"in-place write refused: the target has other names (a hard link)"),{code:"EMLINK",path:e})}
function MR(e){return`${e}.tmp.${Te(4).toString("hex")}`}
var se=4;
var ce=50;
var ke=Array.from({length:se-1},()=>ce);
function le(e,t,n=se-1){return!1}
var ue=128;
async function Ce(e){try{return((await I(e)).mode&ue)===0}catch{return!1}}
async function uD(e,t,n=ke){let r=!1;for(let o=0;;o++)try{return await e(),r}catch(i){if(le(i,o,n.length)){if(o===0&&t!==void 0&&await Ce(t))throw i;r=!0,await Q(n[o]);continue}throw i}}
function os(e,t,n=K){return uD(()=>n(e,t),t)}
var fe=67108864;
var Sse=a.O_NONBLOCK;
async function de(e,t,n="darwin"){return(await We(e,t,n)).kind==="pass"}
async function We(e,t,n="darwin"){if(n!=="win32")return{kind:"pass"};try{return{kind:(await I(e)).isFile()?"pass":"refuse"}}catch(r){if(E(r)==="ENOENT")return{kind:t?"pass":"refuse"};return{kind:"failed",error:r}}}
async function Me(e,t=!1){if(!t&&!await de(e,!0))return{kind:"unavailable"};let n;try{n=await L(e,a.O_RDONLY|(t?0:a.O_NOFOLLOW)|Sse)}catch(r){return E(r)==="ENOENT"?{kind:"absent"}:{kind:"unavailable"}}try{let r=await n.stat();if(!r.isFile()||r.size>fe)return{kind:"unavailable"};return{kind:"snapshot",bytes:new Uint8Array(await n.readFile()),mode:r.mode&4095}}catch{return{kind:"unavailable"}}finally{await n.close().catch(()=>{})}}
async function Ye(e,t,n=!1){if(!n&&!await de(e,!0))return!1;let r;try{r=await L(e,a.O_WRONLY|a.O_CREAT|a.O_TRUNC|(n?0:a.O_NOFOLLOW)|Sse,t.mode)}catch{return!1}try{if(!(await r.stat()).isFile())return await r.close().catch(()=>{}),!1;return await r.writeFile(t.bytes),await r.chmod(t.mode).catch(()=>{}),await r.close(),!0}catch{return await r.close().catch(()=>{}),!1}}
function F(e,t,n){try{if(e instanceof Error)e.message=`${e.message}; ${t!==void 0?`new contents preserved at ${t}; `:""}${n==="restored"?"original target restored":n==="removed"?"partial target removed":n==="untouched"?"target untouched":"target left partial \u2014 treat contents as torn"}`,Object.assign(e,{...t!==void 0&&{preservedTmp:t},targetOutcome:n})}catch{}return e}
function FUe(e){let t=E(e);return t==="EINVAL"||t==="ENOTSUP"||t==="EPERM"||t==="ENOSYS"}
class VWn extends Error{target;constructor(e){super("beforePublish refused the write; nothing was published");this.target=e;this.name="PublishRefusedError"}}
async function De(e,t){if(t===void 0)return e;try{await(await L(t,a.O_RDONLY|a.O_DIRECTORY|a.O_NOFOLLOW)).close()}catch(n){switch(E(n)){case"ENOENT":case"ELOOP":case"ENOTDIR":return e;case"EACCES":case"EPERM":if(await Ue(t))return e;throw n;default:throw n}}return xe(t,ve(e))}
async function Ue(e){let t=process.getuid?.();if(t===void 0)return!1;try{return(await I(e)).uid!==t}catch{return!1}}
var C=3;
async function me(e,t){if(t!=="win32")return!1;try{return await I(e),!0}catch(n){return E(n)==="ENOENT"?!1:{cause:n}}}
function W(e,t){return Object.assign(Error("EEXIST: name already taken (exclusive create)",typeof t==="object"?{cause:t.cause}:void 0),{code:"EEXIST",syscall:"lstat",path:e})}
async function j(e,t,n){for(let r=1;;r++){let o=MR(e),i=await me(o,t);if(i!==!1){if(r<C)continue;throw W(o,i)}try{return await n(o)}catch(s){if(E(s)==="EEXIST"&&r<C)continue;throw s}}}
async function pD(e,t,n,r="darwin"){return j(e,r,async(o)=>{try{return await oe(o,t,{encoding:"utf8",mode:n,flag:"wx"}),o}catch(i){if(E(i)!=="EEXIST")await v(o).catch(()=>{});throw i}})}
async function Be(e,t,n="darwin"){return j(e,n,async(r)=>({fh:await L(r,n==="win32"?"wx":a.O_WRONLY|a.O_CREAT|a.O_EXCL,t),tmp:r}))}
async function K$(e,t="darwin"){let n=await me(e,t);if(n===!0)throw W(e,n);if(n!==!1)throw n.cause}
async function En(e,t,n,r){return ope(e,t,{mode:n,renameFn:r})}
async function ope(e,t,n){let{mode:r,createMode:o,exactMode:i,flush:s,followSymlinks:f,inPlaceOnTempCreateRefused:c,stagingDir:S,renameFn:w,beforePublish:g}=n,P=r??o,k=()=>{if(g!==void 0&&!g())throw new VWn(e)},y=await De(e,S),A=f===!0?0:a.O_NOFOLLOW,m,T=!1,M=!1,G=async(p)=>{let l=await Me(e,f===!0),h=l.kind==="snapshot"?l:void 0;k();let u=await L(e,a.O_WRONLY|a.O_CREAT|A|Sse,i??P),N;try{N=await u.stat()}catch(_){throw await u.close().catch(()=>{}),T=p!==void 0,F(_,p,"untouched")}let d=N.isCharacterDevice();if(!N.isFile()&&!d)throw await u.close().catch(()=>{}),Object.assign(Error("refusing the in-place arm on a non-regular target"),{code:"ENXIO",path:e});if(!d&&N.nlink>1&&GWn())throw await u.close().catch(()=>{}),ae(e,N.nlink);if(g!==void 0&&!g()){if(await u.close().catch(()=>{}),l.kind==="absent"&&N.size===0&&!d)await v(e).catch(()=>{});throw new VWn(e)}let z=!1;try{if(!d)await u.truncate(0),z=!0;await u.writeFile(t,{encoding:"utf8"});let _=d?void 0:i??(l.kind!=="absent"?r:void 0);if(_!==void 0)await u.chmod(_).catch(()=>{});if(s===!0)try{await u.sync()}catch(Y){if(!FUe(Y))throw Y}await u.close()}catch(_){if(await u.close().catch(()=>{}),!z)throw T=p!==void 0,F(_,p,"untouched");T=p!==void 0;let Ee=h!==void 0&&await Ye(e,h,f===!0)?"restored":await v(e).then(()=>!0,(ye)=>E(ye)==="ENOENT")?"removed":"partial";throw F(_,p,Ee)}let q=p??m;if(q!==void 0)await v(q).catch(()=>{})};try{try{if(i!==void 0||s===!0){let l=await Be(y,i??P),h=l.fh;m=l.tmp;let u=!1,N;try{if(await h.writeFile(t,{encoding:"utf8"}),i!==void 0)try{await h.chmod(i)}catch(d){if(!FUe(d))throw d}if(s===!0)try{await h.sync()}catch(d){if(!FUe(d))throw d}}catch(d){u=!0,N=d}if(!u)M=!0;try{await h.close()}catch(d){if(!u)throw T=!0,F(d,m,"untouched")}if(u)throw N}else m=await pD(y,t,P),M=!0}catch(l){if(c!==!0)throw l;if(M)throw l;if(E(l)!=="EACCES")throw l;if(!await Pe(e).then(()=>!0,()=>!1))throw l;await G(void 0);return}if(m===void 0)throw Error("staging block exited without a staging file");let p=m;try{let l=w??K;await os(p,e,(h,u)=>(k(),l(h,u)))}catch(l){let h=E(l);if(h===void 0||!fT.has(h))throw l;await G(p)}}catch(p){if(m!==void 0&&!T)await v(m).catch(()=>{});throw p}}
export { q$ as q$ };
export { En as En };
export { FUe as FUe };
export { os as os };
export { fT as fT };
export { K$ as K$ };
export { GWn as GWn };
export { ETo as ETo };
