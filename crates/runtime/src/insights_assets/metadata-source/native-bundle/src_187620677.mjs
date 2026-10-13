// Extracted source closure for 2.1.295; source module src_187620677.js sha256 2c3111ed122f81b09564595dc7525c83208eeb223dec1125897f3306664bbf1d
import { kO } from "./src_187601633.mjs";
import { Z$ } from "./src_186240709.mjs";
var vTo=new Set(["http","https","ws","wss","ftp"]);
function hV(t){t=t.replace(/^[\x00-\x20]+/,"").replace(/[\t\n\r]/g,"");let n=t.indexOf("://");if(n===-1)return!1;let r=t.slice(n+3),s=t.slice(0,n).toLowerCase();if(vTo.has(s)){let o=r.match(/^[/\\]+/)?.[0]??"";if(o.includes("\\"))return!0;r=r.slice(o.length)}let e=r.search(/[/?#]/);return(e===-1?r:r.slice(0,e)).includes("\\")}
var er="github.com";
var u=/[:/\\?#@\s]/;
function mT(t){let n=c(t.replace(/[\t\n\r]/g,"").toLowerCase());if(n===""||u.test(n))return n;try{let r=new URL(`https://${n}`);if(r.username!==""||r.password!==""||r.port!==""||r.pathname!=="/"||r.search!==""||r.hash!=="")return n;return c(r.hostname)}catch{return n}}
var D9e=kO(function(n){let r=mT(n);while(r.startsWith("www."))r=r.slice(4);return r},(t)=>t,50);
function wHt(t,n){return D9e(t)===n}
function go(t){return wHt(t,er)}
function i(t){return/[%\x00-\x1f\x7f-\u{10FFFF}]/u.test(t)}
function yV(t){if(Z$(t))return!0;if(t.includes("://")){if(hV(t))return!0;try{let e=new URL(t);if(e.protocol==="http:"||e.protocol==="https:")return!1;return i(e.hostname)}catch{return!0}}let n=t.indexOf(":"),r=t.indexOf("@");if(n>=0&&r>n)return!0;let s=t.match(/^(?:[^@]+@)?([^:]+):/)?.[1];return s?i(s):!1}
function c(t){let n=t.length;while(n>0&&t[n-1]===".")n--;return t.slice(0,n)}
export { hV as hV };
export { go as go };
export { mT as mT };
export { er as er };
export { yV as yV };
