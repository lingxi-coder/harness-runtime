// Extracted source closure for 2.1.295; source module src_188469437.js sha256 170f1b3f8ce38c33139deaaac2146c8fd3ce65a4f708855b91ec62d3ccd58720
import { q, B } from "./src_186096608.mjs";
import { ipe } from "./src_186481843.mjs";
import { O } from "./src_186477637.mjs";
import { lt } from "./src_188159463.mjs";
import { ie } from "./src_186240709.mjs";
import { E } from "./src_186056234.mjs";
import { uY, spe } from "./src_186713885.mjs";
function yl(e,n){if(n in e)return e[n];if(O()!=="windows")return;let r=Object.keys(e).find((i)=>i.toUpperCase()===n.toUpperCase());return r===void 0?void 0:e[r]}
async function _Cr(e){if(O()!=="linux"&&O()!=="wsl")return;if(!iue(e))return;try{let n=await ie().readFile(`/proc/${e}/stat`,{encoding:"utf8"});return spe(n)}catch{return}}
async function K4e(e){return uY(await _Cr(e))}
var w=2147483647;
function iue(e){return Number.isInteger(e)&&e>1&&e<=w}
function Hm(e){if(!iue(e))return!1;try{return process.kill(e,0),!1}catch(n){return E(n)==="ESRCH"}}
var y=16;
async function m$e(){let e=O();if(e!=="linux"&&e!=="wsl")return!0;let n=ie(),r=await n.readlink("/proc/self").catch(()=>null);if(r===null||r!==String(process.pid))return!1;let i=await n.readdir("/proc").catch(()=>null);if(!i)return!1;return i.filter((o)=>/^\d+$/.test(o.name)).length>=y}
function JSe(e,n){if(n===e)return!0;return!1}
async function q_(e,n){let r=await Fl(e,{skipCache:!0});return r===void 0?void 0:JSe(n,r)}
class h{#e=void 0;get token(){return this.#e}set(e){return this.#e=e,e}reset(){this.#e=void 0}}
var lue=new h;
class S{#e=new Map;get(e){return this.#e.get(e)}set(e,n){this.#e.set(e,n)}}
var x=new q(()=>new S);
async function Oy(){return lue.token??lue.set(await Fl(process.pid))}
var C=60000;
var F=5000;
async function Fl(e,n){let r=Date.now(),i=x.of(B().host);if(n?.env!==void 0)return P(e,n.env);if(!n?.skipCache){let u=i.get(e),l=u?.miss?F:C;if(u&&r-u.at<l)return u.p}let s=P(e),o={at:r,p:s};i.set(e,o);let a=await s;if(a===void 0&&i.get(e)===o)o.miss=!0;return a}
async function P(e,n){let r=n===void 0?{}:{env:n,extendEnv:!1};try{let i=n===void 0?"ps":ipe("ps",yl(n,"PATH")??"");if(i===null)return;let s=await lt(i,["-o","lstart=","-p",String(e)],{timeout:1000,...r,env:{...n??process.env,LC_ALL:"C",TZ:"UTC"}});return s.code===0&&s.stdout?s.stdout.trim():void 0}catch{return}}
export { Oy as Oy };
export { m$e as m$e };
export { iue as iue };
export { Hm as Hm };
export { K4e as K4e };
export { q_ as q_ };
