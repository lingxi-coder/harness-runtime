// Extracted source closure for 2.1.295; source module src_192962497.js sha256 2f816d69a56f1b07dc708c8ad1bd9b53daa13d21b1e2f0f17926f276dbacdb04
import { of } from "./src_186240709.mjs";
import { isAbsolute as g, sep as a, dirname as u, basename as c, join as p } from "path";
import { j } from "./src_186014760.mjs";
import { xf } from "./src_188118649.mjs";
import { Fe } from "./src_187591836.mjs";
import { Ig } from "./src_188187055.mjs";
function l(e){let n=process.cwd();return n.endsWith(a)?n+e:n+a+e}
function Rvt(e){return(n)=>e.hostFiles.realPath(of.workspace(n===""||g(n)?n:l(n)),{native:!0})}
function Wp(e,n){if(!j()||n===void 0)return;if(!e.endsWith(".jsonl"))return;let t=u(e);if(u(t)!==xf())return;let r=c(t),o=c(e,".jsonl");if(e!==p(xf(),r,`${o}.jsonl`))return;let i=Fe.transcript(r,o);return Ig(i)===void 0?{backend:n,key:i}:void 0}
function iE(e){return e===void 0?void 0:{source:e,hoverRestOn:j()}}
export { Rvt as Rvt };
export { iE as iE };
export { Wp as Wp };
