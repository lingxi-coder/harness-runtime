// Extracted source closure for 2.1.295; source module src_186014760.js sha256 6e59541a2ce3295041427a16a6680474643875969a7430e9752f89ed5190c46e
import { realpathSync as r } from "fs";
import { cwd as u } from "process";
function o(n){return n.normalize("NFC")}
function BOo(){let n="";if(typeof process<"u"&&typeof process.cwd==="function"&&typeof r==="function")try{let e=u();try{n=o(r(e))}catch{n=o(e)}}catch{}return n}
var czn=BOo();
var c=(()=>{if(typeof process>"u"||typeof process.cwd!=="function")return null;try{return process.cwd()}catch{return null}})();
function ipt(){if(c===null)return null;let n=process.env.CLAUDE_CODE_SESSION_KIND;return n!==void 0&&String(n).trim()==="bg"?null:c}
var t;
function j(){return t===!0}
export { j as j };
export { BOo as BOo };
export { czn as czn };
export { ipt as ipt };
