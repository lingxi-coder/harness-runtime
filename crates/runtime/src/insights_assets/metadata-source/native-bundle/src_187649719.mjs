// Extracted source closure for 2.1.295; source module src_187649719.js sha256 8812b6cd3f092a1858fd62522d70783cc99bd083adb986e54d2883b640731c5f
import { De, q, B } from "./src_186096608.mjs";
import { join as D } from "path";
import { a } from "./src_186481843.mjs";
import { mkdirSync as g, openSync as pe, constants as T, lstatSync as me, fstatSync as fe, fchmodSync as le, closeSync as ae } from "fs";
import { E, x } from "./src_186056234.mjs";
import { ee } from "./src_187644646.mjs";
import { rPe, jHt } from "./src_186240709.mjs";
import { mT, go, er } from "./src_187620677.mjs";
function Que(e){let n=mT(e);return go(n)?er:n}
function QAo(e){let n=e.trimStart(),r=/^([A-Za-z][A-Za-z0-9+.-]*):\/\//.exec(n);if(r===null||/^https?$/i.test(r[1]??""))return rPe(n);return rPe(r[0]+jHt(n.slice(r[0].length)))}
function Og(){let e=a.CLAUDE_CODE_TMPDIR;if(e)return e;return"/tmp"}
var wRr=44;
function xxe(e){let n=process.getuid?.();if(n===void 0)return;let r=e.replace(/[\/]+$/,"")||e,o="Set CLAUDE_CODE_TMPDIR to a directory you control, or ask an administrator to remove it.",i;try{i=pe(r,T.O_RDONLY|T.O_DIRECTORY|T.O_NOFOLLOW)}catch(s){let c=E(s);if(c==="ELOOP"||c==="ENOTDIR")throw Error(`Temp directory ${e} is not a directory (may be an attacker-planted symlink). Refusing to use it. ${o}`);if(c==="EACCES"){let d;try{let u=me(r);if(u.uid!==n)d=u.uid}catch{}if(d!==void 0)throw new x(`Temp directory ${e} is owned by uid ${d}, expected ${n}. Refusing to use it \u2014 another user may have pre-created it. ${o}`,"temp dir owned by another uid (EACCES at open); refusing to use it");throw new x(`Temp directory ${e} is not readable (its mode may have been altered, or a path component denies search). Refusing to use it \u2014 restore its permissions (chmod 0700) or remove it. ${o}`,"temp dir unreadable (owner-read or path search bit missing); refusing to use it")}throw s}try{let s=fe(i);if(s.uid!==n){if(n===0&&a.CLAUDE_CODE_CONTAINER_ID){ee("warn","tempdir_owner_mismatch",{observed_uid:s.uid});return}throw Error(`Temp directory ${e} is owned by uid ${s.uid}, expected ${n}. Refusing to use it \u2014 another user may have pre-created it. ${o}`)}if((s.mode&511)!==448)le(i,448)}finally{ae(i)}}
class V{ensured=void 0;childProcessTmpDirMemo=void 0;markEnsured(e){this.ensured=e}setChildProcessTmpDirMemo(e){this.childProcessTmpDirMemo=e}}
var j=new q(()=>new V);
function Y(){return`claude-${process.getuid?.()??0}`}
function ec(){return G(j.of(B().host))}
function G(e){if(De())throw Error("The temp directory is unavailable in a diskless session: nothing is created, verified or written under the shared per-uid temp root");let n=D(Og(),Y());if(n!==e.ensured){if(typeof process.getuid==="function")g(n,{recursive:!0,mode:448}),xxe(n);else try{g(n,{recursive:!0,mode:448})}catch{}e.markEnsured(n)}return n}
function Ixe(){let e=j.of(B().host),n=G(e);if(Buffer.byteLength(n)<=wRr)return n;let r="/tmp",o=e.childProcessTmpDirMemo;if(o?.forDir===n)return o.result;let i=D(r,Y()),s=i;try{g(i,{recursive:!0,mode:448}),xxe(i)}catch{s=n}return e.setChildProcessTmpDirMemo({forDir:n,result:s}),s}
export { ec as ec };
export { Ixe as Ixe };
export { QAo as QAo };
export { Que as Que };
