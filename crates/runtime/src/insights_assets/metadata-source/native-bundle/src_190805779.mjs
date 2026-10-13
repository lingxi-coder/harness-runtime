// Extracted source closure for 2.1.295; source module src_190805779.js sha256 4e88499b22d0a3d0014050efbf80590d8ed6e66cf6586953e03e7c2ab6ff79a5
import { a } from "./src_186481843.mjs";
import { delimiter as Je, join as R, parse as Xe } from "path";
import { Md, IMt, tC, zse } from "./src_186068928.mjs";
import { Ee } from "./src_186220896.mjs";
import { Von } from "./src_186096608.mjs";
import { O } from "./src_186477637.mjs";
var pe="plugins";
var V="cowork_plugins";
function Y(){if(Von())return V;if(a.CLAUDE_CODE_USE_COWORK_PLUGINS)return V;return pe}
function Sl(){let e=a.CLAUDE_CODE_PLUGIN_CACHE_DIR;if(e)return Md(e);return R(Ee(),Y())}
function P0(){let e=a.CLAUDE_CODE_PLUGIN_SEED_DIR;if(!e)return[];return e.split(Je).filter(Boolean).map(Md)}
function Ado(e){if(O()!=="windows")return;let r=Xe(e).root,n=(s)=>IMt(tC(s)),i=n(r);if(i===void 0){let s=zse(r);if(s===null)return"undeterminable";i=n(s)}return i===void 0?void 0:{walkRoot:r,...i}}
export { P0 as P0 };
export { Sl as Sl };
export { Ado as Ado };
