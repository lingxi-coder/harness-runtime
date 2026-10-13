// Extracted source closure for 2.1.295; source module src_188187055.js sha256 48830c3905469c5c659d1cdea066f4fd121d3e16c1051890901173ca5f0031eb
import { ja } from "./src_186238496.mjs";
import { sY, mrn, wO, lr } from "./src_187591836.mjs";
import { bHt } from "./src_188118649.mjs";
import { ie } from "./src_186240709.mjs";
import { se } from "./src_188108105.mjs";
import { sep as te, dirname as Se, isAbsolute as ut, join as L, sep as Ve, resolve as ce, dirname as Ye, join as N, resolve as de, basename as $e } from "path";
import { DN, la, Un, il, HO, Wi, Ln } from "./src_186068928.mjs";
import { realpathSync as We, readFileSync as pe, accessSync as Jo, constants as Zo, lstatSync as Rs, readlinkSync as Ps, lstatSync as Wn, statSync as Yn } from "fs";
import { f, g } from "./src_186441656.mjs";
import { E } from "./src_186056234.mjs";
import { q, B, De } from "./src_186096608.mjs";
import { Kc } from "./src_187601633.mjs";
import { ee } from "./src_187644646.mjs";
import { st, Cn, ks } from "./src_188159463.mjs";
import { vN } from "./src_186481843.mjs";
function gi(e){return ja(e,"expected a segment that is not empty or made only of dots and spaces, with no path separator, NUL or set-aside shape")}
function Ig(e){if(typeof e!=="object"||e===null)return ja("key","expected a key object");let n=_i(e);if(n!==void 0)return n;let r=Ji(e);if(r===void 0)return ja("key",`${e.namespace} is not a storage namespace`);return $t("key",r)}
function $t(e,n){for(let[r,i,o]of n){let s=`${e}.${r}`;if(i===void 0){if(o!=="optional")return ja(s,"required")}else if(pi(r)){if(!(e==="scope"&&r==="agentRelPath"&&Array.isArray(i)&&i.length===0)&&(!Array.isArray(i)||!wO(i)))return ja(s,"expected a non-empty array of segments, none empty or made only of dots and spaces, with no path separator, NUL or set-aside shape")}else if(typeof i!=="string"||!lr(i))return gi(s)}return}
function pi(e){return e==="relPath"||e==="agentRelPath"}
var Wt=["commands","agents","output-styles","skills","workflows","routines","themes","rules","session-env","uploads","mcp-skill-archives","usage-data","mcp-discovery-cache"];
var Ut=new Set(Wt);
var Yt=`must be one of the userConfigDir directory names (${Wt.join(", ")})`;
var Vt=["installed","marketplaces","flagged","catalog","inUseSweep"];
var mi=new Set(Vt);
var hi=`must be one of the pluginRegistry files (${Vt.join(", ")})`;
var zt=["manifest","catalog"];
var yi=new Set(zt);
var bi=`must be one of the marketplaceCache forms (${zt.join(", ")})`;
var Xt=["world"];
var wi=new Set(Xt);
var Ei=`must be one of the session journal names (${Xt.join(", ")})`;
function Si(e){return wi.has(e)}
function Ri(e){return typeof e==="string"&&/^[0-9a-f]{64}$/.test(e)}
function _i(e){if(e.namespace==="transcript")return xi(e);if(e.namespace==="pluginAssetCache"&&!Ri(e.digest))return ja("key.digest","must be a SHA-256 digest: 64 lowercase hexadecimal characters");if(e.namespace==="globalConfig"&&"kind"in e){if(!en(e.kind))return ja("key.kind",qt);if(typeof e.stamp!=="string")return ja("key.stamp","a recovery copy key carries its stamp")}if(e.namespace==="task")return Ki(e);if(e.namespace==="sidecar"){let n=Ke("key.sessionId",e.sessionId);if(n!==void 0)return n;if(He(e.relPath))return ja("key.relPath",Ne);return on(e.relPath)?ja("key.relPath",nn):void 0}if(e.namespace==="recording")return Ke("key.sessionId",e.sessionId)??(rn(e.stamp)?void 0:ja("key.stamp",Ci));if(e.namespace==="jobsRoot")return Hi(e);if(e.namespace==="userConfigDir"&&!Ut.has(e.dir))return ja("key.dir",Yt);if(e.namespace==="fileHistory")return Pi(e);if(e.namespace==="settings"&&!ji(e.layer))return ja("key.layer","must be user, project or local");if(e.namespace==="log")return Mi(e);if(e.namespace==="job"&&pn(e.relPath))return ja("key.relPath",gn);if(e.namespace==="sessionLog")return mn("key",e)??(Vi(e.logName)?void 0:ja("key.logName","must be the session-log stem <sessionId8>[-<title-slug>]: up to eight word characters, then lower-case a-z / 0-9 runs joined by single hyphens; not a bare device name"));if(e.namespace==="pluginRegistry"&&!mi.has(e.file))return ja("key.file",hi);if(e.namespace==="marketplaceCache"){if(!("relPath"in e))return yi.has(e.form)?void 0:ja("key.form",bi);return e.form===void 0?void 0:ja("key.form","a tree file key carries relPath, not form")}if(e.namespace!=="agentMemory")return;if(!Jt(e.layer))return ja("key.layer","must be user, project or local");if(e.layer==="user"&&"projectKey"in e)return ja("key.projectKey","the user layer is not keyed by project");if(e.layer!=="user"&&typeof e.projectKey!=="string")return ja("key.projectKey","required for the project and local layers");return typeof e.agentType==="string"?void 0:ja("key.agentType","an agent memory key names its agent")}
function Pi(e){return typeof e.backupFileName!=="string"||!bHt.test(e.backupFileName)?ja("key.backupFileName","must be a backup file name the engine has ever written (hex hash @v version)"):void 0}
function Jt(e){return e==="user"||e==="project"||e==="local"}
var Zt=["backup","corrupted"];
var ki=new Set(Zt);
var qt=`must be one of the global-config copy kinds (${Zt.join(", ")})`;
function en(e){return ki.has(e)}
function xi(e){let n=Ke("key.sessionId",e.sessionId);if(n!==void 0)return n;if(He(e.agentRelPath))return ja("key.agentRelPath",Ne);if("sessionJournal"in e){if(typeof e.sessionJournal!=="string"||!Si(e.sessionJournal))return ja("key.sessionJournal",Ei);return e.agentId===void 0&&e.agentRelPath===void 0&&!("journal"in e)?void 0:ja("key.sessionJournal","a session journal key names the session's own journal: no agentId, agentRelPath or run journal")}if("journal"in e){if(e.journal!==!0)return ja("key.journal","must be true");if(!Array.isArray(e.agentRelPath))return ja("key.agentRelPath","a run journal key carries its run directory");return e.agentId===void 0?void 0:ja("key.agentId","a transcript key names an agent transcript or the run journal, never both")}return e.agentRelPath!==void 0&&e.agentId===void 0?ja("key.agentRelPath","requires agentId or journal"):void 0}
var Y0t="cloud-snapshots";
var tn=new Set(["memory","tiny_memory","bagel",Y0t,"bridge-pointer.json",".session-aliases"]);
var Ii=/^[0-9]{1,16}$/;
var Ci="must be the recording stamp: 1 to 16 decimal digits (epoch milliseconds)";
var Be=".cast";
var nn=`<stamp>${Be} inside a session's folder is that session's terminal recording stream: address it as keys.recording(projectKey, sessionId, stamp)`;
function rn(e){return typeof e==="string"&&Ii.test(e)}
function on(e){return Array.isArray(e)&&typeof e[0]==="string"&&sY(e[0]).some((n)=>bAo(n)!==void 0)}
function bAo(e){if(!e.endsWith(Be))return;let n=e.slice(0,-Be.length);return rn(n)?n:void 0}
var sn=[".ccr-tip.json",".precompact.json",Be];
var vi=`must not end with ${sn.join(", ")}: those name a session's project-level sibling files`;
function an(e){return sY(e).some((n)=>sn.some((r)=>n.endsWith(r)))}
var tRr=".dir-sync.json";
var Ai=`must not end with ${tRr}: that name is reserved for a cloud session's record at the project level`;
function Oi(e){return sY(e).some((n)=>n.endsWith(tRr))}
var Li=`${[...tn].join(", ")} are reserved: they name project-level entries, not sessions`;
function ln(e){return sY(e).some((n)=>tn.has(n))}
function Ke(e,n){if(typeof n!=="string")return;if(ln(n))return ja(e,Li);if(an(n))return ja(e,vi);if(Oi(n))return ja(e,Ai);return mrn(n)?ja(e,Ne):void 0}
var Fi=".meta.json";
var Ti=".meta is reserved for the list metadata key, under every spelling that opens its file";
function Di(e){return Ni(`${e}.json`)}
function Ni(e){return sY(e).includes(Fi)}
var Ne="names a .jsonl stream, which only a transcript key addresses";
function He(e){return Array.isArray(e)&&e.some((n)=>typeof n==="string"&&mrn(n))}
function ji(e){return e==="user"||e==="project"||e==="local"}
var cn="must be debug, telemetry or apiDump";
function un(e){return e==="debug"||e==="telemetry"||e==="apiDump"}
function Mi(e){if(!un(e.channel))return ja("key.channel",cn);if(e.channel!=="apiDump")return;for(let n of["agentId","runId"])if(n in e&&e[n]!==void 0)return ja(`key.${n}`,"an apiDump key names its dump by one id \u2014 the agent's for a subagent's requests, else the session's (today's dump-prompts/<id>.jsonl); agentId and runId nest nothing on this channel");return}
function Ki(e){if("taskId"in e&&(("meta"in e)||("highWaterMark"in e)))return ja("key.taskId","a task key names an item, the list metadata or the list high-water mark, never more than one");if("meta"in e&&"highWaterMark"in e)return ja("key.highWaterMark","a task key names an item, the list metadata or the list high-water mark, never more than one");if("meta"in e&&e.meta!==!0)return ja("key.meta","must be true");if("highWaterMark"in e&&e.highWaterMark!==!0)return ja("key.highWaterMark","must be true");if(typeof e.listId!=="string")return ja("key.listId","a task key carries its listId");if("meta"in e||"highWaterMark"in e)return;if(typeof e.taskId!=="string")return ja("key.taskId","a task item key carries its taskId");if(Di(e.taskId))return ja("key.taskId",Ti);return}
function Hi(e){if("file"in e&&"draftKey"in e)return ja("key.draftKey","a jobs-root key names the pins file or one draft, never both");if("file"in e)return e.file==="pins"?void 0:ja("key.file","must be pins");if(typeof e.draftKey!=="string")return ja("key.draftKey","a jobs-root draft key carries its draftKey");return Xi.test(e.draftKey)?void 0:ja("key.draftKey","must be 8 lowercase hex characters")}
var fn="timeline.jsonl";
var gn=`${fn} is the job's timeline stream: address it as keys.jobTimeline(jobId)`;
function pn(e){return Array.isArray(e)&&typeof e[0]==="string"&&sY(e[0]).includes(fn)}
var Gi=/^\d{4}$/;
var Gt=/^\d{2}$/;
var $i=/^[A-Za-z0-9_-]{1,8}(?:-[a-z0-9]+)*$/;
var Ui=128;
var Yi=/^(?:con|prn|aux|nul|com\d|lpt\d)$/i;
function Vi(e){return typeof e==="string"&&e.length<=Ui&&$i.test(e)&&!Yi.test(e)}
function mn(e,n){if(n.year!==void 0&&!nt(Gi,n.year))return ja(`${e}.year`,"must be four digits (YYYY)");if(n.month!==void 0&&!nt(Gt,n.month))return ja(`${e}.month`,"must be two digits (MM)");if(n.day!==void 0&&!nt(Gt,n.day))return ja(`${e}.day`,"must be two digits (DD)");return}
function nt(e,n){return typeof n==="string"&&e.test(n)}
var Xi=/^[0-9a-f]{8}$/;
function Ji(e){switch(e.namespace){case"transcript":return[["projectKey",e.projectKey],["sessionId",e.sessionId],["agentId",e.agentId,"optional"],["agentRelPath",e.agentRelPath,"optional"]];case"history":case"identity":return[];case"globalConfig":return"kind"in e?[["stamp",e.stamp]]:[];case"settings":return e.layer==="user"?[]:e.layer==="project"?[["projectKey",e.projectKey]]:[["consentRootKey",e.consentRootKey]];case"task":return"taskId"in e?[["listId",e.listId],["taskId",e.taskId]]:[["listId",e.listId]];case"memory":return[["projectKey",e.projectKey],["relPath",e.relPath]];case"pluginRegistry":return[];case"marketplaceCache":return"relPath"in e?[["marketplace",e.marketplace],["relPath",e.relPath]]:[["marketplace",e.marketplace]];case"pluginCache":return[["marketplace",e.marketplace],["plugin",e.plugin],["version",e.version],["relPath",e.relPath]];case"cache":return[["store",e.store],["id",e.id]];case"paste":return[["id",e.id]];case"pluginAssetCache":return[["digest",e.digest]];case"state":return[["id",e.id]];case"plan":return[["name",e.name]];case"feedbackDraft":return[["draftId",e.draftId]];case"agentMemory":return[...e.layer==="user"?[]:[["projectKey",e.projectKey]],["agentType",e.agentType],["relPath",e.relPath]];case"team":return[["team",e.team]];case"sidecar":return[["projectKey",e.projectKey],["sessionId",e.sessionId],["relPath",e.relPath]];case"scratch":return[["sessionId",e.sessionId],["relPath",e.relPath]];case"userConfigDir":return[["relPath",e.relPath]];case"fileHistory":return[["sessionId",e.sessionId],["backupFileName",e.backupFileName]];case"job":return[["jobId",e.jobId],["relPath",e.relPath]];case"daemon":return[["relPath",e.relPath]];case"jobsRoot":return"file"in e?[]:[["draftKey",e.draftKey]];case"session":return[["file",e.file]];case"bridgePointer":case"sessionAliases":return[["projectKey",e.projectKey]];case"dirSyncRecord":return[["projectKey",e.projectKey],["sessionId",e.sessionId]];case"mailbox":return[["team",e.team],["teammate",e.teammate]];case"log":return[["sessionId",e.sessionId],["agentId",e.agentId,"optional"],["runId",e.runId,"optional"]];case"jobTimeline":return[["jobId",e.jobId]];case"recording":return[["projectKey",e.projectKey],["sessionId",e.sessionId],["stamp",e.stamp]];case"sessionLog":return[["projectKey",e.projectKey],["year",e.year],["month",e.month],["day",e.day],["logName",e.logName]]}return}
function GPs(e){let n=[],r=[],i=null;for(let o of e.split(`
`)){let s=o.trim();if(s.length===0||s[0]==="#"||s[0]===";")continue;if(s[0]==="["){i=at(s,"include",null)||$o.test(s)?"include":at(s,"core",null)?"core":null;continue}let u=i===null?null:Hn(s);if(!u||u.value.length===0)continue;let d=u.key.toLowerCase();if(i==="include"&&d==="path")n.push(u.value);else if(i==="core"&&(d==="excludesfile"||d==="attributesfile"))r.push(u.value)}return{includes:n,files:r}}
var $o=/^\[includeif[ \t]*"(?:[^"\\]|\\.)*"\]/i;
function Hn(e){let n=0;while(n<e.length&&Vo(e[n]))n++;if(n===0)return null;let r=e.slice(0,n);while(n<e.length&&(e[n]===" "||e[n]==="\t"))n++;if(n>=e.length||e[n]!=="=")return null;n++;while(n<e.length&&(e[n]===" "||e[n]==="\t"))n++;let i=Wo(e,n);return{key:r,value:i}}
function Wo(e,n){let r="",i=!1,o=n;while(o<e.length){let s=e[o];if(!i&&(s==="#"||s===";"))break;if(s==='"'){i=!i,o++;continue}if(s==="\\"&&o+1<e.length){let u=e[o+1];if(i){switch(u){case"n":r+=`
`;break;case"t":r+="\t";break;case"b":r+="\b";break;case'"':r+='"';break;case"\\":r+="\\";break;default:r+=u;break}o+=2;continue}if(u==="\\"){r+="\\",o+=2;continue}}r+=s,o++}if(!i)r=Uo(r);return r}
function Uo(e){let n=e.length;while(n>0&&(e[n-1]===" "||e[n-1]==="\t"))n--;return e.slice(0,n)}
function at(e,n,r){let i=Yo(e);return i!==null&&i.sectionLower===n&&i.subsection===r}
function Yo(e){let n=1;while(n<e.length&&e[n]!=="]"&&e[n]!==" "&&e[n]!=="\t"&&e[n]!=='"')n++;let r=e.slice(1,n).toLowerCase();if(n<e.length&&e[n]==="]")return{sectionLower:r,subsection:null};while(n<e.length&&(e[n]===" "||e[n]==="\t"))n++;if(n>=e.length||e[n]!=='"')return null;n++;let i="";while(n<e.length&&e[n]!=='"'){if(e[n]==="\\"&&n+1<e.length){let o=e[n+1];if(o==="\\"||o==='"'){i+=o,n+=2;continue}i+=o,n+=2;continue}i+=e[n],n++}if(n>=e.length||e[n]!=='"')return null;if(n++,n>=e.length||e[n]!=="]")return null;return{sectionLower:r,subsection:i}}
function Vo(e){return e>="a"&&e<="z"||e>="A"&&e<="Z"||e>="0"&&e<="9"||e==="-"}
var eY=Symbol("git-repo-negative-result");
class Gn{rootByPath=new Kc({max:50});canonicalRootByRoot=new Kc({max:50});remoteSlugByRoot=new Kc({max:50});monorepoRuledOutByRoot=new Kc({max:50});monorepoVerdictByDir=new Kc({max:50});gitDirByCwd=new Map;linkedFromRootByPin=new Kc({max:64});hostWorktreeReport;markersByRoot=new Kc({max:50});repositoryByCwd=new Map;remoteHostByCwd=new Map;repoClassByCanonicalRoot=new Map;linguistGeneratedByPath=new Map;wcRootByPath=new Kc({max:50});wcTrunkByRoot=new Map;wcWorktreesByRoot=new Kc({max:50,ttl:60000});wcRemoteHashByRoot=new Kc({max:50});wcHeadByRoot=new Map;sessionRepoNameByCwd=new Kc({max:50});diffCheckoutIsRepo=new Map;gitExecutable=null;reportedHookInstallSkips=new Set;clear(){this.rootByPath.clear(),this.canonicalRootByRoot.clear(),this.remoteSlugByRoot.clear(),this.monorepoRuledOutByRoot.clear(),this.monorepoVerdictByDir.clear(),this.gitDirByCwd.clear(),this.linkedFromRootByPin.clear(),this.hostWorktreeReport=void 0,this.markersByRoot.clear(),this.repositoryByCwd.clear(),this.remoteHostByCwd.clear(),this.repoClassByCanonicalRoot.clear(),this.linguistGeneratedByPath.clear(),this.wcRootByPath.clear(),this.wcTrunkByRoot.clear(),this.wcWorktreesByRoot.clear(),this.wcRemoteHashByRoot.clear(),this.wcHeadByRoot.clear(),this.sessionRepoNameByCwd.clear(),this.diffCheckoutIsRepo.clear(),this.gitExecutable=null,this.reportedHookInstallSkips.clear()}}
function EUe(e,n,r){let i=e.get(n);if(i!==void 0)return i;let o=r(n);return e.set(n,o),o}
var zo=new q(()=>new Gn);
function Ep(){return zo.of(B().host)}
function Ue(e,n){try{let r=Wn(e);if(r.isSymbolicLink()){let i=nHt(e);if(i===null)return!1;if(Wi(i,n))return!1;if(y_(i,n))return!1;let o=Yn(e);return o.isDirectory()||o.isFile()}return r.isDirectory()||r.isFile()}catch{return!1}}
function Vn(e){if(De())return eY;let n=Date.now();ee("info","find_git_root_started");let r=de(e),i=r.substring(0,r.indexOf(te)+1)||te,o=0;while(r!==i){let s=L(r,".git");if(o++,Ue(s,r))return ee("info","find_git_root_completed",{duration_ms:Date.now()-n,stat_count:o,found:!0}),Ln(r);let u=Se(r);if(u===r)break;r=u}if(o++,Ue(L(i,".git"),i))return ee("info","find_git_root_completed",{duration_ms:Date.now()-n,stat_count:o,found:!0}),Ln(i);return ee("info","find_git_root_completed",{duration_ms:Date.now()-n,stat_count:o,found:!1}),eY}
function hr(e){let n=EUe(Ep().rootByPath,e,Vn);return n===eY?null:n}
function oy(e){let n=Vn(e);return n===eY?null:n}
function dt(e){try{return zn(e,pe(L(e,".git"),"utf-8"))}catch{return e}}
function zn(e,n){try{let r=n.trim();if(!r.startsWith("gitdir:"))return e;let i=r.slice(7).trim();if(Wi(i,e))return e;if(y_(i,e))return e;let o=de(e,i);if(G$(L(o,"commondir"),o))return e;let s=pe(L(o,"commondir"),"utf-8").trim();if(Wi(s,o))return e;if(y_(s,o))return e;let u=de(o,s);if(de(Se(o))!==L(u,"worktrees"))return e;if(G$(L(o,"gitdir"),o))return e;let d=pe(L(o,"gitdir"),"utf-8").trim();if(Wi(d,e))return e;if(y_(d,o,e))return e;if(We(de(o,d))!==L(We(e),".git"))return e;if($e(u)!==".git"){if(Ue(L(u,".git"),u))return e;return Ln(u)}return Ln(Se(u))}catch{return e}}
function Ys(e){let n=hr(e);if(!n)return null;return EUe(Ep().canonicalRootByRoot,n,dt)}
function cD(e){let n=oy(e);if(!n)return null;return dt(n)}
function Ht(){let e=Ep();return e.gitExecutable??=vN("git")||"git",e.gitExecutable}
var KAo=async(e)=>hr(e)!==null;
async function bWn(e){let{stdout:n,code:r}=await st(Ht(),[...Cn,"config","--get",e],{preserveOutputOnError:!1,useCwd:!0,env:ks()});if(r!==0)return null;let i=n.trim();return i.length>0?i:null}
var ds=4096;
function $n(e,n,r){let i=/\/+/,o=n.split(i),s=o[0]===""?te:o[0]+te,u=0;for(let d=1;d<o.length;d++){let p=o[d];if(p===""||p===".")continue;if(p===".."){s=Se(s);continue}let h=s.endsWith(te)?s+p:s+te+p;if(r!==void 0?DN(h,r):la(h))return null;let _;try{_=e.lstatSync(h)}catch{return null}if(!_.isSymbolicLink()){s=h;continue}if(++u>64)return null;let w;try{w=e.readlinkSync(h)}catch{return null}if(Un(w))return null;if(r!==void 0?DN(w,r):la(w))return null;let y=w.split(i);if(ut(w))o.splice(0,d+1,...y),s=y[0]===""?te:y[0]+te,d=0;else o.splice(d,1,...y),d--}return s}
function g9e(){let e=ie(),n=se(),r=$n(e,n,n)??n;try{r=We.native(r)}catch{}r=r.normalize("NFC").toLowerCase();let i=r.endsWith(te)?r:r+te,o=(y)=>{let b=y.toLowerCase();return b===r||b.startsWith(i)};function s(y){if(DN(y,n))return null;if(Un(y)){if(!il(y))return null;if(!Un(n))return{canonical:y.normalize("NFC"),crossOs:!0}}let b=$n(e,y,n);if(b===null)return null;try{b=We.native(b)}catch{return null}return{canonical:b.normalize("NFC"),crossOs:!1}}function u(y,b){if(!b&&o(y))return!0;for(let v of y.split(b?/[\\/]+/:te))if(v.toLowerCase()===".git")return!1;return!0}function d(y){try{let b=e.lstatSync(L(y,"HEAD"));if(!b.isFile()||b.size>4096)return!1;let v=pe(L(y,"HEAD"),"utf8").slice(0,255);return/^ref:[ \t]*refs\//.test(v)||/^[0-9a-f]{40}([0-9a-f]{24})?[ \t\n\r]*$/.test(v)}catch{return!1}}function p(y){try{let b=L(y,"HEAD"),v=e.lstatSync(b);if(!v.isFile()||v.size>4096)return!1;let k=pe(b,"utf8").slice(0,255);if(!/^ref:[ \t]*refs\//.test(k)&&!/^[0-9a-f]{40}([0-9a-f]{24})?[ \t\n\r]*$/.test(k))return!1;for(let C of["objects","refs"]){let F=L(y,C);if(!e.statSync(F).isDirectory())return!1;Jo(F,Zo.X_OK)}try{return e.statSync(L(y,"commondir")),!1}catch{}return!0}catch{return!1}}function h(y){try{let b=e.lstatSync(L(y,"HEAD"));if(b.isFile()||b.isSymbolicLink())return!0}catch{}for(let b of["objects","refs"])try{return e.statSync(L(y,b)),!0}catch{}return!1}function _(y){let b=(v)=>{let k=s(v);if(k===null)return f("git_bare_repo_gate","gitdir_target_uncanonical"),"plantable";if(u(k.canonical,k.crossOs))return f("git_bare_repo_gate","gitdir_target_plantable"),"plantable";return d(k.canonical)?"trusted":"none"};try{let v=e.lstatSync(L(y,".git"));if(v.isSymbolicLink()){let k;try{k=e.readlinkSync(L(y,".git"))}catch{return"plantable"}return b(ut(k)?k:y+te+k)}if(v.isFile()){if(v.size>ds)return"oversized";try{let k=pe(L(y,".git"),"utf8");if(k.includes("\x00"))return"plantable";if(!k.startsWith("gitdir: "))return"none";let C=k.slice(8).replace(/[\r\n]+$/,"");return b(ut(C)?C:y+te+C)}catch{return"none"}}if(v.isDirectory())return p(L(y,".git"))?"trusted":"none"}catch{}return"none"}switch(_(n)){case"plantable":return"gitdir-redirect-plantable";case"oversized":return"gitdir-file-oversized";case"trusted":return g("git_bare_repo_gate"),!1;case"none":break}let w=n;for(;;){if(h(w))return f("git_bare_repo_gate","bare_indicators"),"bare-indicators";let y=Se(w);if(y===w)break;switch(_(y)){case"trusted":return g("git_bare_repo_gate"),!1;case"plantable":return"gitdir-redirect-plantable";case"oversized":return"gitdir-file-oversized";case"none":break}w=y}return g("git_bare_repo_gate"),!1}
function ft(e){try{let n=Rs(e);if(n.isSymbolicLink())return"symlink";return n.isFile()?"file":n.isDirectory()?"dir":"special"}catch(n){let r=E(n);return r==="ENOENT"||r==="ENOTDIR"?"absent":"other"}}
function nHt(e){try{let n=Ps(e,{encoding:"buffer"}),r=n.toString("utf-8");if(r.includes("\x00")||!Buffer.from(r,"utf-8").equals(n))return null;return r}catch{return null}}
function G$(e,n){if(!Me(e,n))return!0;let r=ft(e);return r==="symlink"||r==="special"||r==="other"}
function Ds(e,n){return Wi(e,n)}
var Ns=/\/+/;
var js=/\/$/;
var ur=/^\//;
var Ms=/^\//;
function Me(e,n,r=40,i=n,o=!1,s,u=Number.POSITIVE_INFINITY,d){if(r<=0)return!1;let p=Ms.exec(e),h=p?p[0]:"",_=e.slice(h.length).split(Ns),w=Math.min(u,_.length);for(let y=0;y<_.length;y++){let b=_[y];if(b===""||b===".")continue;if(b===".."){if(h=Ye(h),s&&y>=_.length-w)s.missingTail=null;continue}if(h=N(h,b),DN(h,i)||HO(h))return!1;let v=d?nr(d.kinds,h,ft):ft(h);if(s&&s.stoppedAt===void 0&&(v==="absent"||v==="other"||v==="special"&&!o))s.stoppedAt={path:h,kind:v};if(v==="other"||v==="special"&&!o)return!1;if(v==="absent"&&s&&s.missingTail===void 0)s.missingTail=y>=_.length-w?_.length-y:null;if(v==="symlink"){if(s)s.throughLink=!0;let k=d?nr(d.linkTexts,h,nHt):nHt(h);if(k===null)return!1;if(Ds(k,Ye(h)))return!1;let C=ur.test(k)?k:Ye(h)+Ve+k,F=_.slice(y+1).join(Ve),P=F?js.test(C)?C+F:C+Ve+F:C;return Me(P,n,r-1,i,o,s,Math.min(w,_.length-y-1),d)}}if(s&&s.missingTail===void 0)s.missingTail=0;return!0}
function yWn(){return{kinds:new Map,linkTexts:new Map}}
function nr(e,n,r){let i=e.get(n);if(i===void 0&&!e.has(n))i=r(n),e.set(n,i);return i}
function me(e,n,r=n,i=!1,o,s){let u=ur.test(e)?e:n+Ve+e,d=ce(n,e),p=d===u?o:void 0;if(o&&!p)o.throughLink=!0;if(!Me(u,n,void 0,r,i,p,void 0,s)){if(o)o.missingTail=null;return!0}if(d===u)return!1;if(o)o.missingTail=null;return!Me(d,n,void 0,r,i,void 0,void 0,s)}
function y_(e,n,r=n){return me(e,n,r)}
function _Wn(e,n,r){let i={missingTail:void 0};return{unsafe:me(e,n,n,!1,i,r),missingTail:i.missingTail??null,stoppedAt:i.stoppedAt,throughLink:i.throughLink??!1}}
function DAo(e,n){return me(e,n,n,!0)}
export { Ig as Ig };
export { g9e as g9e };
export { _Wn as _Wn };
export { y_ as y_ };
export { hr as hr };
export { Ys as Ys };
export { G$ as G$ };
export { GPs as GPs };
export { DAo as DAo };
export { yWn as yWn };
export { oy as oy };
export { bWn as bWn };
export { KAo as KAo };
export { cD as cD };
