// Extracted source closure for 2.1.295; source module src_188142797.js sha256 b0f1bf3e8dd1dfd5b221eb502dc9c73649eccfbf33ed20de2f335d953971da18
import { O } from "./src_186477637.mjs";
import { normalize as ue, parse as me, resolve as j, isAbsolute as H, dirname as b, join as N, basename as M } from "path";
import { q, B } from "./src_186096608.mjs";
import { closeSync as k, constants as w, openSync as _, fstatSync as D, readSync as de } from "fs";
import { m } from "./src_186441656.mjs";
import { ie, t, ah } from "./src_186240709.mjs";
import { readlink as fe, realpath as Q, open as A, lstat as J } from "fs/promises";
import { E, W, l, x } from "./src_186056234.mjs";
import { randomBytes as Z } from "crypto";
import { FUe, os, fT, K$, GWn, ETo } from "./src_186893596.mjs";
class ne{identities=new Map;record(e,n,r,s){let u=X(e),i=this.identities.get(u);if(i)try{k(i.fd)}catch{}this.identities.set(u,{dev:n,ino:r,fd:s})}identity(e){return this.identities.get(X(e))}reset(){for(let{fd:e}of this.identities.values())try{k(e)}catch{}this.identities.clear()}}
var pe=new q(()=>new ne);
function K(){return pe.of(B().host)}
function X(e){return j(e)}
function DIs(e,n,r,s){K().record(e,n,r,s)}
function yHt(e){if(!e)return;let n=K().identity(e);if(!n)return;let r;try{r=_(e,w.O_RDONLY|w.O_DIRECTORY|w.O_NOFOLLOW)}catch(s){let u=E(s);if(u==="ENOENT"||u==="ENOTDIR"||u==="ELOOP"||u==="EACCES")uut(`Staging dir ${e} was established for a sandboxed command but is now unopenable (${u}) \u2014 refusing atomic write`);throw s}try{let s=D(r);if(s.dev!==n.dev||s.ino!==n.ino)uut(`Staging dir ${e} identity changed (expected ${n.dev}/${n.ino}, found ${s.dev}/${s.ino}) \u2014 refusing atomic write`)}finally{k(r)}}
function MRr(e,n,r,s){let u=`${n}${r}`;if(!e)return u;let i=K().identity(e);if(s&&b(n)!==b(e))return yHt(e),u;let o=w.O_RDONLY|w.O_DIRECTORY|w.O_NOFOLLOW;try{k(_(b(e),o))}catch(y){let h=E(y);if(h==="ELOOP"||h==="ENOTDIR"){if(i)uut(`Staging dir parent ${b(e)} is ${h} but a sandboxed command established ${e} \u2014 refusing sibling fallback`);if(s)return u;throw new gg(`Refusing to stage atomic write under non-directory parent: ${b(e)}`)}if(!W(y))throw y;if(i)uut(`Staging dir parent ${b(e)} is absent but a sandboxed command established ${e} \u2014 refusing sibling fallback`);return u}let f;try{f=_(e,o)}catch(y){let h=E(y);if(h==="ENOENT"||h==="ENOTDIR"||h==="ELOOP"){if(i)uut(`Staging dir ${e} is ${h} but a sandboxed command established it \u2014 refusing sibling fallback`);return u}throw y}try{if(i){let y=D(f);if(y.dev!==i.dev||y.ino!==i.ino)uut(`Staging dir ${e} identity changed (expected ${i.dev}/${i.ino}, found ${y.dev}/${y.ino}) \u2014 refusing atomic write`)}}finally{k(f)}return N(e,`${M(n)}${r}`)}
class gg extends Error{constructor(e){super(e);this.name="SymlinkWriteRefusedError"}}
class DWn extends gg{constructor(e,n){super(ETo(e,n));this.name="HardLinkWriteRefusedError"}}
function re(e){return{flags:w.O_WRONLY|w.O_CREAT|e,flagsIfMissing:void 0}}
class DRr extends Error{constructor(e){super(e);this.name="StagingDirTamperedError"}}
function uut(e){throw m("sandbox_exec","atomic_write_staging_dir_tampered"),new DRr(e)}
function A9e(e,{maxBytes:n,symlinkAtPath:r="follow",regularFileOnly:s=r==="refuse"}){using u=ah`fs.readBoundedSync(${e}, max ${n} bytes)`;let i="r";if(r==="refuse")i=w.O_RDONLY|w.O_NOFOLLOW|w.O_NONBLOCK;else if(s)i=w.O_RDONLY|w.O_NONBLOCK;let o=_(e,i);try{if(s){if(!D(o).isFile())throw Object.assign(Error("EINVAL: not a regular file"),{code:"EINVAL"})}return oe(o,e,n)}finally{k(o)}}
function LIs(e,{maxBytes:n}){using r=ah`fs.readInheritedFdSync(fd ${e}, max ${n} bytes)`;let s=D(e);if(!s.isSocket()&&!s.isFIFO())throw new x(`refusing to read fd ${e}: not a pipe or socket`,"readInheritedFdSync: descriptor is not a pipe or socket");return he(e,n)}
function LRr(e){try{return D(e).isSocket()}catch{return!1}}
function he(e,n){try{return oe(e,`fd ${e}`,n,{untilNewline:!0})}finally{k(e)}}
function oe(e,n,r,{untilNewline:s=!1}={}){let u=[],i=0,o=Buffer.alloc(8192);while(!0){let f=de(e,o,0,o.length,null);if(f===0)return Buffer.concat(u).toString("utf8");if(i+=f,i>r)throw new x(`refusing to read ${n}: content exceeds ${r} byte limit`,"readBoundedSync: content exceeds byte limit");let y=s?o.subarray(0,f).indexOf(10):-1;if(u.push(Buffer.from(o.subarray(0,y===-1?f:y+1))),y!==-1)return Buffer.concat(u).toString("utf8")}}
function U(e,n){if(e instanceof Error&&!(e instanceof gg)&&e!==n)try{if(e.cause===void 0)e.message+=` (atomic write failed first: ${l(n)})`,Object.defineProperty(e,"cause",{value:n,writable:!0,configurable:!0,enumerable:!1})}catch{}throw e}
function ae(e){return e===void 0?void 0:e&511|128}
async function QC(e,n,r={encoding:"utf-8"}){let s=ie(),u=r.allowSymlink?0:w.O_NOFOLLOW,i=e,o,f=!1;if(r.allowSymlink)try{let d=await fe(e);i=H(d)?d:j(await Q(b(e)),d),t(`Writing through symlink: ${e} -> ${i}`)}catch{}else{if(r.checkParentDir)try{await(await A(b(e),w.O_RDONLY|w.O_DIRECTORY|w.O_NOFOLLOW)).close()}catch(d){let S=E(d);if(S==="ELOOP"||S==="ENOTDIR")throw new gg(`Refusing to write into symlinked directory: ${b(e)}`)}try{let d=await J(e);if(d.isSymbolicLink())throw new gg(`Refusing to write through symlink: ${e}. Resolve the symlink and pass the real target path explicitly.`);o=d.mode,f=!0}catch(d){if(!W(d))throw d}}let y=`.tmp.${process.pid}.${Z(6).toString("hex")}`,h=MRr(r.stagingDir,i,y,r.allowSymlink??!1),L=!1;if(r.allowSymlink&&!f)try{o=(await s.stat(i)).mode,f=!0}catch(d){if(!W(d))throw d}if(f&&o!==void 0)t(`Preserving file permissions: ${o.toString(8)}`);else if(r.mode!==void 0)o=r.mode,t(`Setting permissions for new file: ${o.toString(8)}`);try{t(`Writing to temp file: ${h}`);let d=await A(h,w.O_WRONLY|w.O_CREAT|w.O_EXCL|u,f?ae(o):r.mode),S=!1,v;try{if(b(h)!==b(i))yHt(r.stagingDir);if(await d.writeFile(n,{encoding:r.encoding}),f&&o!==void 0)try{await d.chmod(o),t("Applied original permissions to temp file")}catch(g){if(!FUe(g))throw g;t(`fchmod unsupported on this filesystem: ${g}`)}try{await d.sync()}catch(g){if(!FUe(g))throw g;t(`fsync unsupported on this filesystem: ${g}`)}L=!0}catch(g){S=!0,v=g}try{await d.close()}catch(g){if(!S)throw g;t(`close also failed after temp write error: ${g}`,{level:"error"})}if(S)throw v;if(t(`Temp file written successfully, size: ${n.length} bytes`),b(h)!==b(i))yHt(r.stagingDir);t(`Renaming ${h} to ${i}`),await os(h,i,(g,R)=>s.rename(g,R)),t(`File ${i} written atomically`)}catch(d){t(`Failed to write file atomically: ${d}`,{level:"error"});let S=E(d);if(L&&S!==void 0&&fT.has(S)||!L&&f&&S==="EACCES"){let{flags:R,flagsIfMissing:T}=re(u),C=!f&&r.mode!==void 0?r.mode:void 0,F;try{try{F=await A(i,R,C)}catch(p){if(T===void 0||!W(p))throw p;await K$(i),F=await A(i,T,C)}}catch(p){try{await s.unlink(h)}catch(I){t(`Failed to clean up temp file: ${I}`)}if(E(p)==="ELOOP")throw new gg(`Refusing to write through symlink: ${i} (O_NOFOLLOW)`);throw d}try{let p=await F.stat();if(p.nlink>1&&(r.refuseHardLinkedInPlace??GWn()))throw new DWn(i,p.nlink);if(p.isFile())await F.truncate(0)}catch(p){await F.close().catch(()=>{});try{await s.unlink(h)}catch(I){t(`Failed to clean up temp file: ${I}`)}U(p,d)}try{await F.writeFile(n,{encoding:r.encoding});try{await F.sync()}catch(p){if(!FUe(p))throw p;t(`fsync unsupported on this filesystem: ${p}`)}await F.close();try{await s.unlink(h)}catch(p){t(`Failed to clean up temp file: ${p}`)}t(`File ${i} written via in-place fallback`);return}catch(p){try{await F.close()}catch{}try{await s.unlink(i)}catch{}if(L)throw new x(`Write to ${i} failed (${E(p)??p}) after the target was truncated. The new content was preserved at ${h}.`,"writeFileAndFlush: in-place fallback write failed; content preserved at temp path");U(p,d)}}try{await s.unlink(h)}catch(R){t(`Failed to clean up temp file: ${R}`)}throw d}}
function Yu(e){let n=O()==="windows",r=ue(e),s=n?/[\\/]+$/:/\/+$/,u=r.length>me(r).root.length?r.replace(s,""):r;return n?u.replaceAll("/","\\").toLowerCase():u}
function dD(e,n){return Yu(e)===Yu(n)}
export { Yu as Yu };
export { DIs as DIs };
export { uut as uut };
export { QC as QC };
export { dD as dD };
export { gg as gg };
export { A9e as A9e };
export { LRr as LRr };
export { LIs as LIs };
