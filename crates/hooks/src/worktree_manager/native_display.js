// Claude Code 2.1.322 hash-pinned pure display closure; receipt in work/worktree/display-ranges.json.
const Zbt=1e4;
function Q(t,n){if(n<=0)return"";if(t.length<=n)return t;let e=t.slice(0,n),r=e.charCodeAt(n-1);return t1e(r>=55296&&r<=56319?e.slice(0,-1):e)}function Oa(t,n){if(n<=0)return"";if(t.length<=n)return t;let e=t.slice(-n),r=e.charCodeAt(0);return t1e(r>=56320&&r<=57343?e.slice(1):e)}function h$(t,n,e,r){if(t.length<=n+e)return t;let i=n>0?Q(t,n):"",o=e>0?Oa(t,e):"";return`${i}${r(t.length-i.length-o.length)}${o}`}
function t1e(t){if(typeof Buffer<"u")return Buffer.from(t,"utf16le").toString("utf16le");return m(t)}function m(t){let e=[];for(let r=0;r<t.length;r+=8192){let i=Math.min(r+8192,t.length),o=new Uint16Array(i-r);for(let s=r;s<i;s++)o[s-r]=t.charCodeAt(s);e.push(String.fromCharCode(...o))}return e.join("")}
var b=/[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/,a=/[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/g,l=typeof String.prototype.isWellFormed==="function"?Function.prototype.call.bind(String.prototype.isWellFormed):void 0,g=typeof String.prototype.toWellFormed==="function"?Function.prototype.call.bind(String.prototype.toWellFormed):void 0;
function fg(t){if(l&&l(t))return t;return t.replace(a,"")}
var M8=/[\x00-\x08\x0E-\x1F\x7F-\x9F]/g,D8=/[\x00-\x1F\x7F-\x9F]/,HGt=/^(con|prn|aux|nul|com[0-9\u00B9\u00B2\u00B3]|lpt[0-9\u00B9\u00B2\u00B3])$/;
function ro(t){return t.replace(M8,"").replace(/\s+/g," ").trim()}
var h=/[\u001B\u009B][[\]()#;?]*(?:(?:(?:(?:;[-a-zA-Z\d/#&.:=?%@~_]+)*|[a-zA-Z\d]+(?:;[-a-zA-Z\d/#&.:=?%@~_]*)*)?(?:\u0007|\u001B\u005C|\u009C))|(?:(?:\d{1,4}(?:;\d{0,4})*)?[\dA-PR-TZcf-nq-uy=><~]))/g;
function my(t){return fg(t).replace(h,"").replace(/[\p{Cc}\p{Cf}\u2028\u2029]/gu,"")}
var s=/[\p{Pi}\p{Pf}\u201a\u201e\u201f\u2e32\u2e34\u2e41\u2e49\u2e42\u3003\u300c-\u300f\ufe41-\ufe44\u301d-\u301f\u275b-\u2760\u276e\u276f\u{1f676}-\u{1f678}\u2032-\u2037\u2057\u02b9\u02ba\u0374\u02bd-\u02bf\u02c8\u02d2\u02d3\u02ca\u02ce\u02cf\u02dd\u02f4-\u02f6\u02ee\u00b4\u0384\u0385\u1fbd\u1fbf\u1fcd-\u1fcf\u1fdd-\u1fdf\u1ffd\u1ffe\u05f3\u05f4\u0559-\u055b\u07f4\u07f5\ua67f\ua78b\ua78c\uff02\uff07\uff62\uff63]/gu,o=/(?<![^\s\p{P}])\p{M}+/gu;
function Kd(n,e){let t=ro(my(fg(n).replace(/\s+/g," "))).normalize("NFC").replace(s,"'").replace(/["\u02BB\u02BC]/g,"'").replace(o,"").trim();if(!t)return"";return`\u201C${e?h$(t,e.head,e.tail,()=>"\u2026"):t}\u201D`}
var aD=1024,uD=/\n\n\.\.\. \[(\d+) characters truncated\] \.\.\.\n\n/g;function rzn(e){return`

... [${e} characters truncated] ...

`}function Mu(e,n=Zbt){if(e.length<=n+aD)return e;let r=Math.floor(n/2);return h$(e,r,n-r,(s)=>{let h=e.slice(r,e.length-(n-r)),y=0;for(let b of h.matchAll(uD)){let w=b[1].length<=15?Number(b[1]):Number.NaN;if(Number.isSafeInteger(w)&&w>=b[0].length)y+=w-b[0].length}return rzn(s+y)})}
if(input.phase==="create")return Mu(input.text.trim());
for(const line of input.text.split("\n")){const quoted=Kd(line,{head:60,tail:100});if(quoted)return quoted;}
return "";
