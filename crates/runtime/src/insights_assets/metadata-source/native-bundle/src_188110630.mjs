// Extracted source closure for 2.1.295; source module src_188110630.js sha256 909ba3c7841e659bcf685c4bda994b9de73f673b8f3f6858be058c06b869d6d1
import { fm } from "./src_188108105.mjs";
import { OP } from "./src_186096608.mjs";
import { Ln } from "./src_186068928.mjs";
import { normalize as u, join as b, isAbsolute as P, resolve as x } from "path";
import { homedir as c } from "os";
import { O } from "./src_186477637.mjs";
import { kO } from "./src_187601633.mjs";
var EO=kO((e)=>{if(e.startsWith("\\\\"))return e.replaceAll("\\","/");let r=e.match(/^([A-Za-z]):[/\\]/);if(r)return"/"+r[1].toLowerCase()+e.slice(2).replaceAll("\\","/");return e.replaceAll("\\","/")},(e)=>e,500);
var gZ=kO((e)=>{if(e.startsWith("//"))return e.replaceAll("/","\\");let r=e.match(/^\/cygdrive\/([A-Za-z])(\/|$)/);if(r){let n=r[1].toUpperCase(),o=e.slice(("/cygdrive/"+r[1]).length);return n+":"+(o||"\\").replaceAll("/","\\")}let i=e.match(/^\/([A-Za-z])(\/|$)/);if(i){let n=i[1].toUpperCase(),o=e.slice(2);return n+":"+(o||"\\").replaceAll("/","\\")}return e.replaceAll("/","\\")},(e)=>e,500);
class Cbe extends Error{constructor(){super("Path contains null bytes");this.name="NullBytePathError"}}
function brn(e){if(e.includes("\x00"))throw new Cbe}
function Qe(e,r){let i=r??fm();if(typeof e!=="string")throw TypeError(`Path must be a string, received ${typeof e}`);if(i!==null&&typeof i!=="string")throw TypeError(`Base directory must be a string, received ${typeof i}`);if(brn(e),i!==null)brn(i);let n=e.trim();if(!n){if(i===null)throw new OP;return Ln(u(i))}if(n==="~")return Ln(c());if(n.startsWith("~/"))return Ln(b(c(),n.slice(2)));let o=n;if(O()==="windows"&&n.match(/^\/[a-z]\//i))try{o=gZ(n)}catch{o=n}if(P(o))return Ln(u(o));if(i===null)throw new OP;return Ln(x(i,o))}
function _H(e){return/(?:^|[\\/])\.\.(?:[\\/]|$)/.test(e)}
function JW(e){let r=u(e);if(O()==="windows")return r.replaceAll("\\","/");return r}
export { Cbe as Cbe };
export { brn as brn };
export { Qe as Qe };
export { JW as JW };
export { EO as EO };
export { _H as _H };
