// Extracted source closure for 2.1.295; source module src_188118649.js sha256 acde825bbb7f36e5bdd0bd179c777f44eda81a094a77e42c9a8ae3d62186473e
import { hsn, Ee } from "./src_186220896.mjs";
import { Cse } from "./src_186457469.mjs";
import { basename as E, dirname as F, join as S } from "path";
import { Bi } from "./src_188115875.mjs";
import { Ln } from "./src_186068928.mjs";
import { readdir as b, lstat as z, open as L } from "fs/promises";
import { constants as P } from "fs";
var Zb=65536;
var ye=/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
function CP(e){if(typeof e!=="string")return null;return ye.test(e)?e:null}
function R9e(e,n,r){return q(e,r,n)}
function q(e,n,r){let t=r===void 0?void 0:`"type":"${r}"`,i=`"${n}":`,a=e.length;while(a>0){let o=e.lastIndexOf(`
`,a-1),u=e.slice(o+1,a);if(a=o,u.includes(i)&&(t===void 0||u.includes(t)))try{let c=JSON.parse(u);if(typeof c==="object"&&c!==null&&(r===void 0||c.type===r)){let s=c[n];if(typeof s==="string")return s}}catch{}if(o<0)break}return}
function Uxe(e,n){let r=`"${n}":`,t=0;while(t<e.length){let i=e.indexOf(`
`,t),a=i<0?e.slice(t):e.slice(t,i);if(t=i<0?e.length:i+1,a.includes(r))try{let o=JSON.parse(a);if(typeof o==="object"&&o!==null){let u=o[n];if(typeof u==="string")return u}}catch{}}return}
var T=P.O_RDONLY|P.O_NOFOLLOW|P.O_NONBLOCK;
async function X(e){let{backend:n,key:r}=e;try{let t=await n.read([{key:r,offset:0,length:Zb},{key:r,tail:Zb}]);if(!t.ok)return null;let[i,a]=t.value.items;if(!i.found||i.value.length===0)return null;let o=H(i.value),u=a.found&&a.value.length>0?H(a.value):o;return{head:o,tail:u,mtimeMs:i.mtimeMs,totalBytes:i.totalBytes}}catch{return null}}
function H(e){return Buffer.from(e.buffer,e.byteOffset,e.byteLength).toString("utf8")}
var BRr=1e4;
function Abe(e,n){if(e.kind!=="scope"||e.scope.namespace!=="transcript"||e.scope.projectKey===void 0||e.scope.sessionId!==void 0||!n(e.scope.projectKey))return;return e.scope.projectKey}
function C(e){return{skipScopeStats:!0,skipKeyStats:!0,...e!==void 0&&{cursor:e}}}
async function DUe(e,n){if(n!==void 0&&n.hoverRestOn)return ee(n.source);try{if(T===P.O_RDONLY){if(!(await z(e)).isFile())return null}let r=await L(e,T);try{let t=await r.stat();if(!t.isFile())return null;let i=Buffer.allocUnsafe(Zb),a=await r.read(i,0,Zb,0);if(a.bytesRead===0)return null;let o=i.toString("utf8",0,a.bytesRead),u=Math.max(0,t.size-Zb),c=o;if(u>0){let s=await r.read(i,0,Zb,u);c=i.toString("utf8",0,s.bytesRead)}return{mtime:t.mtime.getTime(),size:t.size,head:o,tail:c}}finally{await r.close()}}catch{return null}}
async function ee(e){let n=await X(e);if(n===null)return null;return{mtime:Math.trunc(n.mtimeMs),size:n.totalBytes,head:n.head,tail:n.tail}}
var rpe=200;
function ve(e){return Math.abs(Cse(e)).toString(36)}
function k(e){return e.replace(/[^a-zA-Z0-9]/g,"-")}
function vO(e){let n=k(e);if(n.length<=rpe)return n;return`${n.slice(0,rpe)}-${ve(e)}`}
function xf(){return S(Ee(),"projects")}
function $E(e){return hsn()??vO(e)}
function vrn(e){let n=vO(e);return n===$E(e)?void 0:n}
function Th(e,n=xf()){return S(n,$E(e))}
function gTo(e,n){let r=E(e);return F(e)===xf()&&n(r)?r:void 0}
async function krn(e,n,r,t){let i=t!==void 0&&t.hoverRestOn?t.source:void 0,a=i===void 0?void 0:gTo(e,i.isKeySegment);if(i!==void 0&&a!==void 0){let c=await Le(a,n,r,i);if(c!==void 0)return c}let o=k(n),u;try{u=await b(e,{withFileTypes:!0})}catch{return!1}for(let c of u){if(!c.isFile()||!c.name.endsWith(".jsonl"))continue;let s=await DUe(S(e,c.name));if(s===null)continue;if(ne(s,o,r))return!0}return!1}
function ne(e,n,r){let t=R9e(e.tail,"relocated","relocatedCwd")??Uxe(e.head,"cwd");if(t===void 0)return!1;let i=k(Ln(t));return r?i.toLowerCase()===n.toLowerCase():i===n}
async function Le(e,n,r,t){let{backend:i,transcriptKey:a,isKeySegment:o}=t,u=k(n),c=!1,s=new Set;try{let p=await Bi((l)=>i.listEntries({namespace:"transcript",projectKey:e},C(l)),async(l)=>{for(let f of l){if(f.kind!=="key"||f.key.namespace!=="transcript"||f.key.projectKey!==e||f.key.agentId!==void 0||!o(f.key.sessionId)||s.has(f.key.sessionId))continue;let d=f.key.sessionId,y=await ee({backend:i,key:a(e,d)});if(y===null)continue;if(s.add(d),ne(y,u,r)){c=!0;return}}},{until:()=>c});if(c)return!0;if(p.status!=="done")return}catch{return}return!1}
async function Tbe(e,n,r=xf()){if(n!==void 0&&n.hoverRestOn)return Te(e,n);let t=Th(e,r),i=[];try{await b(t),i.push(t)}catch{}let a=vrn(e);if(a!==void 0){let l=S(r,a);try{await b(l),i.push(l)}catch{}return i}let o=vO(e);if(o.length<=rpe)return i;let u=!1,c=(l)=>u?l.toLowerCase():l,s=c(o.slice(0,rpe)+"-"),p=c(t);try{for(let l of await b(r,{withFileTypes:!0})){if(!l.isDirectory()||!c(l.name).startsWith(s))continue;let f=S(r,l.name);if(c(f)!==p&&await krn(f,e,u))i.push(f)}}catch{}return i}
async function Te(e,n){let r=await $$s(n.source);if(r===null){let t=vrn(e);return[Th(e),...t!==void 0?[S(xf(),t)]:[]]}return U$s(e,r,!1,n)}
async function $$s(e){let{backend:n,isKeySegment:r}=e,t=[],i=new Set,a=0;try{if((await Bi((u)=>n.listEntries({namespace:"transcript"},C(u)),(u)=>{for(let c of u){let s=Abe(c,r);if(s!==void 0&&!i.has(s))i.add(s),t.push(s)}a++})).status==="error")return a===0?null:t}catch{return a===0?null:t}return t}
async function U$s(e,n,r,t){let i=$E(e),a=Th(e),o=vrn(e),u=o===void 0&&i.length>rpe?i.slice(0,rpe)+"-":void 0,c=xf(),s=(g)=>r?g.toLowerCase():g,p=u!==void 0?s(u):void 0,l=s(i),f=!1,d,y=[],m=new Set;for(let g of n)if(g===i)f=!0;else if(o!==void 0){if(d===void 0&&s(g)===s(o))d=S(c,g)}else if(p!==void 0&&s(g).startsWith(p)&&s(g)!==l&&!m.has(s(g)))m.add(s(g)),y.push(g);let h=[];for(let g of y){let K=S(c,g);if(await krn(K,e,r,t))h.push(K)}return[...f?[a]:[],...d!==void 0?[d]:[],...h]}
var hZ=5242880;
var bHt=/^[0-9a-f]{16}(?:[0-9a-f]{48})?@v\d+$/;
function jxe(e){return CP(e)!==null}
export { $E as $E };
export { vrn as vrn };
export { rpe as rpe };
export { BRr as BRr };
export { krn as krn };
export { Tbe as Tbe };
export { jxe as jxe };
export { hZ as hZ };
export { bHt as bHt };
export { xf as xf };
export { vO as vO };
