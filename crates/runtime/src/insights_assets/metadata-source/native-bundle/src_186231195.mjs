// Extracted source closure for 2.1.295; source module src_186231195.js sha256 125f57566b99b67ce958d77cd787163124654cbbb3bdd6f3a92dfd38955a2317
function ml(t){return t.replace(/[.*+?^${}()|[\]\\]/g,"\\$&")}
function Sa(t){return t.charAt(0).toUpperCase()+t.slice(1)}
function I(t,n,e=n+"s"){return t===1?n:e}
function ne(t,n){if(n<=0)return"";if(t.length<=n)return t;let e=t.slice(0,n),r=e.charCodeAt(n-1);return GHt(r>=55296&&r<=56319?e.slice(0,-1):e)}
function GHt(t){if(typeof Buffer<"u")return Buffer.from(t,"utf16le").toString("utf16le");return x(t)}
function x(t){let e=[];for(let r=0;r<t.length;r+=8192){let i=Math.min(r+8192,t.length),o=new Uint16Array(i-r);for(let u=r;u<i;u++)o[u-r]=t.charCodeAt(u);e.push(String.fromCharCode(...o))}return e.join("")}
var m=/[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/;
var a=/[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/g;
var l=typeof String.prototype.isWellFormed==="function"?Function.prototype.call.bind(String.prototype.isWellFormed):void 0;
var g=typeof String.prototype.toWellFormed==="function"?Function.prototype.call.bind(String.prototype.toWellFormed):void 0;
function Yc(t){if(l)return l(t);return!m.test(t)}
function cf(t){if(g)return g(t);return t.replace(a,"\uFFFD")}
function Lw(t){if(l&&l(t))return t;return t.replace(a,"")}
function t1e(t){let n=!1,e=[t];while(e.length>0){let r=e.pop();if(Array.isArray(r))for(let i=0;i<r.length;i++){let o=r[i];if(typeof o==="string"){if(!Yc(o))r[i]=cf(o),n=!0}else if(o!==null&&typeof o==="object")e.push(o)}else if(r!==null&&typeof r==="object"){let i=r;for(let o of Object.keys(i)){let u=i[o];if(typeof u==="string"){if(!Yc(u))i[o]=cf(u),n=!0}else if(u!==null&&typeof u==="object")e.push(u)}}}return n}
function Ze(t,n){let e=t.indexOf(n);return e===-1?t:t.slice(0,e)}
function Ut(t,n,e=0){let r=0,i=t.indexOf(n,e);while(i!==-1)r++,i=t.indexOf(n,i+1);return r}
function kV(t){let n=/[\p{Cc}\p{Cf}\p{Default_Ignorable_Code_Point}\u2028\u2029]/gu;if(!n.test(t))return t;let e="";for(let{segment:r}of new Intl.Segmenter(void 0,{granularity:"grapheme"}).segment(t)){let i=/\p{Extended_Pictographic}|\p{Regional_Indicator}|^[#*0-9]\uFE0F?\u20E3$/u.test(r),o=/^\u{1F3F4}[\u{E0030}-\u{E0039}\u{E0061}-\u{E007A}]{1,6}\u{E007F}$/u.test(r);n.lastIndex=0,e+=r.replace(n,(u,c)=>{if(u===`
`||u==="\t")return u;let s=u.codePointAt(0)??0;if(i&&(u==="\uFE0F"||u==="\uFE0E"||u==="\u20E3"||u==="\u200D"&&c+u.length<r.length||o&&s>=917536&&s<=917631))return u;if(s===27)return"\u241B";if(s<32)return String.fromCodePoint(9216+s);if(s===127)return"\u2421";return`\\u{${s.toString(16).toUpperCase()}}`})}return e}
function df(t){return kV(t).replace(/\n/g,"\u2424").replace(/\t/g,"\u2409")}
export { I as I };
export { t1e as t1e };
export { ne as ne };
export { Ut as Ut };
export { Ze as Ze };
export { Sa as Sa };
export { ml as ml };
export { Yc as Yc };
export { df as df };
export { Lw as Lw };
