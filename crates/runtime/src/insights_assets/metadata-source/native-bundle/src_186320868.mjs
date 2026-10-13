// Extracted source closure for 2.1.295; source module src_186320868.js sha256 9b624c3c6cbdc2968341a83e641007417e67e54f2d1de0cc775250bcae99f084
import { ne, Lw } from "./src_186231195.mjs";
import { wn } from "./src_186240709.mjs";
var i=/(?<![^\s\p{P}])\p{M}+/gu;
function mH(e){return wn(Ann(Lw(e))).replace(/ {2,}/g," ").trim()}
var a=/\x1b\[[\x30-\x3f]*[\x20-\x2f]*[\x40-\x7e]|\x1b[\]PX^_][^\x1b\x07]*(?:\x07|\x1b\\)/g;
var d=4;
function Ann(e){let n=e;for(let t=0;t<d;t++){let r=n.replace(a,"");if(r===n)break;n=r}return n}
function Et(e,n=160){return U3(mH(g(e,n)).normalize("NFC").replace(/[`\uff40\u02cb\u1fef\u2035]/g,"'").replace(i,""),n)}
function U3(e,n=2000){return e.length>n?`${ne(e,n)}\u2026`:e}
function g(e,n){let t=ne(e,n*8);if(t.length===e.length)return t;let r=/\x1b(?:[\]PX^_][^\x1b\x07]*\x1b?|\[[\x30-\x3f]*[\x20-\x2f]*)$/.exec(t);if(r===null)return t;let u=e.slice(r.index);return(u[1]==="["?/^\x1b\[[\x30-\x3f]*[\x20-\x2f]*[\x40-\x7e]/.test(u):/^\x1b[\]PX^_][^\x1b\x07]*(?:\x07|\x1b\\)/.test(u))?t.slice(0,r.index):t}
export { Et as Et };
