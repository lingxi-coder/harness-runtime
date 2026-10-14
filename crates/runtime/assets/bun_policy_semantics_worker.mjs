import { createInterface } from "node:readline";
import { isIPv4 as xu, isIPv6 as Sr } from "net";

// Fixed source: Claude Code 2.1.295, src_187673145.js SHA
// 6fbf3e921382e88e71bd834f2034ee24a15cada553c00e95181e44fa033d22aa.
// Rdt: local 119280; XAr: 158410; JQ: 73743.
// Fixed source: src_186231195.js SHA
// 125f57566b99b67ce958d77cd787163124654cbbb3bdd6f3a92dfd38955a2317.
// kV: local 5703; df: local 6434.
// Fixed source: imported Zod chunk src_186578912.js SHA
// 8886b2923ae669a979e16ab15d7a8151571d85efcd7471e0ba226d4e2d53559f.
// ZodURL check: local 18473; Gs: 18113; qs: 18272.

const PROTOCOL_VERSION = 4;
const WORKER_ID = "native-policy-js-semantics-2.1.295-v4";

function requireUnits(value, label) {
  if (!Array.isArray(value)) throw new TypeError(label + " must be a UTF-16 code-unit array");
  for (const unit of value) {
    if (!Number.isInteger(unit) || unit < 0 || unit > 0xffff) {
      throw new TypeError(label + " contains an invalid UTF-16 code unit");
    }
  }
  return value;
}

function fromUnits(units) {
  const chunks = [];
  for (let start = 0; start < units.length; start += 8192) {
    chunks.push(String.fromCharCode(...units.slice(start, start + 8192)));
  }
  return chunks.join("");
}

function toUnits(value) {
  const units = new Array(value.length);
  for (let index = 0; index < value.length; index += 1) units[index] = value.charCodeAt(index);
  return units;
}

// Native 2.1.295 warning filter and redaction/escaping source slices.
// src_186240709.js full SHA-256: e3196d36258c99d66c4d3ea2401745eae6e9a94e148380f6ff0fe1f4723043bf.
// Jn/Yn/Xn/Ot [4953,6006): d6fee11520c553afb8e404228e87946e86413d6142815e75c21e56ed972f3e60.
// D.redact closure [6006,34006): 505c725f218082e51085c4709850e949aeae14e2ec84811a226804a040e89cfd.
// Je [44351,44444): c2bde4f27969f79ff0aa6c718c947d07bed9579c5ce125d0d7bb040d5b9c0c24.
// level/ki declaration [44853,44951): efdc7ca65818a10a5b52fad71dff1dfe694f55caa1b6dab9db960477fe9bf54f.
const renderNativeWarning = (() => {
function Jn(e){if(!e||e.trim()==="")return null;let n=e.split(",").map((o)=>o.trim()).filter(Boolean);if(n.length===0)return null;let r=n.some((o)=>o.startsWith("!")),i=n.some((o)=>!o.startsWith("!"));if(r&&i)return null;let s=n.map((o)=>o.replace(/^!/,"").toLowerCase());return{include:r?[]:s,exclude:r?s:[],isExclusive:r}}function Yn(e){let n=[],r=e.match(/^MCP server ["']([^"']+)["']/);if(r&&r[1])n.push("mcp"),n.push(r[1].toLowerCase());else{let o=e.match(/^([^:[]+):/);if(o&&o[1])n.push(o[1].trim().toLowerCase())}let i=e.match(/^\[([^\]]+)]/);if(i&&i[1])n.push(i[1].trim().toLowerCase());if(e.toLowerCase().includes("1p event:"))n.push("1p");let s=e.match(/:\s*([^:]+?)(?:\s+(?:type|mode|status|event))?:/);if(s&&s[1]){let o=s[1].trim().toLowerCase();if(o.length<30&&!o.includes(" "))n.push(o)}return Array.from(new Set(n))}function Xn(e,n){if(!n)return!0;if(e.length===0)return!1;if(n.isExclusive)return!e.some((r)=>n.exclude.includes(r));else return e.some((r)=>n.include.includes(r))}function Ot(e,n){if(!n)return!0;let r=Yn(e);return Xn(r,n)}var Qn=new RegExp("[\\p{M}\\uFF9E\\uFF9F]{32,}","gu"),qn=new RegExp("[\\p{M}\\uFF9E\\uFF9F]","u");var Ue=new Map,re=[],Me=new Map;function pe(e,n){return e!==n&&(e+n).normalize("NFD")===n+e}function _t(e){let n=Ue.get(e);if(n!==void 0)return n;let r=String.fromCodePoint(e),i=null;if(r==="\u0334"||pe(r,"\u0334")||pe("\u0345",r)){let s=0,o=re.length;while(s<o&&i===null){let a=s+o>>1,l=re[a];if(pe(r,l.sample))s=a+1;else if(pe(l.sample,r))o=a;else i=l}if(i===null){i={order:s,sample:r},re.splice(s,0,i);for(let a=s+1;a<re.length;a++)re[a].order=a}}return Ue.set(e,i),i}function Pt(e){let n=Me.get(e);if(n===void 0)n=Array.from(String.fromCodePoint(e).normalize("NFKD"),(r)=>r.codePointAt(0)),Me.set(e,n);return n}var Lt=4096;function er(e,n){let r="";for(let i=0;i<n;i+=Lt)r+=String.fromCodePoint(...e.subarray(i,Math.min(i+Lt,n)));return r}function tr(e){let n=new Int32Array(e.length+16),r=0;for(let a=0;a<e.length;){let l=e.codePointAt(a);a+=l>65535?2:1;for(let u of Pt(l)){if(r===n.length){let c=new Int32Array(n.length*2);c.set(n),n=c}n[r++]=u,_t(u)}}let i=new Int16Array(r);for(let a=0;a<r;a++)i[a]=Ue.get(n[a])?.order??-1;let s=n.slice(0,r),o=new Int32Array(re.length);for(let a=0;a<r;){let l=a,u=!0;while(l<r&&i[l]>=0)u&&=l===a||i[l-1]<=i[l],l++;if(u){a=l+1;continue}o.fill(0);for(let d=a;d<l;d++)o[i[d]]++;let c=a;for(let d=0;d<o.length;d++){let p=o[d];o[d]=c,c+=p}for(let d=a;d<l;d++)s[o[i[d]]++]=n[d];a=l}return er(s,r)}function _Ro(e){return e.length<32?e:e.replace(Qn,tr)}var Tt=128;function Ct(e){if(e<768)return!0;let n=Me.get(e);if(n===void 0&&!qn.test(String.fromCodePoint(e)))return!0;return _t((n??Pt(e))[0])===null}function nr(e,n){let r=n;if(r<e.length&&(e.charCodeAt(r)&64512)===56320&&(e.charCodeAt(r-1)&64512)===55296)r++;while(r<e.length){let i=e.codePointAt(r);if(Ct(i))break;r+=i>65535?2:1}return Math.min(r,e.length)}function rr(e){let n=e.length;while(n>0){if(n--,n>0&&(e.charCodeAt(n)&64512)===56320&&(e.charCodeAt(n-1)&64512)===55296)n--;if(Ct(e.codePointAt(n)))return n}return 0}function It(e,n){let r=_Ro(e);if(r.length<=Tt)return r.normalize(n);let i=[],s="";for(let o=0;o<r.length;){let a=nr(r,o+Tt),l=(s+r.slice(o,a)).normalize(n),u=a===r.length?l.length:rr(l);i.push(l.slice(0,u)),s=l.slice(u),o=a}return i.join("")}function CZ(e){return It(e,"NFKC")}function n0s(e){return It(e,"NFKD")}function un(e){return Bun.stripANSI(e)}function Hut(e){return Lw(un(Lw(String(e))))}function RN(e){return e.replace(/[^\n\r\v\f\x85\u2028\u2029]+/g,(n)=>un(n))}var Dt="\\p{Default_Ignorable_Code_Point}\\u2800",S_=`\\p{Cc}\\p{Cf}\\p{Cs}\\p{Co}\\p{Cn}\\u2028\\u2029${Dt}`,nPe=String.raw`\u00ad\u034f\u061c\u115f\u1160\u17b4\u17b5\u180b-\u180f\u200b-\u200f\u202a-\u202e\u2060-\u206f\u2800\u3164\ufe00-\ufe0f\uffa0\ufff0-\ufffb\uDC00-\uDFFF\uDB40-\uDB43`,Ft=String.raw`\u0600-\u0605\u06dd\u070f\u0890\u0891\u08e2`,X9e=`[${nPe}${Ft}\\ufeff]*`,r0s=new RegExp(`[${nPe}${Ft}\\ufeff]`);function ZUe(e){return e.replace(/([a-z)])(?=[a-z(\[])/gi,`$1${X9e}`).replace(/\[_-\]\?/g,`(?:[_-]${X9e})?`)}var Nt=new RegExp(`[${S_}]+`,"gu"),ze="\\u200D\\uFE0E\\uFE0F",ir=new RegExp(`(?:(?![${ze}])[${S_}])+`,"gu"),sr=new RegExp(`(?:(?![${ze}\\n])[${S_}])+`,"gu"),or=new RegExp(`(?<!\\S)[${ze}]+`,"gu"),ar=/\p{Cs}/gu;function xN(e,n,r){let i=r?.keepNewlines===!0||r?.keepEmojiJoiners===!0,s=un(e.replace(ar,"\u200B")).replace(r?.keepNewlines?sr:i?ir:Nt,n);return i?s.replace(or,""):s}var lr=new RegExp(`[\\p{Cf}\\p{Co}\\p{Cn}\\u2028\\u2029\\u007F-\\u009F${Dt}]`,"gu");function ur(e){let n=e.replace(/[\p{Cf}\p{Co}\p{Cn}]/gu,"");return n=n.replace(/[\u200B-\u200F]/g,"").replace(/[\u202A-\u202E]/g,"").replace(/[\u2066-\u2069]/g,"").replace(/[\uFEFF]/g,"").replace(/[\uE000-\uF8FF]/g,""),n}function af(e){let n=Lw(e);for(let r=0;r<10;r++){let i=ur(n);if(i===n)return n;n=i}return n}var cr=/\p{Default_Ignorable_Code_Point}/gu;function Lg(e){return Nr(af(e).replace(cr,""))}function SRo(e){if(!/[^\0-\x7f]/.test(e))return e;let n=e,r="",i=0,s=10;while(n!==r&&i<s)r=n,n=CZ(n),n=af(n),i++;if(i>=s)throw Error(`Unicode sanitization reached maximum iterations (${s}) for input: ${e.slice(0,100)}`);return n}function Jv(e){if(typeof e==="string")return SRo(e);if(Array.isArray(e))return e.map(Jv);if(e!==null&&typeof e==="object"){let n={};for(let[r,i]of Object.entries(e)){let s=Jv(r);if(s!=="__proto__")n[s]=Jv(i)}return n}return e}function Ic(e){return J9e(e.replace(/\\(?=u[0-9a-fA-F]{4})/g,"\\u005c"))}function J9e(e){return e.replace(/[^\x20-\x7e]/g,t4)}function wn(e){return e.replace(/[\p{Cc}\p{Cf}\u2028\u2029]+/gu," ")}function vH(e){return e.replace(lr,(n)=>Array.from({length:n.length},(r,i)=>`\\u${n.charCodeAt(i).toString(16).padStart(4,"0")}`).join(""))}function Ase(e){return Lw(un(e)).replace(Nt," ")}function Js(e,n){let r=Ase(e);if(n?.drop)r=r.replace(n.drop,"");if(r=r.replace(/\s+/g," ").trim(),n?.maxCodeUnits!==void 0)r=ne(r,n.maxCodeUnits).trim();return r}function o0s(e){return Js(e)===e}var Q9e=255;function _T(e){let n=Js(e,{drop:/[`<>]/g}).replace(/\]\(/g,"] (").replace(/!\[/g,"! [").replace(/\]\[/g,"] [").replace(/\]:/g,"] :");return ne(n,Q9e).trim()}var zrn=2048,AGn=/[`\[\]<>]/g;function AZ(e){let n=Js(e).replace(/\]\(/g,"] (").replace(/!\[/g,"! [").replace(/\]\[/g,"] [").replace(/\]:/g,"] :").replace(/</g,"< ").replace(/\s+/g," ").trim();return ne(n,zrn).trim()}function Mut(e){return e.replace(/[\u007F-\u009F\u2028\u2029\p{Cf}]/gu,t4)}function t4(e){return e.split("").map((n)=>"\\u"+n.charCodeAt(0).toString(16).padStart(4,"0")).join("")}function $t(e){let n=new Int32Array(e.length);for(let r=0;r<e.length;r++)n[r]=r;return{text:e,origin:n}}function Ht(e,n,r){let i=[];for(let s of e.text.matchAll(n)){let{keep:o,insert:a,keepEnd:l=0}=r(s),u=s.index??0,c=s[0].length,d=Math.min(Math.max(o|0,0),c),p=Math.min(Math.max(l|0,0),c-d);i.push([u+d,u+c-p,a])}return he(e,i)}function he(e,n){if(n.length===0)return e;let{text:r,origin:i}=e,s=r.length;for(let[c,d,p]of n)s+=p.length-(d-c);let o=new Int32Array(s),a=[],l=0,u=0;for(let[c,d,p]of n)a.push(r.slice(l,c)),o.set(i.subarray(l,c),u),u+=c-l,a.push(p),o.fill(-1,u,u+p.length),u+=p.length,l=d;return a.push(r.slice(l)),o.set(i.subarray(l),u),{text:a.join(""),origin:o}}function Ge(e,n,r){return{text:e.text.slice(n,r),origin:e.origin.slice(n,r)}}function Z$(e){let n=0;while(n<e.length&&e.charCodeAt(n)<=32)n++;let r=e.slice(n),i=/^([A-Za-z0-9][A-Za-z0-9+.-]*):\/\//.exec(r),s=i?.[1]?.toLowerCase(),o=i?r.slice(i[0].length):r,a=o.indexOf(":"),l=o.indexOf("/");if(!(s!==void 0?s!=="file":a!==-1&&(l===-1||a<l)&&!/^[A-Za-z]:/.test(o))){let b=i?de(o):o;return b.includes("@[")||b.startsWith("[")}let c=de(o),d=i?/^(?:[^@/?#\\[\]]+@)?(\[[0-9A-Fa-f:.]+\])(?::\d*)?(?=\/|$)/.exec(c):/^[^@:/[\]]+@(\[[0-9A-Fa-f:.]+\])(?=:)/.exec(c),p=d!==null&&!!rPe(`ssh://${d[1]}/`)?.hostname;return/[[\]]/.test(c.slice(p?d[0].length:0))}function rPe(e){try{return new URL(e)}catch{return null}}function de(e){return e.replace(/%([0-9A-Fa-f]{2})/g,(n,r)=>{let i=parseInt(r,16);return i===0?n:String.fromCharCode(i)})}function Tse(e,{asRewritten:n=!1}={}){let r=e.trimStart(),i=/^([A-Za-z][A-Za-z0-9+.-]*):\/\//.exec(r);if(i){let s=i[1]?.toLowerCase();if(s==="file")return null;if(s==="http"||s==="https"){let o=rPe(r);return o?.hostname?{host:o.hostname,path:o.pathname}:null}return Bt(jHt(r.slice(i[0].length)),"/")}return Bt(n?jHt(r):r,":")}function Ut(e,n){let r=e.indexOf("@["),i=r===-1?0:r+1,s=e[i]==="["?i:-1,o=s===-1?-1:e.indexOf("]",s+1),a=e.indexOf(n,o===-1?0:o+1);return{open:s,close:o,cut:a}}function Bt(e,n){let{open:r,close:i,cut:s}=Ut(e,n),o=r===-1?0:r,a=e.indexOf("/");if(n===":"&&(s===-1||a!==-1&&a<e.indexOf(":")))return null;let l=s===-1?"":e.slice(n===":"?s+1:s);if(i!==-1){let p=e.slice(i+1,s===-1?e.length:s),b=e.slice(0,o)+e.slice(o+1,i),h=Wt(p)!==p?b:Wt(b),w=h.slice(h.lastIndexOf("@")+1),f=rPe(`ssh://[${w}]/`)?.hostname;return w?{host:f||w,path:l}:null}let u=s===-1?e:e.slice(0,s),c=u.slice(u.lastIndexOf("@")+1);if(c.startsWith("["))return null;let d=n==="/"?c.replace(/:\d*$/,""):c;return d?{host:d,path:l}:null}function Mt(e){let{text:n,from:r}=dr(e),{close:i,cut:s}=Ut(n,"/"),o=s===-1?n.length:s,a=[],l=(u,c)=>{let d=n.lastIndexOf("@",c-1);if(d>=u)a.push([r(u),r(d)])};if(i===-1)l(0,o);else l(0,i),l(i+1,o);return a}function jHt(e){return e.replace(/(?:%(?!00)[0-9A-Fa-f]{2})+/g,(n)=>{try{return decodeURIComponent(n)}catch{return de(n)}})}function dr(e){if(!e.includes("%"))return{text:e,from:(s)=>s};let n=[],r="",i=0;for(let s of e.matchAll(/(?:%(?!00)[0-9A-Fa-f]{2})+/g)){for(let c=i;c<s.index;c++)n.push(c);r+=e.slice(i,s.index);let o,a;try{o=decodeURIComponent(s[0]),a=Array.from(o,(c)=>{let d=c.codePointAt(0);return d<128?1:d<2048?2:d<65536?3:4})}catch{o=de(s[0]),a=Array.from(o,()=>1)}let l=s.index,u=0;for(let c of a){let d=o.codePointAt(u)>65535?2:1;for(let p=0;p<d;p++)n.push(l);u+=d,l+=3*c}r+=o,i=s.index+s[0].length}for(let s=i;s<=e.length;s++)n.push(s);return{text:r+e.slice(i),from:(s)=>n[s]}}function Wt(e){let n=e.indexOf(":"),r=e.slice(n+1);if(n===-1||!/^[ \t\n\v\f\r]*[+-]?\d+$/.test(r))return e;let i=Number.parseInt(r.trim(),10);return i>=0&&i<65536?e.slice(0,n):e}var fr=/\s/g,gr=/[\n\r\u2028\u2029]/g,pr=/(?:@|%40)(?:\[|%5B)/gi,hr=/^(?:\[|%5B)/i,mr=/\]|%5D/gi,bRo=/(?:^|[^A-Za-z0-9+.-])(?:https?|wss?|ftps?)$/i,Vt=/(?:^|[^A-Za-z0-9+.-])(?:ssh|git|git\+ssh|ssh\+git)$/i,yr=/[,;|&]/g,br=/(?:^|[^A-Za-z0-9+.-])[A-Za-z][A-Za-z0-9+.-]{0,31}$/,kr=/["'`|,;&<>()[\]\\@/?#]|%40|%5B/i,xr=/["'`]/,Sr=/[;|&\n]/g,Er=64,wr=2048,Ar=/[\s"'`]/g;function Ke(e){let n=[],r=C(e,Ar),i=0;for(let s=e.indexOf("://");s!==-1;){let o=s+3;if(s>=i&&Vt.test(e.slice(Math.max(0,s-8),s))){i=Math.min(r(o),o+wr);for(let[a,l]of Mt(e.slice(o,i)))n.push([o+a,o+l])}s=e.indexOf("://",o)}return n}function s0s(e){return jt(e).spans}function jt(e){let n=[],r=[],i=C(e,fr),s=C(e,/\//g),o=Ve(e,pr),a=C(e,mr),l=C(e,/\//g),u=C(e,Sr),c=C(e,/[/?#]/g),d=C(e,yr),p=C(e,/@/g),b=C(e,/%40/gi),h=C(e,/\[REDACTED|\*\*\*:\*\*\*/g),w=C(e,/[:%]/g),f=C(e,/["'`,;|&<>{}()]/g),m=Kt(e,!1),y=Kt(e,!0);for(let v=e.indexOf("://");v!==-1;){let S=v+3,F,P=e.indexOf("://",S),A=i(S),Q=p(S),I;if(bRo.test(e.slice(Math.max(0,v-8),v))){let O=Math.min(c(S),A),H=Math.min(d(S),O);if(I=Q<O?Math.max(m(S,O),y(Q,O)):Math.max(m(S,Math.min(s(S),A)),y(S,H)),I<=S){let G=Math.min(O,u(S));r.push({start:S,fieldEnd:Math.min(H,G),lastEncodedAt:y(S,G)})}}else{let O=Math.min(s(S),A),H=Math.min(A,P===-1?e.length:P),G=Vt.test(e.slice(Math.max(0,v-8),v)),T=G?o(S):null,R=G?hr.exec(e.slice(S,S+3)):null,L=[[R!==null?S+R[0].length:-1,H],[T!==null&&T.index<H?T.index+T[0].length:-1,A]];for(let[B,V]of L){let te=B===-1?A:a(B);if(te<V)F=Math.max(F??te,te),O=Math.max(O,Math.min(l(te),A))}I=Math.max(m(S,O),y(S,O))}if(I>S)n.push({start:S,end:I,separatorEnd:I+(e[I]==="@"?1:3),loginEnd:Q<=I?Q:Math.min(b(S),I),bracketClose:F});else{let O=h(S);if(O>S&&O<Math.min(A,s(S),f(S))&&w(S)<O)n.push({start:S,end:O,separatorEnd:O,loginEnd:O})}v=P}return{spans:n,hostParts:r}}function Zt(e,n){let{spans:r,hostParts:i}=jt(e);if(r.length===0&&i.length===0)return[];let s=C(e,/[\s"'>}|,;&`)\]]/g),o=C(e,/\s/g),a=[],l=[];for(let h of r)a.push({start:h.start,end:h.end,loginEnd:h.loginEnd}),l.push({...h,userinfo:h});let u=me(e,n);for(let h of i){let w=u(h.start)[0];if(w&&w.keyStart<h.fieldEnd&&e.slice(h.start,w.keyStart).includes("%")){let f=h.lastEncodedAt>w.keyStart?h.lastEncodedAt:w.valueStart;a.push({start:h.start,end:f,loginEnd:f}),l.push(f>w.valueStart?{start:h.start,end:f,userinfo:{start:h.start,end:f,separatorEnd:f+3,loginEnd:f}}:{start:h.start,end:f})}}if(l.length===0)return[];a.sort((h,w)=>h.start-w.start);let c=me(e,n),d=0;for(let h=0,w=c(h);w[0]!==void 0;){let f,m=null;for(let v of w)if(m=v.valueEnd(),m!==null){f=v;break}if(f===void 0||m===null){h=w[0].keyStart+1,w=c(h);continue}while(d<a.length&&a[d].start<=f.keyStart)d++;let y=a[d];if(y!==void 0&&y.start<m){let v=e[f.valueStart]??"",F=(v==='"'||v==="'")&&m-1>f.valueStart&&e[m-1]===v?Math.max(m,y.end):o(Math.max(m,y.end,s(y.loginEnd+1)));l.push({start:f.valueStart,end:F})}h=m,w=c(h)}let p={nextKey:me(e,n),plainKeys:me(e,n),nextLineEnd:C(e,gr),span:0,lineEnd:-1,resumeAt:0,overflowEnd:Or(e,()=>p.lineEnd)},b=[];for(let h of Tr(l)){let w=b.at(-1);b.push(w!==void 0&&h.start<w.end?Gt(e,{start:b.pop().start,end:Math.max(w.end,h.end)},p):Gt(e,h,p))}return b.map((h)=>h.userinfo?h:Lr(e,h))}var zt=new WeakMap;function Rr(e){let n=zt.get(e);if(!n)n={word:new RegExp(e.word,"gi"),notTail:new RegExp(`(?!${e.tail})[^]`,"gi"),separator:new RegExp(e.separator,"iy"),value:new RegExp(e.value,"iy")},zt.set(e,n);return n}function me(e,n){let r=n.map((i)=>{let{word:s,notTail:o,separator:a,value:l}=Rr(i),u=Ve(e,s),c=C(e,o),d=-1,p=-1,b=-1,h=null,w=(v)=>{if(v!==b)b=v,l.lastIndex=v,h=l.test(e)?l.lastIndex:null;return h},f=(v)=>{if(v!==d)d=v,a.lastIndex=v,p=a.test(e)?a.lastIndex:-1;return p},m=-1,y=null;return(v)=>{if(m!==-1&&v>=m&&(y===null||v<=y.keyStart))return y;m=v,y=null;for(let S=u(v);S!==null;){let F=S.index+S[0].length,P=f(i.tail?c(F):F);if(P!==-1){y={keyStart:S.index,valueStart:P,valueEnd:()=>w(P)};break}S=u(S.index+1)}return y}});return(i)=>{let s=[];for(let o of r){let a=o(i);if(a&&(!s[0]||a.keyStart<=s[0].keyStart))s=s[0]&&a.keyStart===s[0].keyStart?[...s,a]:[a]}return s}}function Gt(e,n,r){if(n.userinfo&&vr(e,n.userinfo,r))return n;let i=n.end;r.resumeAt=Math.max(r.resumeAt,n.start);for(let s=r.nextKey(r.resumeAt);s[0]!==void 0&&s[0].keyStart<i;){let o=s[0].keyStart;if(o>r.lineEnd){let c=r.nextLineEnd(o);r.span=Er*(c-r.lineEnd),r.lineEnd=c}let a=null,l=s[0];for(let c of s)if(a=c.valueEnd(),a!==null){l=c;break}let u=a??l.valueStart;if(r.span-=u-o,r.span<0)return{start:n.start,end:Math.max(i,r.overflowEnd(n.start))};i=Math.max(i,u),r.resumeAt=l.valueStart,s=r.nextKey(r.resumeAt)}return i===n.end?n:{start:n.start,end:i}}function vr(e,n,r){if(e[n.end]!=="@"||kr.test(e.slice(n.start,n.end))||!br.test(e.slice(Math.max(0,n.start-36),n.start-3)))return!1;for(let i=r.plainKeys(n.start);i[0]!==void 0&&i[0].keyStart<n.end;i=r.plainKeys(i[0].keyStart+1))if(i.some((s)=>s.valueStart===n.end))return!1;return!0}function Or(e,n){let r=C(e,/["'`]/g),i=-1,s=e.length;return(o)=>{if(n()!==i)i=n(),s=r(o)<i?e.length:i;return s}}function Lr(e,n){let r=e[n.end-1];if(r===void 0||!xr.test(r))return n;return(e.slice(n.start,n.end).split(r).length-1)%2===1?{start:n.start,end:n.end-1}:n}function Tr(e){e.sort((r,i)=>r.start-i.start||i.end-r.end);let n=[];for(let r of e){let i=n.at(-1);if(i===void 0||r.start>=i.end)n.push(r);else n[n.length-1]={start:i.start,end:Math.max(i.end,r.end)}}return n}function C(e,n){let r=Ve(e,n);return(i)=>r(i)?.index??e.length}function Ve(e,n){let r=-1,i=null;return(s)=>{if(r===-1||s<r||i!==null&&s>i.index)n.lastIndex=s,i=n.exec(e),r=s;return i}}function Kt(e,n){let r=null;return(i,s)=>{if(r===null){r=new Int32Array(e.length+1);let a=-1;for(let l=0;l<=e.length;l++){if(n){if(l>=3&&e.startsWith("%40",l-3))a=l-3}else if(l>=1&&e[l-1]==="@")a=l-1;r[l]=a}}let o=r[Math.min(s,e.length)];return o>=i?o:-1}}var Se=new RegExp(ZUe(String.raw`api[_-]?key|secret|token|password|passwd|credential|bearer|authorization|auth[_-]?header|cookie|session[_-]?(?:id|key)|connection[_-]?string|(?:private|ssh|encryption|signing|access|deploy|master|license)[_-]?key|client[_-]?secret`),"i"),ye=`[\\s\\u0085${nPe}]`,Ye=`(?:${ZUe("Bearer|Basic")})[${nPe}]*[\\s\\u0085]+`,Jt="[^\\s\\u0085,;&}\\])]+",Yt="(?![\\w\\[({$/+=~!@%^?\\\\-])",we="\\n\\r\\x0b\\x0c\\u0085\\u2028\\u2029",Z="[\\s\\u0085]",wRo=new RegExp(ye),i0s=new RegExp(`(?:[=:]|["']|\\b(?:${ZUe("bearer|basic")}))$`,"i"),_r=`\\\\+(?:"[^"${we}]*(?<!\\\\)(?=\\\\+")|'[^'${we}]*(?<!\\\\)(?=\\\\+'))`,Xt=`[^\\s\\u0085,;&})"'\\\\]+`,Ae=`"[^"${we}]*"|'[^'${we}]*'|"[^"]*"${Yt}|'[^']*'${Yt}|[^\\s-]{0,4}\\[REDACTED\\]['"\`]?|${Ye}(?:\\[REDACTED\\]|${Jt})|${Jt}`,Pr={word:"\\b(?:Bearer|Basic)",tail:"",separator:`${Z}+`,value:"[A-Za-z0-9+/=._~-]{20,}"},Cr={word:`(?:${Se.source})`,tail:"[\\w.-]",separator:`["']?${Z}*[=:]${Z}*`,value:Ae},Ir={word:"\\b(?:AWS|GOOGLE|GCP|GCLOUD|AZURE)_\\w",tail:"\\w",separator:`${Z}*[=:]${Z}*`,value:Ae},Dr=[Cr,Ir,Pr],Fr=["sk","ant","api"].join("-"),U="[\\w=-]",M=`${U}{20,}(?:\\.[0-9a-z]{9})?`,Qt="[\\w.-]{0,256}(?![\\w.-])",qt=`${Z}*(?!${Z})`,ge="[A-Za-z0-9_-]",Xe=`${ge}{10}${ge}*`,en=`\\.${Xe}\\.${Xe}`,et=`(?<!${ge})(?=${ge}+${en})(?:${ge}*?-)??(eyJ${Xe}${en})`,sn=[{id:"url-userinfo",source:`:\\/\\/((["'])[^/@\\s]*\\2(?=@)|(?:[^/@\\s"'|,;&\`]+(?=@))|[^/@\\s"'|,;&\`]*["'|,;&\`][^/@\\s]*@)@?`,confidence:"low"},{id:"gcp-service-account",source:"\\b([a-z0-9-]{1,64}@[a-z0-9-]+\\.iam\\.gserviceaccount\\.com)\\b",flags:"i",confidence:"low"},{id:"loose-anthropic-key",source:"\\b(sk-ant-?[\\w-]{10,})",confidence:"low"},{id:"http-auth-scheme",source:`\\b${Ye}([A-Za-z0-9+/=._~-]{20,})`,flags:"i",confidence:"low"},{id:"loose-jwt",prefilter:/eyJ/,source:et,confidence:"low"},{id:"sensitive-assign",source:`(?:${Se.source})${Qt}(?:${X9e}["'])?${ye}*[=:]${qt}(${Ae})`,flags:"i",confidence:"low"},{id:"cloud-env-var",source:`\\b(?:${ZUe("AWS|GOOGLE|GCP|GCLOUD|AZURE")})${X9e}_\\w+${ye}*[=:]${Z}*(${Ae})`,flags:"i",confidence:"low"},{id:"url-userinfo-tail",source:":\\/\\/(\\[REDACTED\\](?!@|%40[^\\s\"'>}|,;&`)\\]@/]*(?:[\\s\"'>}|,;&`)\\]/]|$))[^\\s\"'>}|,;&`)\\]]*?(?=[\\s\"'>}|,;&`)\\]]|:\\/\\/|$))",confidence:"low"},{id:"url-userinfo-later-at",source:":\\/\\/(\\[REDACTED\\]@(?:[^\\s:/?#\"'>}|,;&`)\\]]|:(?!\\/\\/)){0,256})(?=@|%40)",confidence:"low"},{id:"url-userinfo-encoded",source:":\\/\\/((?:\\[REDACTED\\]@)?(?:(?:[^/?#@\\s\"',;&<>|)\\]{}`[]*[\"'`<>{}[\\]][^/?#@\\s,;&|{}\"'`<>[\\]()]*(?=%40))|[^/?#@\\s\"',;&<>|)\\]}`[]+(?=%40)|(?=[^/?#@\\s\"',;&<>|)\\]}`[]*?(?:%|:[^/?#@\\s\"',;&<>|)\\]}`[]))[^/?#@\\s\"',;&<>|)\\]}`[]+(?=\\[REDACTED\\])))",confidence:"low"},{id:"masked-value-rest",source:"\\[REDACTED\\](\\][^\\s\"'>}|,;&`)\\]]*)",confidence:"low"},{id:"sensitive-assign-escaped",redactOnly:!0,prefilter:/\\["']/,source:`(?:${Se.source})${Qt}${X9e}\\\\+["']${ye}*[=:]${qt}(${_r}|${Ye}${Xt}|${Xt}|\\\\+["'][^\\s\\u0085,;&})"'\\\\]*)`,flags:"i",confidence:"low"},{id:"aws-access-token",source:"\\b((?:A3T[A-Z0-9]|AKIA|ASIA|ABIA|ACCA)[A-Z2-7]{16})\\b",confidence:"high"},{id:"gcp-api-key",source:"\\b(AIza[\\w-]{35})(?![\\w-])",confidence:"high"},{id:"google-oauth-client-secret",source:"\\bGOCSPX-[\\w-]{28}(?![\\w-])",confidence:"high"},{id:"azure-ad-client-secret",source:`(?:^|[\\\\'"\\x60\\s>=:(,)])([a-zA-Z0-9_~.]{3}\\dQ~[a-zA-Z0-9_~.-]{31,34})(?:$|[\\\\'"\\x60\\s<),])`,confidence:"high"},{id:"digitalocean-pat",source:`\\b(dop_v1_[a-f0-9]{64})(?:[\\x60'"\\s;]|\\\\[nr]|$)`,confidence:"high"},{id:"digitalocean-access-token",source:`\\b(doo_v1_[a-f0-9]{64})(?:[\\x60'"\\s;]|\\\\[nr]|$)`,confidence:"high"},{id:"anthropic-api-key",source:`\\b(${Fr}03-[a-zA-Z0-9_\\-]{93}AA)(?:[\\x60'"\\s;]|\\\\[nr]|$)`,confidence:"high"},{id:"anthropic-admin-api-key",source:`\\b(sk-ant-admin01-[a-zA-Z0-9_\\-]{93}AA)(?:[\\x60'"\\s;]|\\\\[nr]|$)`,confidence:"high"},{id:"anthropic-oauth-token",source:`\\b(sk-ant-(?:oat|ort)\\d{2}-[\\w-]{20,})(?:[\\x60'"\\s;]|\\\\[nr]|$)`,run:"[\\w-]",confidence:"high"},{id:"openai-api-key",source:"sk-[A-Za-z0-9_-]{8,200}T3BlbkFJ[A-Za-z0-9_-]{8,200}",confidence:"high"},{id:"openai-legacy-api-key",source:"\\bsk-[a-zA-Z0-9]{48}(?![a-zA-Z0-9])",confidence:"high"},{id:"huggingface-access-token",source:`\\b(hf_[a-zA-Z]{34})(?:[\\x60'"\\s;]|\\\\[nr]|$)`,confidence:"high"},{id:"supabase-secret-key",source:"\\bsb_secret_[A-Za-z0-9_-]{20,}",confidence:"high"},{id:"supabase-access-token",source:"\\bsbp_[a-z0-9]{40,}",confidence:"high"},{id:"github-pat",source:"ghp_[0-9a-zA-Z]{36}",confidence:"high"},{id:"github-fine-grained-pat",source:"github_pat_\\w{82}",confidence:"high"},{id:"github-app-token",source:"(?:ghu|ghs)_[0-9a-zA-Z]{36}",confidence:"high"},{id:"github-oauth",source:"gho_[0-9a-zA-Z]{36}",confidence:"high"},{id:"github-refresh-token",source:"ghr_[0-9a-zA-Z]{36}",confidence:"high"},{id:"gitlab-pat",source:`glpat-${M}`,displayRun:U,confidence:"high"},{id:"gitlab-deploy-token",source:`gldt-${M}`,displayRun:U,confidence:"high"},{id:"gitlab-runner-authentication-token",source:`glrt-${M}`,displayRun:U,confidence:"high"},{id:"gitlab-oauth-app-secret",source:`gloas-${M}`,displayRun:U,confidence:"high"},{id:"gitlab-pipeline-trigger-token",source:`glptt-${M}`,displayRun:U,confidence:"high"},{id:"gitlab-kubernetes-agent-token",source:`glagent-${M}`,displayRun:U,confidence:"high"},{id:"gitlab-incoming-mail-token",source:`glimt-${M}`,displayRun:U,confidence:"high"},{id:"gitlab-scim-oauth-token",source:`glsoat-${M}`,displayRun:U,confidence:"high"},{id:"gitlab-ci-build-token",source:`glcbt-${M}`,displayRun:U,confidence:"high"},{id:"gitlab-feed-token",source:`glft-${M}`,displayRun:U,confidence:"high"},{id:"gitlab-feature-flag-client-token",source:`glffct-${M}`,displayRun:U,confidence:"high"},{id:"slack-bot-token",source:"xoxb-[0-9]{10,13}-[0-9]{10,13}[a-zA-Z0-9-]*",confidence:"high"},{id:"slack-user-token",source:"xox[a-z](?:-[0-9]{10,13}){3}-[a-zA-Z0-9-]{28,34}",confidence:"high"},{id:"slack-rotation-token",source:"xoxe(?:\\.xox[a-z])?-[0-9]-[A-Za-z0-9-]{28,}",confidence:"high"},{id:"slack-app-token",source:"xapp-\\d-[A-Z0-9]+-\\d+-[a-z0-9]+",flags:"i",confidence:"high"},{id:"slack-workflow-token",source:"\\bxwfp-[a-zA-Z0-9-]{20,}",confidence:"high"},{id:"slack-webhook-url",source:"(?:https?://)?hooks\\.slack\\.com/(?:services|workflows|triggers)/[A-Za-z0-9+/_-]{40,}",flags:"i",confidence:"high"},{id:"twilio-api-key",source:"SK[0-9a-fA-F]{32}",confidence:"high"},{id:"sendgrid-api-token",source:`\\b(SG\\.[a-zA-Z0-9=_\\-.]{66})(?:[\\x60'"\\s;]|\\\\[nr]|$)`,confidence:"high"},{id:"npm-access-token",source:`\\b(npm_[a-zA-Z0-9]{36})(?:[\\x60'"\\s;]|\\\\[nr]|$)`,confidence:"high"},{id:"pypi-upload-token",source:"pypi-AgEIcHlwaS5vcmc[\\w-]{50,1000}",confidence:"high"},{id:"databricks-api-token",source:`\\b(dapi[a-f0-9]{32}(?:-\\d)?)(?:[\\x60'"\\s;]|\\\\[nr]|$)`,confidence:"high"},{id:"hashicorp-tf-api-token",source:"[a-zA-Z0-9]{14}\\.atlasv1\\.[a-zA-Z0-9\\-_=]{60,70}",confidence:"high"},{id:"pulumi-api-token",source:`\\b(pul-[a-f0-9]{40})(?:[\\x60'"\\s;]|\\\\[nr]|$)`,confidence:"high"},{id:"postman-api-token",source:`\\b(PMAK-[a-fA-F0-9]{24}-[a-fA-F0-9]{34})(?:[\\x60'"\\s;]|\\\\[nr]|$)`,confidence:"high"},{id:"grafana-api-key",source:`\\b(eyJrIjoi[A-Za-z0-9+/]{70,400}={0,3})(?:[\\x60'"\\s;]|\\\\[nr]|$)`,confidence:"high"},{id:"grafana-cloud-api-token",source:`\\b(glc_[A-Za-z0-9+/]{32,400}={0,3})(?:[\\x60'"\\s;]|\\\\[nr]|$)`,confidence:"high"},{id:"grafana-service-account-token",source:`\\b(glsa_[A-Za-z0-9]{32}_[A-Fa-f0-9]{8})(?:[\\x60'"\\s;]|\\\\[nr]|$)`,confidence:"high"},{id:"sentry-user-token",source:`\\b(sntryu_[a-f0-9]{64})(?:[\\x60'"\\s;]|\\\\[nr]|$)`,confidence:"high"},{id:"sentry-org-token",source:"\\bsntrys_eyJpYXQiO[a-zA-Z0-9+/]{10,200}(?:LCJyZWdpb25fdXJs|InJlZ2lvbl91cmwi|cmVnaW9uX3VybCI6)[a-zA-Z0-9+/]{10,200}={0,2}_[a-zA-Z0-9+/]{43}",confidence:"high"},{id:"stripe-access-token",source:`\\b((?:sk|rk)_(?:test|live|prod)_[a-zA-Z0-9]{10,99})(?:[\\x60'"\\s;]|\\\\[nr]|$)`,confidence:"high"},{id:"shopify-access-token",source:"shpat_[a-fA-F0-9]{32}",confidence:"high"},{id:"shopify-shared-secret",source:"shpss_[a-fA-F0-9]{32}",confidence:"high"}],$r=64;function Qe(e,n){let r=/-----BEGIN[ A-Z0-9_-]{0,100}?PRIVATE KEY(?: BLOCK)?-----/gi,i=/-----END[ A-Z0-9_-]{0,100}?PRIVATE KEY(?: BLOCK)?-----/gi;r.lastIndex=n;let s=r.exec(e);if(!s)return null;i.lastIndex=s.index+s[0].length+$r;let o=i.exec(e);if(!o)return null;return{start:s.index,end:o.index+o[0].length}}function Hr(e){return Qe(e,0)!==null}function ERo(e){let n=on(e);if(n.length===0)return e;let r="",i=0;for(let s of n)r+=e.slice(i,s.start)+"[REDACTED]",i=s.end;return r+e.slice(i)}function on(e){let n=e.indexOf("-----");while(n!==-1){while(e.charCodeAt(n+5)===45)n++;if((e.charCodeAt(n+5)|32)===98&&(e.charCodeAt(n+6)|32)===101&&(e.charCodeAt(n+7)|32)===103&&(e.charCodeAt(n+8)|32)===105&&(e.charCodeAt(n+9)|32)===110)break;n=e.indexOf("-----",n+5)}let r=[];if(n===-1)return r;for(let i=Qe(e,n);i;i=Qe(e,i.end))r.push(i);return r}var qe=`(?:[\\x60'"\\s;]|\\\\[nr]|$)`,je="(?=[^a-zA-Z0-9_\\-+=]|$)",tn="(?<![a-zA-Z0-9_\\-])";function Br(){return sn.map(({id:e,confidence:n,source:r,flags:i="",displayRun:s})=>{let o={id:e,confidence:n},a=i.replace("g","")+"g";if(n!=="high")return{...o,re:new RegExp(r,a)};let l=an(r);if(s===void 0)return{...o,re:new RegExp(tn+l+je,a)};return{...o,prefilter:new RegExp(l,i.replace("g","")),re:new RegExp(`(?<!${s})(?=${s}+${je})${s}*?${tn}(${l})${je}`,a)}})}function fe(e){return sn.map((n)=>{let r=(n.flags??"").replace("g",""),i={id:n.id,confidence:n.confidence,...n.redactOnly===!0&&{redactOnly:n.redactOnly},...n.prefilter!==void 0&&{prefilter:n.prefilter}};if(n.run===void 0)return{...i,re:new RegExp(n.source,e?r+"g":n.flags??"")};return{...i,prefilter:new RegExp(an(n.source),r),re:new RegExp(`(?:(?<!${n.run})|(?<=\\\\[nr]))(?=${n.run}+${qe})${n.run}*?${n.source}`,e?r+"g":r)}})}function an(e){return e.endsWith(qe)?e.slice(0,-qe.length):e}function be(e,n){if(typeof n!=="string")return"[REDACTED]";let r=n.length>=2&&(n[0]==='"'||n[0]==="'")&&n.at(-1)===n[0]?n[0]:"",i=e.lastIndexOf(n);return`${e.slice(0,i)}${r}[REDACTED]${r}${e.slice(i+n.length)}`}function Wr(e){let n=e[1];if(typeof n!=="string")return{keep:0,insert:"[REDACTED]"};let r=n.length>=2&&(n[0]==='"'||n[0]==="'")&&n.at(-1)===n[0]?1:0,i=e[0].lastIndexOf(n);return{keep:i+r,insert:"[REDACTED]",keepEnd:e[0].length-i-n.length+r}}function Re(e){let n=Zt(e,Dr).map(({start:i,end:s})=>{let o=/^(["'])[^]*\1$/.test(e.slice(i,s))?1:0;return[i+o,s-o]}),r=Ke(e);return r.length===0?n:ln([...n,...r])}function ln(e){let n=e.filter(([i,s])=>s>i).sort((i,s)=>i[0]-s[0]),r=[];for(let[i,s]of n){let o=r.at(-1);if(o!==void 0&&i<=o[1])o[1]=Math.max(o[1],s);else r.push([i,s])}return r}function oe(e,n){let{text:r,origin:i}=e,s=[],o=0;for(let a=0;a<r.length;){let l=i[a];while(o<n.length&&n[o][1]<=l)o++;let u=n[o];if(l<0||u===void 0||l<u[0]){a++;continue}let c=a;while(c>0&&i[c-1]<0)c--;while(a<r.length&&(i[a]<0||i[a]>=u[0]&&i[a]<u[1]))a++;let d=s.at(-1);if(d!==void 0&&d[1]>=c)d[1]=a;else s.push([c,a,"[REDACTED]"])}return he(e,s)}var Ur=512,Mr=512;class cn{testRules=null;redactRules=null;displayRules=null;resultCache=new Map;scan(e){this.testRules??=fe(!1);let n=[];for(let r of this.testRules)if(r.confidence==="high"&&r.prefilter?.test(e)!==!1&&r.re.test(e))n.push({ruleId:r.id,label:r.id.split("-").map((i)=>hi[i]??Sa(i)).join(" ")});if(Hr(e))n.push({ruleId:"private-key",label:"Private Key"});return n}redactContext(e,n=!1){this.redactRules??=fe(!0);let r=ERo(e);for(let i of this.redactRules)if(i.confidence==="low"&&Je(i,n,r))r=r.replace(i.re,be);return r}redactTokens(e){this.redactRules??=fe(!0);let n=e;for(let r of this.redactRules)if(r.confidence==="high"&&r.prefilter?.test(n)!==!1)n=n.replace(r.re,be);return n}trackedByRules(e,n=!1){return this.trackedTokens(this.trackedContext(e,n))}trackedContext(e,n=!1){let r=he($t(e),on(e).map(({start:i,end:s})=>[i,s,"[REDACTED]"]));return this.trackedRules(r,"low",n)}redactOnlyRules(e){this.redactRules??=fe(!0);let n=e;for(let r of this.redactRules)if(r.redactOnly===!0&&Je(r,!0,n))n=n.replace(r.re,be);return n}trackedTokens(e){return this.trackedRules(e,"high")}trackedRules(e,n,r=!1){this.redactRules??=fe(!0);let i=e;for(let s of this.redactRules)if(s.confidence===n&&Je(s,r,i.text))s.re.lastIndex=0,i=Ht(i,s.re,Wr);return i}redactWithUrlRanges(e){let n=Re(e);return n.length===0?this.redactTokens(this.redactContext(e,!0)):oe(this.trackedByRules(e,!0),n).text}redactTracked(e){return oe(this.trackedByRules(e,!0),Re(e))}redact(e){let n=e.length<=Ur;if(n){let i=this.resultCache.get(e);if(i!==void 0)return i}let r=this.redactWithUrlRanges(e);if(n){if(this.resultCache.size>=Mr)this.resultCache.delete(this.resultCache.keys().next().value);this.resultCache.set(e,r)}return r}redactForDisplay(e){this.displayRules??=Br();let n=e;for(let r of this.displayRules){if(r.confidence!=="high"||r.prefilter?.test(n)===!1)continue;n=n.replace(r.re,Kr)}return n}}var D=new cn;function TGn(e){return D.scan(e)}function Je(e,n,r){return(n||e.redactOnly!==!0)&&(e.prefilter===void 0||e.prefilter.test(r))}var ft={verbose:0,debug:1,info:2,warn:3,error:4},ki=new RegExp(`(?!\\t)[${S_}]`,"gu"),xi=10485760;
  return (messageUnits, filterUnits) => {
    const message = fromUnits(requireUnits(messageUnits, "warning_message_units"));
    const filter = filterUnits === null
      ? null
      : Jn(fromUnits(requireUnits(filterUnits, "debug_filter_units")));
    if (!Ot(message, filter)) return null;
    let redacted = D.redact(message.trim());
    redacted = redacted.includes("\n") ? vH(JSON.stringify(redacted)) : redacted.replace(ki, t4);
    return toUnits(`${new Date().toISOString()} [WARN] ${redacted}\n`);
  };
})();

// These Native helpers are copied from the bound source; isIPv4/isIPv6 are
// Bun's built-in net module functions, matching the source import from "net".
var Mu = new Set(["metadata.google.internal", "metadata.goog", "metadata", "instance-data", "instance-data.ec2.internal", "ip6-localhost", "ip6-loopback", "localhost.localdomain", "localhost4", "localhost4.localdomain4", "localhost6", "localhost6.localdomain6"]);
var yr = new Set(["100.100.100.200", "168.63.129.16", "192.0.0.192"]);
function _r(e, n, s, r) { return e === 127 || e === 169 && n === 254 || e === 0; }
function br(e) {
  let n = e.indexOf("%"), r = (n >= 0 ? e.slice(0, n) : e).toLowerCase().split("::");
  if (r.length > 2) return;
  let i = r[0] ? r[0].split(":") : [], d = r.length === 2 && r[1] ? r[1].split(":") : [], c = r.length === 2 ? d : i, g = [], m = c.at(-1);
  if (m !== void 0 && m.includes(".")) {
    let b = m.split(".").map(Number);
    if (b.length !== 4 || b.some((k) => !Number.isInteger(k) || k < 0 || k > 255)) return;
    g = b; c.pop();
  }
  let f = (b) => {
    let k = [];
    for (let x of b) {
      if (!/^[0-9a-f]{1,4}$/.test(x)) return;
      let R = parseInt(x, 16); k.push(R >> 8, R & 255);
    }
    return k;
  };
  let h = f(r.length === 2 ? i : []), y = f(c);
  if (h === void 0 || y === void 0) return;
  let S = h.length + y.length + g.length;
  if (S > 16 || r.length === 1 && S !== 16) return;
  return [...h, ...Array(16 - S).fill(0), ...y, ...g];
}
function Er(e) { return so(e) && e[4] === 0 && e[5] === 1; }
function so(e) { return e[0] === 0 && e[1] === 100 && e[2] === 255 && e[3] === 155; }
function Iu(e) {
  let n = [];
  if (e[0] === 32 && e[1] === 2) n.push(e.slice(2, 6));
  let s = e.slice(0, 10).every((g) => g === 0), r = s && e[10] === 255 && e[11] === 255, i = s && e[10] === 0 && e[11] === 0, d = so(e) && e.slice(4, 12).every((g) => g === 0), c = (e[8] === 0 || e[8] === 2) && e[9] === 0 && e[10] === 94 && e[11] === 254;
  if (r || i || d || c) n.push(e.slice(12, 16));
  return n;
}
function JQ(e) {
  let n = e.toLowerCase().replace(/^\[|\]$/g, "");
  if (n.endsWith(".")) n = n.slice(0, -1);
  if (n === "" || n === "localhost" || n.endsWith(".localhost")) return true;
  if (Mu.has(n)) return true;
  if (n.startsWith("instance-data.") && n.endsWith(".compute.internal")) return true;
  if (xu(n)) {
    if (yr.has(n)) return true;
    let [r = 0, i = 0, d = 0, c = 0] = n.split(".").map(Number);
    return _r(r, i, d, c);
  }
  if (!Sr(n)) return false;
  let s = br(n);
  if (s === void 0) return true;
  if (Er(s)) return true;
  if (s.every((r) => r === 0)) return true;
  if (s.slice(0, 15).every((r) => r === 0) && s[15] === 1) return true;
  if (n === "fd00:ec2::254") return true;
  if (s[0] === 254 && (s[1] ?? 0) >= 128 && (s[1] ?? 0) <= 191) return true;
  return Iu(s).some((r) => {
    let [i = 0, d = 0, c = 0, g = 0] = r;
    return _r(i, d, c, g) || yr.has(i + "." + d + "." + c + "." + g);
  });
}

// Source Rdt, no flags and no trimming before the prefix test.
function Rdt(e) {
  if (!/^https?:\/\/[^/\\]/i.test(e)) return false;
  try {
    let { protocol } = new URL(e);
    return protocol === "https:" || protocol === "http:";
  } catch {
    return false;
  }
}

// Source XAr: new URL, HTTPS only, then JQ(hostname).
function archiveRefine(e) {
  try {
    let n = new URL(e);
    if (n.protocol !== "https:") return { valid: false, hostnameBlocked: null };
    let hostnameBlocked = JQ(n.hostname);
    return { valid: !hostnameBlocked, hostnameBlocked };
  } catch {
    return { valid: false, hostnameBlocked: null };
  }
}

// Source Zod $ZodURL no-options path: n=input.trim(); new URL(n); return
// qs(n), where qs removes HT/LF/CR, unless normalize is enabled. Native calls
// here do not pass normalize or protocol options.
const removeUrlWhitespace = /[\t\n\r]/g;
function observeUrl(input) {
  const rawRdtValid = Rdt(input);
  const rawArchive = archiveRefine(input);
  const trimmed = input.trim();
  let parsed;
  try {
    parsed = new URL(trimmed);
  } catch {
    return {
      input_units: toUnits(input),
      raw_rdt_valid: rawRdtValid,
      raw_archive_refine_valid: rawArchive.valid,
      raw_archive_hostname_blocked: rawArchive.hostnameBlocked,
      zod_url: { kind: "invalid" },
    };
  }
  const zodValue = trimmed.replace(removeUrlWhitespace, "");
  const parsedArchive = archiveRefine(zodValue);
  return {
    input_units: toUnits(input),
    raw_rdt_valid: rawRdtValid,
    raw_archive_refine_valid: rawArchive.valid,
    raw_archive_hostname_blocked: rawArchive.hostnameBlocked,
    zod_url: {
      kind: "valid",
      zod_value_units: toUnits(zodValue),
      href_units: toUnits(parsed.href),
      protocol_units: toUnits(parsed.protocol),
      username_units: toUnits(parsed.username),
      password_units: toUnits(parsed.password),
      host_units: toUnits(parsed.host),
      hostname_units: toUnits(parsed.hostname),
      port_units: toUnits(parsed.port),
      pathname_units: toUnits(parsed.pathname),
      search_units: toUnits(parsed.search),
      hash_units: toUnits(parsed.hash),
      origin_units: toUnits(parsed.origin),
      rdt_zod_value_valid: Rdt(zodValue),
      archive_zod_refine_valid: parsedArchive.valid,
      archive_zod_hostname_blocked: parsedArchive.hostnameBlocked,
    },
  };
}

// Exact kV+df source semantics from src_186231195.js, including Intl.Segmenter.
function df(text) {
  let n = /[\p{Cc}\p{Cf}\p{Default_Ignorable_Code_Point}\u2028\u2029]/gu;
  if (!n.test(text)) return text.replace(/\n/g, "\u2424").replace(/\t/g, "\u2409");
  let e = "";
  for (let { segment: r } of new Intl.Segmenter(void 0, { granularity: "grapheme" }).segment(text)) {
    let i = /\p{Extended_Pictographic}|\p{Regional_Indicator}|^[#*0-9]\uFE0F?\u20E3$/u.test(r);
    let o = /^\u{1F3F4}[\u{E0030}-\u{E0039}\u{E0061}-\u{E007A}]{1,6}\u{E007F}$/u.test(r);
    n.lastIndex = 0;
    e += r.replace(n, (u, c) => {
      if (u === "\n" || u === "\t") return u;
      let s = u.codePointAt(0) ?? 0;
      if (i && (u === "\uFE0F" || u === "\uFE0E" || u === "\u20E3" || u === "\u200D" && c + u.length < r.length || o && s >= 917536 && s <= 917631)) return u;
      if (s === 27) return "\u241B";
      if (s < 32) return String.fromCodePoint(9216 + s);
      if (s === 127) return "\u2421";
      return "\\u{" + s.toString(16).toUpperCase() + "}";
    });
  }
  return e.replace(/\n/g, "\u2424").replace(/\t/g, "\u2409");
}

function inspectPattern(patternUnits) {
  const units = requireUnits(patternUnits, "pattern_units");
  const pattern = fromUnits(units);
  try {
    const regexp = new RegExp(pattern);
    const captureCount = new RegExp(regexp.source + "|").exec("").length - 1;
    return {
      pattern_units: units,
      outcome: {
        kind: "valid",
        source_units: toUnits(regexp.source),
        capture_count: captureCount,
      },
    };
  } catch (error) {
    return {
      pattern_units: units,
      outcome: {
        kind: "invalid",
        error_message_units: toUnits(String(error.message)),
      },
    };
  }
}

function handleRequest(request) {
  if (request === null || typeof request !== "object" || Array.isArray(request)) {
    throw new TypeError("request must be an object");
  }
  const id = request.id;
  if (!Number.isSafeInteger(id) || id < 0) throw new TypeError("request id must be a nonnegative safe integer");
  if (request.action === "ping") {
    return {
      id,
      ok: true,
      runtime: "bun",
      bun_version: Bun.version,
      bun_revision: Bun.revision,
      unicode_version: process.versions.unicode,
      icu_version: process.versions.icu ?? null,
      protocol_version: PROTOCOL_VERSION,
      worker_id: WORKER_ID,
    };
  }
  if (request.action === "validate_patterns") {
    if (!Array.isArray(request.patterns)) throw new TypeError("patterns must be an array");
    return { id, ok: true, rows: request.patterns.map(inspectPattern) };
  }
  if (request.action === "render_policy_warnings") {
    if (!Array.isArray(request.messages)) throw new TypeError("messages must be an array");
    if (request.filter_units !== null && !Array.isArray(request.filter_units)) {
      throw new TypeError("filter_units must be null or a UTF-16 code-unit array");
    }
    const messages = request.messages.map((value) => requireUnits(value, "warning_message_units"));
    const filterUnits = request.filter_units === null
      ? null
      : requireUnits(request.filter_units, "debug_filter_units");
    return {
      id,
      ok: true,
      rows: messages.map((input_units) => ({
        input_units,
        line_units: renderNativeWarning(input_units, filterUnits),
      })),
    };
  }
  if (request.action === "observe_semantics") {
    if (!Array.isArray(request.url_inputs)) throw new TypeError("url_inputs must be an array");
    if (!Array.isArray(request.display_inputs)) throw new TypeError("display_inputs must be an array");
    const urlInputs = request.url_inputs.map((value) => requireUnits(value, "url_input_units"));
    const displayInputs = request.display_inputs.map((value) => requireUnits(value, "display_input_units"));
    return {
      id,
      ok: true,
      url_rows: urlInputs.map((input) => observeUrl(fromUnits(input))),
      display_rows: displayInputs.map((input) => ({
        input_units: input,
        output_units: toUnits(df(fromUnits(input))),
      })),
    };
  }
  throw new TypeError("unsupported action: " + String(request.action));
}

const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });
for await (const line of lines) {
  let id = null;
  try {
    const request = JSON.parse(line);
    id = request?.id ?? null;
    process.stdout.write(JSON.stringify(handleRequest(request)) + "\n");
  } catch (error) {
    process.stdout.write(JSON.stringify({
      id,
      ok: false,
      error_name: String(error.name),
      error_message: String(error.message),
    }) + "\n");
  }
}
