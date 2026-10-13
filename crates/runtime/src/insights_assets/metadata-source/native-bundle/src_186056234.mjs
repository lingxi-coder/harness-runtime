// Extracted source closure for 2.1.295; source module src_186056234.js sha256 a462b175f36ed4ae079da572a7a6da5993bc4c8fd2b53f4dfbbb82e14d3a0b52
import { vn, S } from "./src_186016069.mjs";
class WR extends Error{filePath;defaultConfig;constructor(n,e,t){super(n);this.name="ConfigParseError",this.filePath=e,this.defaultConfig=t}}
class x extends Error{telemetryMessage;errorClass;constructor(n,e,t){super(n);this.name="TelemetrySafeError",this.telemetryMessage=e??n,this.errorClass=t}}
var rt=(n,e)=>{try{if(n!==null&&typeof n==="object"&&!("telemetryMessage"in n)&&Object.isExtensible(n))Object.assign(n,{telemetryMessage:e})}catch{}return n};
function DV(n){try{if(n!==null&&typeof n==="object"&&"telemetryMessage"in n&&typeof n.telemetryMessage==="string")return n.telemetryMessage}catch{}return}
function G(n){return n instanceof Error?n:Error(String(n))}
function l(n){return n instanceof Error?n.message:String(n)}
function E(n){if(n&&typeof n==="object"&&"code"in n&&typeof n.code==="string")return n.code;return}
function zr(n){return oA(E(n))}
function oA(n){return n&&/^[A-Z][A-Z0-9_]{0,63}$/.test(n)?vn(n):void 0}
function xH(n){return typeof n==="string"&&/^[A-Z][a-zA-Z]{0,63}$/.test(n)?vn(n):void 0}
function ksn(n){return typeof n==="string"&&/^[a-z][a-z_]{0,39}$/.test(n)?vn(n):S("unparseable")}
function nwe(n){if(typeof n!=="string")return;return/^[A-Za-z][A-Za-z0-9_-]{0,63}$/.test(n)?vn(n):S("nonconforming")}
function yIr(n){return p.test(n)?vn(n):void 0}
var p=/^(?:<user-code>|(?:[\w$.\-<>[\]]{1,72}\.(?:[cm]?[jt]sx?|json|node|wasm|exe)|node:internal(?:\/[a-z0-9_-]{1,40}){1,4}|node:[a-z0-9_]{1,30}(?:\/(?:promises|web|consumers|posix|win32|types|strict|reporters))?|bun:[a-z0-9_]{1,20}|internal:[a-z0-9_-]{1,30}(?:\/[a-z0-9_-]{1,40}){0,3}|<anonymous>|\[eval\](?:-wrapper)?|native|unknown|cli|claude|ws|undici|node-fetch|isomorphic-fetch|vercel_fetch|utf-8-validate)(?::\d{1,8}){2})$/;
function As(n){return n!==null&&typeof n==="object"&&"errno"in n&&typeof n.errno==="number"}
function W(n){return E(n)==="ENOENT"}
function HY(n){return E(n)==="EISDIR"}
function UMt(n){let e=n.message?`${n.name}: ${n.message}`:n.name,t=n.stack;if(!t)return e;if(t.startsWith(e))return t;let r=t.split(`
`),o=r[0]??"",i=o===n.name||o.startsWith(`${n.name}:`)?r.slice(1):r;return[e,...i].join(`
`)}
var u=new Set(["ENOENT","EACCES","EPERM","ENOTDIR","ELOOP","ENAMETOOLONG","EROFS"]);
function Bt(n){return zR(E(n))}
function zR(n){return n!==void 0&&u.has(n)}
function kD(n){let e=n instanceof Error?n.cause:void 0;if(typeof e!=="object"||e===null||!("telemetryCode"in e))return;return typeof e.telemetryCode==="string"?e.telemetryCode:void 0}
var a=new Set(["ENOSPC","EDQUOT","ENFILE","EIO"]);
function uy(n){return _Ir(E(n))}
function _Ir(n){return n==="EDEADLK"||n==="EINTR"||n==="ENXIO"||n==="ENODEV"||n==="ECANCELED"||n==="ENEEDAUTH"||n==="ESTALE"||n==="EUNKNOWN"||n==="ENOMEM"||n!==void 0&&c(n)}
function c(n){return n==="UNKNOWN"||n.startsWith("Unknown system error")}
function BMt(n,e="darwin"){if(e!=="win32")return!1;try{return As(n)&&n.code==="EBADF"&&n.syscall==="read"}catch{return!1}}
function pj(n){if(typeof n!=="string")return;return/^E[A-Z0-9]+$/.test(n)||c(n)?n:void 0}
function If(n){return jMt(E(n))}
function jMt(n){return n!==void 0&&(u.has(n)||n==="EISDIR"||a.has(n))}
function Ap(n,e){if(e?.(n))return!0;if(!n||typeof n!=="object"||!("isAxiosError"in n)||!n.isAxiosError)return!1;let t=n.response?.status;return t===void 0||t===401||t===403||t===429}
function Qs(n){let e=l(n);if(!n||typeof n!=="object"||!("isAxiosError"in n)||!n.isAxiosError)return{kind:"other",message:e};let t=n,r=t.response?.status;if(r===401||r===403)return{kind:"auth",status:r,message:e};if(t.code==="ECONNABORTED")return{kind:"timeout",status:r,message:e};if(t.code==="ECONNREFUSED"||t.code==="ENOTFOUND")return{kind:"network",status:r,message:e};return{kind:"http",status:r,message:e}}
export { pj as pj };
export { BMt as BMt };
export { kD as kD };
export { rt as rt };
export { E as E };
export { Bt as Bt };
export { If as If };
export { uy as uy };
export { DV as DV };
export { l as l };
export { G as G };
export { W as W };
export { HY as HY };
export { nwe as nwe };
export { UMt as UMt };
export { x as x };
export { Ap as Ap };
export { Qs as Qs };
export { WR as WR };
export { ksn as ksn };
export { zr as zr };
export { As as As };
export { xH as xH };
export { yIr as yIr };
