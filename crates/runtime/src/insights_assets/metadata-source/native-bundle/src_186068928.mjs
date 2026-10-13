// Extracted source closure for 2.1.295; source module src_186068928.js sha256 88e71b90c23c57422aad6e86ca189e1fa0db0ded507932db63c8455889a77de4
import { homedir as it } from "os";
import * as et from "path";
import { resolve as f, relative as tt, isAbsolute as x, parse as st, sep as d, dirname as c, join as q, basename as X } from "path";
import { readFileSync as hn, statSync as Rt, lstatSync as gn, readlinkSync as rt, realpathSync as ot } from "fs";
import { W } from "./src_186056234.mjs";
var qt=typeof global=="object"&&global&&global.Object===Object&&global;
var lIr=qt;
var Qt=typeof self=="object"&&self&&self.Object===Object&&self;
var te=lIr||Qt||Function("return this")();
var iU=te;
var ee=iU.Symbol;
var fG=ee;
var at=Object.prototype;
var {hasOwnProperty:ne,toString:re}=at;
var E=fG?fG.toStringTag:void 0;
function oe(t){var e=ne.call(t,E),n=t[E];try{t[E]=void 0;var r=!0}catch(i){}var o=re.call(t);if(r)if(e)t[E]=n;else delete t[E];return o}
var ut=oe;
var ie=Object.prototype;
var se=ie.toString;
function ae(t){return se.call(t)}
var lt=ae;
var ue="[object Null]";
var le="[object Undefined]";
var ct=fG?fG.toStringTag:void 0;
function ce(t){if(t==null)return t===void 0?le:ue;return ct&&ct in Object(t)?ut(t):lt(t)}
var jse=ce;
function fe(t){var e=typeof t;return t!=null&&(e=="object"||e=="function")}
var eC=fe;
var pe="[object AsyncFunction]";
var de="[object Function]";
var ge="[object GeneratorFunction]";
var he="[object Proxy]";
function me(t){if(!eC(t))return!1;var e=jse(t);return e==de||e==ge||e==pe||e==he}
var xMt=me;
var xe=iU["__core-js_shared__"];
var M=xe;
var ft=function(){var t=/[^.]+$/.exec(M&&M.keys&&M.keys.IE_PROTO||"");return t?"Symbol(src)_1."+t:""}();
function we(t){return!!ft&&ft in t}
var pt=we;
var be=Function.prototype;
var _e=be.toString;
function Se(t){if(t!=null){try{return _e.call(t)}catch(e){}try{return t+""}catch(e){}}return""}
var T8e=Se;
var ye=/[\\^$.*+?()[\]{}|]/g;
var Te=/^\[object .+?Constructor\]$/;
var ve=Function.prototype;
var Ae=Object.prototype;
var Oe=ve.toString;
var Ce=Ae.hasOwnProperty;
var Ee=RegExp("^"+Oe.call(Ce).replace(ye,"\\$&").replace(/hasOwnProperty|(function).*?(?=\\\()| for .+?(?=\\\])/g,"$1.*?")+"$");
function Ne(t){if(!eC(t)||pt(t))return!1;var e=xMt(t)?Ee:Te;return e.test(T8e(t))}
var dt=Ne;
function Pe(t,e){return t==null?void 0:t[e]}
var gt=Pe;
function Re(t,e){var n=gt(t,e);return dt(n)?n:void 0}
var Wse=Re;
var De=Wse(Object,"create");
var m=De;
function Le(){this.__data__=m?m(null):{},this.size=0}
var ht=Le;
function Me(t){var e=this.has(t)&&delete this.__data__[t];return this.size-=e?1:0,e}
var mt=Me;
var Ie="__lodash_hash_undefined__";
var ke=Object.prototype;
var $e=ke.hasOwnProperty;
function je(t){var e=this.__data__;if(m){var n=e[t];return n===Ie?void 0:n}return $e.call(e,t)?e[t]:void 0}
var xt=je;
var We=Object.prototype;
var He=We.hasOwnProperty;
function Ue(t){var e=this.__data__;return m?e[t]!==void 0:He.call(e,t)}
var wt=Ue;
var ze="__lodash_hash_undefined__";
function Fe(t,e){var n=this.__data__;return this.size+=this.has(t)?0:1,n[t]=m&&e===void 0?ze:e,this}
var bt=Fe;
function T(t){var e=-1,n=t==null?0:t.length;this.clear();while(++e<n){var r=t[e];this.set(r[0],r[1])}}
T.prototype.clear=ht;
T.prototype.delete=mt;
T.prototype.get=xt;
T.prototype.has=wt;
T.prototype.set=bt;
var G=T;
function Be(){this.__data__=[],this.size=0}
var _t=Be;
function Ve(t,e){return t===e||t!==t&&e!==e}
var ewe=Ve;
function Je(t,e){var n=t.length;while(n--)if(ewe(t[n][0],e))return n;return-1}
var b=Je;
var Ke=Array.prototype;
var Ze=Ke.splice;
function Ye(t){var e=this.__data__,n=b(e,t);if(n<0)return!1;var r=e.length-1;if(n==r)e.pop();else Ze.call(e,n,1);return--this.size,!0}
var St=Ye;
function Xe(t){var e=this.__data__,n=b(e,t);return n<0?void 0:e[n][1]}
var yt=Xe;
function qe(t){return b(this.__data__,t)>-1}
var Tt=qe;
function Qe(t,e){var n=this.__data__,r=b(n,t);if(r<0)++this.size,n.push([t,e]);else n[r][1]=e;return this}
var vt=Qe;
function v(t){var e=-1,n=t==null?0:t.length;this.clear();while(++e<n){var r=t[e];this.set(r[0],r[1])}}
v.prototype.clear=_t;
v.prototype.delete=St;
v.prototype.get=yt;
v.prototype.has=Tt;
v.prototype.set=vt;
var dpt=v;
var tn=Wse(iU,"Map");
var upt=tn;
function en(){this.size=0,this.__data__={hash:new G,map:new(upt||dpt),string:new G}}
var At=en;
function nn(t){var e=typeof t;return e=="string"||e=="number"||e=="symbol"||e=="boolean"?t!=="__proto__":t===null}
var Ot=nn;
function rn(t,e){var n=t.__data__;return Ot(e)?n[typeof e=="string"?"string":"hash"]:n.map}
var _=rn;
function on(t){var e=_(this,t).delete(t);return this.size-=e?1:0,e}
var Ct=on;
function sn(t){return _(this,t).get(t)}
var Et=sn;
function an(t){return _(this,t).has(t)}
var Nt=an;
function un(t,e){var n=_(this,t),r=n.size;return n.set(t,e),this.size+=n.size==r?0:1,this}
var Pt=un;
function A(t){var e=-1,n=t==null?0:t.length;this.clear();while(++e<n){var r=t[e];this.set(r[0],r[1])}}
A.prototype.clear=At;
A.prototype.delete=Ct;
A.prototype.get=Et;
A.prototype.has=Nt;
A.prototype.set=Pt;
var PMt=A;
var ln="Expected a function";
function V(t,e){if(typeof t!="function"||e!=null&&typeof e!="function")throw TypeError(ln);var n=function(){var r=arguments,o=e?e.apply(this,r):r[0],i=n.cache;if(i.has(o))return i.get(o);var s=t.apply(this,r);return n.cache=i.set(o,s)||i,s};return n.cache=new(V.Cache||PMt),n}
V.Cache=PMt;
var Eo=V;
function Ge(t,e){return t}
function zE(t){let e=t.indexOf("."),n=(e===-1?t:t.slice(0,e)).replace(/ +$/,"");return/^(con|prn|aux|nul|com[0-9\u00B9\u00B2\u00B3]|lpt[0-9\u00B9\u00B2\u00B3])$/i.test(n)}
function Ln(t){return t.normalize("NFC")}
function Un(t){return/^[\\/]{2}/.test(t)||LN(t)}
function cy(t){return/(^|[\\/])\.\.([\\/]|$)/.test(t)}
function dy(t){return/^[\\/]\?\?[\\/]/.test(t)}
function OO(t){if(/^[\\/]{2}[?.][\\/](?!unc[\\/])/i.test(t))return null;return t.match(/^[\\/]{2}(?:[?.][\\/]unc[\\/])?([^\\/]+)/i)?.[1]?.replace(/[A-Z]/g,(e)=>e.toLowerCase())??null}
function Ja(t){return/(^|[\\/])\.{1,2}[. ]*([\\/]|$)/.test(t)}
function Szn(t,e="darwin"){return e==="win32"?/(^|[\\/])\.{1,2}([\\/]|$)/.test(t):/(^|\/)\.{1,2}(\/|$)/.test(t)}
function rA(t){return/^[\\/]{2}[?.][\\/]/.test(t)}
function tw(t){if(!/\/\.(?:vol|file|nofollow|resolve)(?:\/|$)/i.test(t))return!1;if(!t.startsWith("/"))return!1;let e=[];for(let n of t.split("/")){if(n===""||n===".")continue;if(n===".."){e.pop();continue}if(e.push(n),e.length===1&&/^\.(?:vol|file|nofollow|resolve)$/i.test(n))return!0}return!1}
function dIr(t){return/^[\\/](GLOBAL\?\?|GLOBALROOT|DosDevices|Device)[\\/]/i.test(t)}
function Gse(t,e){if(!Un(t))return!1;if(rA(t)||LN(t))return!0;if(Ja(t))return!0;let n=OO(t);return n===null||n!==OO(e)}
function C(t){return t.length===2&&jR(t[0])==="net"||t.length===3&&jR(t[0])==="network"&&jR(t[1])==="servers"}
function jZ(t){if(!t.startsWith("/"))return null;let e=[];for(let n of t.split("/")){if(n===""||n===".")continue;if(n===".."){e.pop();continue}if(e.push(n),C(e))return O(n)}return null}
function O(t){return t.replace(/[A-Z]/g,(e)=>e.toLowerCase())}
function jR(t){return t.replace(/[\u200c-\u200f\u202a-\u202e\u206a-\u206f\ufeff]/g,"").toUpperCase().toLowerCase()}
function DN(t,e){if(tw(t))return!0;if(la(t)){let n=jZ(t);if(n===null||n!==jZ(e))return!0}return pIr(t,e)}
function Wi(t,e){let n=f(e,t);if(Gse(t,e)||Gse(n,e))return!0;let r=jZ(e);for(let o of[t,n]){if(la(o)){let i=jZ(o);if(i===null||i!==r)return!0}if(HO(o)||jS(o)||tw(o)||pIr(o,e))return!0}return!1}
function pIr(t,e,n="darwin",r=n==="darwin"&&Ft()){if(n!=="darwin"||!aU(t,n,r))return!1;let o=nt(e,n,r).complete[0]??null,{complete:i,partial:s}=nt(t,n,r);if(i.length===0&&s.length===0)return!0;if(o===null)return!0;return i.some((a)=>a!==o)||s.some((a)=>!o.startsWith(a))}
var J=Ge({value:void 0},(t)=>{t.value=void 0});
function Ft(t=Dt){let e=t===Dt;if(e&&J.value!==void 0)return J.value;let n;try{let r=!1;try{r=t.readAutoMaster().split(`
`).some((o)=>{let i=o.replace(/#.*$/,"").trim().split(/\s+/);return i.length>=2&&i[0].toLowerCase()==="/home"})}catch(o){if(!W(o))throw o}n=r||t.homeIsOwnMount()}catch{return!0}if(e)J.value=n;return n}
var Dt={readAutoMaster:()=>hn("/etc/auto_master","utf8"),homeIsOwnMount:()=>{let t;try{t=Rt("/home")}catch(e){if(W(e))return!0;throw e}return t.dev!==Rt("/Users").dev}};
var xn=6;
function nt(t,e,n){let r=[],o=[];if(!t.startsWith("/"))return{complete:r,partial:o};let i=[];for(let s of t.split("/")){if(s===""||s===".")continue;if(s===".."){i.pop();continue}if(i.push(s),i.length>xn)continue;let a="/"+i.join("/"),u=(e==="darwin"?cj(a):a).split("/").filter((w)=>w!==""),h=u[0]?.toLowerCase();if(h==="network"){if(u.length===1)o.push("/network/");else if(u.length===2)o.push(`/network/${O(u[1])}/`);else if(u.length===3)r.push(`/network/${O(u[1])}/${O(u[2])}`)}else if(n&&h==="home"&&u.length===2)r.push(`/home/${u[1]}`)}return{complete:r,partial:o}}
function la(t){return Wa(t)}
function Qa(t){return Un(t)||la(t)||DP(t)||tw(t)}
function Wa(t){return EPe(t)!==null}
var Bt=1024;
var Gt=4096;
var I=new Map;
var k=new Map;
function EPe(t){if(!t.startsWith("/"))return null;if(t.length>Gt)return Lt(t);let e=I.get(t);if(e===void 0){if(e=Lt(t),I.size>=Bt)I.clear();I.set(t,e)}return e}
function Lt(t){let e=[];for(let n of t.split("/")){if(n===""||n===".")continue;if(n===".."){e.pop();continue}if(e.push(n),C(e))return"/"+e.join("/")}return null}
var wn=/^([\\/]{2}(?:wsl\.localhost|wsl\$)[\\/]+[^\\/]+)((?:[\\/].*)?)$/i;
function IMt(t){let e=wn.exec(t);return e===null?void 0:{prefix:e[1],rest:e[2]??""}}
function jS(t){if(!t.startsWith("/"))return!1;let e=[];for(let n of t.split("/")){if(n===""||n===".")continue;if(n===".."){e.pop();continue}e.push(n)}return e.length===1&&e[0].toLowerCase()==="net"}
function HO(t){if(!t.startsWith("/"))return!1;if(t.length>Gt)return Mt(t);let e=k.get(t);if(e===void 0){if(e=Mt(t),k.size>=Bt)k.clear();k.set(t,e)}return e}
function Mt(t){let e=[];for(let n of t.split("/")){if(n===""||n===".")continue;if(n===".."){e.pop();continue}if(e.push(n),e.length===1&&e[0].toLowerCase()==="network")return!0}return!1}
function DP(t){return HO(t)}
function mm(t){return Wa(t)||jS(t)||HO(t)||tw(t)}
function aU(t,e="darwin",n=e==="darwin"&&Ft()){if(e!=="darwin")return!1;if(HO(t)||tw(t))return!0;if(!t.startsWith("/"))return!1;let{complete:r,partial:o}=nt(t,e,n);if(o.length>0)return!0;if(r.some((s)=>s.startsWith("/network/")))return!0;let i=r.filter((s)=>s.startsWith("/home/"));if(i.length>0){let s=bn();if(i.some((a)=>a!==s))return!0}return!1}
function bn(){let t;try{t=it()}catch{return null}let e=/^\/home\/([^/]+)/.exec(cj(t));return e?`/home/${e[1]}`:null}
function WZ(t){return Ssn(t)||HO(t)}
function Ssn(t){return ws(t)||Wa(t)||jS(t)||rA(t)||LN(t)||dIr(t)||tw(t)}
function il(t){let e=/^[\\/]{2}wsl(?:\$|\.localhost)[\\/]([^\\/]*)/i.exec(t);return e!==null&&!/^\.{0,2}[. ]*$/.test(e[1]??"")}
function ws(t){return Un(t)&&!il(t)}
function Md(t){if(Vt(t))return it()+t.slice(1);return t}
function Vt(t){return t==="~"||t.startsWith("~/")||!1}
function tC(t){if(t.startsWith("\\\\?\\UNC\\"))return"\\\\"+t.slice(8);if(t.startsWith("\\\\?\\")&&t.length>=7&&t[5]===":")return t.slice(4);return t}
function RH(t){if(/^\\\\\?\\volume\{/i.test(t))return kt(t);if(LN(t))return!0;let e=tC(t);if(e!==t&&kt(e))return!0;return Un(e)&&!il(e)}
function kt(t){return Ja(t)||t.includes("/")}
function Cp(t){try{return tC(ot.native(t))}catch{return null}}
function zse(t){return gIr(t).real??null}
function wzn(t,e){let n=f(e).toLowerCase(),r=c(f(t)).toLowerCase();if(r===n||jt(r,n)||Wt(r,n,"lexical"))return!0;let o=zse(e)?.toLowerCase();if(o==null)return!1;let i=zse(c(f(t)))?.toLowerCase();if(i==null)return!0;return i===o||jt(i,o)||Wt(i,o,"canonical")}
var yn=new Set(["node_modules",".venv","venv","env",".env","virtualenv",".tox",".nox",".direnv","__pypackages__"]);
function jt(t,e){if(!t.startsWith(H(e)+d))return!1;return t.split(d).some((n)=>yn.has(n))}
function H(t){return t.endsWith(d)?t.slice(0,-1):t}
var Z;
function Wt(t,e,n){let r=it(),o=(process.env.LOCALAPPDATA??"").trim();if(n==="lexical")return Y(Ht(r,o,(l)=>f(l).toLowerCase()),t,e);let i=r+"\x00"+o;if(Z?.key===i)return Y(Z.aliasDirs,t,e);let s=!0,a=Ht(r,o,(l)=>{if(!Tn(l))return f(l).toLowerCase();let u=zse(l)?.toLowerCase();if(u==null)return s=!1,f(l).toLowerCase();return u});if(s)Z={key:i,aliasDirs:a};return Y(a,t,e)}
function Ht(t,e,n){let r=[];if(t)r.push(H(n(t))+d+["appdata","local","microsoft","windowsapps"].join(d));if(e){let o=H(n(e))+d+["microsoft","windowsapps"].join(d);if(!r.includes(o))r.push(o)}return r}
function Tn(t){return t.startsWith("/")&&!Un(t)}
function Y(t,e,n){let r=H(n);for(let o of t){if(!(o===r||o.startsWith(r+d)))continue;if(e===o||e.startsWith(o+d))return!0}return!1}
var vn=["usr/local","usr/libexec/cups","usr/share/snmp","AppleInternal","Applications","Library","Users","Volumes","cores","home","media","mnt","opt","pkg","private","sw"];
function cj(t){let e=t.split("/");if(e.length<5||e[0]!==""||N(e[1]??"")!=="system"||N(e[2]??"")!=="volumes"||N(e[3]??"")!=="data")return t;let n=e.slice(4);for(let r of vn){let o=r.split("/");if(n.length<o.length)continue;if(o.every((i,s)=>N(n[s]??"")===N(i)))return"/"+n.join("/")}return t}
function N(t){return t.toUpperCase().toLowerCase()}
function Ezn(t){return!1}
function mpt(t){return t}
var An=/^\/mnt\/[a-z](?:\/|$)/i;
function vzn(t,e,n=""){if(!t.startsWith("/")||An.test(t)||Szn(t,"linux"))return;let r=n===""?t:t.startsWith(n+"/")?t.slice(n.length):void 0;return r===void 0?void 0:e.replace(/[\\/]+$/,"")+r.replace(/\//g,"\\")}
function kzn(t,{exactDots:e=!1}={}){return}
function Czn(t,{exactDots:e=!1}={}){let n=Ppe(t);if(e?Szn(n,"win32"):Ja(n))return;let r=/^([A-Za-z]):[\\/]+(.*)$/.exec(n),o=IMt(n)?.rest.replace(/^[\\/]+/,""),i=(r?.[2]??o??"").replace(/\\/g,"/").replace(/\/+$/,"");if(i==="")return;let s=r?`/mnt/${r[1].toLowerCase()}/${i}`:`/${i}`;return VE(s)?void 0:s}
function VE(t){let e=Ppe(t);return XOo(e)||mm(e)||dIr(e)}
function mIr(t,{allowLocalWsl:e=!1}={}){let n=Ppe(t);if(!x(n))return!0;let{root:r}=st(n);return y(r,S(n.slice(r.length)),0,e)}
function C1e(t,e={}){return mIr(t,e)||Ja(t)&&mIr(f(t),e)}
var On=40;
function S(t){return t.split(/\/+/).filter(Boolean)}
function j(t,e){return VE(t)&&!(e&&Ezn(t))}
function Jt(t){return!1}
function y(t,e,n,r,{trustedStart:o=!1,trustedRoot:i=t}={}){let s=i.endsWith(d)?i:i+d,a=(u)=>o&&(u===i||u.startsWith(s)),l=t;if(!a(l)&&j(l,r))return!0;for(let u=0;u<e.length;u++){let h=e[u];if(h===".")continue;if(h===".."){l=c(l);continue}let w=q(l,h);if(!a(w)&&j(w,r))return!0;let D=R(()=>gn(w));if(D.value===void 0){if(D.code!=="ENOENT"&&D.code!=="ENOTDIR")return!0;let g=w;for(let L of e.slice(u+1))if(g=L===".."?c(g):L==="."?g:q(g,L),!a(g)&&j(g,r))return!0;return!1}let F;if(D.value.isSymbolicLink()){if(n>=On)return!0;if(F=R(()=>rt(w)).value,F===void 0)return!0}else{l=w;continue}let p=F.replace(/^\/{2,}/,"/");if(!(o&&x(p)&&!Ja(p)&&a(p))&&j(p,r))return!0;let B=e.slice(u+1);if(Jt(p))return!0;if(x(p)){if(o&&!Ja(p)&&a(p))return y(i,[...S(p.slice(s.length)),...B],n+1,r,{trustedStart:!0,trustedRoot:i});let g=st(p).root;return y(g,[...S(p.slice(g.length)),...B],n+1,r)}return y(l,[...S(p),...B],n+1,r,{trustedStart:a(l),trustedRoot:i})}return!1}
function gIr(t){let e=R(()=>tC(ot.native(t)));return{real:e.value,code:e.code}}
function z(t){return t!=null&&typeof t==="object"&&"code"in t&&typeof t.code==="string"?t.code:void 0}
function R(t){try{return{value:Cn(t)}}catch(e){return{code:z(e)}}}
function Cn(t){return Kt(t,()=>!0)}
function Kt(t,e){for(let n=0;;n++)try{return t()}catch(r){let o=z(r);throw r}}
function XOo(t){return dj(t)||Wa(t)}
function Yt(t){return Ja(t)||/[. :](?=[\\/]|$)/.test(t)||t.split(/[\\/]/).some((e)=>zE(e)||zE(e.slice(0,Math.max(0,e.indexOf(":")))))}
function Ppe(t){let e=/^(?:\\\\[?.]\\|\\\?\?\\)(?=[A-Za-z]:[\\/])/.exec(t);if(!e)return t;let n=t.slice(e[0].length),r=n.slice(2);return e[0].includes("\\")&&r.includes("/")||Yt(r)?t:n}
function qE(t,{foldCase:e,knownNotSuspect:n=!1}={}){if(e??=!0,t=Ppe(t),dj(t)||Wa(t)){let a=mpt(f(mpt(t))).normalize("NFC");return e?a.toLowerCase():a}if(!n&&C1e(x(t)?t:f(t))){let a=f(t).normalize("NFC");return e?a.toLowerCase():a}let r=f(t),o=[],i=Cp(r);while(i===null&&c(r)!==r)o.unshift(X(r)),r=c(r),i=Cp(r);i=o.length>0?f(i??r,...o):i??r;let s=mpt(i).normalize("NFC");return e?s.toLowerCase():s}
function lh(t,e,{alreadyComparable:n=!1,foldCase:r}={}){let o=tt(n?t:qE(t,{foldCase:r}),n?e:qE(e,{foldCase:r}));return o===""||!x(o)&&o!==".."&&!o.startsWith(`..${d}`)}
function dj(t){return Un(t)}
function LN(t){return zt.test(t)||t.includes("??")&&zt.test(Xt(t))}
var zt=/^[\\/]\?\?[\\/]/;
function Xt(t){return et.win32?et.win32.normalize(t):t}
export { Eo as Eo };
export { Ge as Ge };
export { Md as Md };
export { RH as RH };
export { Ln as Ln };
export { jse as jse };
export { eC as eC };
export { fG as fG };
export { iU as iU };
export { dpt as dpt };
export { lIr as lIr };
export { PMt as PMt };
export { ewe as ewe };
export { xMt as xMt };
export { Wse as Wse };
export { WZ as WZ };
export { cy as cy };
export { ws as ws };
export { Wa as Wa };
export { jS as jS };
export { HO as HO };
export { DN as DN };
export { la as la };
export { Un as Un };
export { il as il };
export { Gse as Gse };
export { Ja as Ja };
export { EPe as EPe };
export { tw as tw };
export { jZ as jZ };
export { upt as upt };
export { Wi as Wi };
export { lh as lh };
export { Cp as Cp };
export { Qa as Qa };
export { rA as rA };
export { OO as OO };
export { zse as zse };
export { Ppe as Ppe };
export { kzn as kzn };
export { vzn as vzn };
export { Szn as Szn };
export { VE as VE };
export { Ezn as Ezn };
export { Czn as Czn };
export { tC as tC };
export { dy as dy };
export { DP as DP };
export { IMt as IMt };
export { wzn as wzn };
