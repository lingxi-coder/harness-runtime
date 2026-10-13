// Extracted source closure for 2.1.295; source module src_188445685.js sha256 851ff1c516fef57112e984bd1887d86e0c0a9a5d1bc4218da03165ee2ca14b3a
import { AsyncLocalStorage as Z } from "async_hooks";
import { Pw, O3, UOt, gEo, O$, nbe, vR, Wen, iH } from "./src_188442564.mjs";
import { ie, t, Y, _ } from "./src_186240709.mjs";
import { $a } from "./src_188419429.mjs";
import { join as Q, join as re } from "path";
import { l, E } from "./src_186056234.mjs";
import { f, g, m } from "./src_186441656.mjs";
import { Dxe, st } from "./src_188159463.mjs";
import { Hg } from "./src_186715191.mjs";
import { j } from "./src_186014760.mjs";
import { q, B, De } from "./src_186096608.mjs";
import { En } from "./src_186893596.mjs";
import { chmod as te } from "fs/promises";
var Ha=Symbol("secureStorage.READ_FAILED");
var I=new Z;
var x=Promise.resolve();
async function sEo(e){if(I.getStore())return e();let r=x,a;x=new Promise((s)=>{a=s});try{await r;let s=Pw();await ie().mkdir(s);let n=await $a(Q(s,".storage-write"),{realpath:!1,retries:{retries:10,minTimeout:100,maxTimeout:1000},stale:15000,onCompromised:(o)=>t(`[secureStorage] write lock compromised: ${l(o)}`,{level:"warn"})});try{return await I.run(!0,e)}finally{await n().catch((o)=>t(`[secureStorage] write lock release failed: ${l(o)}`,{level:"warn"}))}}finally{a()}}
function d(e,r,a){return sEo(async()=>{e.invalidateCache?.();let s=await(e.readAsyncStrict?.(a,{inaccessibleAs:"failureIfTransient"})??e.readAsync(a));if(s===Ha)return f("secure_storage_credentials_write","read_failed_skip_write"),{success:!1,transient:!0};let n=s??{},o=r(n);return o===n?{success:!0}:await e.update(o,a)})}
var L=Symbol("secureStorage.fallbackLegs");
function M(e,r){let a=r.osGuarded===!0?(n)=>n:aEo,s={name:`${e.name}-with-${r.name}-fallback`,[L]:[e,r],osGuarded:e.osGuarded===!0,read(n){let o=e.read(n);if(o!==null&&o!==void 0)return o;return a(r.read(n))||{}},async readAsync(n){let o=await e.readAsync();if(o!==null&&o!==void 0)return o;return a(await r.readAsync(n))||{}},async readAsyncStrict(n,o){let i=await(e.readAsyncStrict?.(void 0,o)??e.readAsync());if(i===Ha)return Ha;if(i!==null||o?.inaccessibleAs==="failure")return i;let c=await(r.readAsyncStrict?.(n,o)??r.readAsync(n));return c===Ha?c:a(c)||{}},invalidateCache(){e.invalidateCache?.(),r.invalidateCache?.()},mutate(n,o){return d(s,n,o)},async update(n,o){let i=await e.readAsync(),c=await e.update(n);if(c.success){if(i===null)await r.delete(o);return g("secure_storage_credentials_write"),c}if(c.transient)return f("secure_storage_credentials_write",c.keychainLocked?"keychain_locked_skip_fallback":"primary_transient_skip_fallback"),c;let N=await r.update(n,o);if(N.success){if(i!==null&&!await e.delete())return m("secure_storage_credentials_write","plaintext_fallback_primary_not_cleared",c.exitCode===void 0?void 0:{primary_write_exit_code:c.exitCode}),{success:!1,transient:!0};return f("secure_storage_credentials_write","plaintext_fallback_used"),{success:!0,warning:N.warning}}return m("secure_storage_credentials_write","primary_and_fallback_failed"),{success:!1}},async delete(n){let o=await r.readAsync(n)!==null,[i,c]=await Promise.all([e.delete(),r.delete(n)]);return i||o&&c}};return s}
function aEo(e){let r=e?.slackTag;if(typeof r!=="object"||r===null||!("lastBinding"in r))return e;let{lastBinding:a,...s}=r;return{...e,slackTag:s}}
var p=2000;
var ee=4032;
var P=44;
var F=36;
var b={name:"keychain",osGuarded:!0,read(){let e=O3(),r=e.cache;if(Date.now()-r.cachedAt<UOt)return r.data;let a=e.lastReadFailure;if(a!==null&&Date.now()-a<gEo)return r.data;try{let s=O$(nbe),n=vR(),o=Dxe(`security find-generic-password -a "${n}" -w -s "${s}"`,{timeout:p});if(o){e.keychainHoldsItem=!0;let i=Y(o);return e.cache={data:i,cachedAt:Date.now()},i}}catch(s){}if(r.data!==null)return t("[keychain] read failed; serving stale cache",{level:"warn"}),e.cache={data:r.data,cachedAt:Date.now()},r.data;return e.cache={data:null,cachedAt:Date.now()},null},async readAsync(){let e=O3(),r=e.cache;if(Date.now()-r.cachedAt<UOt)return r.data;if(e.readInFlight)return e.readInFlight;let a=e.lastReadFailure;if(a!==null&&Date.now()-a<gEo)return null;let s=e.generation,n=v(e).then((o)=>{if(s!==e.generation)return o===Ha?null:o;if(e.readInFlight=null,o===Ha){if(t("[keychain] readAsync failed; not caching a null",{level:"warn"}),e.lastReadFailure=Date.now(),r.data!==null)e.cache={data:r.data,cachedAt:Date.now()};return r.data}let i=o??r.data;return e.cache={data:i,cachedAt:Date.now()},e.lastReadFailure=null,i});return e.readInFlight=n,n},async readAsyncStrict(e,r){let a=O3(),s=a.generation,n=await v(a,r);if(n!==Ha&&s===a.generation)a.cache={data:n??a.cache.data,cachedAt:Date.now()},a.lastReadFailure=null;return n},invalidateCache(){iH()},mutate(e){return d(b,e)},async update(e){let r=O3();iH();try{let a=O$(nbe),s=vR(),n=_(e),o=Buffer.from(n,"utf-8").toString("hex"),i=`add-generic-password -U -a "${s}" -s "${a}" -X "${o}"
`,c;if(i.length<=ee)c=await Hg("security",["-i"],{input:i,stdio:["pipe","pipe","pipe"],reject:!1,timeout:p});else t(`Keychain payload (${n.length}B JSON) exceeds security -i stdin limit; using argv`,{level:"warn"}),c=await Hg("security",["add-generic-password","-U","-a",s,"-s",a,"-X",o],{stdio:["ignore","pipe","pipe"],reject:!1,timeout:p});if(c.exitCode!==0){if(c.exitCode===F&&r.keychainHoldsItem)return{success:!1,transient:!0,keychainLocked:!0};return{success:!1,transient:c.timedOut,exitCode:c.exitCode}}return r.keychainHoldsItem=!0,r.cache={data:e,cachedAt:Date.now()},r.generation++,r.readInFlight=null,{success:!0}}catch(a){return{success:!1}}},async delete(){let e=O3();iH();try{let r=O$(nbe),a=vR(),{code:s}=await st("security",["delete-generic-password","-a",a,"-s",r],{timeout:p,useCwd:!1}),n=s===0||s===P;if(n)e.keychainHoldsItem=!1;return e.generation++,e.readInFlight=null,n}catch(r){return!1}}};
async function v(e,r){let a=e.generation;try{let s=O$(nbe),n=vR(),{stdout:o,code:i}=await st("security",["find-generic-password","-a",n,"-w","-s",s],{useCwd:!1,preserveOutputOnError:!1,timeout:p});if(i===0&&o)return Wen(e,!0,a),Y(o.trim());if(i===P)return Wen(e,!1,a),null;if(i===0)return null;if(i===F)switch(r?.inaccessibleAs??"absent"){case"absent":return null;case"failure":return Ha;case"failureIfTransient":return e.keychainHoldsItem?Ha:null}return Ha}catch(s){return r?.unparsableEntryAs==="failure"?Ha:null}}
function u(){let e=Pw(),r=".credentials.json";return{storageDir:e,storagePath:re(e,".credentials.json")}}
var GCr="Warning: Storing credentials in plaintext.";
function ne(e,r){if(e==="ENOENT"||e==="EISDIR"||e==="ENOTDIR")return null;if((e==="EACCES"||e==="EPERM")&&r!=="win32")return null;return Ha}
function ae(e,r){return e==="ENOENT"?null:Ha}
var oe={async read(){let{storagePath:e}=u();try{let r=await ie().readFile(e,{encoding:"utf8"});return Y(r)}catch{return null}},async readStrict(e){let{storagePath:r}=u(),a;try{a=await ie().readFile(r,{encoding:"utf8"})}catch(s){return(e?ae:ne)(E(s),"darwin")}try{return Y(a)}catch{if(e)return Ha;return null}},async write(e){try{let{storageDir:r,storagePath:a}=u();return await ie().mkdir(r),await En(a,_(e),384),await te(a,384),{success:!0,warning:GCr}}catch{return{success:!1}}finally{if(j())VRe()}},async remove(){let{storagePath:e}=u();try{return await ie().unlink(e),!0}catch(r){if(E(r)==="ENOENT")return!0;return!1}finally{if(j())VRe()}}};
var se={read:async()=>null,readStrict:async()=>null,write:async()=>({success:!1}),remove:async()=>!0};
function ce(e){switch(e.state){case"present":return e.data;case"absent":case"corrupt":return null;case"refused-symlink":case"read-failed":return Ha;default:{let r=e;return Ha}}}
function ue(e){return{async read(){let{storagePath:r}=u(),a=y(),s=a.generation,n=await A(e.readCredentials(),a);return U(a,n,r,s),n.state==="present"?n.data:null},async readStrict(r){let{storagePath:a}=u(),s=y(),n=s.generation,o=await A(r?e.readCredentials():e.readCredentialsStrict(),s);if(U(s,o,a,n),r)return o.state==="present"?o.data:o.state==="absent"?null:Ha;return ce(o)},async write(r){let{storagePath:a}=u(),s=y(),n=s.generation,o=await A(e.writeCredentials(r),s);if(o.state==="written"&&n===s.generation)w(s,a,_(r));else h(s);return o.state==="written"?{success:!0,warning:GCr}:{success:!1}},async remove(){let{storagePath:r}=u(),a=y(),s=a.generation,n=await A(e.deleteCredentials(),a);if(n.state==="deleted"&&s===a.generation)w(a,r,null);else h(a);return n.state==="deleted"}}}
async function A(e,r){try{return await e}catch(a){throw h(r),a}}
class H{copy=void 0;generation=0}
var le=new q(()=>new H);
function y(){return le.of(B().host)}
function w(e,r,a){e.copy={storagePath:r,text:a},e.generation++}
function U(e,r,a,s){if(s!==e.generation){h(e);return}switch(r.state){case"present":w(e,a,_(r.data));return;case"absent":case"corrupt":case"refused-symlink":w(e,a,null);return;case"read-failed":h(e);return;default:{let n=r;h(e)}}}
function h(e){e.copy=void 0,e.generation++}
function VRe(){h(y())}
function R(e){return j()&&e!==void 0?ue(e):De()?se:oe}
var T={name:"plaintext",osGuarded:!1,read(e){let{storagePath:r}=u();try{let a=j()&&e?.fromStoreCopy===!0?y().copy:void 0;if(a!==void 0&&a.storagePath===r){if(a.text===null)return null;return JSON.parse(a.text)}if(De())return null;let s=ie().readFileSync(r,{encoding:"utf8"});return Y(s)}catch{return null}},invalidateCache(){VRe()},readAsync(e){return R(e).read()},readAsyncStrict(e,r){return R(e).readStrict(r?.unreadableFileAs==="failure")},mutate(e,r){return d(T,e,r)},update(e,r){return R(r).write(e)},delete(e){return R(e).remove()}};
var W;
function Xn(){if(W)return W;return M(b,T)}
export { Xn as Xn };
export { VRe as VRe };
export { Ha as Ha };
