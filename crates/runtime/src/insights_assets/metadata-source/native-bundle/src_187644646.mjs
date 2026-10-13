// Extracted source closure for 2.1.295; source module src_187644646.js sha256 2216f67a9cb1cb6c37dc6776255b4ac3280a12e54528fb360665ec18eacac588
import { a } from "./src_186481843.mjs";
import { _, ie, St } from "./src_186240709.mjs";
import { ys, q } from "./src_186096608.mjs";
import { dirname as u } from "path";
function s(){}
async function p(n,t){let i=ie();try{await i.appendFile(n,t)}catch{await i.mkdir(u(n)).catch(s),await i.appendFile(n,t)}}
class c{pendingWrite=Promise.resolve();cleanupRegistered=!1;append(n,t){if(this.pendingWrite=this.pendingWrite.then(p.bind(null,n,t)).catch(s),!this.cleanupRegistered)this.cleanupRegistered=!0,St(()=>this.flush())}flush(){return this.pendingWrite}}
var f=new q(()=>new c);
function g(){return ys(f)}
function ee(n,t,i){let r=m();if(!r)return;let o;try{o=e(n,t,l(i))}catch{o=e(n,t,{diagnostics_payload_failed:!0})}g().append(r,o)}
function l(n){try{return(typeof n==="function"?n():n)??{}}catch{return{diagnostics_payload_failed:!0}}}
function e(n,t,i){let r={timestamp:new Date().toISOString(),level:n,event:t,data:i};return _(r)+`
`}
function m(){return a.CLAUDE_CODE_DIAGNOSTICS_FILE}
export { ee as ee };
