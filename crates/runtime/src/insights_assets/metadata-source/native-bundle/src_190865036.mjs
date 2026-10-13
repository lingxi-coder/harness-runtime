// Extracted source closure for 2.1.295; source module src_190865036.js sha256 01c32ebb48fb7cdf04bb092aa648a1dc2df9e983fd91955dd8677bde0c584ece
import { resolve as P, join as W, isAbsolute as S } from "path";
import { we, IP } from "./src_186096608.mjs";
import { Sl, Ado, P0 } from "./src_190805779.mjs";
import { it } from "./src_190860966.mjs";
import { statSync as L, readFileSync as _ } from "fs";
import { p } from "./src_186562918.mjs";
import { u, o, C, oe } from "./src_186578912.mjs";
import { Ppe, kzn, vzn, Szn, VE, Ezn, Czn } from "./src_186068928.mjs";
import { D } from "./src_186447803.mjs";
import { Ee } from "./src_186220896.mjs";
function MK(e,r,t=Date.now()){return Number.isFinite(e)&&Math.abs(t-e)<r}
function kC(){return P(we(),Sl())}
function KMn(){return D([P(Ee(),"plugins"),P(Ee(),"cowork_plugins"),kC(),...P0().map((e)=>P(we(),e))])}
var N=4194304;
var I=p(()=>u({sourceCommand:o().optional().catch(void 0),sourceProducerPath:o().optional().catch(void 0),previousProducerPaths:C(oe()).transform((e)=>e.filter((r)=>typeof r==="string")).optional().catch(void 0)}).passthrough());
function bdo(e){let r=it(),t=r.commandProducerDirsDenied,i=R();r.commandProducerDirsWslProviderUndeterminable=i==="undeterminable";let n=i==="undeterminable"?void 0:i;r.commandProducerDirsScannedAt=Date.now(),r.commandProducerDirsComparable=null;for(let b of new Set(e)){let s;try{let m=W(b,"installed_plugins.json"),f=L(m);if(!f.isFile()||f.size>N)continue;let l=_(m,"utf8");s=JSON.parse(l)}catch{continue}let a=typeof s==="object"&&s!==null&&"plugins"in s?s.plugins:void 0;if(typeof a!=="object"||a===null)continue;for(let m of Object.values(a)){if(!Array.isArray(m))continue;for(let f of m){let l=I().safeParse(f);if(!l.success)continue;for(let g of[l.data.sourceProducerPath,...l.data.previousProducerPaths??[]]){let d=g===void 0?void 0:Ppe(g),h=d===void 0?void 0:y(d,n);if(h!==void 0)t.add(h)}}}}return[...t]}
var wdo=IP(()=>it().commandProducerDirsChanged);
function R(){let e=it(),r=kC(),t=e.wslProviderOfPluginsRootMemo;if(t?.root===r&&MK(t.at,M))return t.value;let i=Ado(r);if(i==="undeterminable")return"undeterminable";let n=i===void 0?void 0:{prefix:i.prefix};return e.wslProviderOfPluginsRootMemo={root:r,value:n,at:Date.now()},n}
var M=30000;
function y(e,r){let t=kzn(e,{exactDots:!0})??(r===void 0?void 0:vzn(e,r.prefix));if(t!==void 0)return t;if(S(e))return!Szn(e)&&(!VE(e)||Ezn(e))?e:void 0;return Czn(e,{exactDots:!0})}
export { kC as kC };
export { bdo as bdo };
export { KMn as KMn };
export { wdo as wdo };
