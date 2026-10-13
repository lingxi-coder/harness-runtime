// Extracted source closure for 2.1.295; source module src_187659079.js sha256 0dcdb7aa805b9fee39ffc8f93b4a8af550f1e6f043b36375f5c327d71f884159
import { m1e, $xr, B } from "./src_186096608.mjs";
function $i(){return $xr.of(B().host)}
function Ir(e){$i().invalidateAll(e)}
var xs=["userSettings","projectSettings","localSettings","flagSettings","policySettings"];
function No(){let e=m1e(),n=$i();if(n.enabledSources?.allowed===e)return n.enabledSources.result;let t=new Set(e);t.add("flagSettings"),t.add("policySettings");let o=xs.filter((i)=>t.has(i));return n.enabledSources={allowed:e,result:o},o}
function gn(e){return No().includes(e)}
var mN=["projectSettings","localSettings"];
var Af=new Set(mN);
export { gn as gn };
export { xs as xs };
export { Af as Af };
export { $i as $i };
export { Ir as Ir };
export { mN as mN };
