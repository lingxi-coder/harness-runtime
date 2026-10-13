// Extracted source closure for 2.1.295; source module src_193940785.js sha256 aa5ff8b2fd16420732995847d3ff2f77ec463eccd2fa7ea4b2837a1a80d0d35b
import { wS } from "./src_190872709.mjs";
import { Lce, cKe, fyr, jst, pyr, myr, CDn, ADn, gyr, GTt, zTt } from "./src_190934551.mjs";
function S9t(e){return fyr.test(e)||F0n(e)}
function F0n(e){return jst.test(e)||pyr.test(e)||myr.test(e)||CDn.test(e)||ADn.test(e)||gyr.test(e)}
function uqe(e){return e.type==="ERROR"||e.children.some(uqe)}
var m=new Set(["program","list","pipeline"]);
var E=new Set(["&&","||","|",";","&","|&",`
`]);
var v0=1e4;
function Vg(e){if(!e)return[];if(e.length>v0)return[e];let t=wS().parse(e);if(!t)return[e];let r=[],s=(i)=>{if(E.has(i.type)||i.type==="comment")return;if(i.type==="redirected_statement"){for(let n of i.children)if(!n.type.endsWith("_redirect"))s(n);return}if(m.has(i.type)){for(let n of i.children)s(n);return}r.push(i.text)};return s(t.type==="ERROR"&&t.children[0]?.type==="program"?t.children[0]:t),r}
function fE(e){if(!e||e.length>v0)return[];let t=wS().parse(e);if(!t)return[];let r=Lce(t,null);if(!r)return[];return cKe(r)}
var x=new Set(["command_substitution","process_substitution","expansion","simple_expansion","arithmetic_expansion"]);
function S(e){if(x.has(e.type))return!0;return e.children.some(S)}
var z=new Set(["ansi_c_string","translated_string"]);
function N(e){if(z.has(e.type))return!0;return e.children.some(N)}
var v=new Set(["<",">",">>","<<","<<-","<<<","<&",">&","&>","&>>",">|",">&-","<&-","file_descriptor","heredoc_start","heredoc_body","heredoc_content","heredoc_end"]);
function A(e){if(e.type.endsWith("_redirect")){let t=e.children.filter((n)=>!v.has(n.type)),r=e.children.some((n)=>n.type===">&-"||n.type==="<&-"),s=!r&&e.children.some((n)=>n.type===">&"||n.type==="<&")&&t.some((n)=>y(n).startsWith("-")),i=e.type==="heredoc_redirect"||r||s?0:1;if(t.length>i)return!0}return e.children.some(A)}
function b(e){if(e.type==="heredoc_redirect"){let r=e.children.find((i)=>i.type==="heredoc_start")?.text??"";if(!(r.length>=2&&(r.startsWith("'")&&r.endsWith("'")||r.startsWith('"')&&r.endsWith('"')))||r.includes("\\"))return!0}return e.children.some(b)}
var K=new Set(["word","string","raw_string","number"]);
var Q=/(?:^|[^\\])(?:\\\\)*[;|&<>]/;
var j=/(?:^|[^\\])(?:\\\\)*\\$/;
function w(e,t=!1){if(e.type==="concatenation")return e.children.every((r)=>w(r,t));if(e.type==="word"){if(GTt.test(e.text))return!1;if(Q.test(e.text)||j.test(e.text))return!1;if(t&&zTt.test(e.text))return!1;return!0}if(e.type==="string"||e.type==="raw_string"){let r=e.type==="raw_string"?"'":'"';return e.text.length>=2&&e.text.startsWith(r)&&e.text.endsWith(r)}return K.has(e.type)}
function L(e){let t=null,r=!1,s=!1;for(let i=0;i<e.length;i++){let n=e[i];if(t==="'"){if(n==="'")t=null;continue}if(t==='"'){if(n==="\\"&&i+1<e.length&&'$`"\\'.includes(e[i+1])){i++;continue}if(n==="`")return!0;if(n==="$"&&/[A-Za-z0-9_{(@*#?$!-]/.test(e[i+1]??""))return!0;if(n==='"')t=null;continue}if(n==="\\"){i++;continue}if(n==="`")return!0;if(n==="$"&&(e[i+1]==="'"||e[i+1]==='"'))return!0;if(n==="$"&&/[A-Za-z0-9_{(@*#?$!-]/.test(e[i+1]??""))return!0;if(n==="="&&e[i+1]==="(")return!0;if(n==="*"||n==="?"||n==="[")return!0;if(n==="'"||n==='"'){t=n;continue}if(n===`
`)return!1;if(n===" "||n==="\t"){r=!1,s=!1;continue}if(n==="{"){r=!0;continue}if(r&&(n===","||n==="."&&e[i+1]===".")){s=!0;continue}if(n==="}"&&r&&s)return!0}return t!==null}
function $0n(e){if(!e||e.length>v0)return!0;if(S9t(e))return!0;if(L(e))return!0;let t=wS().parse(e);if(!t||uqe(t))return!0;let r=t.children.filter((i)=>i.type!=="comment");if(r.length!==1||r[0].type!=="command"&&!(r[0].type==="redirected_statement"&&r[0].children.some((i)=>i.type==="command")))return!0;if(S(t)||N(t)||A(t)||b(t))return!0;let s=Lce(t,null);if(!s)return!0;for(let i of s.children){if(i.type==="command_name"||i.type==="variable_assignment")continue;if(i.type.endsWith("_redirect"))continue;if(!w(i,/\s/.test(i.text)))return!0}return!1}
function y(e){switch(e.type){case"raw_string":return e.text.slice(1,-1);case"string":return e.text.slice(1,-1).replace(/\\([$`"\\\n])/g,(t,r)=>r===`
`?"":r);case"word":return e.text.replace(/\\([\s\S])/g,(t,r)=>r===`
`?"":r);default:return e.text}}
function Hye(e){let t={commandWithoutRedirections:e,redirections:[],hasDangerousRedirection:!1,dangerousRedirectionReason:void 0};if(!e||e.length>v0)return t;let r=wS().parse(e);if(!r)return t;let s=[],i=!1,n,o=(l)=>{if(l.type==="file_redirect"){let d=null,g=!1,a=null,C=0;for(let c of l.children)if(c.type===">"||c.type==="&>"||c.type===">|")d=">";else if(c.type===">>"||c.type==="&>>"||c.type===">>|")d=">>";else if(c.type===">&")d=">",g=!0;else if(c.type==="<&"){let _=l.children.filter((p)=>p!==c&&p.type!=="file_descriptor");if(_.length>1||_.some((p)=>y(p).startsWith("-"))){if(i=!0,n!=="network_device")n="shell_expansion"}return}else if(c.type===">&-"||c.type==="<&-"){if(l.children.filter((p)=>p!==c&&p.type!=="file_descriptor").length>0){if(i=!0,n!=="network_device")n="shell_expansion"}return}else if(c.type==="<"){let _=l.children.filter((T)=>T!==c&&T.type!=="file_descriptor");if(_.length>1){if(i=!0,n!=="network_device")n="shell_expansion";return}let p=_[0];if(p){let T=y(p);if(/^\/dev\/(tcp|udp)\//.test(T))i=!0,n="network_device"}return}else if(c.type!=="file_descriptor")a=c,C++;if(!d||!a)return;if(C>1){if(i=!0,n!=="network_device")n="shell_expansion";return}if(g&&y(a).startsWith("-")){if(i=!0,n!=="network_device")n="shell_expansion";return}if(a.type==="number"&&a.children.length===0&&g)return;if(!(a.type==="word"&&a.children.length===0||a.type==="number"&&a.children.length===0||a.type==="raw_string"||a.type==="string"&&!a.children.some((c)=>c.type!=="string_content"&&c.type!=='"'))){if(i=!0,n!=="network_device")n="shell_expansion";return}let h=y(a);if(/^~|[*?[]/.test(h)){if(i=!0,n!=="network_device")n="shell_expansion";return}if(h.startsWith("!")||h.startsWith("=")){if(i=!0,n!=="network_device")n="shell_expansion";return}if(g&&!/^[A-Za-z0-9./_-]+$/.test(h)){if(i=!0,n!=="network_device")n="shell_expansion";return}if(/^\/dev\/(tcp|udp)\//.test(h)){i=!0,n="network_device";return}s.push({target:h,operator:d});return}for(let d of l.children)o(d)};o(r);let u=[],f=(l)=>{if(l.type==="comment")return;if(l.type==="redirected_statement"){for(let d of l.children)if(!d.type.endsWith("_redirect"))f(d);return}if(m.has(l.type)){for(let d of l.children)f(d);return}u.push(l.text)};return f(r.type==="ERROR"&&r.children[0]?.type==="program"?r.children[0]:r),{commandWithoutRedirections:u.length>0?u.join(" "):e,redirections:s,hasDangerousRedirection:i,dangerousRedirectionReason:n}}
export { v0 as v0 };
export { fE as fE };
export { Vg as Vg };
export { Hye as Hye };
export { $0n as $0n };
