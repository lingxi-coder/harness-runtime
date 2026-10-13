// Extracted source closure for 2.1.295; source module src_187623132.js sha256 24649f6b13bb71cd1aa78cabb6bc647e79a462a76b4c2414202e6ec870e56218
import { c } from "./src_186460102.mjs";
import { rt } from "./src_186056234.mjs";
import { kO } from "./src_187601633.mjs";
function Po(e){return e.startsWith("\uFEFF")?e.slice(1):e}
var be=8192;
function se(e,i){try{return{ok:!0,value:JSON.parse(Po(e))}}catch(r){if(i)c(rt(r,`safeParseJSON: invalid JSON (${r instanceof Error?r.constructor.name:typeof r}, ${e.length} bytes)`));return{ok:!1}}}
class oe{cached=kO(se,(e)=>e,50);cache=this.cached.cache;parse(e,i){return this.cached(e,i)}}
var le=new oe;
var mt=Object.assign(function(i,r=!0){if(!i)return null;let n=i.length>be?se(i,r):le.parse(i,r);return n.ok?n.value:null},{cache:le.cache});
function OCo(){return Bun.JSONL?.parseChunk}
function ye(e,i){let r=e.length,n=i(e);if(!n.error||n.done||n.read>=r)return n.values;let{values:l,read:s}=n;while(s<r){let u=typeof e==="string"?e.indexOf(`
`,s):e.indexOf(10,s);if(u===-1)break;s=u+1;let o=i(e,s);if(o.values.length>0)l=l.concat(o.values);if(!o.error||o.done||o.read>=r)break;s=o.read}return l}
function Oe(e){let i=e.length,r=0;if(e[0]===239&&e[1]===187&&e[2]===191)r=3;let n=[];while(r<i){let l=e.indexOf(10,r);if(l===-1)l=i;let s=e.toString("utf8",r,l).trim();if(r=l+1,!s)continue;try{n.push(JSON.parse(s))}catch{}}return n}
function Te(e){let i=Po(e),r=i.length,n=0,l=[];while(n<r){let s=i.indexOf(`
`,n);if(s===-1)s=r;let u=i.substring(n,s).trim();if(n=s+1,!u)continue;try{l.push(JSON.parse(u))}catch{}}return l}
function X5e(e){let i=OCo();if(i)return ye(e,i);if(typeof e==="string")return Te(e);return Oe(e)}
export { OCo as OCo };
export { X5e as X5e };
export { mt as mt };
export { Po as Po };
