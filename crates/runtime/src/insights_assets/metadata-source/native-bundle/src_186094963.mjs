// Extracted source closure for 2.1.295; source module src_186094963.js sha256 e611ce126b834ee8ddbfc8c891bca38fe0743df4e7bd46cb4b58f1af902f7903
import { createHash as f } from "crypto";
function bo(t){return t}
var d=/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
function pG(t,s){let i=Buffer.from(s.replace(/-/g,""),"hex"),n=f("sha1").update(i).update(Buffer.from(t,"utf8")).digest();n[6]=n[6]&15|80,n[8]=n[8]&63|128;let e=n.subarray(0,16).toString("hex");return`${e.slice(0,8)}-${e.slice(8,12)}-${e.slice(12,16)}-${e.slice(16,20)}-${e.slice(20,32)}`}
function Jt(t){if(typeof t!=="string")return null;return d.test(t)?t:null}
export { Jt as Jt };
export { bo as bo };
export { pG as pG };
