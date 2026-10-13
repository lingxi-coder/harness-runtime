// Extracted source closure for 2.1.295; source module src_186220896.js sha256 687cb3f6dffa3cd7e4d002249bb8537c255492cce767104e382e2d0b23def2ac
import { Eo, Cp, Ln } from "./src_186068928.mjs";
import { join as u, resolve as t, dirname as O, basename as g } from "path";
import { homedir as R } from "os";
import { Le } from "./src_186092222.mjs";
function o(){return process.env.CLAUDE_CONFIG_DIR}
function dzn(e){let n=e==="CLAUDE_CONFIG_DIR"?o():process.env[e];if(n===void 0)return;let r=String(n).trim();return r?r:void 0}
var Ee=Eo(()=>(o()??u(R(),".claude")).normalize("NFC"),o);
function $Z(e){return t(Ee())===t(e)}
function p(){return process.env.CLAUDE_CODE_PROJECT_DIR_NAME}
var D=/^[A-Za-z0-9_-]{1,64}$/;
var A=/^(?:con|prn|aux|nul|com[0-9]|lpt[0-9])$/i;
function qOo(e){if(!e||!D.test(e)||A.test(e))return;return e}
function C(){return`${o()??""}\x00${p()??""}`}
var hsn=Eo(()=>o()?qOo(p()):void 0,C);
function UZ(e){let n=Ee();if(t(e)===t(n))return!0;return _(e)===_(n)}
function _(e){let n=t(e),r=O(n),s=Cp(r)??r;return Ln(u(s,g(n)))}
function i(e){let n=process.argv.indexOf("--");return(n===-1?process.argv:process.argv.slice(0,n)).includes(e)}
function ur(){return Le(process.env.CLAUDE_CODE_SIMPLE)||i("--bare")}
function Fw(){return!1}
export { hsn as hsn };
export { Ee as Ee };
export { ur as ur };
export { Fw as Fw };
export { UZ as UZ };
export { $Z as $Z };
export { dzn as dzn };
