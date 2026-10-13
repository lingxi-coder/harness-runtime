// Extracted source closure for 2.1.295; source module src_187646551.js sha256 0ac857cb2962506d3f229be3cb9a7bad6e5edccd145e08a95c9d4d8ae0cebd11
import { ie, ho, t, yY, hD } from "./src_186240709.mjs";
import { open as l } from "fs/promises";
function u(n,e,i){Wxe(n.statSync(e),e,i)}
function Wxe(n,e,i){if(n.isDirectory())throw Object.assign(Error("EISDIR: illegal operation on a directory, read"),{code:"EISDIR",errno:-21,syscall:"read",path:e});if(!n.isFile())throw Object.assign(Error("Not a regular file (device, FIFO, or socket)"),{code:"ERR_NOT_REGULAR_FILE",path:e});if(i!==void 0&&n.size>i)throw Object.assign(Error("File exceeds maxBytes limit"),{code:"ERR_FILE_TOO_LARGE",path:e,size:n.size,maxBytes:i})}
function VRr(n,e,i){if(i===void 0)return;if(n>i)throw Object.assign(Error("File exceeds maxBytes limit"),{code:"ERR_FILE_TOO_LARGE",path:e,size:n,maxBytes:i})}
function qB(n){return n!=null&&typeof n==="object"&&"code"in n&&n.code==="ERR_FILE_TOO_LARGE"}
function jWn(n){if(n.byteLength===0)return"utf8";if(n.byteLength>=2){if(n[0]===255&&n[1]===254)return"utf16le"}if(n.byteLength>=3&&n[0]===239&&n[1]===187&&n[2]===191)return"utf8";return"utf8"}
function Rbe(n){let e=jWn(n.subarray(0,4096));return Buffer.from(n.buffer,n.byteOffset,n.byteLength).toString(e).replaceAll(`\r
`,`
`)}
function STo(n){let{buffer:e,bytesRead:i}=ie().readSync(n,{length:4096});return jWn(e.subarray(0,i))}
function WWn(n){let e=0,i=0;for(let r=0;r<n.length;r++)if(n[r]===`
`)if(r>0&&n[r-1]==="\r")e++;else i++;return e>i?"CRLF":"LF"}
function bTo(n,e){let i=ie(),{resolvedPath:r,isSymlink:s}=ho(i,n);if(s)t(`Reading through symlink: ${n} -> ${r}`);u(i,n,e);let o=STo(n),c;if(e===void 0)c=i.readFileSync(n,{encoding:o});else{let{buffer:g,bytesRead:a}=i.readSync(n,{length:e+1});VRr(a,n,e),c=g.subarray(0,a).toString(o)}let d=WWn(c.slice(0,4096));return{content:c.replaceAll(`\r
`,`
`),encoding:o,lineEndings:d}}
function z$(n,e){return bTo(n,e).content}
async function __(n,e){let i=ie(),{resolvedPath:r,isSymlink:s}=ho(i,n);if(s)t(`Reading through symlink: ${n} -> ${r}`);Wxe(await i.stat(n),n,e);await using o=await l(n,yY());return await xbe(o,n,e)}
async function xbe(n,e,i){Wxe(await n.stat(),e,i);let r=i===void 0?await n.readFile():await hD(n,i+1,"file");VRr(r.length,e,i);let s=jWn(r.subarray(0,4096)),o=r.toString(s),c=WWn(o.slice(0,4096));return{content:o.replaceAll(`\r
`,`
`),encoding:s,lineEndings:c}}
export { __ as __ };
export { z$ as z$ };
export { Rbe as Rbe };
export { qB as qB };
