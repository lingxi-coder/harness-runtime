// Extracted source closure for 2.1.295; source module src_186453827.js sha256 bb5459662fbbeee3a51ca8a997b0ea9815b5ff6b9b142589d9b5fba54af9b701
import { q, B } from "./src_186096608.mjs";
import { lstatSync as g } from "fs";
import { dzn } from "./src_186220896.mjs";
import { join as m } from "path";
import { execFileSync as p } from "child_process";
import { wzn } from "./src_186068928.mjs";
function E(){return!1}
class f{resolved=new Map;lookup(e){return this.resolved.get(e)}remember(e,t){this.resolved.set(e,t)}forget(e){this.resolved.delete(e)}}
var w=new q(()=>new f);
var h=5000;
function u(e){try{return g(e,{throwIfNoEntry:!1})===void 0}catch{return!1}}
var S=new Set([".com",".exe",".bat",".cmd"]);
function b(e){let t=e.toLowerCase().replace(/.*[\\/]/,"").replace(/[. ]+$/,""),n=t.lastIndexOf(".");return n>0&&S.has(t.slice(n))}
function txr(e,t=!1){let n=w.of(B().host),o=n.lookup(e);if(o!==void 0)if(o!==null){if(!u(o))return o;n.forget(e)}else{if(!t)return o;n.forget(e)}let c=dzn("SYSTEMROOT")||"C:\\Windows",d=m(c,"System32","where.exe");try{let i=p(d,[e],{stdio:"pipe",encoding:"utf8",timeout:h,windowsHide:!0,env:process.env}).trim().split(/\r?\n/).filter(Boolean),a=process.cwd(),l=!1;for(let r of i){if(u(r))continue;if(wzn(r,a)){l=!0;continue}if(!b(r))continue;return n.remember(e,r),r}if(i.length>0&&!l)n.remember(e,null);return null}catch(s){if(v(s))n.remember(e,null);return null}}
function v(e){if(e===null||typeof e!=="object")return!1;let t="status"in e?e.status:void 0,n="signal"in e?e.signal:void 0,o="code"in e?e.code:void 0;return t===1&&!n&&!o}
function YB(e,t=!1){if(!E())return e;if(e.includes("/")||e.includes("\\"))return e;return txr(e,t)}
export { YB as YB };
