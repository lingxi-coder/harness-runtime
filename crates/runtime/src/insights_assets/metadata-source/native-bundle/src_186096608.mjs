// Extracted source closure for 2.1.295; source module src_186096608.js sha256 b90390ba0836cfb687512dbcaa24a581540cdf1607370f7718d67fb8326f4410
import { BOo, czn, j } from "./src_186014760.mjs";
import { Ln, jse, eC, Eo, fG, iU, dpt, lIr, PMt, ewe, xMt, Wse, upt } from "./src_186068928.mjs";
import { Ie } from "./src_186092222.mjs";
import { bo, pG } from "./src_186094963.mjs";
import { bIr } from "./src_186066580.mjs";
import { randomUUID as Bbe } from "crypto";
import { dirname as Dt, basename as Kr } from "path";
var Z="3ab19d7e-9f35-45c2-926e-75e271cc60b3";
function HGn(){let e=process.env.CLAUDE_CODE_REMOTE_SESSION_ID?.trim();return e?pG(e,Z):null}
function Pn(){this.__data__=new dpt,this.size=0}
var Ze=Pn;
function An(e){var t=this.__data__,o=t.delete(e);return this.size=t.size,o}
var et=An;
function Mn(e){return this.__data__.get(e)}
var tt=Mn;
function wn(e){return this.__data__.has(e)}
var nt=wn;
var On=200;
function In(e,t){var o=this.__data__;if(o instanceof dpt){var r=o.__data__;if(!upt||r.length<On-1)return r.push([e,t]),this.size=++o.size,this;o=this.__data__=new PMt(r)}return o.set(e,t),this.size=o.size,this}
var ot=In;
function P(e){var t=this.__data__=new dpt(e);this.size=t.size}
P.prototype.clear=Ze;
P.prototype.delete=et;
P.prototype.get=tt;
P.prototype.has=nt;
P.prototype.set=ot;
var n1e=P;
var Fn="__lodash_hash_undefined__";
function En(e){return this.__data__.set(e,Fn),this}
var rt=En;
function Dn(e){return this.__data__.has(e)}
var it=Dn;
function E(e){var t=-1,o=e==null?0:e.length;this.__data__=new PMt;while(++t<o)this.add(e[t])}
E.prototype.add=E.prototype.push=rt;
E.prototype.has=it;
var Oxr=E;
function Hn(e,t){var o=-1,r=e==null?0:e.length;while(++o<r)if(t(e[o],o,e))return!0;return!1}
var st=Hn;
function Bn(e,t){return e.has(t)}
var Hxr=Bn;
var Un=1;
var _n=2;
function qn(e,t,o,r,i,s){var a=o&Un,l=e.length,d=t.length;if(l!=d&&!(a&&d>l))return!1;var p=s.get(e),m=s.get(t);if(p&&m)return p==t&&m==e;var c=-1,f=!0,u=o&_n?new Oxr:void 0;s.set(e,t),s.set(t,e);while(++c<l){var g=e[c],v=t[c];if(r)var b=a?r(v,g,c,t,e,s):r(g,v,c,e,t,s);if(b!==void 0){if(b)continue;f=!1;break}if(u){if(!st(t,function(C,x){if(!Hxr(u,x)&&(g===C||i(g,C,o,r,s)))return u.push(x)})){f=!1;break}}else if(!(g===v||i(g,v,o,r,s))){f=!1;break}}return s.delete(e),s.delete(t),f}
var D=qn;
var Wn=iU.Uint8Array;
var oon=Wn;
function Nn(e){var t=-1,o=Array(e.size);return e.forEach(function(r,i){o[++t]=[i,r]}),o}
var at=Nn;
function Gn(e){var t=-1,o=Array(e.size);return e.forEach(function(r){o[++t]=r}),o}
var son=Gn;
var Kn=1;
var Vn=2;
var Jn="[object Boolean]";
var zn="[object Date]";
var Qn="[object Error]";
var Xn="[object Map]";
var $n="[object Number]";
var Zn="[object RegExp]";
var eo="[object Set]";
var to="[object String]";
var no="[object Symbol]";
var oo="[object ArrayBuffer]";
var io="[object DataView]";
var lt=fG?fG.prototype:void 0;
var ee=lt?lt.valueOf:void 0;
function so(e,t,o,r,i,s,a){switch(o){case io:if(e.byteLength!=t.byteLength||e.byteOffset!=t.byteOffset)return!1;e=e.buffer,t=t.buffer;case oo:if(e.byteLength!=t.byteLength||!s(new oon(e),new oon(t)))return!1;return!0;case Jn:case zn:case $n:return ewe(+e,+t);case Qn:return e.name==t.name&&e.message==t.message;case Zn:case to:return e==t+"";case Xn:var l=at;case eo:var d=r&Kn;if(l||(l=son),e.size!=t.size&&!d)return!1;var p=a.get(e);if(p)return p==t;r|=Vn,a.set(e,t);var m=D(l(e),l(t),r,i,s,a);return a.delete(e),m;case no:if(ee)return ee.call(e)==ee.call(t)}return!1}
var dt=so;
function ao(e,t){var o=-1,r=t.length,i=e.length;while(++o<r)e[i+o]=t[o];return e}
var KHt=ao;
var lo=Array.isArray;
var Nw=lo;
function uo(e,t,o){var r=t(e);return Nw(e)?r:KHt(r,o(e))}
var Mxr=uo;
function co(e,t){var o=-1,r=e==null?0:e.length,i=0,s=[];while(++o<r){var a=e[o];if(t(a,o,e))s[i++]=a}return s}
var Dxr=co;
function po(){return[]}
var Lxr=po;
var fo=Object.prototype;
var go=fo.propertyIsEnumerable;
var ut=Object.getOwnPropertySymbols;
var ho=!ut?Lxr:function(e){if(e==null)return[];return e=Object(e),Dxr(ut(e),function(t){return go.call(e,t)})};
var YHt=ho;
function mo(e,t){var o=-1,r=Array(e);while(++o<e)r[o]=t(o);return r}
var ct=mo;
function vo(e){return e!=null&&typeof e=="object"}
var iG=vo;
var yo="[object Arguments]";
function ko(e){return iG(e)&&jse(e)==yo}
var te=ko;
var ft=Object.prototype;
var {hasOwnProperty:Co,propertyIsEnumerable:xo}=ft;
var Ro=te(function(){return arguments}())?te:function(e){return iG(e)&&Co.call(e,"callee")&&!xo.call(e,"callee")};
var n8e=Ro;
var U={};
function To(){return!1}
var gt=To;
var St=typeof U=="object"&&U&&!U.nodeType&&U;
var ht=St&&typeof H=="object"&&H&&!H.nodeType&&H;
var Po=ht&&ht.exports===St;
var mt=Po?iU.Buffer:void 0;
var Ao=mt?mt.isBuffer:void 0;
var Mo=Ao||gt;
var r1e=Mo;
var wo=9007199254740991;
var Lo=/^(?:0|[1-9]\d*)$/;
function Oo(e,t){var o=typeof e;return t=t==null?wo:t,!!t&&(o=="number"||o!="symbol"&&Lo.test(e))&&(e>-1&&e%1==0&&e<t)}
var r8e=Oo;
var Io=9007199254740991;
function Fo(e){return typeof e=="number"&&e>-1&&e%1==0&&e<=Io}
var A=Fo;
var Do="[object Arguments]";
var Ho="[object Array]";
var Bo="[object Boolean]";
var Uo="[object Date]";
var jo="[object Error]";
var qo="[object Function]";
var Wo="[object Map]";
var No="[object Number]";
var Go="[object Object]";
var Ko="[object RegExp]";
var Vo="[object Set]";
var Jo="[object String]";
var zo="[object WeakMap]";
var Qo="[object ArrayBuffer]";
var Xo="[object DataView]";
var Yo="[object Float32Array]";
var $o="[object Float64Array]";
var Zo="[object Int8Array]";
var er="[object Int16Array]";
var tr="[object Int32Array]";
var nr="[object Uint8Array]";
var or="[object Uint8ClampedArray]";
var rr="[object Uint16Array]";
var ir="[object Uint32Array]";
var S={};
S[Yo]=S[$o]=S[Zo]=S[er]=S[tr]=S[nr]=S[or]=S[rr]=S[ir]=!0;
S[Do]=S[Ho]=S[Qo]=S[Bo]=S[Xo]=S[Uo]=S[jo]=S[qo]=S[Wo]=S[No]=S[Go]=S[Ko]=S[Vo]=S[Jo]=S[zo]=!1;
function sr(e){return iG(e)&&A(e.length)&&!!S[jse(e)]}
var vt=sr;
function ar(e){return function(t){return e(t)}}
var ion=ar;
var W={};
var yt=typeof W=="object"&&W&&!W.nodeType&&W;
var L=yt&&typeof _=="object"&&_&&!_.nodeType&&_;
var lr=L&&L.exports===yt;
var ne=lr&&lIr.process;
var dr=function(){try{var e=L&&L.require&&L.require("util").types;if(e)return e;return ne&&ne.binding&&ne.binding("util")}catch(t){}}();
var o1e=dr;
var bt=o1e&&o1e.isTypedArray;
var ur=bt?ion(bt):vt;
var aon=ur;
var pr=Object.prototype;
var fr=pr.hasOwnProperty;
function gr(e,t){var o=Nw(e),r=!o&&n8e(e),i=!o&&!r&&r1e(e),s=!o&&!r&&!i&&aon(e),a=o||r||i||s,l=a?ct(e.length,String):[],d=l.length;for(var p in e)if((t||fr.call(e,p))&&!(a&&(p=="length"||i&&(p=="offset"||p=="parent")||s&&(p=="buffer"||p=="byteLength"||p=="byteOffset")||r8e(p,d))))l.push(p);return l}
var Nxr=gr;
var hr=Object.prototype;
function mr(e){var t=e&&e.constructor,o=typeof t=="function"&&t.prototype||hr;return e===o}
var XHt=mr;
function Sr(e,t){return function(o){return e(t(o))}}
var Fxr=Sr;
var vr=Fxr(Object.keys,Object);
var kt=vr;
var yr=Object.prototype;
var br=yr.hasOwnProperty;
function kr(e){if(!XHt(e))return kt(e);var t=[];for(var o in Object(e))if(br.call(e,o)&&o!="constructor")t.push(o);return t}
var Ct=kr;
function Cr(e){return e!=null&&A(e.length)&&!xMt(e)}
var jbe=Cr;
function xr(e){return jbe(e)?Nxr(e):Ct(e)}
var gpe=xr;
function Rr(e){return Mxr(e,gpe,YHt)}
var MGn=Rr;
var Tr=1;
var Pr=Object.prototype;
var Ar=Pr.hasOwnProperty;
function Mr(e,t,o,r,i,s){var a=o&Tr,l=MGn(e),d=l.length,p=MGn(t),m=p.length;if(d!=m&&!a)return!1;var c=d;while(c--){var f=l[c];if(!(a?f in t:Ar.call(t,f)))return!1}var u=s.get(e),g=s.get(t);if(u&&g)return u==t&&g==e;var v=!0;s.set(e,t),s.set(t,e);var b=a;while(++c<d){f=l[c];var C=e[f],x=t[f];if(r)var $e=a?r(x,C,f,t,e,s):r(C,x,f,e,t,s);if(!($e===void 0?C===x||i(C,x,o,r,s):$e)){v=!1;break}b||(b=f=="constructor")}if(v&&!b){var I=e.constructor,F=t.constructor;if(I!=F&&(("constructor"in e)&&("constructor"in t))&&!(typeof I=="function"&&I instanceof I&&typeof F=="function"&&F instanceof F))v=!1}return s.delete(e),s.delete(t),v}
var xt=Mr;
var Or=Wse(iU,"Set");
var Lut=Or;
var R=jse;
var o8e=R;
var _r=1;
var wt="[object Arguments]";
var Lt="[object Array]";
var J="[object Object]";
var qr=Object.prototype;
var Ot=qr.hasOwnProperty;
function Wr(e,t,o,r,i,s){var a=Nw(e),l=Nw(t),d=a?Lt:o8e(e),p=l?Lt:o8e(t);d=d==wt?J:d,p=p==wt?J:p;var m=d==J,c=p==J,f=d==p;if(f&&r1e(e)){if(!r1e(t))return!1;a=!0,m=!1}if(f&&!m)return s||(s=new n1e),a||aon(e)?D(e,t,o,r,i,s):dt(e,t,d,o,r,i,s);if(!(o&_r)){var u=m&&Ot.call(e,"__wrapped__"),g=c&&Ot.call(t,"__wrapped__");if(u||g){var v=u?e.value():e,b=g?t.value():t;return s||(s=new n1e),i(v,b,o,r,s)}}if(!f)return!1;return s||(s=new n1e),xt(e,t,o,r,i,s)}
var It=Wr;
function Ft(e,t,o,r,i){if(e===t)return!0;if(e==null||t==null||!iG(e)&&!iG(t))return e!==e&&t!==t;return It(e,t,o,r,Ft,i)}
var M=Ft;
function Nr(e,t){return M(e,t)}
var Yn=Nr;
class q{#e;#t=new WeakMap;constructor(e){this.#e=e}of(e){let t=this.#t.get(e);if(t!==void 0)return t;let o=this.#e();return this.#t.set(e,o),o}}
function IP(e){return{subscribe(t){return e().subscribe(t)},emit(...t){e().emit(...t)},clear(){e().clear()}}}
class oe{walked=new Map;folderIsLink=new Map;managedMcp=null;clearWalk(){this.walked.clear(),this.folderIsLink.clear()}noteWalked(e,t){if(!this.walked.has(e))this.walked.set(e,t())}all(){return[...this.walked.values(),this.managedMcp].filter((e)=>e!==null)}}
class re{holders=0;tiers;baseline;lingering;tier(e){if(this.holders===0&&this.lingering!==void 0){let t=this.lingering();if(t===void 0)this.lingering=void 0;this.tiers=t}return this.tiers?.get(e)}engage(e,t){let o;if(this.holders===0){let i=e(),s=t?.()??i;if(this.lingering!==void 0)o={tiers:this.tiers,lingering:this.lingering},this.lingering=void 0;this.tiers=i,this.baseline=s}this.holders++;let r=!1;return{release:()=>{if(r)return{last:!1};if(r=!0,this.holders--,this.holders>0)return{last:!1};return this.tiers=void 0,this.lingering=void 0,this.baseline=void 0,{last:!0}},abandon:()=>{if(r)return;if(r=!0,this.holders--,this.holders===0)this.tiers=o?.tiers,this.lingering=o?.lingering,this.baseline=void 0}}}whileLive(e){let t=this.tiers,o=this.lingering;this.tiers=void 0,this.lingering=void 0;try{return e()}finally{this.tiers=t,this.lingering=o}}baselineTier(e){return this.baseline?.get(e)}differingTiers(e){return this.baseline===void 0?[]:Gr(this.baseline,e)}rebaseline(e,t){if(this.baseline===void 0)return;let o=new Map(this.baseline);for(let r of t){let i=e.get(r);if(i===void 0)o.delete(r);else o.set(r,i)}this.baseline=o}linger(e,t){if(this.holders===0)this.tiers=e,this.lingering=t,this.baseline=void 0}isHeld(){return this.holders>0}}
function JHt(e){return e!==null&&Object.keys(e).length===0?null:e}
function Et(e){return{...e,settings:JHt(e.settings),unfiltered:JHt(e.unfiltered),...e.legacy!==void 0&&{legacy:JHt(e.legacy)}}}
function Gr(e,t){return[...e.keys(),...[...t.keys()].filter((o)=>!e.has(o))].filter((o)=>{let r=e.get(o),i=t.get(o);return!Yn(r&&Et(r),i&&Et(i))})}
class s8e{mergedSettings=null;perSource=new Map;parsedFiles=new Map;managedDocumentLinks=new oe;folderListings=new Map;managedFileReads=new Map;primedFiles=new Set;policyWalks=0;walkedFolders=new Map;policy={};lastPolicyEnvComposition=null;isLoadingFromDisk=!1;autoModeUntrustedSourceWarned=!1;changed=Ie();internalWrites=new Map;enabledSources;inlineFlagParse;repoTierFreeze=new re;pluginBase;epoch=0;systemSpaceServingLogged=!1;systemAttestationContradicted=!1;backendReadResetTail=Promise.resolve();backgroundWorkTail=Promise.resolve();invalidated=Ie();pluginBaseLoaded=!1;primer;localStoreProbes=new Ht;retained=new Map;retainedListings=new Map;setPluginBase(e){this.pluginBase=e,this.pluginBaseLoaded=!0}clearPluginBase(){this.pluginBase=void 0}invalidateAll(e){if(this.epoch++,this.mergedSettings=null,this.perSource.clear(),this.parsedFiles.clear(),this.managedDocumentLinks.clearWalk(),this.primedFiles.clear(),this.policyWalks=0,this.walkedFolders.clear(),this.folderListings.clear(),this.policy={},e?.userLayer==="retain"&&j()&&this.primer!==void 0){for(let[t,o]of this.retained)this.parsedFiles.set(t,o.parsed),this.primedFiles.add(t);for(let[t,o]of this.retainedListings)this.folderListings.set(t,o.names)}else this.retained.clear(),this.retainedListings.clear();this.invalidated.emit()}invalidatePolicyLayer(){this.mergedSettings=null,this.perSource.delete("policySettings"),this.policy={}}onInvalidate(e){return this.invalidated.subscribe(e)}seedParsedFile(e,t,o,r){if(r!==this.epoch)return!1;let i=this.parsedFiles.get(e),s=i===void 0||!Yn(i,o);if(this.parsedFiles.set(e,o),s||this.primedFiles.has(e))this.primedFiles.add(e);let a=this.retained.get(e);if(a!==void 0)a.parsed=o;if(s)this.dropDerivedCaches(t);return!0}walkReadDiffers(e,t){let o=this.parsedFiles.get(e);return o!==void 0&&!this.primedFiles.has(e)&&!Yn(o,t)}unseedParsedFile(e,t,o){if(o!==this.epoch)return;if(this.retained.delete(e),!this.primedFiles.has(e)||t==="policySettings"&&this.policyWalks>0)return;this.primedFiles.delete(e),this.parsedFiles.delete(e),this.dropDerivedCaches(t)}dropDerivedCaches(e){if(this.perSource.delete(e),this.mergedSettings=null,e==="policySettings")this.policy={}}primedFolderListing(e){return j()?this.folderListings.get(e):void 0}folderListingForPolicyWalk(e){if(!j())return;this.policyWalks++;let t=this.folderListings.get(e);if(!this.walkedFolders.has(e)||t!==void 0)this.walkedFolders.set(e,t??null);return t}noteWalkListing(e,t){if(!j())return;this.walkedFolders.set(e,t)}get policyWalkCount(){return this.policyWalks}policyInstallVerdict(e,t,o){let r=this.parsedFiles.get(e);if(r===void 0||Yn(r,t))return"install";if(!this.primedFiles.has(e))return"raced";if(this.policyWalks===0)return"install";return this.policyWalks>o?"raced":"deferred"}folderInstallVerdict(e,t,o){if(!this.walkedFolders.has(e))return this.hasParsedDropInOutside(e,t)?"raced":"install";let r=this.walkedFolders.get(e);if(r!==null&&r!==void 0&&Yn(r,t))return"install";return this.policyWalks>o?"raced":"deferred"}seedFolderListing(e,t,o){if(o!==this.epoch)return!1;let r=this.folderListings.get(e);this.folderListings.set(e,t);let i=this.retainedListings.get(e);if(i!==void 0)i.names=t;if(r!==void 0&&!Yn(r,t))this.dropDerivedCaches("policySettings");return!0}walkReadManagedFileIn(e,t){for(let[o,r]of this.parsedFiles)if(!o.includes("\x00")&&!this.primedFiles.has(o)&&(r.settings!==null||r.errors.length>0)&&(o===e||Dt(o)===t))return!0;return!1}walkRead(e){return this.parsedFiles.has(e)&&!this.primedFiles.has(e)}hasParsedDropInOutside(e,t){for(let o of this.parsedFiles.keys())if(!o.includes("\x00")&&!this.primedFiles.has(o)&&Dt(o)===e&&!t.includes(Kr(o)))return!0;return!1}clearFolderListing(e,t){if(t!==this.epoch)return;if(this.retainedListings.delete(e),!this.folderListings.has(e)||this.walkedFolders.has(e))return;this.folderListings.delete(e),this.dropDerivedCaches("policySettings")}retainLayer(e,t){let o={parsed:t};return this.retained.set(e,o),()=>{if(this.retained.get(e)===o)this.retained.delete(e)}}dropRetainedLayer(e){this.retained.delete(e)}retainFolderListing(e,t){let o={names:t};return this.retainedListings.set(e,o),()=>{if(this.retainedListings.get(e)===o)this.retainedListings.delete(e)}}}
class Ht{ownerUidsByRoot=new Map;realHomeDir=void 0;canonicalRootOwnerUids(e,t){let o=this.ownerUidsByRoot.get(e);if(o!==void 0)return o;let r=t(e);return this.ownerUidsByRoot.set(e,r),r}hasCanonicalRootOwnerUids(e){return this.ownerUidsByRoot.has(e)}primeCanonicalRootOwnerUids(e,t){if(this.ownerUidsByRoot.has(e))return!1;return this.ownerUidsByRoot.set(e,t),!0}clearCanonicalRootOwnerUids(){this.ownerUidsByRoot.clear()}normalizedRealHomeDir(e){return this.realHomeDir??=e(),this.realHomeDir}clearNormalizedRealHomeDir(){this.realHomeDir=void 0}}
var $xr=new q(()=>new s8e);
function xZ(){return{sent:new Set,rejected:new Set,declaredTools:void 0,noChannelTools:void 0,declaredToolsDecided:!1,nameOnlyAnnouncements:new Set,surfacedOnWire:new Set,definedByValue:new Set,inlineToolsRefusal:{rejectedHeader:void 0,announcementsByName:new Set},advisorHeld:void 0,wireShapeSeed:void 0,webSearchHistoryRejected:!1,mediaItemCap:void 0,systemMessagesAcceptedBy:new Set,nameOnlyEntries:{held:new Set,refused:!1,reported:void 0}}}
function URo(e,t){for(let o of t.sent)yD(e,o);for(let o of t.rejected)Wm(e,o);e.wireShapeSeed=t}
function z(e){let t=xZ();if(t.declaredTools=e.declaredTools,t.noChannelTools=e.noChannelTools,t.declaredToolsDecided=e.declaredToolsDecided,t.advisorHeld=e.advisorHeld,e.wireShapeSeed!==void 0)URo(t,e.wireShapeSeed);let{rejectedHeader:o,announcementsByName:r}=e.inlineToolsRefusal;if(o!==void 0)BRo(t,o);return jRo(t,r),t.webSearchHistoryRejected=e.webSearchHistoryRejected,t.mediaItemCap=e.mediaItemCap,t.nameOnlyEntries.refused=e.nameOnlyEntries.refused,t.nameOnlyEntries.reported=e.nameOnlyEntries.reported,t}
function BRo(e,t){Wm(e,t),e.inlineToolsRefusal.rejectedHeader=t}
function jRo(e,t){for(let o of t)e.inlineToolsRefusal.announcementsByName.add(o),e.nameOnlyAnnouncements.add(o)}
function yD(e,t){if(!e.rejected.has(t))e.sent.add(t)}
function Wm(e,t){e.sent.delete(t),e.rejected.add(t)}
class ie{spawner=void 0;pairings=new Map;mainEnsureInFlight=!1;mainSlotBlocked=!1}
class se{#e=xZ();#t=void 0;#n;stickyBetas(){return this.#e}unlatchStickyBetas(){this.#e=z(this.#e)}atisLatch(){return this.#t}replaceAtisLatch(e){this.#t=e,this.#n?.(e)}watchAtisLatch(e){this.#n=e}reset(){this.#e=xZ(),this.#t=void 0}}
var Vr=function(){try{var e=Wse(Object,"defineProperty");return e({},"",{}),e}catch(t){}}();
var con=Vr;
function Jr(e,t,o){if(t=="__proto__"&&con)con(e,t,{configurable:!0,enumerable:!0,value:o,writable:!0});else e[t]=o}
var PZ=Jr;
function zr(e){return function(t,o,r){var i=-1,s=Object(t),a=r(t),l=a.length;while(l--){var d=a[e?l:++i];if(o(s[d],d,s)===!1)break}return t}}
var Bt=zr;
var Qr=Bt();
var Gxr=Qr;
function Xr(e,t){return e&&Gxr(e,t,gpe)}
var QHt=Xr;
var $r=1;
var Zr=2;
function ei(e,t,o,r){var i=o.length,s=i,a=!r;if(e==null)return!s;e=Object(e);while(i--){var l=o[i];if(a&&l[2]?l[1]!==e[l[0]]:!(l[0]in e))return!1}while(++i<s){l=o[i];var d=l[0],p=e[d],m=l[1];if(a&&l[2]){if(p===void 0&&!(d in e))return!1}else{var c=new n1e;if(r)var f=r(p,m,d,e,t,c);if(!(f===void 0?M(m,p,$r|Zr,r,c):f))return!1}}return!0}
var Ut=ei;
function ti(e){return e===e&&!eC(e)}
var Q=ti;
function ni(e){var t=gpe(e),o=t.length;while(o--){var r=t[o],i=e[r];t[o]=[r,i,Q(i)]}return t}
var jt=ni;
function oi(e,t){return function(o){if(o==null)return!1;return o[e]===t&&(t!==void 0||(e in Object(o)))}}
var X=oi;
function ri(e){var t=jt(e);if(t.length==1&&t[0][2])return X(t[0][0],t[0][1]);return function(o){return o===e||Ut(o,e,t)}}
var _t=ri;
var si="[object Symbol]";
function ai(e){return typeof e=="symbol"||iG(e)&&jse(e)==si}
var xse=ai;
var li=/\.|\[(?:[^[\]]*|(["'])(?:(?!\1)[^\\]|\\.)*?\1)\]/;
var di=/^\w*$/;
function ui(e,t){if(Nw(e))return!1;var o=typeof e;if(o=="number"||o=="symbol"||o=="boolean"||e==null||xse(e))return!0;return di.test(e)||!li.test(e)||t!=null&&e in Object(t)}
var w=ui;
var ci=500;
function pi(e){var t=Eo(e,function(r){if(o.size===ci)o.clear();return r}),o=t.cache;return t}
var qt=pi;
var fi=/[^.[\]]+|\[(?:(-?\d+(?:\.\d+)?)|(["'])((?:(?!\2)[^\\]|\\.)*?)\2)\]|(?=(?:\.|\[\])(?:\.|\[\]|$))/g;
var gi=/\\(\\)?/g;
var hi=qt(function(e){var t=[];if(e.charCodeAt(0)===46)t.push("");return e.replace(fi,function(o,r,i,s){t.push(i?s.replace(gi,"$1"):r||o)}),t});
var Wt=hi;
function mi(e,t){var o=-1,r=e==null?0:e.length,i=Array(r);while(++o<r)i[o]=t(e[o],o,e);return i}
var s1e=mi;
var Si=1/0;
var Nt=fG?fG.prototype:void 0;
var Gt=Nt?Nt.toString:void 0;
function Kt(e){if(typeof e=="string")return e;if(Nw(e))return s1e(e,Kt)+"";if(xse(e))return Gt?Gt.call(e):"";var t=e+"";return t=="0"&&1/e==-Si?"-0":t}
var Vt=Kt;
function vi(e){return e==null?"":Vt(e)}
var hpe=vi;
function yi(e,t){if(Nw(e))return e;return w(e,t)?[e]:Wt(hpe(e))}
var ype=yi;
var bi=1/0;
function ki(e){if(typeof e=="string"||xse(e))return e;var t=e+"";return t=="0"&&1/e==-bi?"-0":t}
var Pse=ki;
function Ci(e,t){t=ype(t,e);var o=0,r=t.length;while(e!=null&&o<r)e=e[Pse(t[o++])];return o&&o==r?e:void 0}
var i8e=Ci;
function xi(e,t,o){var r=e==null?void 0:i8e(e,t);return r===void 0?o:r}
var zxr=xi;
function Ri(e,t){return e!=null&&t in Object(e)}
var zt=Ri;
function Ti(e,t,o){t=ype(t,e);var r=-1,i=t.length,s=!1;while(++r<i){var a=Pse(t[r]);if(!(s=e!=null&&o(e,a)))break;e=e[a]}if(s||++r!=i)return s;return i=e==null?0:e.length,!!i&&A(i)&&r8e(a,i)&&(Nw(e)||n8e(e))}
var Qt=Ti;
function Pi(e,t){return e!=null&&Qt(e,t,zt)}
var Vxr=Pi;
var Ai=1;
var Mi=2;
function wi(e,t){if(w(e)&&Q(t))return X(Pse(e),t);return function(o){var r=zxr(o,e);return r===void 0&&r===t?Vxr(o,e):M(t,r,Ai|Mi)}}
var Xt=wi;
function Li(e){return e}
var a8e=Li;
function Oi(e){return function(t){return t==null?void 0:t[e]}}
var Yt=Oi;
function Ii(e){return function(t){return i8e(t,e)}}
var $t=Ii;
function Fi(e){return w(e)?Yt(Pse(e)):$t(e)}
var Zt=Fi;
function Ei(e){if(typeof e=="function")return e;if(e==null)return a8e;if(typeof e=="object")return Nw(e)?Xt(e[0],e[1]):_t(e);return Zt(e)}
var rj=Ei;
function Di(e,t){var o={};return t=rj(t,3),QHt(e,function(r,i,s){PZ(o,i,t(r,i,s))}),o}
var ro=Di;
function Hi(e,t){var o,r=-1,i=e.length;while(++r<i){var s=t(e[r]);if(s!==void 0)o=o===void 0?s:o+s}return o}
var en=Hi;
function Bi(e,t){return e&&e.length?en(e,rj(t,2)):0}
var Ise=Bi;
var tn={inputTokens:0,outputTokens:0,cacheReadInputTokens:0,cacheCreationInputTokens:0};
function Y(e){let t=Object.create(null);return Object.assign(t,e)}
class ae{#e=0;#t=0;#n=0;#o=0;#r=Date.now();#i=void 0;#a=0;#u=0;#l=0;#d=tn;#c=!1;#p=0;#s=Y();#f=null;#h=null;#g=null;totalCostUSD(){return this.#e}restoredCostUSD(){return this.#l}restoredTokens(){return this.#d}totalAPIDuration(){return this.#t}totalAPIDurationWithoutRetries(){return this.#n}totalToolDuration(){return this.#o}totalDuration(){return Math.max(0,Date.now()-this.#r)}sessionStartTime(){return this.#i??this.#r}totalLinesAdded(){return this.#a}totalLinesRemoved(){return this.#u}hasUnknownModelCost(){return this.#c}safetyStops(){return this.#p}modelUsage(){return this.#s}usageForModel(e){return this.#s[e]}totalInputTokens(){return Ise(Object.values(this.#s),"inputTokens")}totalOutputTokens(){return Ise(Object.values(this.#s),"outputTokens")}totalCacheReadInputTokens(){return Ise(Object.values(this.#s),"cacheReadInputTokens")}totalCacheCreationInputTokens(){return Ise(Object.values(this.#s),"cacheCreationInputTokens")}totalWebSearchRequests(){return Ise(Object.values(this.#s),"webSearchRequests")}recordApiDuration(e,t){this.#t+=e,this.#n+=t}recordCost(e,t,o){this.#s[o]=t,this.#e+=e}recordToolDuration(e){this.#o+=e}recordLinesChanged(e,t){this.#a+=e,this.#u+=t}markUnknownModelCost(){this.#c=!0}recordSafetyStop(){this.#p+=1}zeroDurationsAndCostForTests(){this.#t=0,this.#n=0,this.#e=0,this.#l=0}restartClock(){this.#r=Date.now(),this.anchorLogicalStart(void 0)}anchorLogicalStart(e){this.#i=e===void 0?void 0:Math.min(e,this.#r)}restore({totalCostUSD:e,totalAPIDuration:t,totalAPIDurationWithoutRetries:o,totalToolDuration:r,totalLinesAdded:i,totalLinesRemoved:s,lastDuration:a,startTime:l,modelUsage:d,hasUnknownModelCost:p},m){if(this.#g=m,this.#e=e,this.#l=e,this.#t=t,this.#n=o,this.#o=r,this.#a=i,this.#u=s,this.#c=p??!1,d)this.#s=Y(d);if(this.#d={inputTokens:this.totalInputTokens(),outputTokens:this.totalOutputTokens(),cacheReadInputTokens:this.totalCacheReadInputTokens(),cacheCreationInputTokens:this.totalCacheCreationInputTokens()},a!==void 0)this.#r=Date.now()-a;this.anchorLogicalStart(l)}registerSaver(e){this.#f=e}runSaver(e,t){(this.#f??(t?t.#f:null))?.(e)}registerTranscriptRecorder(e){this.#h=e}runTranscriptRecorder(e,t,o){(this.#h??(o?o.#h:null))?.(e,t)}snapshot(){return{ownerSessionId:this.#g,totalCostUSD:this.#e,totalAPIDuration:this.#t,totalAPIDurationWithoutRetries:this.#n,totalToolDuration:this.#o,startTime:this.#r,sessionLogicalStartTime:this.#i,totalLinesAdded:this.#a,totalLinesRemoved:this.#u,restoredCostUSD:this.#l,restoredTokens:this.#d,hasUnknownModelCost:this.#c,safetyStops:this.#p,modelUsage:ro(this.#s,(e)=>({...e}))}}restoreSnapshot(e){this.#g=e.ownerSessionId,this.#e=e.totalCostUSD,this.#t=e.totalAPIDuration,this.#n=e.totalAPIDurationWithoutRetries,this.#o=e.totalToolDuration,this.#r=e.startTime,this.#i=e.sessionLogicalStartTime,this.#a=e.totalLinesAdded,this.#u=e.totalLinesRemoved,this.#l=e.restoredCostUSD,this.#d=e.restoredTokens,this.#c=e.hasUnknownModelCost,this.#p=e.safetyStops,this.#s=Y(ro(e.modelUsage,(t)=>({...t})))}claim(e){this.#g??=e}scopeTo(e){this.#g=e}ownerSessionId(){return this.#g}belongsTo(e){return this.#g===e}reset(e){this.#g=e,this.#e=0,this.#l=0,this.#d=tn,this.#t=0,this.#n=0,this.#o=0,this.#r=Date.now(),this.anchorLogicalStart(void 0),this.#a=0,this.#u=0,this.#c=!1,this.#p=0,this.#s=Y()}}
class le{#e=new Set;#t=new Map;consentSessionFallback(e){return this.#e.has(e)}replaceConsentSessionFallback(e,t){if(t)this.#e.add(e);else this.#e.delete(e)}sessionConsentKey(e){return this.#t.get(e)??null}replaceSessionConsentKey(e,t){if(t===null)this.#t.delete(e);else this.#t.set(e,t)}reset(){this.#e.clear(),this.#t.clear()}}
class de{#e={registeredHooks:null};#t=void 0;#n=void 0;holder(){return this.#e}mainThreadAgentType(){return this.#t}replaceMainThreadAgentType(e){this.#t=e}mainThreadAgentHooks(){return this.#n}replaceMainThreadAgentHooks(e){this.#n=e}reset(){this.#e={registeredHooks:null},this.#t=void 0,this.#n=void 0}}
class ue{#e=new Map;skills(){return this.#e}lookup(e){return this.#e.get(e)}record(e,t){this.#e.set(e,t)}forget(e){this.#e.delete(e)}forgetAll(){this.#e.clear()}reset(){this.#e=new Map}}
class ce{#e=[];#t;#n;#o;#r;approvedServers(){return this.#e}approveServers(e,t){for(let o of t)if(!this.#e.some((r)=>r.name===o&&r.workspaceKey===e))this.#e.push({name:o,workspaceKey:e})}registerClientsAccessor(e){this.#t=e}acquireClientsAccessor(e){if(this.#t)return()=>{};return this.#t=e,()=>{if(this.#t===e)this.#t=void 0}}clientsFromAccessor(){return this.#t?.()}registerConnectedClientWiring(e){this.#n=e}connectedClientWiring(){return this.#n}registerToolsSwapper(e){this.#o=e}acquireToolsSwapper(e){if(this.#o)return()=>{};return this.#o=e,()=>{if(this.#o===e)this.#o=void 0}}swapServerTools(e,t,o){if(!this.#o)return!1;return this.#o(e,t,o)}registerConnections(e){this.#r=e}acquireConnections(e){if(this.#r)return()=>{};return this.#r=e,()=>{if(this.#r===e)this.#r=void 0}}connections(){return this.#r}reset(){this.#e=[],this.#t=void 0}}
class pe{#e=void 0;#t=void 0;#n=void 0;#o=void 0;#r=void 0;#i=void 0;#a=!1;#u=!1;#l=void 0;#d=!1;#c=void 0;#p=void 0;#s=void 0;mainLoopModelOverride(){return this.#e}overrideMainLoopModel(e){this.#e=e}mainLoopEffortState(){return this.#t}replaceMainLoopEffortState(e){this.#t=e}initialMainLoopModel(){return this.#n}replaceInitialMainLoopModel(e){this.#n=e}initialModelSettingLayer(){return this.#o}replaceInitialModelSettingLayer(e){this.#o=e}resolvedOrgDefault(){return this.#r}replaceResolvedOrgDefault(e){this.#r=e}initialEnvDefaultModel(){return this.#i}replaceInitialEnvDefaultModel(e){this.#i=e}refusalFallbackOccurred(){return this.#a}markRefusalFallbackOccurred(e){this.#a=!0,this.#l??=e}refusalFallbackHeaderArmed(){return this.#u}armRefusalFallbackHeader(e){this.#u=!0,this.#l??=e}refusalFallbackLatchOriginRequestId(){return this.#l}silentLaneServerArmed(){return this.#d}armSilentLaneFromServer(){this.#d=!0}forgetRefusalFallbackOccurred(){this.#a=!1,this.#u=!1,this.#d=!1,this.#l=void 0}refusalFallbackModelLatch(){return this.#c}replaceRefusalFallbackModelLatch(e,t="superseded"){if(this.#c?.carriedEffort!==void 0&&e?.carriedEffort===void 0)this.#p=t;this.#c=e}unlatchRefusalFallbackModel(e){this.replaceRefusalFallbackModelLatch(void 0,e)}takeCarriedEffortEnd(){let e=this.#p;return this.#p=void 0,e}availabilityFallbackChain(){return this.#s}replaceAvailabilityFallbackChain(e){this.#s=e}reset(){this.#e=void 0,this.#t=void 0,this.#n=void 0,this.#o=void 0,this.#r=void 0,this.#i=void 0,this.#a=!1,this.#u=!1,this.#d=!1,this.#l=void 0,this.#c=void 0,this.#p=void 0,this.#s=void 0}}
class fe{#e=new Map;#t=0;#n=void 0;sections(){return this.#e}recordSection(e,t){this.#e.set(e,t)}forgetAllSections(){this.#e.clear()}noteInvalidation(e="other"){this.#t+=1,this.#n=e}epoch(){return this.#t}lastInvalidationReason(){return this.#n}reset(){this.#e=new Map}}
class ge{#e=null;#t=null;#n=null;#o=null;#r=null;#i=0;#a=null;#u=void 0;#l=0;#d=null;#c=null;#p=null;#s=null;#f=null;#h=!1;#g=null;#v=null;#S=!1;lastAPIRequest(){return this.#e}replaceLastAPIRequest(e){this.#e=e}lastCancelledAPIMessageId(){return this.#t}replaceLastCancelledAPIMessageId(e){this.#t=e}lastAPIRequestMessages(){return this.#n}replaceLastAPIRequestMessages(e){this.#n=e}lastClassifierRequests(){return this.#o}replaceLastClassifierRequests(e){this.#o=e}promptId(){return this.#r}replacePromptId(e){this.#r=e}promptIndex(){return this.#i}replacePromptIndex(e){this.#i=e}incrementPromptIndex(){return this.#i++,this.#i}lastPersonPrompt(){return this.#a}replaceLastPersonPrompt(e){this.#a=e}lastMainRequestId(){return this.#u}replaceLastMainRequestId(e){if(this.#u=e,e===void 0)this.#l+=1}mainChainReleaseCount(){return this.#l}lastMainThreadCacheTtlMs(){return this.#d}replaceLastMainThreadCacheTtlMs(e,t,o=!1){this.#d=e,this.#s=Date.now(),this.#p={ttlMs:e,model:t,requestAt:this.#s,askedAt:o?this.#p?.askedAt??this.#s:this.#s},this.#f=this.#s,this.#h=!0}isLastMainThreadAskLive(e,t){let o=this.#p;return o!==null&&o.requestAt===this.#s&&o.ttlMs===e&&o.model===t&&Date.now()-o.askedAt<e}adjustLastMainThreadCacheTtlMs(e){this.#d=e}recordLastMainThreadCacheRenewal(e){if(this.#s!==null)this.#c={requestAt:this.#s,at:Math.max(e,this.lastMainThreadCacheRenewedAt()??-1/0)}}lastMainThreadCacheRenewedAt(){let e=this.#c;return e!==null&&e.requestAt===this.#s?e.at:null}lastLiveMainThreadRequestAt(){return this.#f}lastMainThreadRequestAt(){return this.#s}mainThreadRequestedInProcess(){return this.#h}recordMainThreadTurnStart(){this.#h=!0}replaceLastMainThreadRequest(e,t){this.#s=e,this.#d=t}lastMainThreadContextTokens(){return this.#g}replaceLastMainThreadContextTokens(e){this.#g=e}stageResumeSeed(e){this.#m=e}applyResumeSeed(e){this.#m=null,this.#d=e.ttlMs,this.#s=e.requestAt,this.#g=e.contextTokens}#m=null;clearLastMainThreadRequest(e){let t=e!==void 0&&this.#m?.sessionId===e?this.#m:null;this.#m=null,this.#d=t?.ttlMs??null,this.#s=t?.requestAt??null,this.#g=t?.contextTokens??null,this.#f=null,this.#h=!1}lastApiCompletionTimestamp(){return this.#v}replaceLastApiCompletionTimestamp(e){this.#v=e}pendingPostCompaction(){return this.#S}replacePendingPostCompaction(e){this.#S=e}reset(){this.#e=null,this.#t=null,this.#n=null,this.#o=null,this.#r=null,this.#i=0,this.#a=null,this.#u=void 0,this.#l+=1,this.#d=null,this.#s=null,this.#f=null,this.#h=!1,this.#g=null,this.#m=null,this.#v=null,this.#S=!1}}
class he{#e=[];#t=Object.create(null);#n=null;#o=0;#r=!1;#i=0;changed=Ie();pendingLoopWakeup=Ie();tasks(){return this.#e}schedule(e){this.#e.push(e)}replaceTasks(e){this.#e=e}chainStartedAt(e){return this.#t[e]}recordChainStart(e,t){this.#t[e]=t}forgetChainStart(e){delete this.#t[e]}tickInFlightPrompt(){return this.#n}replaceTickInFlightPrompt(e){this.#n=e}consecutiveKeepalives(){return this.#o}replaceConsecutiveKeepalives(e){this.#o=e}ended(){return this.#r}replaceEnded(e){this.#r=e}wakeFires(){return this.#i}recordWakeFire(){this.#i++}resetWakeFires(){this.#i=0}reset(){this.#e=[],this.#t=Object.create(null),this.#n=null,this.#o=0,this.#r=!1,this.#i=0}}
class me{#e=!1;#t=!1;#n=!1;#o=!1;#r=!1;#i=!1;#a=!1;#u=!1;#l=!1;#d=!1;#c=void 0;#p=!1;#s=void 0;#f=!1;#h=!1;#g=null;#v=null;#S=null;#m=!1;#C=null;#b=!1;#y=!1;#R=!1;#k=!1;#T=!1;#x=void 0;#A=void 0;#M=void 0;#P=null;#w=null;onboardingShownThisSession(){return this.#e}replaceOnboardingShownThisSession(e){this.#e=e}lspRecommendationShownThisSession(){return this.#t}replaceLspRecommendationShownThisSession(e){this.#t=e}sessionTrustAccepted(){return this.#n}replaceSessionTrustAccepted(e){this.#n=e}homeTrustDialogAccepted(){return this.#o}replaceHomeTrustDialogAccepted(e){this.#o=e}hasExitedPlanMode(){return this.#r}replaceHasExitedPlanMode(e){this.#r=e}needsPlanModeExitAttachment(){return this.#i}replaceNeedsPlanModeExitAttachment(e){this.#i=e}needsAutoModeExitAttachment(){return this.#a}replaceNeedsAutoModeExitAttachment(e){this.#a=e}memoryToggledOff(){return this.#u}replaceMemoryToggledOff(e){this.#u=e}autoMemoryOffThisSession(){return this.#l}replaceAutoMemoryOffThisSession(e){this.#l=e}accountMemorySessionMode(){return this.#c}replaceAccountMemorySessionMode(e,t=!1){this.#c=e,this.#p=t&&e!==void 0}accountMemorySessionModeRestored(){return this.#p}accountMemoryLaunchMode(){return this.#s}replaceAccountMemoryLaunchMode(e){this.#s=e}restoredMemoryPause(){return this.#f}replaceRestoredMemoryPause(e){this.#f=e}restoredAutoMemoryOff(){return this.#d}replaceRestoredAutoMemoryOff(e){this.#d=e}teardownUnwindRequested(){return this.#m}replaceTeardownUnwindRequested(e){this.#m=e}backgroundAutoModeSetupInFlight(){return this.#h}replaceBackgroundAutoModeSetupInFlight(e){this.#h=e}deferredToolStubGateLatch(){return this.#g}replaceDeferredToolStubGateLatch(e){this.#g=e}verifySkillRolloutGateLatch(){return this.#v}replaceVerifySkillRolloutGateLatch(e){this.#v=e}commitSkillRolloutGateLatch(){return this.#S}replaceCommitSkillRolloutGateLatch(e){this.#S=e}memoryToolsShapeLatch(){return this.#C}replaceMemoryToolsShapeLatch(e){this.#C=e}proposeGoalAvailabilityLogged(){return this.#b}markProposeGoalAvailabilityLogged(){this.#b=!0}activeRoutine(){return this.#A}replaceActiveRoutine(e){this.#A=e}inheritedTeamName(){return this.#M}replaceInheritedTeamName(e){this.#M=e}teleportedSessionInfo(){return this.#P}replaceTeleportedSessionInfo(e){this.#P=e}markFirstTeleportMessageLogged(){if(this.#P)this.#P.hasLoggedFirstMessage=!0}cachedClaudeMdContent(){return this.#w}replaceCachedClaudeMdContent(e){this.#w=e}accountSkillsSyncEnabled(){return this.#y}replaceAccountSkillsSyncEnabled(e){this.#y=e}skillsSyncVetoed(){return this.#R}replaceSkillsSyncVetoed(e){this.#R=e}accountPluginsSyncEnabled(){return this.#k}replaceAccountPluginsSyncEnabled(e){this.#k=e}pluginsSyncVetoed(){return this.#T}replacePluginsSyncVetoed(e){this.#T=e}armPendingContextCompacted(e){this.#x=e}consumePendingContextCompacted(){let e=this.#x;return this.#x=void 0,e}forgetPendingContextCompacted(){this.#x=void 0}reset(){this.#y=!1,this.#R=!1,this.#k=!1,this.#T=!1,this.#e=!1,this.#t=!1,this.#n=!1,this.#o=!1,this.#r=!1,this.#i=!1,this.#a=!1,this.#u=!1,this.#l=!1,this.#c=void 0,this.#p=!1,this.#s=void 0,this.#f=!1,this.#d=!1,this.#m=!1,this.#h=!1,this.#g=null,this.#v=null,this.#S=null,this.#C=null,this.#b=!1,this.#A=void 0,this.#M=void 0,this.#P=null,this.#w=null,this.#x=void 0}}
function nn(){return{messagesArriving:0,lastInLine:Promise.resolve(),gaveUpOn:void 0,besideLine:0,lastBesideLine:Promise.resolve(),gaveUpOnBeside:void 0,pathsArrivingBeside:new Map,toolCallsWaiting:0,pathsArriving:new Map}}
class Se{#e=new Map;#t=new Map;#n=null;#o=new Map;#r=new Set;#i=new Set;#a=new Set;#u=new Set;#l=new Set;#d=new Set;#c=new Set;#p=new Set;#s=new Set;#f=new Set;#h=new Map;#g=new Map;#v=new Map;#S;#m={byFileUuid:new Map,recentPrefetchStarts:[]};#C=nn();#b=new Set;#y=void 0;#R=void 0;#k=void 0;#T=void 0;#x=void 0;planSlugCache(){return this.#e}forgetPlanSlug(e){this.#e.delete(e)}pendingBranchLinks(){return this.#t}replacePendingBranchLinks(){this.#t=new Map}vimSharedState(){return this.#n}replaceVimSharedState(e){this.#n=e}agentColorMap(){return this.#o}sessionCreatedTeams(){return this.#r}sessionSpawnedTeammatePanes(){return this.#i}surfacedHookSpawnFailures(){return this.#a}bareMcpServerMatchersWarned(){return this.#u}pendingConversationEditKinds(){return this.#l}clientTruncatedAssistantIds(){return this.#d}heldStatelessReplyIds(){return this.#c}unsupportedThreadKeys(){return this.#p}pendingPrLinks(){return this.#s}policyPredicateTelemetryEmitted(){return this.#f}humanAttachmentDigests(){return this.#h}inboundAttachmentFiles(){return this.#g}filesPlacedByName(){return this.#v}filesPlacedByNameRead(){return this.#S}setFilesPlacedByNameRead(e){this.#S=e}inboundAttachmentDownloads(){return this.#m}arrivingInboundFiles(){return this.#C}chromeAvailabilityStagesLogged(){return this.#b}replaceChromeAvailabilityStagesLogged(e){if(this.#b=new Set,e)this.#y=Date.now()}chromeAvailabilityAnchorMs(){return this.#y}pendingGoalIdleCheckin(){return this.#R}replacePendingGoalIdleCheckin(e){this.#R=e}pendingGoalReprompt(){return this.#k}replacePendingGoalReprompt(e){this.#k=e}goalInterruptionStreak(){return this.#T}replaceGoalInterruptionStreak(e){this.#T=e}workerCheckin(){return this.#x}replaceWorkerCheckin(e){this.#x=e}reset(){this.#e=new Map,this.#t=new Map,this.#n=null,this.#o=new Map,this.#r=new Set,this.#i=new Set,this.#a=new Set,this.#u=new Set,this.#l=new Set,this.#d=new Set,this.#c=new Set,this.#p=new Set,this.#s=new Set,this.#f=new Set,this.#h=new Map,this.#g=new Map,this.#v=new Map,this.#S=void 0;for(let e of this.#m.byFileUuid.values())clearTimeout(e.expiry),e.abort.abort();this.#m={byFileUuid:new Map,recentPrefetchStarts:[]},this.#C=nn(),this.#b=new Set,this.#y=void 0,clearTimeout(this.#R),this.#R=void 0,clearTimeout(this.#k),this.#k=void 0,this.#T=void 0,clearInterval(this.#x?.timer),this.#x=void 0}}
var on={renderTarget:"ink",workspace:"local",canDrive:!0,transcriptSource:"local-jsonl",remote:null};
class ye{#e=!1;#t=void 0;#n=void 0;#o=void 0;#r=void 0;#i=void 0;#a=void 0;#u=null;#l=!1;#d=void 0;#c=on;#p=!1;#s="idle";attacherCapsChanged=Ie();rvSupervisorLinkChanged=Ie();sdkDialogHostActive(){return this.#e}markSdkDialogHostActive(e){this.#e=e}sdkSupportedDialogKinds(){return this.#t}sdkSupportedDialogKindsSource(){return this.#n}declareDialogKinds(e,t){this.#t=e,this.#n=e===void 0?void 0:t}sdkPerTaskStopAffordance(){return this.#o}declarePerTaskStopAffordance(e){this.#o=e}sdkRapidFollowupPreempt(){return this.#r}declareRapidFollowupPreempt(e){this.#r=e}sdkForwardNestedSubagentToolCalls(){return this.#i}declareForwardNestedSubagentToolCalls(e){this.#i=e}hostRendersChromeSetupOffer(){return this.#a}declareHostRendersChromeSetupOffer(e){this.#a=e}attacherCaps(){return this.#u}replaceAttacherCaps(e){this.#u=e,this.attacherCapsChanged.emit()}rvSupervisorLinkLive(){return this.#l}replaceRvSupervisorLinkLive(e){if(this.#l===e)return;this.#l=e,this.rvSupervisorLinkChanged.emit()}sdkBetas(){return this.#d}replaceSdkBetas(e){this.#d=e}caps(){return this.#c}replaceCaps(e){this.#c=e}markRemote(e){this.#c={...this.#c,workspace:e?"remote":"local"}}replBridgeActive(){return this.#p}replaceReplBridgeActive(e){if(this.#p===e)return;this.#p=e}mainLoopBusy(){return this.#s!=="idle"}mainQueryRunning(){return this.#s==="running"}replaceMainLoopStatus(e){this.#s=e}reset(){this.#e=!1,this.#t=void 0,this.#n=void 0,this.#o=void 0,this.#r=void 0,this.#i=void 0,this.#a=void 0,this.#u=null,this.#d=void 0,this.#c=on,this.#p=!1,this.#s="idle",this.#l=!1,this.attacherCapsChanged.clear(),this.rvSupervisorLinkChanged.clear()}}
var rn=150;
class be{#e=Date.now();#t=!1;#n=0;interactionFired=Ie();#o=void 0;terminalFocusFired=Ie();#r=!1;#i;lastInteractionTime(){return this.#e}lastSubmitTime(){return this.#n}recordSubmit(){this.#n=Date.now()}recordInteraction(e){if(e)this.#a();else this.#t=!0}flushIfDirty(){if(this.#t)this.#a()}#a(){this.#e=Date.now(),this.#t=!1,this.interactionFired.emit()}resetBaseline(){this.#e=Date.now(),this.#t=!1}terminalFocus(){return this.#o}updateTerminalFocus(e){this.#o=e,this.terminalFocusFired.emit()}scrollDraining(){return this.#r}markScrollActivity(){if(this.#r=!0,this.#i)clearTimeout(this.#i);this.#i=setTimeout(()=>{this.#r=!1,this.#i=void 0},rn),this.#i.unref?.()}async waitForScrollIdle(){while(this.#r)await new Promise((e)=>setTimeout(e,rn))}reset(){this.#e=Date.now(),this.#n=0,this.#o=void 0,this.interactionFired.clear(),this.terminalFocusFired.clear()}}
var Ui=["sdk_single_prompt_gate","subagent_estimate","subagent_final_turn","rows_unreadable"];
class ke{byAgent=new Map;attemptsByAgent=new Map;cappedFailuresByAgent=new Map;armGateEventEmitted=new Set;vetoedUnderPrompt=new Map;sidecarIo=Promise.resolve();rehydrateAttemptedSessions=new Set;sidecarReadsAhead=new Map;get(e){return this.byAgent.get(e)}has(e){return this.byAgent.has(e)}put(e,t){this.byAgent.set(e,t)}remove(e){this.byAgent.delete(e)}nextAttemptNumber(e){let t=(this.attemptsByAgent.get(e)??0)+1;return this.attemptsByAgent.set(e,t),t}consecutiveCountedFailures(e){return this.cappedFailuresByAgent.get(e)??0}recordCountedFailure(e){let t=(this.cappedFailuresByAgent.get(e)??0)+1;return this.cappedFailuresByAgent.set(e,t),t}clearCountedFailures(e){this.cappedFailuresByAgent.delete(e)}recordVeto(e,t){this.vetoedUnderPrompt.set(e,t)}isVetoedUnder(e,t){return this.vetoedUnderPrompt.get(e)===t}latchArmGateEvent(e){if(this.armGateEventEmitted.has(e))return!1;return this.armGateEventEmitted.add(e),!0}enqueueSidecarIo(e,t){return this.sidecarIo=this.sidecarIo.then(e,e).catch(t),this.sidecarIo}sidecarIoSettled(){return this.sidecarIo}hasAttemptedRehydrate(e){return this.rehydrateAttemptedSessions.has(e)}markRehydrateAttempted(e){this.rehydrateAttemptedSessions.add(e)}keepSidecarReadAhead(e,t){this.sidecarReadsAhead.delete(e);while(this.sidecarReadsAhead.size>=8){let o=this.sidecarReadsAhead.keys().next().value;if(o===void 0)break;this.sidecarReadsAhead.delete(o)}this.sidecarReadsAhead.set(e,t)}takeSidecarReadAhead(e){let t=this.sidecarReadsAhead.get(e);return this.sidecarReadsAhead.delete(e),t}dropSidecarReadAhead(e){this.sidecarReadsAhead.delete(e)}forgetSubagentTelemetry(e){this.attemptsByAgent.delete(e),this.cappedFailuresByAgent.delete(e),this.vetoedUnderPrompt.delete(e);for(let t of Ui)this.armGateEventEmitted.delete(`${e}:${t}`)}}
class Ce{#e=void 0;get current(){return this.#e}adopt(e){this.#e=e}}
class xe{#e=null;#t=null;mainAgentId(e){return this.#e??=bo(e),this.#e}projectDir(){return this.#t}replaceProjectDir(e){this.#t=e}}
class Re{openToolUseId;answeredThisSession=!1;get answered(){return this.answeredThisSession}markAnswered(){this.answeredThisSession=!0}isOpenElsewhere(e){return this.openToolUseId!==void 0&&this.openToolUseId!==e}open(e){this.openToolUseId=e}closeFor(e){if(e!==void 0&&this.openToolUseId===e)this.openToolUseId=void 0}}
class Te{granted=!1;isGranted(){return this.granted}grant(){this.granted=!0}}
class Pe{inFlight=null;startedForTurnEnd=!1;reset(){this.inFlight?.abort(),this.inFlight=null,this.startedForTurnEnd=!1}}
class Ae{firstSyncPromise=null;accountListing="pending";syncErrors=[];lastRoundListed=!1;syncedLaneOpened=!1;removalsDeferredHere=new Set;pendingTrashRemovals=[];ensureBucketRoot=null;deferredRelist=null;missingEntry={resync:null,resyncAt:-1/0,swept:!1};settingsInstall={current:null,next:null,loaded:null}}
class Me{abortController=null;eligibleTurns=0;unusedInARow=0;askOnNextTurn(){this.eligibleTurns=0}reset(){this.abortController?.abort(),this.abortController=null}arm(){let e=new AbortController;return this.abortController=e,e}}
class Le{#e=void 0;#t=void 0;ccrSessionID(){return this.#e}latchCcrSessionID(e){this.#e=e}syncEnabled(){return this.#t}latchSyncEnabled(e){return this.#t=e,e}}
class Oe{autonomousPreambleDelivered=!1;lastLoopFileDelivered=null;reset(){this.autonomousPreambleDelivered=!1,this.lastLoopFileDelivered=null}}
class Fe{exchanges=[];replace(e){this.exchanges=e}append(e,t,o){this.exchanges=[...this.exchanges,{question:e,response:t,...o&&{fallbackNotice:o}}].slice(-20)}pendingReopen=null;reopenAway=!1;get reopenPending(){return this.pendingReopen!==null}armReopen(e,t){if(this.pendingReopen){this.pendingReopen.owners.add(e);return}this.pendingReopen={owners:new Set([e]),cancel:t()}}clearPendingReopen(e){let t=this.pendingReopen;if(!t){this.reopenAway=!1;return}if(e!==void 0){if(t.owners.delete(e),t.owners.size>0)return}t.cancel(),this.pendingReopen=null,this.reopenAway=!1}reopener=null;setReopener(e){this.reopener=e}get hasReopener(){return this.reopener!==null}reopenNow(){return this.reopener?.()??!1}inFlightQuestions=[];get inFlight(){return this.inFlightQuestions.at(-1)??null}pendingBesidesTop(){return this.inFlightQuestions.slice(0,-1)}setInFlight(e){if(!this.inFlightQuestions.includes(e))this.inFlightQuestions=[...this.inFlightQuestions,e]}clearInFlight(e){if(this.inFlightQuestions.includes(e))this.inFlightQuestions=this.inFlightQuestions.filter((t)=>t!==e)}}
var _i=16;
class Ee{pending=null;shownThisSession=!1;changed=Ie();submitted=[];submittedChanged=Ie();subscribe=this.changed.subscribe;getSnapshot=()=>this.pending;subscribeSubmitted=this.submittedChanged.subscribe;submit(e){if(this.shownThisSession)return;if(this.submitted.length>=_i)this.submitted.shift();this.submitted.push(e),this.submittedChanged.emit()}takeSubmitted(){let e=this.submitted;return this.submitted=[],e}offer(e){if(this.shownThisSession)return;this.pending=e,this.changed.emit()}dismiss(){if(this.pending!==null)this.pending=null,this.changed.emit()}markShown(){this.shownThisSession=!0}}
class FGn{lastEmittedAt=new Map;shouldEmit(e,t,o){let r=this.lastEmittedAt.get(e)||0;if(t-r<o)return!1;if(this.lastEmittedAt.size>=100){let i=this.lastEmittedAt.keys().next().value;if(i!==void 0)this.lastEmittedAt.delete(i)}return this.lastEmittedAt.set(e,t),!0}}
var c8e=Symbol("permission-stash-evicted");
class Be{entriesCap;evictedKeysCap;lanes={write:new Map,read:new Map};evicted={write:new Set,read:new Set};poisoned={write:!1,read:!1};identities=new Map;identitiesEvicted=new Set;identitiesPoisoned=!1;constructor(e=256,t=1048576){this.entriesCap=e;this.evictedKeysCap=t}stash(e,t,o,r="write"){if(e===void 0)return;let i=this.lanes[r],s=this.evicted[r],a=He(e,t);if(s.has(a))return;let l=i.get(a);if(l!==void 0){let d=new Set(o);i.set(a,l.filter((p)=>d.has(p)));return}if(i.size>=this.entriesCap){let d=i.keys().next().value;if(d!==void 0){if(i.delete(d),s.add(d),s.size>this.evictedKeysCap){this.poisoned[r]=!0;let p=s.values().next().value;if(p!==void 0)s.delete(p)}}}i.set(a,o)}holds(e,t,o="write"){return e!==void 0&&this.lanes[o].has(He(e,t))}stashIdentity(e,t,o){if(e===void 0)return;let r=this.identities.get(e),i=r?.get(t);if(r!==void 0&&i!==void 0){if(i===null||o===null||i.dev!==o.dev||i.ino!==o.ino)r.set(t,null);return}let s=this.identitiesEvicted.has(e)||this.identitiesPoisoned;if(r===void 0){if(!s&&this.identities.size>=this.entriesCap){let a=this.identities.keys().next().value;if(a!==void 0){if(this.identities.delete(a),this.identitiesEvicted.add(a),this.identitiesEvicted.size>this.evictedKeysCap){this.identitiesPoisoned=!0;let l=this.identitiesEvicted.values().next().value;if(l!==void 0)this.identitiesEvicted.delete(l)}}}r=new Map,this.identities.set(e,r)}r.set(t,s?null:o)}takeIdentity(e,t){if(e===void 0)return;let o=this.identities.get(e),r=o?.get(t);if(o?.delete(t)&&o.size===0)this.identities.delete(e);return r}consume(e,t,o="write"){if(e===void 0)return;let r=this.lanes[o],i=He(e,t),s=r.get(i),a=s===void 0&&(this.evicted[o].delete(i)||this.poisoned[o]),l=`${e}\x00`;for(let d of r.keys())if(d.startsWith(l))r.delete(d);if(o==="read")this.identities.delete(e),this.identitiesEvicted.delete(e);if(a)return c8e;return s}}
function He(e,t){return`${e}\x00${t}`}
var an="NoProjectDirectoryError";
class OP extends Error{name=an;constructor(){super("This session has no working directory.")}}
function _o(e){let{project:t}=e;if(t===null)throw new OP;return t}
function Yxr(e){return ln({kind:"root",host:e.host,id:e.id,parentId:e.parentId},e.project,e.project?.isolationRoot)}
function O(e,t){let o=t.originalCwd??e?.originalCwd,r=t.projectRoot??e?.projectRoot,i=t.cwd??e?.cwd;if(o===void 0&&r===void 0&&i===void 0)return null;if(o===void 0||r===void 0||i===void 0)throw new OP;return{originalCwd:Ln(o),projectRoot:Ln(r),cwd:Ln(i)}}
function ln(e,t,o){let r={project:t===null?null:O(null,t)},i=o,s=Ie(),a=e.kind==="fork"?e.root.observers:new ie,l=e.kind==="fork"?e.root.autonomousLoopPreamble:new Oe,d=e.kind==="fork"?e.root.precompute:new ke,p={get originalCwd(){return _o(r).originalCwd},get projectRoot(){return _o(r).projectRoot},get cwd(){return _o(r).cwd},get isolationRoot(){return i}},m={host:e.kind==="fork"?e.root.host:e.host,get id(){return e.kind==="fork"?e.root.id:e.id},get parentId(){return e.kind==="fork"?e.root.parentId:e.parentId},get root(){return e.kind==="fork"?e.root:m},get project(){return r.project===null?null:p},observers:a,autonomousLoopPreamble:l,btwHistory:e.kind==="fork"?e.root.btwHistory:new Fe,ccrRecap:e.kind==="fork"?e.root.ccrRecap:new Pe,conversationLatches:e.kind==="fork"?e.root.conversationLatches:new se,costLedger:e.kind==="fork"?e.root.costLedger:new ae,engineStore:e.kind==="fork"?e.root.engineStore:new Ce,usageCreditsOnlyModelConsentSlots:e.kind==="fork"?e.root.usageCreditsOnlyModelConsentSlots:new le,hookRegistry:e.kind==="fork"?e.root.hookRegistry:new de,identity:e.kind==="fork"?e.root.identity:new xe,invokedSkills:e.kind==="fork"?e.root.invokedSkills:new ue,mcpSessionWiring:e.kind==="fork"?e.root.mcpSessionWiring:new ce,modelSelection:e.kind==="fork"?e.root.modelSelection:new pe,promptAssembly:e.kind==="fork"?e.root.promptAssembly:new fe,promptSuggestion:e.kind==="fork"?e.root.promptSuggestion:new Me,pendingHint:e.kind==="fork"?e.root.pendingHint:new Ee,pluginsSync:e.kind==="fork"?e.root.pluginsSync:new Ae,precompute:d,requestJournal:e.kind==="fork"?e.root.requestJournal:new ge,sessionCron:e.kind==="fork"?e.root.sessionCron:new he,sessionFlags:e.kind==="fork"?e.root.sessionFlags:new me,sessionRefsGate:e.kind==="fork"?e.root.sessionRefsGate:new Le,sessionScratch:e.kind==="fork"?e.root.sessionScratch:new Se,surfaceCapabilities:e.kind==="fork"?e.root.surfaceCapabilities:new ye,toolProgressThrottle:e.kind==="fork"?e.root.toolProgressThrottle:new FGn,userPresence:e.kind==="fork"?e.root.userPresence:new be,workflowUsageConsent:e.kind==="fork"?e.root.workflowUsageConsent:new Te,outsideReadPrompt:e.kind==="fork"?e.root.outsideReadPrompt:new Re,writePermissionStash:e.kind==="fork"?e.root.writePermissionStash:new Be,subscribe(c){let f=s.subscribe(c);if(e.kind!=="fork")return f;let u=e.root,{id:g,parentId:v}=u,b=u.subscribe(()=>{if(u.id===g&&u.parentId===v)return;g=u.id,v=u.parentId,c()});return()=>{f(),b()}},setCwd(c){m.update({project:{cwd:c}})},setIsolationRoot(c){if(c!==i)i=c,s.emit()},withProject(c){return ln({kind:"fork",root:m.root},O(r.project,c),c.isolationRoot??i)},update(c){let f=c.project===void 0?r.project:c.project===null?null:O(r.project,c.project),u=!1;if(c.id!==void 0||"parentId"in c){if(e.kind==="fork")throw Error("A withProject fork cannot re-identify the session \u2014 update the root session instead");if(c.id!==void 0&&c.id!==e.id)e.id=c.id,u=!0;if("parentId"in c&&c.parentId!==e.parentId)e.parentId=c.parentId,u=!0}if(f?.originalCwd!==r.project?.originalCwd||f?.projectRoot!==r.project?.projectRoot||f?.cwd!==r.project?.cwd)r.project=f,u=!0;let g=c.project?.isolationRoot;if(g!==void 0&&g!==i)i=g,u=!0;if(u)s.emit()}};return m}
class pt{#e;#t=new WeakMap;constructor(e){this.#e=e}peek(e){return this.#t.get(e.root)}of(e){let t=e.root,o=this.#t.get(t);if(o!==void 0)return o;let r=this.#e();return this.#t.set(t,r),r}drop(e){this.#t.delete(e.root)}}
class Ue{#e=!1;#t=null;#n=!1;#o=void 0;#r=!1;#i=!1;#a=!1;#u=!1;#l=void 0;#d=!1;#c=!1;#p=null;#s=null;#f=new Map;#h="cli";#g="fresh";#v=void 0;#S=!1;#m=!1;#C=[];#b=!1;#y=!1;#R=!1;#k=new Set;#T=!1;#x="none";#A=!1;#M=!1;#P=!1;#w=!1;#L=!1;#I=void 0;#F=void 0;#E=void 0;#D=!1;#H=!1;#B=!1;#U=null;#j=!0;#_=null;#O=!1;#z=Ie();#q=null;#W=!1;#N=!1;#G={};#K=null;#V="none";#J=void 0;isInteractive(){return this.#e}replaceIsInteractive(e){this.#e=e}printOutputFormat(){return this.#t}replacePrintOutputFormat(e){this.#t=e}thinkingDisplayExplicit(){return this.#n}replaceThinkingDisplayExplicit(e){this.#n=e}permissionPromptToolName(){return this.#o}replacePermissionPromptToolName(e){this.#o=e}hasStreamingInput(){return this.#r}replaceHasStreamingInput(e){this.#r=e}singleShotPrintSession(){return this.#i}replaceSingleShotPrintSession(e){this.#i=e}printInputClosed(){return this.#a}markPrintInputClosed(){this.#a=!0}modelOverrideOptOutForSession(){return this.#u}replaceModelOverrideOptOutForSession(e){this.#u=e}rendererMode(){return this.#l}replaceRendererMode(e){this.#l=e}strictToolResultPairing(){return this.#d}replaceStrictToolResultPairing(e){this.#d=e}restrictedSession(){return this.#c}replaceRestrictedSession(e){this.#c=e}workspaceRestrictionState(){return this.#p}replaceWorkspaceRestrictionState(e){this.#p=e}replaceWorkspaceRestrictionUnsubscribe(e){this.#s?.(),this.#s=e}restrictedWorkspaceRoots(){return this.#f}addRestrictedWorkspaceRoot(e,t){this.#f.set(e,t)}clearRestrictedWorkspaceRoots(){this.#f=new Map}clientType(){return this.#h}replaceClientType(e){this.#h=e}sessionStartType(){return this.#g}replaceSessionStartType(e){this.#g=e}questionPreviewFormat(){return this.#v}replaceQuestionPreviewFormat(e){this.#v=e}extendedQuestionsEnabled(){return this.#S}replaceExtendedQuestionsEnabled(e){this.#S=e}questionOptionDescriptionsOptional(){return this.#m}replaceQuestionOptionDescriptionsOptional(e){this.#m=e}replConfigArgv(){return this.#C}replaceReplConfigArgv(e){this.#C=e}userMsgOptIn(){return this.#b}replaceUserMsgOptIn(e){this.#b=e}searchToolsOptIn(){return this.#y}replaceSearchToolsOptIn(e){this.#y=e}todoToolsOptIn(){return this.#R}replaceTodoToolsOptIn(e){this.#R=e}toolsOmittedByLaunchList(){return this.#k}replaceToolsOmittedByLaunchList(e){this.#k=e}wizardOperatorToolsEnabled(){return this.#T}replaceWizardOperatorToolsEnabled(e){this.#T=e}pollEventIngress(){return this.#x}markPollEventIngress(e){this.#x=e}ownEventDeliveryRequested(){return this.#A&&!this.#M}replaceOwnEventDeliveryHeld(e){this.#M=e}markOwnEventDeliveryRequested(){this.#A=!0}sdkAgentProgressSummariesEnabled(){return this.#P}replaceSdkAgentProgressSummariesEnabled(e){this.#P=e}sessionPersistenceDisabled(){return this.#w}replaceSessionPersistenceDisabled(e){this.#w=e}diskless(){return this.#L}replaceDiskless(e){this.#L=e}ccrSessionUrl(){return this.#I}replaceCcrSessionUrl(e){this.#I=e}ccrDerivedSurface(){return this.#F}replaceCcrDerivedSurface(e){this.#F=e}toolsMachine(){return this.#E}replaceToolsMachine(e){this.#E=e}sessionBypassPermissionsMode(){return this.#D}replaceSessionBypassPermissionsMode(e){this.#D=e}disableSlashCommands(){return this.#H}replaceDisableSlashCommands(e){this.#H=e}disableFileMentions(){return this.#B}replaceDisableFileMentions(e){this.#B=e}sessionUploadsDir(){return this.#U}replaceSessionUploadsDir(e){this.#U=e}mayForwardHomeSettings(){return this.#j}replaceMayForwardHomeSettings(e){this.#j=e}homeSettingsHostConsent(){return this.#_}replaceHomeSettingsHostConsent(e){this.#_=e}scheduledTasksEnabled(){return this.#O}replaceScheduledTasksEnabled(e){let t=e&&!this.#O;if(this.#O=e,t)this.#z.emit()}onScheduledTasksEnabled(e){return this.#z.subscribe(e)}initJsonSchema(){return this.#q}replaceInitJsonSchema(e){this.#q=e}cliSessionConfigCarried(){return this.#W}replaceCliSessionConfigCarried(e){this.#W=e}forkRestrictedLaunchConfig(){return this.#N}replaceForkRestrictedLaunchConfig(e){this.#N=e}forkReplayLaunchConfig(){return this.#G}spareClaimState(){return this.#V}replaceSpareClaimState(e){this.#V=e}spareParkedAtMs(){return this.#J}replaceSpareParkedAtMs(e){this.#J=e}replaceForkReplayLaunchConfig(e){this.#G=e}projectConfigRoot(){return this.#K}replaceProjectConfigRoot(e){this.#K=e}reset(){this.#e=!1,this.#t=null,this.#V="none",this.#J=void 0,this.#n=!1,this.#o=void 0,this.#r=!1,this.#i=!1,this.#a=!1,this.#u=!1,this.#l=void 0,this.#d=!1,this.#c=!1,this.#p=null,this.replaceWorkspaceRestrictionUnsubscribe(null),this.#f=new Map,this.#h="cli",this.#g="fresh",this.#v=void 0,this.#S=!1,this.#m=!1,this.#C=[],this.#b=!1,this.#y=!1,this.#R=!1,this.#k=new Set,this.#T=!1,this.#x="none",this.#A=!1,this.#M=!1,this.#P=!1,this.#w=!1,this.#L=!1,this.#I=void 0,this.#F=void 0,this.#E=void 0,this.#D=!1,this.#H=!1,this.#B=!1,this.#U=null,this.#j=!0,this.#_=null,this.#O=!1,this.#q=null,this.#W=!1,this.#N=!1,this.#G={},this.#K=null}}
class je{#e=void 0;#t=[];#n=void 0;#o=void 0;#r=null;#i=null;#a=!1;#u=dn();#l=null;#d=!1;flagSettingsPath(){return this.#e}replaceFlagSettingsPath(e){this.#e=e}flagAgentsFilePaths(){return this.#t}replaceFlagAgentsFilePaths(e){this.#t=e}flagSettingsExpectedContent(){return this.#n}replaceFlagSettingsExpectedContent(e){this.#n=e}flagSettingsFilePinnedContent(){return this.#o}replaceFlagSettingsFilePinnedContent(e){this.#o=e}flagSettingsInline(){return this.#r}replaceFlagSettingsInline(e){this.#r=e}parentManagedSettings(){return this.#i}replaceParentManagedSettings(e){this.#i=e}parentManagedSettingsInvalid(){return this.#a}replaceParentManagedSettingsInvalid(e){this.#a=e}allowedSettingSources(){return this.#u}replaceAllowedSettingSources(e){this.#u=e}allowedSettingSourcesWithoutRepoDirSources(){let e=this.#u;if(this.#l?.of!==e)this.#l={of:e,result:e.filter((t)=>t!=="projectSettings"&&t!=="localSettings")};return this.#l.result}useCoworkPlugins(){return this.#d}replaceUseCoworkPlugins(e){this.#d=e}reset(){this.#e=void 0,this.#t=[],this.#n=void 0,this.#o=void 0,this.#r=null,this.#i=null,this.#a=!1,this.#u=dn(),this.#l=null,this.#d=!1}}
function dn(){return["userSettings","projectSettings","localSettings","flagSettings","policySettings"]}
class _e{#e=[];#t=[];#n=[];#o=void 0;#r=new Map;#i=new Map;#a=[];#u=[];#l=!1;#d=void 0;#c=void 0;#p=void 0;inlinePlugins(){return this.#e}replaceInlinePlugins(e){this.#e=e}inlinePluginsNoMcp(){return this.#t}replaceInlinePluginsNoMcp(e){this.#t=e}inlinePluginUrls(){return this.#n}replaceInlinePluginUrls(e){this.#n=e}syncedPluginDirs(){return this.#o??[]}syncedPluginDirsRegistered(){return this.#o!==void 0}syncedPluginAttestation(e){return this.#r.get(e)}replaceSyncedPluginDirs(e,t=new Map){this.#o=e,this.#r=t,this.#i=this.#s(this.#i)}clearSyncedPluginDirs(){this.#o=void 0,this.#r=new Map,this.#i=new Map}attestedOfficialPluginMarketplace(e){return this.#i.get(e)}replaceAttestedOfficialPluginIds(e){this.#i=this.#s(e)}#s(e){let t=new Set([...this.#r.values()].map((o)=>`${o.pluginName}@${o.marketplace}`));return new Map([...e].filter(([o,r])=>t.has(`${o.slice(0,o.lastIndexOf("@"))}@${r}`)))}additionalDirectoriesForClaudeMd(){return this.#a}replaceAdditionalDirectoriesForClaudeMd(e){this.#a=e}allowedChannels(){return this.#u}replaceAllowedChannels(e){this.#u=e}hasDevChannels(){return this.#l}replaceHasDevChannels(e){this.#l=e}sessionSkillAllowlist(){return this.#d}replaceSessionSkillAllowlist(e){this.#d=e}chromeFlagOverride(){return this.#c}replaceChromeFlagOverride(e){this.#c=e}teammateAgentId(){return this.#p}replaceTeammateAgentId(e){this.#p=e}reset(){this.#e=[],this.#t=[],this.#n=[],this.clearSyncedPluginDirs(),this.#a=[],this.#u=[],this.#l=!1,this.#d=void 0,this.#c=void 0,this.#p=void 0}}
class qe{#e=null;#t=null;modelStrings(){return this.#e}replaceModelStrings(e){this.#e=e}invalidate(){this.#e=null}admin3PSteeringSnapshot(){return this.#t}recordAdmin3PSteeringSnapshot(e){this.#t=e}reset(){this.#e=null,this.#t=null}}
var un=[];
class We{#e=[];#t=[];#n=void 0;errorLog(){return this.#e}recordError(e){if(this.#e.length>=100)this.#e.shift();this.#e.push(e)}recordSlowOperation(e,t){return}slowOperations(){if(this.#t.length===0)return un;let e=Date.now();if(this.#t.some((t)=>e-t.timestamp>=1e4)){if(this.#t=this.#t.filter((t)=>e-t.timestamp<1e4),this.#t.length===0)return un}return this.#t}recordDevBarAlert(e){return}devBarAlert(){let e=this.#n;if(e&&Date.now()-e.timestamp>=60000){this.#n=void 0;return}return e}reset(){this.#e=[],this.#t=[],this.#n=void 0}}
function cn(){return{rateTokens:null,rateLastRefillMs:null,featureOkLogged:!1,reportedDropReasons:new Set}}
class Ne{#e=null;#t=null;#n=null;#o=null;#r=null;#i=null;#a=null;#u=null;#l=null;#d=null;#c=null;#p=null;#s=null;#f=[];#h=null;#g=cn();#v=null;#S=null;#m=null;#C={direct:null,proxied:null};#b=null;#y=null;installMeter(e,t,{omitUnits:o=!1}={}){this.#e=e;let r=(i)=>o?void 0:i;this.#t=t("claude_code.session.count",{description:"Count of CLI sessions started"}),this.#n=t("claude_code.lines_of_code.count",{description:"Count of lines of code modified, with the 'type' attribute indicating whether lines were added or removed and the 'model' attribute indicating which model made the change"}),this.#o=t("claude_code.pull_request.count",{description:"Number of pull requests created"}),this.#r=t("claude_code.commit.count",{description:"Number of git commits created"}),this.#i=t("claude_code.cost.usage",{description:"Cost of the Claude Code session",unit:r("USD")}),this.#a=t("claude_code.token.usage",{description:"Number of tokens used",unit:r("tokens")}),this.#u=t("claude_code.code_edit_tool.decision",{description:"Count of code editing tool permission decisions (accept/reject) for Edit, Write, and NotebookEdit tools"}),this.#l=t("claude_code.active_time.total",{description:"Total active time in seconds",unit:r("s")})}meter(){return this.#e}sessionCounter(){return this.#t}locCounter(){return this.#n}prCounter(){return this.#o}commitCounter(){return this.#r}costCounter(){return this.#i}tokenCounter(){return this.#a}codeEditToolDecisionCounter(){return this.#u}activeTimeCounter(){return this.#l}statsStore(){return this.#d}replaceStatsStore(e){this.#d=e}loggerProvider(){return this.#c}replaceLoggerProvider(e){this.#c=e}eventLogger(){return this.#p}eventLoggerOwner(){return this.#s}attachEventLogger(e,t){if(this.#p=e,this.#s=e?t:null,!e)return;let o=this.#f;if(this.#f=null,o)for(let r of o)e.emit(r)}bufferPendingEvent(e){if(this.#f===null||this.#f.length>=100)return!1;return this.#f.push(e),!0}closeWindow(e){this.#f=null,this.#h=e}windowCloseCause(){return this.#h}isWindowOpen(){return this.#f!==null}hostOtel(){return this.#g}meterProvider(){return this.#v}replaceMeterProvider(e){this.#v=e}tracerProvider(){return this.#S}replaceTracerProvider(e){this.#S=e}cachedTelemetryResource(){return this.#m}replaceCachedTelemetryResource(e){this.#m=e}cachedOtlpHttpAgentFactory(e){return this.#C[e?"proxied":"direct"]}replaceCachedOtlpHttpAgentFactory(e,t){this.#C[e?"proxied":"direct"]=t}inClusterOtlpTrustRoots(){return this.#b}replaceInClusterOtlpTrustRoots(e){this.#b=e}inClusterOtlpAgentFactory(){return this.#y}replaceInClusterOtlpAgentFactory(e){this.#y=e}reset(){this.#e=null,this.#t=null,this.#n=null,this.#o=null,this.#r=null,this.#i=null,this.#a=null,this.#u=null,this.#l=null,this.#d=null,this.#c=null,this.#p=null,this.#s=null,this.#f=[],this.#h=null,this.#g=cn(),this.#v=null,this.#S=null,this.#m=null,this.#C={direct:null,proxied:null},this.#b=null,this.#y=null}}
class Ge{#e=void 0;#t=void 0;#n=!1;#o=void 0;#r=void 0;#i=new Set;#a=void 0;#u=null;#l=null;#d=0;#c=!1;#p=()=>!1;#s=null;#f=void 0;#h=null;#g=null;sessionIngressToken(){return this.#e}replaceSessionIngressToken(e){this.#e=e}oauthTokenFromFd(){return this.#t}replaceOauthTokenFromFd(e){this.#t=e}oauthTokenFromBgSnapshot(){return this.#n}replaceOauthTokenFromBgSnapshot(e){this.#n=e}oauthScopesFromFd(){return this.#o}replaceOauthScopesFromFd(e){this.#o=e}apiKeyFromFd(){return this.#r}replaceApiKeyFromFd(e){this.#r=e}descriptorAnnouncementConsumed(e){return this.#i.has(e)}markDescriptorAnnouncementConsumed(e){this.#i.add(e)}gatewayTokenFromDescriptor(){return this.#a}replaceGatewayTokenFromDescriptor(e){this.#a=e}resetFdCredentialState(){this.#e=void 0,this.#t=void 0,this.#n=!1,this.#o=void 0,this.#r=void 0}gatewayAuth(){return this.#u}replaceGatewayAuth(e){this.#u=e}gatewayServerProcess(){return this.#c}replaceGatewayServerProcess(e){this.#c=e}gatewayRequiredByHostPolicy(){return this.#p()}replaceGatewayRequiredByHostPolicyReader(e){this.#p=e}authenticatedAccount(){return this.#l}authenticatedAccountEpoch(){return this.#d}stampAuthenticatedAccount(e){let t=this.#l;if(e!==null&&t!==null&&e.accountUuid===t.accountUuid&&e.emailAddress===t.emailAddress&&e.organizationUuid===t.organizationUuid)return;this.#d+=1,this.#l=e}startupPolicySnapshot(){return this.#f}replaceStartupPolicySnapshot(e){this.#f=e}gatewayRefreshInFlight(){return this.#s}replaceGatewayRefreshInFlight(e){this.#s=e}sdkOAuthTokenRefreshCallback(){return this.#h}replaceSdkOAuthTokenRefreshCallback(e){this.#h=e}hostAuthTokenRefreshCallback(){return this.#g}replaceHostAuthTokenRefreshCallback(e){this.#g=e}resetForTests(){this.#e=null,this.#t=null,this.#n=!1,this.#o=void 0,this.#r=null,this.#i.clear(),this.#a=void 0,this.#u=null,this.#l=null,this.#d=0,this.#c=!1,this.#p=()=>!1,this.#s=null,this.#f=void 0,this.#h=null,this.#g=null}}
class Ke{#e=void 0;#t=!1;#n=!1;#o;directConnectServerUrl(){return this.#e}replaceDirectConnectServerUrl(e){this.#e=e}connectNonBlocking(){return this.#t}replaceConnectNonBlocking(e){this.#t=e}strictConfig(){return this.#n}replaceStrictConfig(e){this.#n=e}registerEnsureConnectedClient(e){this.#o=e}ensureConnectedClient(){return this.#o}reset(){this.#e=void 0,this.#t=!1,this.#n=!1}}
class Ve{#e=null;#t=new Map;#n=new Map;#o=new Set;#r=new Set;#i=new Set;#a=new Map;#u=!1;#l=!1;#d=!1;#c=!1;#p=!1;#s=!1;#f=!1;#h=!1;#g=!1;#v=new Set;#S=!1;#m=new Set;#C=!1;#b=!1;#y=!1;#R=!1;#k=new Map;#T=void 0;#x=void 0;#A=!1;#M=void 0;#P=void 0;#w=new Map;#L=new Map;promptCache1hAllowlist(){return this.#e}replacePromptCache1hAllowlist(e){this.#e=e}thinkingTypeOverrides(){return this.#t}recordThinkingTypeOverride(e,t){this.#t.set(e,t)}servedModelsByRequestedModel(){return this.#n}recordServedModels(e,t){this.#n.set(e,t)}effortUnsupportedModels(){return this.#o}markEffortUnsupported(e){this.#o.add(e)}structuredOutputsUnsupportedModels(){return this.#r}markStructuredOutputsUnsupported(e){this.#r.add(e)}clearStructuredOutputsUnsupported(){this.#r=new Set}context1mRefusedModels(){return this.#i}markContext1mRefused(e){this.#i.add(e)}clearContext1mRefused(){this.#i=new Set}refusedBetas(e){return this.#a.get(e)??[]}markBetaRefused(e,t){let o=this.#a.get(e);if(o)o.add(t);else this.#a.set(e,new Set([t]))}midConvCachePromotionRejected(){return this.#u}markMidConvCachePromotionRejected(){this.#u=!0}perTurnEffortOkEmitted(){return this.#l}markPerTurnEffortOkEmitted(){this.#l=!0}lateToolAdditionsOkEmitted(){return this.#d}markLateToolAdditionsOkEmitted(){this.#d=!0}toolResultScreeningOkEmitted(){return this.#c}markToolResultScreeningOkEmitted(){this.#c=!0}toolChangeHeaderRefused(){return this.#p}markToolChangeHeaderRefused(){this.#p=!0}promptEndMarkRefused(){return this.#s}markPromptEndMarkRefused(){this.#s=!0}inlineToolsRefused(){return this.#f}markInlineToolsRefused(){this.#f=!0}inlineToolsOkEmitted(){return this.#h}markInlineToolsOkEmitted(){this.#h=!0}thinkingResumptionRefused(){return this.#g}markThinkingResumptionRefused(){this.#g=!0}refusedBetaHeaders(){return this.#v}markBetaHeaderRefused(e){this.#v.add(e)}thinkingHighlightsRefused(){return this.#S}markThinkingHighlightsRefused(){this.#S=!0}toolChangeUnsupportedModels(){return this.#m}markToolChangeUnsupportedModel(e){this.#m.add(e)}internalModelFieldsRefused(){return this.#C}markInternalModelFieldsRefused(){this.#C=!0}internalModelFieldsRefusalAnnounced(){return this.#b}markInternalModelFieldsRefusalAnnounced(){this.#b=!0}thirdPartyServerClassifierRefused(){return this.#y}markThirdPartyServerClassifierRefused(){this.#y=!0}threadsHeldForServerClassifier(){return this.#R}markThreadsHeldForServerClassifier(){this.#R=!0}autoModeUnavailableResponse(e){return this.#k.get(e)}autoModeUnavailableResponsesInARow(e){let t=0;for(let o of this.#k.values())if(o.runKey===e)t++;return t}noteAutoModeUnavailableResponse(e,t){this.#k.set(e,t)}clearAutoModeUnavailableResponses(e){for(let[t,o]of this.#k)if(o.runKey===e)this.#k.delete(t)}setAutoModeUnavailableStopNotice(e){this.#T=e}autoModeUnavailableStopNotice(){return this.#T}setLocalCommandExitCode(e){this.#x=e}localCommandExitCode(){return this.#x}observedGateway(){return this.#M}serverClassifierFallbackChoice(){return this.#P}setServerClassifierFallbackChoice(e){this.#P=e}clearServerClassifierFallbackChoice(){this.#P=void 0}noteObservedGateway(e){this.#M??=e}takeServerClassifierFallbackAnnouncement(){if(!this.#y||this.#A)return!1;return this.#A=!0,!0}inferenceProfileBackingModels(){return this.#w}recordInferenceProfileBackingModel(e,t){this.#w.set(e,t)}foundryDeploymentCapabilities(){return this.#L}reset(){this.#e=null,this.#t=new Map,this.#n=new Map,this.#o=new Set,this.#r=new Set,this.#i=new Set,this.#a=new Map,this.#u=!1,this.#l=!1,this.#d=!1,this.#c=!1,this.#p=!1,this.#s=!1,this.#f=!1,this.#h=!1,this.#g=!1,this.#v=new Set,this.#S=!1,this.#m=new Set,this.#C=!1,this.#b=!1,this.#y=!1,this.#R=!1,this.#k.clear(),this.#T=void 0,this.#x=void 0,this.#A=!1,this.#M=void 0,this.#P=void 0,this.#w=new Map,this.#L=new Map}}
class Je{#e=!1;#t=!1;longContext1mCreditsBlocked(){return this.#e}replaceLongContext1mCreditsBlocked(e){this.#e=e}usageCreditsRequired(){return this.#t}replaceUsageCreditsRequired(e){this.#t=e}reset(){this.#e=!1,this.#t=!1}}
class ze{#e=!1;#t=!1;#n=pn;#o=Ie();selectorGate(){return this.#e}selectorGateEverOn(){return this.#t}replaceSelectorGate(e){this.#e=e,this.#t||=e}inheritSelectorGateEverOn(){this.#t=!0}resampleSelectorGate(e){let t=this.#e;if(this.#e=e,this.#t||=e,t!==e)this.#o.emit(e)}subscribeSelectorGateChanged(e){return this.#o.subscribe(e)}replaceHostGateSubscription(e){let t=this.#n;this.#n=e,t()}dropHostGateSubscription(){this.replaceHostGateSubscription(pn)}}
function pn(){}
class Qe{started=!1;stagingReaped=!1;claim(){if(this.started)return!1;return this.started=!0,!0}claimStagingReap(){if(this.stagingReaped)return!1;return this.stagingReaped=!0,!0}}
class Xe{clock;backgroundHousekeeping;launchOptions;settingsSource;extensionsConfig;modelStringsCache;diagnostics;telemetryHandles;credentialSlots;mcpProcessWiring;requestLatches;accountCreditLatches;proactivity;constructor(e){this.clock=e.clock,this.backgroundHousekeeping=e.backgroundHousekeeping,this.launchOptions=e.launchOptions,this.settingsSource=e.settingsSource,this.extensionsConfig=e.extensionsConfig,this.modelStringsCache=e.modelStringsCache,this.diagnostics=e.diagnostics,this.telemetryHandles=e.telemetryHandles,this.credentialSlots=e.credentialSlots,this.mcpProcessWiring=e.mcpProcessWiring,this.requestLatches=e.requestLatches,this.accountCreditLatches=e.accountCreditLatches,this.proactivity=e.proactivity}}
function fn(){return new Xe({clock:new bIr,backgroundHousekeeping:new Qe,launchOptions:new Ue,settingsSource:new je,extensionsConfig:new _e,modelStringsCache:new qe,diagnostics:new We,telemetryHandles:new Ne,credentialSlots:new Ge,mcpProcessWiring:new Ke,requestLatches:new Ve,accountCreditLatches:new Je,proactivity:new ze})}
function Wi(e=BOo()){return Yxr({host:fn(),id:HGn()??Bbe(),project:{originalCwd:e,projectRoot:e,cwd:e}})}
var y=Wi(czn);
function B(){return n()}
function n(){return k()?.session??y}
function ys(e){return e.of(y.host)}
function h(){let e=k();return e?.session?void 0:e}
var k=()=>{return};
function K(){return h()?.sessionId??n().id}
function _pe(){let e=h();return e?e.sessionProjectDir:n().identity.projectDir()}
function we(){return h()?.originalCwd??_o(n()).originalCwd}
function ql(){if(h()?.originalCwd===void 0&&n().project===null)return null;return we()}
function cr(){return h()?.projectRoot??_o(n()).projectRoot}
function UR(){return _o(n()).cwd}
function Yr(){return n().host.launchOptions.projectConfigRoot()}
function ve(){return!n().host.launchOptions.isInteractive()}
function kp(){return n().host.launchOptions.isInteractive()}
function Mon(){return n().host.launchOptions.workspaceRestrictionState()?.restriction??null}
function bT(){return Mon()!==null}
function PO(){return n().host.settingsSource.flagSettingsPath()}
function vpe(){return n().host.settingsSource.flagAgentsFilePaths()}
function Qv(){return n().host.settingsSource.flagSettingsExpectedContent()}
function HZ(){return n().host.settingsSource.flagSettingsFilePinnedContent()}
function ay(){return n().host.settingsSource.flagSettingsInline()}
function Yut(){return n().host.settingsSource.parentManagedSettings()}
function PV(){return n().host.credentialSlots.oauthTokenFromFd()}
function IN(e){n().host.credentialSlots.replaceOauthTokenFromFd(e)}
function IV(){return n().host.credentialSlots.oauthTokenFromBgSnapshot()}
function gMt(e){n().host.credentialSlots.replaceOauthTokenFromBgSnapshot(e)}
function ZPo(){return n().host.credentialSlots.oauthScopesFromFd()}
function hMt(e){n().host.credentialSlots.replaceOauthScopesFromFd(e)}
function EPr(){return n().host.credentialSlots.apiKeyFromFd()}
function vPr(e){n().host.credentialSlots.replaceApiKeyFromFd(e)}
function eIo(e){return n().host.credentialSlots.descriptorAnnouncementConsumed(e)}
function Xut(e){n().host.credentialSlots.markDescriptorAnnouncementConsumed(e)}
function VGn(e){n().host.credentialSlots.replaceGatewayTokenFromDescriptor(e)}
function So(){return n().host.credentialSlots.gatewayAuth()}
function Lon(){return n().host.credentialSlots.gatewayServerProcess()}
function Non(){return n().host.credentialSlots.gatewayRequiredByHostPolicy()}
function sIo(){return n().host.credentialSlots.authenticatedAccount()}
function qbe(e){n().host.credentialSlots.stampAuthenticatedAccount(e)}
function kpe(){return n().host.credentialSlots.authenticatedAccountEpoch()}
function m1e(){let{settingsSource:e,launchOptions:t}=n().host;return t.workspaceRestrictionState()?.restriction?e.allowedSettingSourcesWithoutRepoDirSources():e.allowedSettingSources()}
function Uon(){return ve()&&n().host.launchOptions.clientType()!=="claude-vscode"}
function Von(){return n().host.settingsSource.useCoworkPlugins()}
function Ybe(){return n().sessionFlags.sessionTrustAccepted()}
function De(){return ept()}
function ept(){return n().host.launchOptions.diskless()}
function Cb(){return y.host.launchOptions.diskless()}
function S1e(){return n().host.launchOptions.ccrDerivedSurface()}
function IO(){return n().surfaceCapabilities.caps()}
function jn(){return n().surfaceCapabilities.caps().workspace==="remote"}
function id(){return n().host.extensionsConfig.additionalDirectoriesForClaudeMd()}
export { pt as pt };
export { _pe as _pe };
export { ql as ql };
export { we as we };
export { K as K };
export { B as B };
export { Ise as Ise };
export { q as q };
export { ys as ys };
export { De as De };
export { UR as UR };
export { vpe as vpe };
export { cr as cr };
export { Yr as Yr };
export { id as id };
export { OP as OP };
export { ve as ve };
export { Yn as Yn };
export { Ybe as Ybe };
export { Uon as Uon };
export { ZPo as ZPo };
export { IV as IV };
export { qbe as qbe };
export { PV as PV };
export { sIo as sIo };
export { kpe as kpe };
export { S1e as S1e };
export { jn as jn };
export { IO as IO };
export { m1e as m1e };
export { Yut as Yut };
export { ay as ay };
export { PO as PO };
export { Qv as Qv };
export { HZ as HZ };
export { Von as Von };
export { kp as kp };
export { bT as bT };
export { s8e as s8e };
export { JHt as JHt };
export { con as con };
export { a8e as a8e };
export { Nw as Nw };
export { n8e as n8e };
export { KHt as KHt };
export { s1e as s1e };
export { ype as ype };
export { PZ as PZ };
export { o8e as o8e };
export { r1e as r1e };
export { XHt as XHt };
export { YHt as YHt };
export { gpe as gpe };
export { oon as oon };
export { n1e as n1e };
export { o1e as o1e };
export { ion as ion };
export { iG as iG };
export { MGn as MGn };
export { Pse as Pse };
export { i8e as i8e };
export { Vxr as Vxr };
export { $xr as $xr };
export { IP as IP };
export { Cb as Cb };
export { So as So };
export { Lon as Lon };
export { Non as Non };
export { Xut as Xut };
export { EPr as EPr };
export { vPr as vPr };
export { VGn as VGn };
export { IN as IN };
export { gMt as gMt };
export { hMt as hMt };
export { eIo as eIo };
export { ept as ept };
export { Mxr as Mxr };
export { jbe as jbe };
export { Nxr as Nxr };
export { Lxr as Lxr };
export { Fxr as Fxr };
export { rj as rj };
export { r8e as r8e };
export { Lut as Lut };
export { son as son };
export { Hxr as Hxr };
export { Oxr as Oxr };
export { ro as ro };
export { Gxr as Gxr };
export { aon as aon };
