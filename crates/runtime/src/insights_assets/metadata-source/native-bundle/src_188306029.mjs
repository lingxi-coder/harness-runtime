// Extracted source closure for 2.1.295; source module src_188306029.js sha256 93e1595fd1989cb42b1fc443504ba0b7bc426ab6724265d7579b9fdccf5c959d
import { a } from "./src_186481843.mjs";
import { tnn, ETr, L2n, D2n, tUe, mxe, C0t, A0t, rUe, xko, Tf, Ztn, Qxs, OE, W5e, nZ, Xko, qC, Dy, Lue, b0t, E0t, PB, I5e, fTr, F5e, SP, zxs, U$, eZ, FW, NS, Mdt, iTr, jko, Tko, vko, Cko, Ako, bko, Wtn, wko, Eko, Rko, jxs, kko, rTr, ATr, lV, $W, Fdt, TTr, U5e, iCo, xTr, Jko, ose, xR, q8, k0t, cT, OB, TB, N3, RTr, rnn, uH, Nue, rZ, $5e, T0t, mO } from "./src_187673145.mjs";
import { $i, Ir, Af, mN } from "./src_187659079.mjs";
import { ql, Yr, m1e, q, B, Yut, ay, PO, Qv, HZ, Von, kp, bT, OP, s8e, JHt, Yn, we } from "./src_186096608.mjs";
import { i } from "./src_186440090.mjs";
import { d, S } from "./src_186016069.mjs";
import { dirname as kr, join as ae, win32 as xn, posix as Mn, win32 as Jt, resolve as he, join as Qe, isAbsolute as Ft, dirname as Kr, basename as Ct, dirname as ce, join as Se } from "path";
import { Ge, Cp, Ln, zse } from "./src_186068928.mjs";
import { Ys, KAo, cD } from "./src_188187055.mjs";
import { wvo, Wjn } from "./src_188299680.mjs";
import { mt } from "./src_187623132.mjs";
import { L } from "./src_186332575.mjs";
import { fo } from "./src_187670201.mjs";
import { Vjn, dtn, KOt, lH, qjn } from "./src_187665336.mjs";
import { j } from "./src_186014760.mjs";
import { __, z$, Rbe } from "./src_187646551.mjs";
import { E, W, G, x, l } from "./src_186056234.mjs";
import { ie, t, _, ho, wd, tj, of } from "./src_186240709.mjs";
import { ee } from "./src_187644646.mjs";
import { Ie } from "./src_186092222.mjs";
import { c } from "./src_186460102.mjs";
import { m, f, g } from "./src_186441656.mjs";
import { O } from "./src_186477637.mjs";
import { Xe, PH, Q } from "./src_186066580.mjs";
import { lt } from "./src_188159463.mjs";
import { D } from "./src_186447803.mjs";
import { p } from "./src_186562918.mjs";
import { _t, oe, o } from "./src_186578912.mjs";
import { zd } from "./src_188304227.mjs";
import { ne } from "./src_186231195.mjs";
import { nu } from "./src_188305035.mjs";
import { AH, lpt, C8e, nA } from "./src_188283656.mjs";
import { so } from "./src_188290059.mjs";
import { se } from "./src_188108105.mjs";
import { homedir as Ht } from "os";
import { mkdir as $r, readFile as Gr, appendFile as Ur, writeFile as Wr } from "fs/promises";
import { Rh } from "./src_186758878.mjs";
import { dD, QC, gg } from "./src_188142797.mjs";
import { UZ } from "./src_186220896.mjs";
import { q$ } from "./src_186893596.mjs";
import { Rxr, Dut } from "./src_186238496.mjs";
import { Fe } from "./src_187591836.mjs";
import { ct } from "./src_187672238.mjs";
function OW(e,{uncached:n=!1}={}){return null}
var wt=Ge(new Map,(e)=>e.clear());
function tAr(e,{uncached:n=!1}={}){let r=n?void 0:wt.get(e);if(r!==void 0)return r;let s=Cp(e);if(s===null)return;let h;try{h=ETr()}catch{return}let y=Ln(s)===h;return wt.set(e,y),y}
function xr(e,{uncached:n=!1}={}){return!1}
function nAr(e,{uncached:n=!1}={}){let r=e;for(;;){let s=OW(r,{uncached:n});if(s===null)return null;let h=xr(s,{uncached:n});if(h!==!1)return{root:s,readable:h===!0};let y=kr(s);if(y===s)return null;r=y}}
function Dt(e,n,r){let s=nAr(e,{uncached:r});if(s===null)return n();if(!s.readable)return null;switch(tAr(s.root,{uncached:r})){case!1:return s.root;case!0:return n();default:return null}}
function mP(e){return Dt(e,()=>Ys(e),!1)}
function TE(e){return Dt(e,()=>cD(e),!0)}
async function rAr(e,n){let{code:r}=await lt("git",["check-ignore","--",e],{preserveOutputOnError:!1,cwd:n});return r===0}
async function Br(e){let{stdout:n,code:r}=await lt("git",["config","--global","--get","core.excludesfile"],{preserveOutputOnError:!1,cwd:e});return zr(r===0?n:"")}
function zr(e){let n=e.trim();if(n){if(n==="~"||n.startsWith("~/"))return Qe(Ht(),n.slice(2));if(Ft(n))return n}let r=a.XDG_CONFIG_HOME;if(r&&Ft(r))return Qe(r,"git","ignore");return Qe(Ht(),".config","git","ignore")}
async function oAr(e,n=se()){try{if(!await KAo(n))return{written:!1,effective:!1};let r=e.replaceAll("\\","/"),s=`**/${r}`,h=r.endsWith("/")?`${r}sample-file.txt`:r;if(await rAr(h,n))return{written:!1,effective:!0};let y=await Br(n),R=Kr(y);await $r(R,{recursive:!0});try{if((await Gr(y,{encoding:"utf-8"})).includes(s)){let b=await Mt(h,n)?"already_tracked":"excludesfile_not_read";return t(`[gitignore] '${s}' already present in ${y} but git check-ignore reports not-ignored \u2014 ${kt(b,h)}`,{level:"warn"}),{written:!1,effective:!1,reason:b}}await Ur(y,`
${s}
`)}catch(T){if(E(T)==="ENOENT")await Wr(y,`${s}
`,"utf-8");else throw T}if(!await rAr(h,n)){let T=await Mt(h,n)?"already_tracked":"excludesfile_not_read";return t(`[gitignore] wrote '${s}' to ${y} but git check-ignore still reports not-ignored \u2014 ${kt(T,h)}`,{level:"warn"}),{written:!0,effective:!1,reason:T}}return{written:!0,effective:!0}}catch(r){return t(`Failed to add gitignore entry to global gitignore: ${r instanceof Error?r.message:String(r)}`,{level:"error"}),{written:!1,effective:!1}}}
async function Mt(e,n){let{code:r}=await lt("git",["ls-files","--error-unmatch","--",e],{preserveOutputOnError:!1,cwd:n});return r===0}
function kt(e,n){return e==="already_tracked"?`'${n}' is tracked in the index; gitignore rules do not apply to tracked files`:"core.excludesfile may point elsewhere"}
function jr(e){return e==="--cloud"||e==="--remote"||e.startsWith("--cloud=")||e.startsWith("--remote=")}
function yue(){return AH("--attach-serve")!==void 0||lpt(jr)!==-1&&C8e()&&!nA("-p")&&!nA("--print")}
function xjn(e){return e!=="globalConfig"&&Af.has(e)&&yue()}
function M$(){return a.CLAUDE_CODE_REMOTE_TOOLS_PIN_STORED_LOGIN===!0}
var Ven=5000;
function tdt(e){$i().internalWrites.set(e,Date.now())}
function Pjn(e,n){let r=$i().internalWrites,s=r.get(e);if(s!==void 0&&Date.now()-s<n)return r.delete(e),!0;return!1}
function ERs(){$i().internalWrites.clear()}
var Jr=Object.freeze({settings:{},errors:[],loadState:"absent"});
var Ee=Object.freeze({settings:{},errors:[]});
class Bt{mdm=null;hkcu=null;wslInherits=!1;loadPromise=null;startLoad(e){if(this.loadPromise)return;this.loadPromise=(async()=>{let n=Date.now(),s=await(wvo()??Wjn()),{mdm:h,hkcu:y,wslInherits:R}=await zt(s,e);this.replace(h,y,R);let P=Date.now()-n;t(`MDM settings load completed in ${P}ms`);try{i("tengu_managed_settings_os_read",io(s,P))}catch{}if(Object.keys(h.settings).length>0){t(`MDM settings found: ${Object.keys(h.settings).join(", ")}`);try{ee("info","mdm_settings_loaded",{duration_ms:P,key_count:Object.keys(h.settings).length,error_count:h.errors.length})}catch{}}})()}replace(e,n,r){this.mdm=e,this.hkcu=n,this.wslInherits=r}reset(){this.mdm=null,this.hkcu=null,this.wslInherits=!1,this.loadPromise=null}}
var Ut="@builtin";
var Xr=["prependPlugins","appendPlugins"];
function Zr(e){return typeof e==="object"&&e!==null&&!Array.isArray(e)}
var Qr=new q(()=>new Bt);
function pe(){return Qr.of(B().host)}
function $8(){return pe().mdm??Jr}
function a5e(){return pe().hkcu??Ee}
function HW(){return pe().wslInherits}
function l5e(e,n,r){pe().replace(e,n,r)}
async function rdt(e){let n=await Wjn();return zt(n,e)}
function qe(e,n,{userWritable:r=!1}={}){let s=mt(e,!1);if(!L(s))return{settings:{},errors:[L2n(n,{userWritable:r})],documentHasPolicyContent:!1,loadState:"didNotLoad"};let h=[],y=s;for(let H of r?D2n:[]){if(!(H in y))continue;if(y=y===s?{...s}:y,delete y[H],!tUe(H))h.push({file:n,path:H,message:`"${H}" is only honored from administrator-controlled managed settings and was ignored in ${n}, which the user account can write.`,severity:"warning",statusOnly:!0})}if(r){for(let k of Xr){if(!(k in y))continue;y=y===s?{...s}:y,delete y[k],h.push({file:n,path:k,message:`"${k}" is only honored from administrator-controlled managed settings and was ignored in ${n}, which the user account can write.`,severity:"warning",statusOnly:!0})}let H=y.enabledPlugins;if(Zr(H)){if(Object.entries(H).filter(([w,F])=>w.endsWith(Ut)||F!==!1).length>0){y=y===s?{...s}:y;let w=fo(H,(F,K)=>!K.endsWith(Ut)&&F===!1);if(Object.keys(w).length>0)y.enabledPlugins=w;else delete y.enabledPlugins;h.push({file:n,path:"enabledPlugins",message:`"enabledPlugins" entries that enable a plugin, or name a built-in, are only honored from administrator-controlled managed settings and were ignored in ${n}, which the user account can write.`,severity:"warning",statusOnly:!0})}}}let{settings:R,errors:P,documentHasPolicyContent:T,loadState:b,onlySubstitutes:I}=mxe(y,n),v=[...h,...P];return{settings:R??{},errors:r?v.map((H)=>({...H.severity==="warning"&&!H.startupFatal?H:{...eo(H),severity:"warning",statusOnly:!0},userWritable:!0})):v,documentHasPolicyContent:T,loadState:b,...I&&{onlySubstitutes:I}}}
function eo({startupFatal:e,...n}){return n}
function $t(e,n,r,s){return{file:e,path:"",message:`Managed settings document (${e}) could not be read: ${n}; none of its settings are in effect.`,severity:r?"warning":"fatal",statusOnly:!0,errorClass:"unreadable",...s!==void 0&&{errno:s},...r&&{userWritable:!0}}}
function Gt(e,n="Settings"){let r=n.replace(/[.*+?^${}()|[\]\\]/g,"\\$&"),s=new RegExp(`^[ \\t]+${r}[ \\t]+REG_(?:EXPAND_)?SZ[ \\t]+([\\s\\S]*)`,"im"),h=e.match(s)?.[1]?.trimEnd();return h?h:null}
var to=new Map([["ENOENT",S("ENOENT")],["EACCES",S("EACCES")],["EPERM",S("EPERM")],["ENOEXEC",S("ENOEXEC")],["EAGAIN",S("EAGAIN")],["EMFILE",S("EMFILE")],["ENOMEM",S("ENOMEM")],["ETIMEDOUT",S("ETIMEDOUT")],["ERR_CHILD_PROCESS_STDIO_MAXBUFFER",S("ERR_CHILD_PROCESS_STDIO_MAXBUFFER")]]);
var no=new Map([["SIGTERM",S("SIGTERM")],["SIGKILL",S("SIGKILL")],["SIGINT",S("SIGINT")]]);
function ro(e){if(!e)return;return to.get(e)??S("other")}
function oo(e){if(!e)return;return no.get(e)??S("other")}
function Wt(e,n){if(!n)return{};return{[`${e}_status`]:d(n.status),[`${e}_exit_code`]:n.exitCode??void 0,[`${e}_errno`]:ro(n.errno),[`${e}_signal`]:oo(n.signal),[`${e}_duration_ms`]:n.durationMs}}
function io(e,n){return{is_wsl:KOt(),await_ms:n,...Wt("hklm",e.outcomes.hklm),...Wt("hkcu",e.outcomes.hkcu)}}
async function zt(e,n){let r=[],s=!1,h="absent";for(let F of e.plistStdouts??[]){let{label:K,userWritable:U}=F;if(U&&s)continue;let z=F.stdout===null?{settings:{},errors:[$t(K,F.unreadReason,U,F.unreadErrno)],documentHasPolicyContent:!1,loadState:"didNotLoad",...U&&{userWritable:U}}:{...qe(F.stdout,K,{userWritable:U}),...U&&{userWritable:U}};if(U?C0t(z.settings):z.documentHasPolicyContent)return{mdm:{...z,errors:[...r,...z.errors],documentHasPolicyContent:!0},hkcu:Ee,wslInherits:!1};if(s||=A0t(z),r.push(...z.errors),!U)h=rUe(h,z.loadState)}let y=`Registry: ${Vjn}\\${dtn}`,R=null,P="disarmed";if(e.hklmStdout!==null){let F=Gt(e.hklmStdout)??"";R=qe(F,y);let K=mt(F,!1);if(L(K))P=xko(K)}else if(e.hklmUnreadReason!==void 0)R={settings:{},errors:[$t(y,e.hklmUnreadReason,!1)],documentHasPolicyContent:!1,loadState:"didNotLoad"};let T=R!==null&&A0t(R);if(R)r.push(...R.errors),h=rUe(h,R.loadState);let b=KOt(),I="disarmed",v=y,H=[];if(b){if(I=P,I!=="armed"){let F=await lo(n);if(H=F.links,F.flag==="armed"||I==="disarmed")I=F.flag,v=F.unreadableFlagPath??lH;if(I==="disarmed")r.push(...F.records),h=rUe(h,F.loadState)}}let k=I!=="disarmed",w={settings:{},errors:r,loadState:h};if(b&&!k){let F=new Set(R?.errors.filter((K)=>K.severity==="warning"));return{mdm:{...w,errors:w.errors.map((K)=>F.has(K)?{...K,wslIgnored:!0}:K),...H.length>0&&{managedDocumentLinks:H}},hkcu:Ee,wslInherits:!1}}if(R&&T)return{mdm:R,hkcu:Ee,wslInherits:k};if(I==="unreadable"){let F=await tt(lH,n);return{mdm:{...w,errors:F?w.errors:[...w.errors,Xko(v)],documentHasPolicyContent:!0,loadState:rUe(w.loadState,"loaded")},hkcu:Ee,wslInherits:k}}if(e.hkcuStdout!==null&&!await ao(k,n)){let F=qe(Gt(e.hkcuStdout)??"",`Registry: ${qjn}\\${dtn}`,{userWritable:!0});if(!b||F.settings.wslInheritsWindowsSettings===!0){let{wslInheritsWindowsSettings:K,managedSourcesBehavior:U,...z}=F.settings;return{mdm:w,hkcu:{settings:z,errors:F.errors},wslInherits:k}}if(F.errors.length>0)return{mdm:w,hkcu:{settings:{},errors:F.errors},wslInherits:k}}return{mdm:w,hkcu:Ee,wslInherits:k}}
async function Ce(e,n){if(j()&&n!==void 0)return(await __(e,Tf)).content;return z$(e,Tf)}
async function ao(e,n){if(e&&await tt(lH,n))return!0;return tt(qC(),n)}
async function Kt(e,n){let r;try{r=await Ce(e,n)}catch(s){return!W(s)}try{let s=OE(r,e,!0);return A0t({...s,loadState:W5e(s)})}catch{return!0}}
async function EEo(e){if(!KOt()||!pe().wslInherits)return"";let n=[];try{n.push(await Ce(ae(lH,"managed-settings.json"),e))}catch(r){n.push(et(r))}try{let r=ae(lH,"managed-settings.d"),s=(await ie().readdir(r)).filter((h)=>(h.isFile()||h.isSymbolicLink())&&h.name.endsWith(".json")&&!h.name.startsWith(".")).map((h)=>h.name).sort();for(let h of s)try{n.push(`${h}\x00${await Ce(ae(r,h),e)}`)}catch(y){n.push(`${h}\x00${et(y)}`)}}catch(r){let s=et(r);if(s!=="")n.push(s)}return n.join("\x01")}
function et(e){return Ne(e)?"":"\x00unreadable"}
function Ne(e){let n=E(e);return n==="ENOENT"||n==="ENOTDIR"}
async function lo(e){let n=[],r="absent",s=[],h=new Map;async function y(v,H){let k=ae(lH,...v),w;try{w=await Ce(k,e)}catch(U){if(Ne(U))return"disarmed";n.push(Ztn(k,U)),r="didNotLoad"}let F=await Qxs(lH,v,H,h);if(F!==null)s.push(F);if(w===void 0)return"disarmed";let K=w.trim()===""?{}:mt(w,!1);if(!L(K))return n.push(L2n(k)),r="didNotLoad","disarmed";return r=rUe(r,"loaded"),xko(K)}let R=ae(lH,"managed-settings.json"),P=await y(["managed-settings.json"]);if(P==="armed")return{flag:P,records:n,loadState:r,links:s};let T=P==="unreadable"?R:void 0,b=ae(lH,"managed-settings.d"),I;try{I=await ie().readdir(b)}catch(v){if(!Ne(v))n.push(Ztn(b,v,"directory")),r="didNotLoad";return{flag:P,unreadableFlagPath:T,records:n,loadState:r,links:s}}for(let v of I){if(!(v.isFile()||v.isSymbolicLink())||!v.name.endsWith(".json")||v.name.startsWith("."))continue;let H=await y(["managed-settings.d",v.name],v.isSymbolicLink());if(H==="armed")return{flag:H,records:n,loadState:r,links:s};if(H==="unreadable")P=H,T??=ae(b,v.name)}return{flag:P,unreadableFlagPath:T,records:n,loadState:r,links:s}}
async function tt(e,n){if(await Kt(ae(e,"managed-settings.json"),n))return!0;let r=ae(e,"managed-settings.d"),s;try{s=await ie().readdir(r)}catch(h){return!Ne(h)}for(let h of s)if((h.isFile()||h.isSymbolicLink())&&nZ(h.name)&&await Kt(ae(r,h.name),n))return!0;return!1}
var jt=["-NoProfile","-NonInteractive","-ExecutionPolicy","Bypass"];
var Xt="C:\\Windows";
var be="C:\\Program Files";
var Le=`${Xt}\\System32`;
var nt=`${Le}\\WindowsPowerShell\\v1.0`;
var co=`${Le}\\cmd.exe`;
var uo=[`${be}\\PowerShell\\7\\pwsh.exe`,`${be}\\PowerShell\\7-preview\\pwsh.exe`,`${nt}\\powershell.exe`];
function po(){if(O()!=="windows")return null;for(let e of uo){let n=zse(e);if(n===null){if(ie().existsSync(e))return null;continue}if(n.toLowerCase()===e.toLowerCase())return e}return null}
var go="PowerShell not found at any of its stock install locations on C: (fixed absolute candidates only; PATH is never consulted)";
var rt="CLAUDE_CODE_POLICY_HELPER_PSMODULEPATH";
var ot="CLAUDE_CODE_POLICY_HELPER_PS1_PATH";
var _o=`$env:PSModulePath = $env:${rt}; `;
var mo="if ($ExecutionContext.SessionState.LanguageMode -eq 'FullLanguage') { try { [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new() } catch {} }; ";
function Zt(e){return`${_o}${mo}$LASTEXITCODE = 0; try { ${e} } catch { Write-Error $_; exit 1 }; exit $LASTEXITCODE`}
var So=Zt(`& ($env:${ot})`);
var Eo=Zt("Invoke-Expression (@($input) -join [char]10)");
function yo(e){return D([`${e}\\Modules`,`${be}\\PowerShell\\Modules`,`${be}\\WindowsPowerShell\\Modules`,`${nt}\\Modules`]).join(";")}
function Ro(e){return D([e,Le,Xt,`${Le}\\Wbem`,nt]).join(";")}
var Po=".COM;.EXE;.BAT;.CMD;.VBS;.VBE;.JS;.JSE;.WSF;.WSH;.MSC";
var To=["DOTNET_","COMPLUS_","COR_","CORECLR_","APPDOMAIN_MANAGER_"];
var Ao=new Set(["DEVPATH","__PSLOCKDOWNPOLICY","PSMODULEANALYSISCACHEPATH","PSMODULEPATH","PATH","PATHEXT","COMSPEC","TERM",rt,ot]);
var Oo=["LD_","DYLD_","BASH_FUNC_","__BASH_FUNC<","LC_"];
var Io=new Set(["ENV","BASH_ENV","SHELLOPTS","PS4","GCONV_PATH","IFS","PWD","CDPATH","OLDPWD","TMOUT","POSIXLY_CORRECT","BASHOPTS","BASH_COMPAT","EXECIGNORE","BASH_SOURCE_PATH","BASH_LOADABLES_PATH","GLOBIGNORE","GLOBSORT","LOCPATH","PATH_LOCALE","NLSPATH","LANG","TMPDIR","TMP","TEMP"]);
var Co="/usr/bin:/bin:/usr/sbin:/sbin";
function Vt(e,n,r,s){for(let h of Object.keys(e)){let y=s(h);if(n.has(y)||r.some((R)=>y.startsWith(R)))delete e[h]}}
function Qt(e,n){let r={...e};switch(n.routedInterpreter){case void 0:return r;case"sh":return Vt(r,Io,Oo,(s)=>s),r.PATH=Co,r.LC_ALL="C",r;case"pwsh":{Vt(r,Ao,To,(h)=>h.toUpperCase());let s=Jt.dirname(n.file);if(r.PSModulePath=yo(s),r[rt]=r.PSModulePath,r.PATH=Ro(s),r.PATHEXT=Po,r.COMSPEC=co,r.TERM="dumb",n.ps1Path!==void 0)r[ot]=n.ps1Path;return r}}}
function qt(e){return O()==="windows"?Jt.dirname(e.file):"/"}
function en(e){if(e.script!=null){if(e.interpreter==="sh")return{plan:{file:"/bin/sh",args:["-s"],input:e.script,routedInterpreter:"sh"}};return Yt({args:[...jt,"-Command",Eo],input:e.script})}let{path:n}=e;if(n==null)return{error:"no path or script configured",code:"bad_path"};if(O()!=="windows"||!kko(n))return{plan:{file:n,args:[]}};return Yt({args:[...jt,"-Command",So],ps1Path:n})}
function Yt(e){let n=po();if(n===null)return{error:go,code:"interpreter_unavailable"};return{plan:{file:n,routedInterpreter:"pwsh",...e}}}
async function tn(e,n,r){if(e.routedInterpreter!==void 0)return null;let s;try{s=await Xe(bo(e.file,r),n)}catch(h){return`cannot stat path (${E(h)??"unknown error"}): ${e.file}`}if(s===void 0)return it(e.file,n);return s.isFile()?null:`path is not a regular file: ${e.file}`}
function bo(e,n){let r=n.get(e);if(r===void 0){r=ie().stat(e),n.set(e,r);let s=()=>n.delete(e);r.then(s,s)}return r}
function it(e,n){return`cannot stat path (timed out after ${n} ms): ${e}`}
var Lo=["ANTHROPIC_API_KEY","CLAUDE_CODE_OAUTH_TOKEN","CLAUDE_CODE_ARTIFACTS_API_TOKEN","CLAUDE_CODE_MEMORY_API_TOKEN","CLAUDE_CODE_SLACK_TAG_TOKEN","ANTHROPIC_AUTH_TOKEN","ANTHROPIC_FOUNDRY_API_KEY","ANTHROPIC_FOUNDRY_AUTH_TOKEN","ANTHROPIC_AWS_API_KEY","ANTHROPIC_CUSTOM_HEADERS","AWS_SECRET_ACCESS_KEY","AWS_SESSION_TOKEN","AWS_BEARER_TOKEN_BEDROCK","GOOGLE_APPLICATION_CREDENTIALS","GOOGLE_GHA_CREDS_PATH","AZURE_CLIENT_SECRET","IDENTITY_HEADER","MSI_SECRET","AZURE_CLIENT_CERTIFICATE_PATH","AZURE_CLIENT_CERTIFICATE_PASSWORD","AZURE_PASSWORD","AZURE_FEDERATED_TOKEN_FILE","AWS_WEB_IDENTITY_TOKEN_FILE","AWS_CONTAINER_CREDENTIALS_RELATIVE_URI","AWS_CONTAINER_CREDENTIALS_FULL_URI","AWS_CONTAINER_AUTHORIZATION_TOKEN","AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE","CLOUDSDK_AUTH_ACCESS_TOKEN","GOOGLE_OAUTH_ACCESS_TOKEN","CLAUDE_CODE_OAUTH_REFRESH_TOKEN","HF_TOKEN","HUGGING_FACE_HUB_TOKEN","HUGGINGFACEHUB_API_TOKEN","NODE_AUTH_TOKEN","NUGET_AUTH_TOKEN","CARGO_REGISTRY_TOKEN","TWINE_PASSWORD","TWINE_USERNAME","PYPI_TOKEN","PYPI_API_TOKEN","UV_PUBLISH_TOKEN","UV_PUBLISH_PASSWORD","UV_PUBLISH_USERNAME","FLIT_PASSWORD","FLIT_USERNAME","HATCH_INDEX_AUTH","HATCH_INDEX_USER","GEM_HOST_API_KEY","MATURIN_PYPI_TOKEN","MATURIN_PASSWORD","MATURIN_USERNAME","CONAN_LOGIN_USERNAME","CONAN_PASSWORD","ANACONDA_API_TOKEN","BINSTAR_API_TOKEN","VAULT_TOKEN","VAULT_AUTH_TOKEN","VAULT_ROLE_ID","VAULT_SECRET_ID","CONSUL_HTTP_TOKEN","CONSUL_HTTP_AUTH","NOMAD_TOKEN","NOMAD_HTTP_AUTH","CI_REGISTRY_USER","CI_DEPLOY_USER","JF_USER","FASTLANE_SESSION","MATCH_GIT_BASIC_AUTHORIZATION","SONAR_TOKEN","SONARQUBE_SCANNER_PARAMS","SONAR_SCANNER_JSON_PARAMS","SLACK_WEBHOOK_URL","SLACK_WEBHOOK","DISCORD_WEBHOOK","DISCORD_WEBHOOK_URL","TEAMS_WEBHOOK_URL","MS_TEAMS_WEBHOOK_URI","ANTHROPIC_IDENTITY_TOKEN","ANTHROPIC_IDENTITY_TOKEN_FILE","CLOUDSDK_AUTH_ACCESS_TOKEN_FILE","CLOUDSDK_AUTH_AUTHORIZATION_TOKEN_FILE","AZURE_AUTH_LOCATION","ACTIONS_ID_TOKEN_REQUEST_TOKEN","ACTIONS_ID_TOKEN_REQUEST_URL","ACTIONS_RUNTIME_TOKEN","ACTIONS_RUNTIME_URL","ALL_INPUTS","VSS_NUGET_EXTERNAL_FEED_ENDPOINTS","ARTIFACTS_CREDENTIALPROVIDER_EXTERNAL_FEED_ENDPOINTS","VSS_NUGET_ACCESSTOKEN","ARTIFACTS_CREDENTIALPROVIDER_ACCESSTOKEN","COMPOSER_AUTH","OVERRIDE_GITHUB_TOKEN","DEFAULT_WORKFLOW_TOKEN","SSH_SIGNING_KEY"];
var vo=["AWS_SHARED_CREDENTIALS_FILE","AWS_CONFIG_FILE","CLOUDSDK_CONFIG","AZURE_CONFIG_DIR","KUBECONFIG","NETRC","PGPASSFILE","PGSERVICEFILE","DOCKER_CONFIG","GH_CONFIG_DIR","GNUPGHOME","NPM_CONFIG_USERCONFIG","NPM_CONFIG_GLOBALCONFIG","SSH_AUTH_SOCK","GIT_SSH_COMMAND","GIT_SSH","GIT_ASKPASS","SSH_ASKPASS","SSH_AGENT_PID","TF_CLI_CONFIG_FILE","WGETRC","PIP_CONFIG_FILE","UV_CONFIG_FILE","RCLONE_CONFIG","BOTO_CONFIG","BOTO_PATH","S3CMD_CONFIG","SOPS_AGE_KEY_FILE","ANSIBLE_VAULT_PASSWORD_FILE","ANSIBLE_VAULT_IDENTITY_LIST","ANSIBLE_CONFIG","GOAUTH","SBT_CREDENTIALS","COURSIER_CREDENTIALS","CONSUL_HTTP_TOKEN_FILE","SYSTEM_WGETRC","CLOUDSDK_ROOT_DIR","M2_HOME","MAVEN_HOME","LEIN_HOME","SSLKEYLOGFILE","GIT_CONFIG_GLOBAL","GIT_CONFIG_SYSTEM","XDG_CONFIG_HOME","XDG_CONFIG_DIRS","XDG_CACHE_HOME","XDG_DATA_HOME","XDG_STATE_HOME","XDG_RUNTIME_DIR","HELM_REGISTRY_CONFIG","HELM_REPOSITORY_CONFIG","HELM_CONFIG_HOME","REGISTRY_AUTH_FILE","DOCKER_CERT_PATH","SSL_CLIENT_CERT","GIT_SSL_CERT","GIT_SSL_KEY","PIP_CLIENT_CERT","PGSSLKEY","VAULT_CLIENT_KEY","VAULT_CLIENT_CERT","CONSUL_CLIENT_KEY","CONSUL_CLIENT_CERT","NOMAD_CLIENT_KEY","NOMAD_CLIENT_CERT","PGSSLCERT","PGSYSCONFDIR","CURL_HOME","CARGO_HOME","COMPOSER_HOME","GRADLE_USER_HOME","BUNDLE_CONFIG","BUNDLE_USER_CONFIG","BUNDLE_USER_HOME","BUNDLE_APP_CONFIG","POETRY_CONFIG_DIR","COURSIER_CONFIG_DIR","HEX_HOME","HF_HOME","HF_TOKEN_PATH","HF_STORED_TOKENS_PATH","CONDARC","BUN_CONFIG_FILE","GOENV","NPM_CONFIG_PREFIX","CLOUDSDK_AUTH_CREDENTIAL_FILE_OVERRIDE","CLAUDE_CONFIG_DIR","ANTHROPIC_CONFIG_DIR","ANTHROPIC_PROFILE","CLAUDE_CODE_FEDERATION_CACHE_DIR","CLAUDE_SECURESTORAGE_CONFIG_DIR"];
var un=["PIP_INDEX_URL","PIP_EXTRA_INDEX_URL","PIP_FIND_LINKS","UV_INDEX_URL","UV_EXTRA_INDEX_URL","UV_DEFAULT_INDEX","UV_INDEX","UV_FIND_LINKS","UV_PUBLISH_URL","TWINE_REPOSITORY_URL","FLIT_INDEX_URL","HATCH_INDEX_REPO","NPM_CONFIG_REGISTRY","YARN_REGISTRY","YARN_NPM_REGISTRY_SERVER","YARN_NPM_PUBLISH_REGISTRY","COREPACK_NPM_REGISTRY","BUN_CONFIG_REGISTRY","GOPROXY"];
var wo=new Set(un);
var Do=/^(?:CARGO_REGISTRIES_[A-Z0-9_]+_INDEX|POETRY_REPOSITORIES_[A-Z0-9_]+_URL|NPM_CONFIG_@[^:]+:REGISTRY)$/i;
function odt(e){let n=e.toUpperCase().replace(/-/g,"_").replace(/^INPUT_/,"");return wo.has(n)||Do.test(n)}
function dn(e){return pn[e.toUpperCase().replace(/-/g,"_").replace(/^INPUT_/,"")]}
var pn={PIP_EXTRA_INDEX_URL:"PIP_INDEX_URL",PIP_FIND_LINKS:"PIP_INDEX_URL",UV_EXTRA_INDEX_URL:"UV_DEFAULT_INDEX",UV_INDEX:"UV_DEFAULT_INDEX",UV_FIND_LINKS:"UV_DEFAULT_INDEX"};
var O$e="http://index.invalid/";
var gn={UV_DEFAULT_INDEX:["UV_INDEX_URL"]};
function vEo(e,n){return[n,...gn[n]??[]].some((r)=>(e[r]??"")!=="")}
var Ho=/^[a-z][a-z0-9+.-]*:\/\//i;
function Fo(e){let n=e.startsWith("//"),r;try{r=new URL(n?`https:${e}`:e)}catch{let P=e.replace(/^([a-z][a-z0-9+.-]*:)?\/\/[^/?#]*@/i,"$1//");return kR(P)?{text:O$e,cut:!0}:{text:P,cut:!1,stripped:P!==e}}let s=r.password!==""||r.username!==""&&/(^|\+)https?:$/i.test(r.protocol),h=s;if(s)r.username="",r.password="";let y=!1;if(kR(Mo(r.search)))r.search="",r.hash="",y=!0;let R=r.href;return{text:n?R.replace(/^https:/,""):R,cut:y,stripped:h}}
function Mo(e){try{return decodeURIComponent(e)}catch{return e}}
function qen(e,n){if(n.trim()==="")return{value:n,cut:!1,stripped:!1};let r=/^(?:INPUT_)?GOPROXY$/i.test(e),s=!/^INPUT_/i.test(e)&&dn(e)!==void 0,h=!1,y=!1,R=!1,P=r?/(\s*[,|]\s*|\s+)/:s?/(\s+)/:null,I=(P===null?[n]:n.split(P)).flatMap((v)=>{if(R)return[];let H=/^([A-Za-z0-9_.-]+=)(?=[a-z][a-z0-9+.-]*:\/\/|\/\/)/i.exec(v),k=H?H[1]:"",w=v.slice(k.length);if(!w.startsWith("//")&&!Ho.test(w)){if(v.trim()!==""&&kR(v))return h=!0,y=!0,[r?"off":O$e];return[v]}let F=Fo(w);if(y||=F.cut||F.stripped===!0,F.cut||F.stripped&&(r||s)){if(h=!0,r)return R=!0,["off"]}return[k+F.text]}).join("");if(r)I=I.replace(/[,|\s]+$/,"");if(I!==""&&kR(I))return{value:r?"off":O$e,cut:!0,stripped:!0};return{value:I===""?O$e:I,cut:h,stripped:y}}
function _ue(e){let n=fn(e);for(let[r,s]of Object.entries(e)){if(s===void 0)continue;let h=typeof s==="string"?s:String(s),y=Ken(r,h);if(y!==void 0&&y!==h)n.respelled[r]=y,n.lostCredential.push(r)}return n}
function fn(e){let n={},r=[];for(let[s,h]of Object.entries(e)){if(h===void 0||!odt(s))continue;let y=typeof h==="string"?h:String(h),R=qen(s,y);if(R.value!==y){if(n[s]=R.value,R.cut||R.stripped)r.push(s)}if(R.cut){let P=kEo(s,R.value);if(P!==void 0&&!vEo(e,P.name)&&n[P.name]===void 0)n[P.name]=P.value}}return{respelled:n,lostCredential:r}}
function kEo(e,n){let r=/^INPUT_/i.test(e)?void 0:dn(e);if(r===void 0)return;let s=n.split(/\s+|,|\|/).map((y)=>y.replace(/^[A-Za-z0-9_.-]+=(?=[a-z][a-z0-9+.-]*:\/\/|\/\/)/i,"")).find((y)=>y.startsWith("//")||/^https?:\/\//i.test(y)),h=O$e;if(s!==void 0)try{let y=new URL(s.startsWith("//")?`https:${s}`:s).origin;h=y==="null"?O$e:`${y}/`}catch{h=O$e}return{name:r,value:h}}
function sdt(){return sAr(),Re?vo.flatMap((e)=>[e,`INPUT_${e}`,...e.startsWith("NPM_CONFIG_")?[e.toLowerCase()]:[]]):[]}
function sAr(){let e=process.env.CLAUDE_CODE_SUBPROCESS_ENV_SCRUB;Re??=nn(e)||nn(process.env.GITHUB_ACTIONS)&&!ko(e)}
var Re;
function kRs(){Re=void 0}
function ko(e){return e!==void 0&&/^(?:0|false|no|off)$/i.test(String(e).trim())}
function nn(e){let n=e===void 0?void 0:String(e).trim().toLowerCase();return n==="1"||n==="true"||n==="yes"||n==="on"}
var c5e=Lo.flatMap((e)=>[e,`INPUT_${e}`]);
var CEo=["ANTHROPIC_API_KEY","ANTHROPIC_AUTH_TOKEN","ANTHROPIC_CUSTOM_HEADERS","CLAUDE_CODE_OAUTH_TOKEN","CLAUDE_CODE_OAUTH_REFRESH_TOKEN","CLAUDE_CODE_ARTIFACTS_API_TOKEN","CLAUDE_CODE_MEMORY_API_TOKEN","CLAUDE_CODE_SLACK_TAG_TOKEN","ANTHROPIC_FOUNDRY_API_KEY","ANTHROPIC_FOUNDRY_AUTH_TOKEN","ANTHROPIC_AWS_API_KEY","ANTHROPIC_IDENTITY_TOKEN","ANTHROPIC_IDENTITY_TOKEN_FILE","ALL_INPUTS"];
var AEo=CEo.flatMap((e)=>[e,`INPUT_${e}`]);
function Yoe(e){return e.toUpperCase().replace(/-/g,"_")}
var st=/CONN(ECT(ION)?)?_?STR(ING)?S?(?=$|[_0-9])/i;
function TEo(e){return st.test(e)||st.test(mn(e))}
var _n=["OAuth","NextAuth"];
function mn(e){return _n.reduce((n,r)=>n.replaceAll(r,r[0]+r.slice(1).toLowerCase()),e).replace(/([a-z0-9])([A-Z])/g,"$1_$2").replace(/([A-Z])([A-Z][a-z])/g,"$1_$2")}
var Sn=["TOKEN","SECRET","PASSWORD","PASSWD","PASSPHRASE","KEY","AUTH","COOKIE","PAT","DSN","WEBHOOK","CREDENTIAL","CREDENTIALS","CREDS","APIKEY","ACCESSKEY","SECRETKEY","ACCOUNTKEY","PRIVATEKEY","AUTHKEY","SSHKEY","SIGNINGKEY","MASTERKEY","DEPLOYKEY","ENCRYPTIONKEY","PGPASSWORD","SSHPASS"];
var En=["PWD","PASS","JWT"];
var hn=["TOKEN","SECRET","PASSWORD","PASSWD","PASSPHRASE"];
var Rn=["KEY","SECRET","PASSWORD","CREDENTIAL"];
var rn=new RegExp(`((^|_)(${Sn.join("|")}|(${Rn.join("|")})S)|_(${En.join("|")})|(${hn.join("|")}))(?=$|[_0-9])`,"i");
var $o=/^GIT_CONFIG_KEY_[0-9][A-Za-z0-9_]*$/;
function Go(e){return/^GIT_CONFIG_(?:COUNT|PARAMETERS|(?:KEY|VALUE)_[0-9]+)$/.test(e)}
function jOt(e){return Go(e)||Wo.test(e)}
var Wo=/^(?:(?:https?|ftp|all|no)_proxy|npm_config_(?:https?_)?proxy|npm_config_noproxy|yarn_proxy|(?:yarn|global_agent|docker|claude_code)_(?:https?|no)_proxy|cloudsdk_proxy_[a-z]+|electron_get_use_proxy)$/i;
function REo(e,n){let r=Ken(e,n);return r===void 0?kR(n):r!==n}
function Ken(e,n){if(Ojn(e)==="GOFLAGS")return n.length>De?"":Pi(n);if(!On.has(Ojn(e)))return;let r=n.length>De?null:idt(n);if(r===null)return"";sAr();let s=r.map((R,P)=>{let T=P>0&&/^(?:-D|--define)$/.test(d5e(r[P-1]))?`-D${R}`:Cn(d5e(R));return{raw:R,word:T,residual:In.test(T),goes:!1}});for(let R=0;R<s.length;R++){let P=s[R];if(P.residual)continue;let T=R>0?s[R-1].word:null,b=s[R+1];if(P.goes=P.goes||Jo(P.word,T)||b!==void 0&&!b.residual&&!Pe(b.word)&&at.test(P.word)&&kR(`${P.word} ${b.word}`),P.goes&&b!==void 0&&!Pe(b.word)&&at.test(P.word))b.goes=!0}for(let R=1;R<s.length;R++){if(jo.test(s[R-1].word))s[R].goes=!0;if(s[R].goes&&!d5e(s[R].raw).startsWith("-")&&Bo(s[R-1].word))s[R-1].goes=!0}let h=s.filter((R)=>!R.goes);e:while(Ijn(h.filter((R)=>!R.residual).flatMap(on))){let R=h.filter((P)=>!P.residual);for(let P=2;P<=4;P++)for(let T=0;T+P<=R.length;T++){let b=R.slice(T,T+P);if(Ijn(b.flatMap(on))){for(let I of b)I.goes=!0;h=h.filter((I)=>!I.goes);continue e}}for(let P of R)P.goes=!0;h=h.filter((P)=>!P.goes)}if(h.length===s.length)return n;let y=qo.has(Ojn(e))?Ko:Pn;return h.every((R)=>y.test(R.raw))?h.map((R)=>R.raw).join(" "):""}
var Pn=/^[A-Za-z0-9_.,:=\/@+~*?-]+$/;
var Ko=/^[A-Za-z0-9_.,:=\/@+~|*?-]+$/;
var at=/(?:\b(?:Bearer|Basic|token)|authorization\s*[:=]\s*["']?(?:[a-z][a-z0-9_-]*)?|-token\s*[:=])\s*["']?$/i;
function Bo(e){return IEo(e)===""||OEo(e)===""||zo.test(e)||An.test(e)}
var zo=/^(?:-D|--define|-f|--file|-pl|--projects|-rf|--resume-from|-l|--log-file|-t|--toolchains|-gt|--global-toolchains|-itr|--install-toolchains|-P|--activate-profiles|-b|--builder|-T|--threads|-emp|--encrypt-master-password|-ep|--encrypt-password)$/;
var jo=/^(?:-emp|--encrypt-master-password|-ep|--encrypt-password)$/;
var Vo=/^(?:-emp|--encrypt-master-password|-ep|--encrypt-password)=./s;
function Pe(e){return/^-D[A-Za-z_][\w.-]*=.+$/s.test(e)||/^-D[A-Za-z_]\w*(?:\.\w+)+=$/.test(e)||/^-D[A-Za-z_][A-Za-z_.]*$/.test(e)||e.startsWith("-J-")&&Pe(e.slice(2))||/^--[a-z][a-z0-9-]*(?:=.*)?$/s.test(e)||Yo.test(e)||/^(?:@|-[A-Za-z]{1,4}\d{0,2}$)/.test(e)}
var Yo=/^-(?:X(?:mx|ms|ss|mn|rs|int|comp|mixed|batch|diag|debug|future|prof|noclassgc|internalversion|check:jni|share:(?:on|off|auto|dump)|verify(?::[a-z]+)?|log(?::.*)?|loggc:.+|bootclasspath(?:\/[ap])?:.+|runjdwp:.+|dock:.+|startOnFirstThread|showSettings(?::[a-z]+)?)[0-9kKmMgGtT]*$|XX:[+-]?[A-Za-z]\w*(?:=.*)?$|(?:javaagent|agentlib|agentpath):.|verbose(?::[a-z]+)?$|[ed](?:s?a)(?::[a-z][\w$]*(?:\.[\w$]+)*(?:\.\.\.)?)?$|(?:server|client|d64|showversion)$|-(?:add-opens|add-exports|add-reads|add-modules|limit-modules|patch-module|module-path|upgrade-module-path|class-path|enable-native-access|illegal-access|enable-preview|source|release)(?:=.*)?$|(?:cp|classpath|p)$)/s;
function Ijn(e){return kR(e.map((n)=>Pe(n)?`; ${n.includes("=")?n.replace(/^[^=]*=/,""):""}`:n).join(" "))}
function Jo(e,n){let r=Tn(e);if(r===null)return sn(e,n);let s=/^-D([^=]+)=/.exec(e)[1];return Xo(e)||Nn(s,e.slice(s.length+3))||Ijn(r)||r.some((h,y)=>sn(h,y>0?r[y-1]:null)||y+1<r.length&&!Pe(r[y+1])&&at.test(h)&&kR(`${h} ${r[y+1]}`))}
function on(e){return Tn(e.word)??[e.word]}
function Tn(e){let n=/^-D[^=]+=\s*(-.*)$/s.exec(e);if(n===null)return null;return(idt(n[1])??[]).map(d5e).filter((r)=>!In.test(r))}
function Xo(e){let n=/^-D[^=]+=\s*(-.*)$/s.exec(e);return n!==null&&idt(n[1])===null}
function sn(e,n){if(Vo.test(e))return!0;let r=/^-D([^=]+)=(.+)$/s.exec(e);if(r!==null){let[,s,h]=r;if(Nn(s,h)||kR(h))return!0;if(Re===!0)return PEo.test(s)||e.includes("\\")||!Qo.test(s)&&!(/^sonar\./i.test(s)&&!iAr(s)&&!/key.?store|cert|pkcs|p12|pfx|\.pem$/i.test(s))&&an(h);return!1}if(kR(e))return!0;if(Re===!0)return!Zo.test(e)&&!(n!==null&&An.test(n))&&an(e);return!1}
function an(e){return/[\\/]|^[@~]/.test(e)}
var Zo=/^-(?:X|XX:|javaagent:|agentlib:|agentpath:|verbose|ea\b|da\b|esa$|dsa$|server$|client$|d64$|showversion$|-(?:add-opens|add-exports|add-reads|add-modules|limit-modules|patch-module|module-path|upgrade-module-path|class-path|enable-native-access|illegal-access|enable-preview|source|release)\b|(?:cp|classpath|p)$)/;
var An=/^-(?:-(?:add-opens|add-exports|add-reads|add-modules|limit-modules|patch-module|module-path|upgrade-module-path|class-path|enable-native-access|source|release)|cp|classpath|p)$/;
var Qo=/^(?:javax\.net\.ssl\.trustStore|java\.io\.tmpdir|java\.security\.egd|java\.library\.path|jna\.library\.path|java\.class\.path|(?:https?|ftp)\.nonProxyHosts|socksNonProxyHosts|jna\.tmpdir|java\.util\.logging\.config\.file|log4j2?\.configurationFile|logback\.configurationFile|logging\.config|user\.dir|file\.encoding|maven\.multiModuleProjectDirectory|library\.jansi\.path|jansi\.tmpdir)$/i;
var xEo=["JAVA_TOOL_OPTIONS","JDK_JAVA_OPTIONS","_JAVA_OPTIONS","IBM_JAVA_OPTIONS","OPENJ9_JAVA_OPTIONS","MAVEN_OPTS","GRADLE_OPTS","MAVEN_ARGS","MAVEN_CONFIG","ANT_OPTS","ANT_ARGS","JAVA_OPTS","SBT_OPTS","JVM_OPTS","LEIN_JVM_OPTS","ES_JAVA_OPTS","SONAR_SCANNER_OPTS","SONAR_SCANNER_JAVA_OPTS"];
function Ojn(e){return e.toUpperCase().replace(/^INPUT_/,"").replace(/-/g,"_")}
var qo=new Set(["JAVA_TOOL_OPTIONS","JDK_JAVA_OPTIONS","_JAVA_OPTIONS","IBM_JAVA_OPTIONS","OPENJ9_JAVA_OPTIONS"]);
var On=new Set(xEo);
var In=/^-D(?:(?:(?:https?|ftp)\.proxy(?:User|Password)|socksProxy(?:User|Password)|java\.net\.socks\.(?:username|password))=\S*$|[\w.-]*trust-?store-?password=changeit$)/i;
function idt(e){let n=[],r="",s=!1,h=0;while(h<e.length){let y=e[h];if(y===" "||y==="\t"||y===`
`||y==="\r"){if(s)n.push(r),r="",s=!1;h++;continue}if(y==="'"||y==='"'){let R=e.indexOf(y,h+1);if(R===-1)return null;r+=e.slice(h+1,R),s=!0,h=R+1;continue}r+=y,s=!0,h++}if(s)n.push(r);return n}
function ve(e){return e.replace(/([a-z0-9])([A-Z])/g,"$1_$2").replace(/([A-Z])([A-Z][a-z])/g,"$1_$2")}
var PEo=/^(?:maven\.(?:user\.settings|global\.settings|installation\.settings|project\.settings|settings\.security|repo\.local|home)|settings\.security|gradle\.user\.home|sbt\.(?:boot\.credentials|repository\.config|global\.base|ivy\.home|boot\.directory)|user\.home)$/i;
function IEo(e){if(/^-(?:show-version|strict-checksums)$/.test(e))return null;let n=/^(?:--?settings|--?global-settings|--?install-settings|--?project-settings|-gs|-is|-ps|-s)(?:=(.+))?$/.exec(e)??/^-g?s=?(.+)$/.exec(e);return n===null?null:n[1]??""}
function OEo(e){let n=/^-{1,2}(?:ivy|sbt-dir|sbt-boot)(?:=(.+))?$/.exec(e);return n===null?null:n[1]??""}
function d5e(e){return e.replace(/^-J(?=-)/,"")}
function Cn(e){return e.replace(/^--define=/,"-D")}
function Nn(e,n){return bn(e,n)&&!/^(?:[\\/~]|[a-z]:[\\/]|file:)/i.test(n)}
function bn(e,n){let r=e.split(".").at(-1)??"";return iAr(e)&&(ei.test(ve(e))||!/(?:user(?:name)?|id|store|file|path|dir|location|url|host|port|alias|type)$/i.test(r))&&!HEo(e,n)}
var ei=/(?:PASSWORD|PASSWD|SECRET)S?[._-]ID$/i;
var ti=new Set(["sonar.login","sonar.token","sonar.password"]);
var ni=/^(?:projectKey|moduleKey|componentKey|ruleKey|resourceKey)$/;
var ri=new Set(["sonar.pullrequest.key"]);
function HEo(e,n){let r=e.split(".").at(-1)??"";return/^(?:\d+[a-z]{0,2}|true|false)$/i.test(n)&&!Sue(r.replace(/-/g,"_"))&&!ut(e)}
function iAr(e){if(ti.has(e.toLowerCase()))return!0;let n=e.split(".").at(-1)??"",r=ve(n).split(/[_-]+/).filter(Boolean).map((h)=>h.toUpperCase().replace(/\d+$/,"")).filter(Boolean),s=r.at(-1);if(!s)return!1;if(/^sonar\./i.test(e)){if(s==="SECURED")return!0;if((ni.test(n)||ri.has(e.toLowerCase()))&&!ut(e))return!1}if(Sue(n.replace(/-/g,"_"))||ut(e)||st.test(ve(e).replace(/[.-]/g,"_")))return!0;return s==="USERNAME"||s==="USER"&&r.length>1}
function ut(e){let n=ve(e).toUpperCase().split(/[._-]+/).filter(Boolean),r=n.at(-1)??"",s=n.at(-2)??"";return n.slice(0,-1).some((h)=>oi.test(h))&&(!ii.test(r)||/^IDS?$/.test(r)&&/^(?:PASSWORDS?|PASSWD|SECRETS?)$/.test(s))}
var oi=/^(?:PASSWORDS?|PASSWD|PASSPHRASE|SECRETS?|TOKENS?|KEYS?|APIKEY|CREDENTIALS?)$/;
var ii=/^(?:USER(?:NAME)?|ID|IDS|STORE|FILE|FILES|PATH|PATHS|DIR|DIRECTORY|LOCATION|URL|URI|HOST|PORT|ALIAS|TYPE|PREFERENCE|SCHEMES?|ENABLED?|DISABLED?|MODE|CLASS|PROVIDER|ALGORITHM|FORMAT|LENGTH|SIZE|BITS|TIMEOUT|TTL|HEADER|NAME|NAMES|PREFIX|SUFFIX|VERSION|ENCODING|POLICY|COUNT|CACHE|SERVICE|ENDPOINT|REGION|SCOPE|SCOPES|AUDIENCE|ISSUER|EXPIRY|EXPIRATION|ROTATION|REQUIRED|OPTIONAL|SECONDS?|SECS?|MS|MILLIS(?:ECONDS)?|MINUTES?|MINS?|HOURS?|DAYS?|AGE|CAPACITY|RATE|INTERVAL|DELAY|PERIOD|DURATION|VALIDITY|LIFETIME|WINDOW|MAX|MIN|LIMIT|THRESHOLD|BUDGET|RETRY|RETRIES|ATTEMPTS)$/;
function Sue(e){let n=e.replace(si,"").replace(/-/g,"_");return(rn.test(n)||rn.test(mn(n))||TEo(n)||Yen.test(n))&&!$o.test(e)}
var si=/^AUTH0_/i;
var vn=["BUILD","LOCAL","MIRROR","PATH","WITH","WITHOUT","CACHE","DISABLE","IGNORE","ONLY"];
var li="(?:[A-Za-z0-9]+(?:___[A-Za-z0-9]+)*__)+[A-Za-z]{2,}";
var Yen=new RegExp(`^(?:INPUT_)?BUNDLE_(?!(?:${vn.join("|")})__(?!${li}$))\\w*__`,"i");
var di=new Set(["CLAUDE_CODE_CLIENT_KEY","CLAUDE_CODE_API_KEY_HELPER_TTL_MS","CLAUDE_CODE_PROXY_AUTH_HELPER_TTL_MS","CLAUDE_CODE_ENABLE_PROXY_AUTH_HELPER","CLAUDE_CODE_AUTH_FAIL_EXIT_MS","CLAUDE_CODE_ENABLE_TOKEN_USAGE_ATTACHMENT","CLAUDE_CODE_IDLE_TOKEN_THRESHOLD","CLAUDE_CODE_RESUME_TOKEN_THRESHOLD","CLAUDE_CODE_ARG_KEY_SHAPE"]);
var pi=/^CLAUDE_CODE_SKIP_[A-Z0-9_]+_AUTH$/;
function Mjn(e){return di.has(e)||pi.test(e)}
var CRs=new Set(["GITHUB_TOKEN","GH_TOKEN","GH_ENTERPRISE_TOKEN","GITHUB_ENTERPRISE_TOKEN"]);
var wn=["INPUT_","ORG_GRADLE_PROJECT_","POETRY_PYPI_TOKEN_","POETRY_HTTP_BASIC_","CARGO_REGISTRIES_","CONAN_LOGIN_USERNAME_","CONAN_PASSWORD_"];
var dt=["POETRY_HTTP_BASIC_","CONAN_LOGIN_USERNAME_"];
var gi=new RegExp(`^(?:${wn.join("|")})`,"i");
var fi=new RegExp(`^(?:INPUT_)?(?:${dt.join("|")})`,"i");
function Xen(e){let n=e.replace(/-/g,"_");return gi.test(n)&&(fi.test(n)||/USER(?:_?NAME)?_?[0-9]*$/i.test(n)||Sue(n))}
function obe(e={}){let n={...process.env,...e},r=new Set(MEo(n));for(let[s,h]of Object.entries(n))if(h!==void 0&&!jOt(s)&&!odt(s)&&Ken(s,"")===void 0&&REo(s,typeof h==="string"?h:String(h)))r.add(s);return[...r]}
function MEo(e){let n=new Set([...c5e,...sdt(),"GITHUB_TOKEN","GH_TOKEN"]);for(let r of Object.keys(e))if(Sue(r)||Xen(r))n.add(r);return[...n]}
function kR(e){let n=e.length>De?e.slice(0,De):e,r=(h)=>{let y=h.search(/\s/),R=y===-1?h:h.slice(0,y),P=R.lastIndexOf("@");if(P===-1)return!1;let T=R.slice(0,P);if(ln(T,R.slice(P+1)))return!1;return T.includes(":")||!/[/?#]/.test(T)&&mi(T)},s=n.indexOf("://");while(s!==-1){if(s>0&&/[a-z0-9+.-]/i.test(n[s-1]??"")){if(r(n.slice(s+3)))return!0}s=n.indexOf("://",s+3)}if(n.startsWith("//")&&r(n.slice(2)))return!0;return/^[^\s/@:]+:(?!\/\/)(?![\\/])[^\s@]*@[^\s/@]+/.test(n)&&!/^[a-z]:[\\/]/i.test(n)&&!Si.test(n)&&!ln(n.slice(0,n.lastIndexOf("@",n.search(/\s|$/))),n.slice(n.lastIndexOf("@",n.search(/\s|$/))+1))&&!/^(?:mailto|sips?|xmpp|im|acct):/i.test(n)||Ri.test(n)||yi.test(n)||/["']?sonar\.login["']?\s*[:=]\s*["']?[^\s"',}]{8,}/.test(n)||DEo.test(n)||/\bauthorization\s*[:=]\s*["']?[a-z][a-z0-9_-]*\s+[A-Za-z0-9._~+\/=-]{8,}/i.test(n)||/\b(?:[Tt]oken|TOKEN)\s+(?=[A-Za-z0-9_~+=-]*[0-9])(?=[A-Za-z0-9_~+=-]*[A-Z])(?=([A-Za-z0-9_~+=-]{20,}))\1(?![\/.])/.test(n)||/\b(?:Bearer|Basic)\s+(?=[A-Za-z._~+\/=-]*[0-9]|(?:[A-Za-z0-9._~+\/=-]*?[a-z][A-Z](?![a-z])){2}|[A-Za-z0-9.-]*[_~+\/=])[A-Za-z0-9._~+\/=-]{8,}/.test(n)}
var De=8192;
function mi(e){return/^(?:gh[opusr]_|github_pat_|glpat-|xox[abpr]-|sk-|pk-|AKIA|eyJ|ya29\.|npm_)/.test(e)||e.length>=20&&/[0-9]/.test(e)&&/[a-z]/i.test(e)}
function ln(e,n){let r=/^([a-z0-9_-]+(?:\.[a-z0-9_-]+)*):\d+([/?#].*)$/is.exec(e);if(!r)return!1;if(/[?#&]/.test(r[2]))return!0;if(Ei.test(n))return!1;if(e.endsWith("/"))return!0;if(/^(?:v?\d|sha\d*:)/i.test(n))return!0;return!hi.test(n)}
var Si=/^(?:[\w.-]+(?::[\w.-]+)?@sha(?:256|384|512):[0-9a-f]{32,}|npm:(?:@[\w.-]+\/)?[\w.-]+@[\w.^~<>=*|+-]*|[a-z_][\w.-]*:[\w.-]+:v?\d[\w.+\[\](),-]*@(?:jar|war|ear|aar|apk|aab|pom|zip|tar|tgz|module|klib|exe|dll|so|dylib)?)$/i;
var Ei=/^(?:localhost|[a-z0-9_-]+(?:\.[a-z0-9_-]+)*\.(?=[a-z0-9-]*[a-z])[a-z0-9-]+|[a-z0-9_.-]+(?=:\d+(?:[/?#]|$))|\d{1,3}(?:\.\d{1,3}){3}|\[[0-9a-f:.]+\])(?::\d+)?(?:[/?#]|$)/i;
var hi=/^(?:(?=[a-z0-9._-]*[a-z])[a-z0-9._-]+|\d{1,3}(?:\.\d{1,3}){3}|\[[0-9a-f:.]+\])(?::\d+)?(?:[/?#]|$)/i;
var DEo=/-----BEGIN [A-Z ]*PRIVATE KEY(?: BLOCK)?-----/;
var yi=/https:\/\/(?:hooks\.slack\.com\/(?:services|workflows|triggers)\/|(?:ptb\.|canary\.)?discord(?:app)?\.com\/api\/webhooks\/\d+\/|[\w.-]+\.webhook\.office\.com\/webhookb2\/)[\w\/@.~-]{16,}/i;
var Ri=/(?:(?:^|[;&?#,{]|\/:)\s*|\s)["']?(?!-*jobserver-auth\s*[=:])(?:[a-z0-9_.-]{0,64}(?:password|passwd|pwd|secret|token|(?:account|access|api|private|subscription)[-_]?key|signature|sig|credential)|[a-z0-9_.-]{0,63}[_.-]auth)["']?\s*[=:]\s*["']?(?!(?:true|false|none|null|yes|no|on|off|enabled|disabled|required|optional)(?:$|[;&,\s"']))[^;&,\s"']+/i;
function Pi(e){let n=e.split(/\s+/).filter(Boolean),r=!1,s=n.map((P)=>{let T=P.replace(/["']/g,""),b=T.replace(/^--?[\w-]+=/,""),I=/(?:^|[=\s])--?X[= ]?([^\s=]+)=(.*)$/.exec(T),v=I??(r?/^()([^\s=-][^\s=]*)=(.*)$/.exec(T):null);if(r=/(?:^|=)--?X$/.test(T),v===null)return{bare:T,contribution:b,goes:/(?:^|[=\s])--?X\b/.test(T)&&!/(?:^|=)--?X$/.test(T)||kR(b)};let H=v.at(-2)??"",k=H.split("/").at(-1)??"",w=v.at(-1)??"",F=I===null?"":T.slice(0,I.index);return{bare:T,contribution:b,goes:bn(k,w)||kR(w)||kR(H)||F!==""&&kR(F)}});for(let P=0;P<s.length;P++)for(let T=2;T<=4&&P+T<=s.length;T++){let b=s.slice(P,P+T);if(b.some((I)=>I.goes)||b.slice(1).some((I)=>I.bare.startsWith("-")))break;if(kR(b.map((I)=>I.contribution).join(" "))){for(let I of b)I.goes=!0;break}}s.forEach((P,T)=>{if(!/(?:^|=)--?X$/.test(P.bare))return;let b=s[T+1],I=b===void 0||b.goes||P.goes;if(P.goes=I,b!==void 0)b.goes=I});let h=n.map(()=>-1),y=-1,R="";if(n.forEach((P,T)=>{if(y===-1){let b=P[0];if((b==='"'||b==="'")&&!(P.length>1&&P.endsWith(b)))y=T,R=b,h[T]=y;return}if(h[T]=y,P.endsWith(R))y=-1}),y!==-1)s.forEach((P,T)=>{if(h[T]===y)P.goes=!0});if(s.forEach((P,T)=>{if(P.goes&&h[T]!==-1)s.forEach((b,I)=>{if(h[I]===h[T])b.goes=!0})}),s.every((P)=>!P.goes))return e;return n.filter((P,T)=>!s[T].goes&&(h[T]!==-1||Pn.test(P))).join(" ")}
var Ti=["LD_","DYLD_","BASH_FUNC_","__BASH_FUNC<","PYTHON","PERL5","RUBY","LUA_","DOTNET_","COMPLUS_","COR_","CORECLR_","APPDOMAIN_MANAGER_","GIT_"];
var Ai=new Set(["NODE_OPTIONS","NODE_PATH","BASH_ENV","ENV","SHELLOPTS","BASHOPTS","PS4","PERLLIB","GEM_PATH","GEM_HOME","JAVA_TOOL_OPTIONS","_JAVA_OPTIONS","JDK_JAVA_OPTIONS","IBM_JAVA_OPTIONS","OPENJ9_JAVA_OPTIONS","CLASSPATH","BUN_OPTIONS","MONO_PATH","R_PROFILE_USER","DEVPATH","GCONV_PATH","OPENSSL_CONF","OPENSSL_MODULES","OPENSSL_ENGINES","KRB5_CONFIG","GTK_PATH","QT_PLUGIN_PATH","GIO_MODULE_DIR","FPATH","IFS","CDPATH","SSH_ASKPASS","SSH_ASKPASS_REQUIRE","SASL_PATH","PHPRC","PHP_INI_SCAN_DIR","PATH","COMSPEC","ZDOTDIR","XDG_CONFIG_HOME","PSMODULEPATH","SYSTEMROOT","WINDIR","SYSTEMDRIVE","PATHEXT"]);
var Oi="/usr/bin:/bin:/usr/sbin:/sbin";
var Ii="C:\\Windows\\System32;C:\\Windows;C:\\Windows\\System32\\Wbem;C:\\Windows\\System32\\WindowsPowerShell\\v1.0";
var Ci="C:\\Windows\\System32\\cmd.exe";
var Ni="C:\\Program Files\\WindowsPowerShell\\Modules;C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\Modules";
var bi=".COM;.EXE;.BAT;.CMD;.VBS;.VBE;.JS;.JSE;.WSF;.WSH;.MSC";
var He="/var/empty/claude-code-policy-helper";
var Li="C:\\Windows\\claude-code-policy-helper";
function vi(e){let n=e.toUpperCase();return Ai.has(n)||Ti.some((r)=>n.startsWith(r))}
function aAr(e,n){let r={...e},s=new Set(obe(e).map((h)=>h.toUpperCase()));for(let h of Object.keys(r))if(vi(h)||s.has(h.toUpperCase()))delete r[h];if(Object.assign(r,_ue(r).respelled),r.PYTHONNOUSERSITE="1",r.GIT_CONFIG_NOSYSTEM="1",n==="win32")r.GIT_CONFIG_GLOBAL=Li,r.PATH=Ii,r.COMSPEC=Ci,r.PSModulePath=Ni,r.PATHEXT=bi,r.SystemRoot="C:\\Windows",r.windir="C:\\Windows",r.SystemDrive="C:";else r.GIT_CONFIG_GLOBAL=He,r.PATH=Oi,r.ZDOTDIR=He,r.XDG_CONFIG_HOME=He,r.PSModulePath=He;return r}
var Di=1e4;
var Me=1048576;
var Hi=p(()=>_t({managedSettings:oe().optional(),claudeMd:o().optional(),appendSystemPrompt:o().optional()}));
function ke(e){return{message:e,remote:null}}
function fe(){let e=$i();if(j()&&e.primer!==void 0)e.invalidatePolicyLayer();else Ir()}
class Un{state=null;selectionWarnings=[];initializeAttempted=!1;structuralRefusalLogged=!1;noEntrySadLogged=!1;defaultFallback=null;payloadRefusalError=null;refreshTimer=null;refreshInFlight=!1;midSessionArmingEnabled=!1;osAdminArming=null;startsNoSession=!1;decidedOrigin=null;pendingStats=new Map;remoteArmGeneration=0;remoteArmingConfig=null;remoteFailedConfig=null;lastFailure=null;selectedEntry=null;releaseLatchWhenTickSettles=!1;readRemotePayload=null;armedFromUserWritableBase=!1;retiredHelperPaths=new Set;remoteDecision=null;refreshed=Ie();claimInitialize(){if(this.initializeAttempted)return!1;return this.initializeAttempted=!0,!0}apply(e){if(this.state=e,this.remoteFailedConfig=null,this.lastFailure=null,fe(),e.config)this.startRefreshTimer(e.config,e.fromPerOs,e.armedFromRemote);else this.stopRefreshTimer()}retireState(){let e=this.state?.config?.path;if(e!==void 0)this.retiredHelperPaths.add(e);this.state=null,this.lastFailure=null,this.selectedEntry=null}reset(){this.remoteArmGeneration++,this.remoteArmingConfig=null,this.armedFromUserWritableBase=!1,this.remoteDecision=null,this.state=null,this.retiredHelperPaths.clear(),this.selectionWarnings=[],this.initializeAttempted=!1,this.structuralRefusalLogged=!1,this.noEntrySadLogged=!1,this.defaultFallback=null,this.payloadRefusalError=null,this.refreshInFlight=!1,this.midSessionArmingEnabled=!1,this.osAdminArming=null,this.decidedOrigin=null,this.remoteFailedConfig=null,this.lastFailure=null,this.selectedEntry=null,this.releaseLatchWhenTickSettles=!1,this.readRemotePayload=null,this.stopRefreshTimer(),fe()}announceTierChange(){fe();try{$i().changed.emit("policySettings"),this.refreshed.emit()}catch(e){c(e)}}stopRefreshTimer(){if(this.refreshTimer)clearInterval(this.refreshTimer),this.refreshTimer=null}holdRemoteFailure({config:e,fromPerOs:n},r){if(this.remoteFailedConfig=e,!r)return;if(e.path!==void 0)this.retiredHelperPaths.add(e.path);this.startRefreshTimer(e,n,!0)}retireRemoteFailure(){if(this.remoteFailedConfig===null)return;this.remoteFailedConfig=null,this.lastFailure=null,this.selectedEntry=null,this.stopRefreshTimer(),this.releaseLatch()}releaseLatch(){if(this.refreshInFlight)this.releaseLatchWhenTickSettles=!0;else this.initializeAttempted=!1}settleRefreshExec(){if(this.refreshInFlight=!1,this.releaseLatchWhenTickSettles)this.releaseLatchWhenTickSettles=!1,this.releaseLatchAfterTornDownExec()}releaseLatchAfterTornDownExec(){if(this.initializeAttempted=!1,this.readRemotePayload!==null)cAr(this.readRemotePayload)}startRefreshTimer(e,n,r){this.stopRefreshTimer();let s=e.refreshIntervalMs??0;if(s<=0)return;this.refreshTimer=setInterval((h,y,R)=>{if(this.refreshInFlight)return;if(this.refreshInFlight=!0,R&&this.state===null){Wi(h,y).finally(()=>this.settleRefreshExec());return}let P=this.state;It(h,y,R,()=>this.state===P).then((T)=>{if(this.state===null||this.state!==P)return;let b=R&&Ue(h)||h;if("error"in T){if(this.defaultFallback){let I=`the static ${this.defaultFallback.sourceField} settings payload governs`;if(this.state.serving==="default"){if(t(`policyHelper refresh: helper still failing (${T.error}); the static default settings payload continues to govern`,{level:"debug"}),pt(T.error,I,y),this.lastFailure!==T.code)this.lastFailure=T.code,this.announceTierChange();return}t(`policyHelper refresh failed (${T.error}); applying the static ${this.defaultFallback.sourceField} settings payload`,{level:"warn"}),pt(T.error,I,y),f("settings_policy_helpers_per_os","refresh_fell_back_to_default"),this.state.serving="default",this.lastFailure=T.code,this.state.mergesOutput=ht(b,this.defaultFallback),this.state.output={managedSettings:$e(this.defaultFallback.settings,R,Dy())},this.state.warnings=this.defaultFallback.warnings,this.announceTierChange();return}if(t(`policyHelper refresh failed (retaining current policy): ${T.error}`,{level:"warn"}),pt(T.error,"the last successful helper output still governs",y),f("settings_policy_helper","refresh_failed",{attempts:T.attempts}),this.lastFailure!==T.code)this.lastFailure=T.code,this.announceTierChange();return}if(Tt(),this.state.serving==="default")t("policyHelper refresh: helper recovered; its output replaces the static default settings payload",{level:"info"}),g("settings_policy_helpers_per_os");this.state.serving="helper",this.lastFailure=null,this.state.mergesOutput=b.outputBehavior==="merge",this.state.output=T.output,this.state.warnings=T.warnings,this.announceTierChange()}).catch((T)=>{c(T)}).finally(()=>this.settleRefreshExec())},Math.min(s,rTr),e,n,r),this.refreshTimer.unref?.()}}
var A=new Un;
var $n=new Set(["plist","hklm","file"]);
async function WOt(e,n,r){let s=await Bn(e,n,r);if(s===null)return null;if(s.remote){if(A.startsNoSession)return Gn(s.message,s.remote),null;Wn(s.remote)}return s.message}
function Gn(e,n){t(`policyHelper: ${e} (reached in a command that starts no session; it proceeds without the helper)`,{level:"error"}),ge('onFailure is "refuse", but this command starts no Claude Code session, so it proceeds without the helper; sessions will not start until the helper succeeds',n.tuple),f("settings_policy_helpers_per_os","remote_refuse_not_enforced_no_session")}
function Wn(e){m("settings_policy_helpers_per_os",e.code,e.helperFailureReason===void 0?void 0:{helper_failure_reason:d(e.helperFailureReason)})}
function Kn(e){return e.find((n)=>n.startupFatal&&(n.path===""||n.path==="policyHelpers"||n.path.startsWith("policyHelpers.")))}
async function Bn(e,n,r){let s=Kn(r);if(s){if(!A.structuralRefusalLogged){A.structuralRefusalLogged=!0;let w=s.path;m("settings_policy_helpers_per_os",w===""?"document_invalid":w==="policyHelpers"?"structural_invalid":w==="policyHelpers.default"||w.endsWith(".defaultSettings")?"default_payload_invalid":"entry_invalid")}return ke(`${s.file??"managed settings"}: ${s.message}`)}if(A.payloadRefusalError!==null)return ke(A.payloadRefusalError);if(!A.claimInitialize())return null;A.decidedOrigin=n,Xn(),A.remoteDecision=null,A.selectedEntry=null,_e(Qn),_e(St);let h=e?.policyHelpers,y=n==="remote"&&lAr(e),R=y?At(e):null,P=R?.tuple,T=y?void 0:e?.policyHelper;if(!h&&!T){if(FW()===!0&&!NS())t("policyHelper: no helper configuration present at helper-pass time (remote managed settings eligible, no payload in cache); a payload landing later arms one only through a fetch cycle after preAction",{level:"debug"});return A.initializeAttempted=!1,null}if(R){if(A.remoteDecision=R,!R.verified&&P!==void 0)return A.initializeAttempted=!1,ge("remote settings not verified this session",P),null}else if(n==="remote"&&h)return t("remote policyHelpers names no helper and carries no static payload for this platform",{level:"debug"}),A.initializeAttempted=!1,null;else if(n===null||!$n.has(n)){if(t(`policyHelper ignored: delivered via non-admin source '${n??"unknown"}'`,{level:"warn"}),n==="remote")A.initializeAttempted=!1;return null}A.armedFromUserWritableBase=n==="plist"&&$8().userWritable===!0;let b=h?Lue():null,I=null;if(R)Et(R);else I=xi(h,b?.chain??[]);if(I)return A.payloadRefusalError=I,ke(I);let v=Gi(h,T,b,y);if(!v)return A.initializeAttempted=!1,null;if(A.selectedEntry=v.kind==="helper"?{key:v.entryKey,config:v.config}:{key:PB.find((w)=>v.fallback.sourceField===`policyHelpers.${w}.defaultSettings`)??"default",config:null},v.kind==="default"){if(R&&!R.payloadServable){if(A.initializeAttempted=!1,ge(R.verified?`${v.fallback.sourceField} carries settings not yet approved in the managed-settings dialog; it is not applied until they are`:`${v.fallback.sourceField} carries settings the managed-settings dialog lists and remote settings are not verified this session; it is not applied until they are`,void 0),R.verified)f("settings_policy_helpers_per_os","remote_consent_missing");return null}return f("settings_policy_helpers_per_os","fell_back_to_default"),t(`policyHelper: no policyHelpers helper entry for platform "${b?.platform??"unknown"}"; applying the static ${v.fallback.sourceField} settings payload`,{level:"info"}),gt(null,v.fallback.mergesOutput,y),null}let H=A.defaultFallback,k=zn(v.fromPerOs?v.config:null,y);if(R&&!R.helperMayExec){if(H&&R.payloadServable)return gt(null,ht(v.config,H),!0),ge(`not yet approved in the managed-settings dialog; the static ${H.sourceField} settings payload governs until it is`,P),f("settings_policy_helpers_per_os","remote_consent_missing"),null;if(A.initializeAttempted=!1,ge(H?`not yet approved in the managed-settings dialog, and neither are settings the static ${H.sourceField} payload carries; nothing from the entry applies until they are`:"not yet approved in the managed-settings dialog",P),k)return{message:`Claude Code cannot start: your organization's remote managed settings require the policy helper ${jn(P)} to succeed (onFailure "refuse"), and it cannot run until those settings are approved in the managed-settings dialog. Contact your administrator.`,remote:{code:"remote_refused_on_failure",helperFailureReason:void 0,tuple:P}};return f("settings_policy_helpers_per_os","remote_consent_missing"),null}try{if(v.fromPerOs){if(await Hn(v,{armedFromRemote:y,suppressExecEvents:y||H!==null})==="applied")g("settings_policy_helpers_per_os")}else await Hn(v)}catch(w){let F=A.defaultFallback,K=y?A.remoteDecision?.helperRefusesOnFailure??k:k;if(v.fromPerOs&&!F)if(!(w instanceof We))m("settings_policy_helpers_per_os","error");else if(y&&K);else if(!y&&!k)m("settings_policy_helpers_per_os","continued_on_failure",{helper_failure_reason:d(w.code)});else m("settings_policy_helpers_per_os",w.code);if(w instanceof We){if(F)return t(`${w.message}; applying the static ${F.sourceField} settings payload instead`,{level:"warn"}),f("settings_policy_helpers_per_os","fell_back_to_default_on_failure"),gt(w.code==="bad_path"?null:v,ht(v.config,F),y),A.lastFailure=w.code,null;if(A.lastFailure=w.code,y)return ge(w.message,P),A.holdRemoteFailure(v,w.code!=="bad_path"),K?Fi(w,P):null;if(!k)return Mi(w.message),null;return ke(w.message)}throw w}return null}
function zn(e,n){let r=e?.onFailure;return r==null?!n:r!=="continue"}
function Fi(e,n){return{message:`Claude Code cannot start: your organization's remote managed settings require the policy helper ${jn(n)} to succeed (onFailure "refuse"), and it failed [${e.code}] \u2014 ${e.message}. Contact your administrator.`,remote:{code:"remote_refused_on_failure",helperFailureReason:e.code,tuple:n}}}
function jn(e){return(e===void 0?void 0:iTr(e))??"it configures"}
var St='policyHelpers entry skipped (onFailure "continue"): ';
var Dn="; Claude Code started without the helper's output";
function Mi(e){let n=Ke(Mdt(e.replace(/\s+/gu," ")),yt-St.length-Dn.length);Pt(`${St}${n}${Dn}`)}
var Vn="remote policyHelpers entry not run: ";
var Jn="remote policyHelpers static settings payload not applied: ";
var yt=512;
function Ke(e,n=yt){return e.length>n?`${e.slice(0,n-1)}\u2026`:e}
function ge(e,n){let r=Mdt(e.replace(/\s+/gu," ")),s=n===void 0?void 0:iTr(n),h=Ke(n===void 0?`${Jn}${r}`:`${Vn}${s?`${s}: `:""}${r}`);Pt(h)}
var Rt="remote policyHelpers static settings payload skipped: ";
function ki(e,n,r){if(Pt(Ke(`${Rt}${e} is not a valid static settings payload (${Mdt(n.replace(/\s+/gu," "))})`)),r)f("settings_policy_helpers_per_os","remote_default_payload_invalid")}
function Pt(e){A.selectionWarnings.push({file:"policyHelper",path:"policyHelpers",message:e,severity:"warning",statusOnly:!0}),fe(),t(e,{level:"warn"})}
function Xn(){Zn(),_e(Rt)}
function Zn(){_e(Vn),_e(Jn),A.retireRemoteFailure()}
var xe="policyHelper refresh failing: ";
function pt(e,n,r){let s=`; ${n}`,h=Ke(Mdt(e.replace(/\s+/gu," ")),yt-xe.length-s.length),y=`${xe}${h}${s}`;if(A.selectionWarnings.some((T)=>T.message===y))return;let R=A.selectionWarnings.filter((T)=>!T.message.startsWith(xe)),P=R.length!==A.selectionWarnings.length;if(A.selectionWarnings=[...R,{file:"policyHelper",path:r?"policyHelpers":"policyHelper",message:y,severity:"warning",statusOnly:!0}],!P)fe()}
function Tt(){_e(xe)}
var Qn="policyHelpers is configured but has no entry for platform ";
function _e(e){let n=A.selectionWarnings.filter((r)=>!r.message.startsWith(e));if(n.length===A.selectionWarnings.length)return;A.selectionWarnings=n,fe()}
function qn(e,n){let r=fTr(e,n);for(let s of r.strippedKeys)t(`${n}: stripped ${s} from the settings payload (no recursion)`,{level:"warn"});for(let s of r.warnings)t(`${n}: ${s.message}`,{level:"warn"});return r}
function er(e,n){let r=[];for(let s of n){let h=e[s],y=h?.defaultSettings;if(y!==void 0&&y!==null)r.push({field:`policyHelpers.${s}.defaultSettings`,raw:y,onChain:!0,mergesOutput:h?.outputBehavior==="merge"})}for(let s of PB){let h=e[s]?.defaultSettings;if(h!==void 0&&h!==null&&!n.includes(s))r.push({field:`policyHelpers.${s}.defaultSettings`,raw:h,onChain:!1,mergesOutput:!1})}if(e.default!==void 0&&e.default!==null)r.push({field:"policyHelpers.default",raw:e.default,onChain:!0,mergesOutput:!1});return r}
function tr(e,n){if(e.raw!==null&&typeof e.raw==="object"&&!Array.isArray(e.raw)){let{policyHelper:s,policyHelpers:h,...y}=e.raw,R=I5e().safeParse(y);if(!R.success)return{error:R.error.issues[0]?.message??"failed validation"}}let r=n?qn(e.raw,e.field):fTr(e.raw,e.field);if("error"in r)return{error:r.error};return{settings:r.settings,warnings:r.warnings.map((s)=>({...s,statusOnly:!0})),sourceField:e.field,mergesOutput:e.mergesOutput}}
function xi(e,n){if(A.defaultFallback=null,!e)return null;let r=null;for(let s of er(e,n)){let h=tr(s,s.onChain&&!r);if("error"in h)return m("settings_policy_helpers_per_os","default_payload_invalid"),`${s.field} is not a valid static settings payload (${h.error}); Claude Code will not start until it is fixed`;if(s.onChain&&!r)r=h}return A.defaultFallback=r,null}
function nr(e){let n=null,r=[];for(let s of e){let h=tr(s,!1);if("error"in h)r.push({field:s.field,error:h.error});else if(!n)n={...h,warnings:F5e(h.warnings)}}return{winner:n,skipped:r}}
function Et(e){_e(Rt),A.defaultFallback=e.winner;for(let{field:n,error:r}of e.skipped)ki(n,r,e.verified)}
function or(e,n){return zxs(U$(e),n.sourceField)||eZ(U$(n.settings))}
function ir(e){let n=e?.policyHelpers;return n?er(n,Lue().chain).filter((r)=>r.onChain):[]}
function $e(e,n,r){return n&&!r?jko(e):e}
function At(e){let n=Dy(),r=E0t(),s=e?.policyHelpers,h=s?Ae(s,Lue().chain,!0)?.config??null:null,y=b0t(h),R=ir(e),{winner:P,skipped:T}=nr(R);return{verified:n,tuple:y,helperMayExec:y!==void 0&&r,helperMergesOutput:h?.outputBehavior==="merge",helperRefusesOnFailure:zn(h,!0),payloadKey:_(SP(R.map((b)=>[b.field,b.raw,b.mergesOutput]))),winner:P,skipped:T,payloadServable:P!==null&&(r||!or(e,P)&&(n||y===void 0))}}
function sr(e,n){return e.verified===n.verified&&e.tuple===n.tuple&&e.helperMayExec===n.helperMayExec&&e.helperMergesOutput===n.helperMergesOutput&&e.helperRefusesOnFailure===n.helperRefusesOnFailure&&e.payloadKey===n.payloadKey&&e.payloadServable===n.payloadServable}
function ht(e,n){return e.outputBehavior==="merge"||n?.mergesOutput===!0}
function gt(e,n,r){let s=A.defaultFallback;if(!s)return;let h=Dy();A.apply({config:e?.config??null,fromPerOs:e?.fromPerOs??!1,serving:"default",armedFromRemote:r,mergesOutput:n,output:{managedSettings:$e(s.settings,r,h)},warnings:s.warnings}),t(`${s.sourceField} static settings payload applied${r?` from remote managed settings${h?"":" (cached, not yet verified this session)"}`:""} (keys: ${Object.keys(s.settings).join(",")})`,{level:"debug"})}
function lAr(e){let n=e?.policyHelpers;if(!n)return!1;let r=Lue().chain;return Ae(n,r,!0)!==null||n.default!==void 0&&n.default!==null||r.some((s)=>{let h=n[s]?.defaultSettings;return h!==void 0&&h!==null})}
function Ae(e,n,r){for(let s of n){let h=e[s];if(h==null)continue;if(r?b0t(h)!==void 0:h.path!=null||h.script!=null){let{defaultSettings:R,...P}=h;return{osKey:s,config:P}}}return null}
function Gi(e,n,r,s){if(!e||!r)return n?{kind:"helper",entryKey:"policyHelper",config:n,fromPerOs:!1}:null;let{platform:h,chain:y}=r,R=Ae(e,y,s);if(R)return{kind:"helper",entryKey:R.osKey,config:R.config,fromPerOs:!0};let P=A.defaultFallback;if(P)return{kind:"default",fallback:P};if(n)return f("settings_policy_helpers_per_os","fell_back_to_singular"),{kind:"helper",entryKey:"policyHelper",config:n,fromPerOs:!1};if(s)return null;let T=`${Qn}"${h}", no default settings payload, and no policyHelper fallback; no policy helper will run`;if(A.selectionWarnings.push({file:"policyHelper",path:"policyHelpers",message:T,severity:"warning",statusOnly:!0}),fe(),t(T,{level:"warn"}),!A.noEntrySadLogged)A.noEntrySadLogged=!0,f("settings_policy_helpers_per_os","no_entry_for_platform");return null}
class We extends Error{code;constructor(e,n){super(n);this.code=e}}
async function Hn({config:e,fromPerOs:n},r){let s=r?.armedFromRemote===!0,h=A.remoteArmGeneration;if(s)A.remoteArmingConfig=e;let y;try{y=await It(e,n,s,()=>!s||A.remoteArmGeneration===h)}finally{if(A.remoteArmingConfig===e)A.remoteArmingConfig=null}if(s&&A.remoteArmGeneration!==h){if(e.path!==void 0&&!(("error"in y)&&y.code==="bad_path"))A.retiredHelperPaths.add(e.path);return t("policyHelper: remote arming revoked during exec; discarding output",{level:"warn"}),f("settings_policy_helpers_per_os","deactivated_during_exec"),A.releaseLatchAfterTornDownExec(),"dropped"}if("error"in y){if(!r?.suppressExecEvents)m("settings_policy_helper",y.code,{attempts:y.attempts});throw new We(y.code,`policyHelper failed: ${y.error}`)}if(A.apply({config:e,fromPerOs:n,serving:"helper",armedFromRemote:s,mergesOutput:e.outputBehavior==="merge",output:y.output,warnings:y.warnings}),t(`policyHelper applied (keys: ${Object.keys(y.output).join(",")})`,{level:"debug"}),!r?.suppressExecEvents)g("settings_policy_helper",{attempts:y.attempts});return"applied"}
function ar(){return A.state?.output.managedSettings??null}
function cr(){return A.state?.armedFromRemote===!0}
function Jen(){return A.state!==null&&A.armedFromUserWritableBase}
function ur(){return A.state?.mergesOutput===!0}
function Te(){if(A.remoteArmGeneration++,Xn(),A.remoteDecision!==null)A.defaultFallback=null,A.remoteDecision=null,A.selectedEntry=null;if(A.state?.armedFromRemote!==!0)return;Tt(),A.stopRefreshTimer(),A.retireState(),A.releaseLatch(),t("policyHelper: remote-armed helper (or remote static payload) deactivated",{level:"info"}),A.announceTierChange()}
function Ue(e){let n=A.readRemotePayload;if(n===null||!E0t())return null;let r=n()?.policyHelpers,s=r?Ae(r,Lue().chain,!0)?.config??null:null;return s!==null&&b0t(s)===b0t(e)?s:null}
async function Wi(e,n){try{if(Ue(e)===null){t("policyHelper: remote retry stopped; the payload in force no longer authorizes the entry",{level:"info"}),Te();return}let r=A.remoteArmGeneration;A.remoteArmingConfig=e;let s;try{s=await It(e,n,!0,()=>A.remoteArmGeneration===r&&Ue(e)!==null)}finally{if(A.remoteArmingConfig===e)A.remoteArmingConfig=null}let h=A.remoteArmGeneration!==r,y=h?null:Ue(e);if(y===null){if(!h)Te();t("policyHelper: remote retry revoked during exec; discarding output",{level:"warn"}),f("settings_policy_helpers_per_os","deactivated_during_exec");return}if("error"in s){t(`policyHelper retry failed (remote entry still not armed): ${s.error}`,{level:"warn"}),f("settings_policy_helper","refresh_failed",{attempts:s.attempts});return}let R={...e,outputBehavior:y.outputBehavior};A.apply({config:R,fromPerOs:n,serving:"helper",armedFromRemote:!0,mergesOutput:R.outputBehavior==="merge",output:s.output,warnings:s.warnings}),Zn(),t("policyHelper: remote entry armed by a retry tick",{level:"info"}),g("settings_policy_helpers_per_os"),A.announceTierChange()}catch(r){c(r)}}
function cAr(e){Bi(e),Ki()}
function Ki(){let e=A.osAdminArming;if(e===null||!A.midSessionArmingEnabled||A.initializeAttempted||!e.baseSettled())return;let{settings:n,origin:r,loadErrors:s}=e.readBase();if(r===null||!$n.has(r)||r===A.decidedOrigin)return;WOt(n,r,s).then((h)=>{if(h!==null){t(`policyHelper: the ${r} source became the base mid-session and its pass refused; exiting as the launch would have: ${h}`,{level:"error"}),e.refuse(h),A.decidedOrigin=r;return}if(A.state===null)return;t(`policyHelper: armed from the ${r} source, which became the base mid-session`,{level:"info"}),A.announceTierChange()}).catch((h)=>{c(h)})}
function Bi(e){if(A.readRemotePayload=e,!A.midSessionArmingEnabled||A.initializeAttempted||!Dy())return;let n=e();if(!lAr(n))return;let r=A.remoteDecision;if(r!==null&&sr(r,At(n)))return;let s=A.retiredHelperPaths.size,h=A.remoteFailedConfig;Bn(n,"remote",[]).then((y)=>{if(y===null)return;if(y.remote&&A.startsNoSession){Gn(y.message,y.remote);return}let R=A.osAdminArming;if(R!==null&&(!kp()||R.isUnwatchedRepl())){if(t(`policyHelper: ${y.message} (reached after this unwatched session started; ending it as the launch refusal would have)`,{level:"error"}),y.remote)Wn(y.remote);R.refuse(y.message),A.midSessionArmingEnabled=!1;return}if(t(`policyHelper: ${y.message} (reached after this interactive session started; it continues without the helper)`,{level:"error"}),y.remote)ge('onFailure is "refuse", but this was reached after this interactive session started, so it continues on the remote managed settings without the helper; new sessions will not start (and non-interactive or unwatched ones are ended) until the helper succeeds',y.remote.tuple),f("settings_policy_helpers_per_os","remote_refuse_unenforceable_midsession")}).catch((y)=>{c(y)}).finally(()=>{let y=A.state?.armedFromRemote===!0;if(y)t(`policyHelper: remote ${A.state?.serving==="default"?"static payload applied":"entry armed"} mid-session`,{level:"info"});if(y||A.retiredHelperPaths.size>s||A.remoteFailedConfig!==h)A.announceTierChange()})}
function LRs(){return A.state?.config?.path??null}
function NRs(){return[...A.retiredHelperPaths]}
function FRs(e){let n=e?.policyHelpers,r=n?Lue().chain.map((y)=>n[y]).filter((y)=>y!=null).map((y)=>({config:y,fromPerOs:!0})):[];if(e?.policyHelper!=null)r.push({config:e.policyHelper,fromPerOs:!1});let s=pr(),h=[];for(let{config:y,fromPerOs:R}of r)if(typeof y.path==="string"&&gr(y,s,R,!1)===null)h.push(y.path);return h}
function Ot(){return[...A.selectionWarnings,...A.state?.warnings??[]]}
var dr=null;
var ji=500;
var Vi=250;
var Yi=4000;
function Ji(e){switch(e){case"exit_nonzero":case"timed_out":case"not_a_file":case"interpreter_unavailable":return!0;case"bad_path":case"oversize":case"parse_failed":case"envelope_invalid":case"schema_rejected":return!1}}
var Fn=2048;
function Xi(e){let n=zd(e);if(n.length<=Fn)return n;let r=ne(n,Fn);return`${r}\u2026 (+${n.length-r.length} chars not shown; the full output is in the debug log)`}
async function It(e,n,r,s){let h=n?e.retries:void 0,y=h!==void 0&&Number.isInteger(h)&&h>0?Math.min(h,Tko):0;for(let R=1;;R++){let P=await Zi(e,n,r);if(!("error"in P))return{...P,attempts:R};let T=Xi(P.error),b={error:R>1?`after ${R} attempts: ${T}`:T,code:P.code,attempts:R};if(R>y||!Ji(P.code)||!s())return b;let I=PH({baseMs:ji,floorMs:Vi,capMs:Yi,attempt:R-1});if(t(`policyHelper attempt ${R} of ${y+1} failed [${P.code}]; retrying in ${I}ms: ${T}`,{level:"warn"}),await Q(I),!s())return b}}
async function Zi(e,n,r){let s=pr(),h=gr(e,s,n,r);if(h)return{error:h,code:"bad_path"};let y=en(e);if("error"in y)return y;let{plan:R}=y,P=Math.min(e.timeoutMs??Di,rTr),T=performance.now(),b=await tn(R,P,A.pendingStats);if(b)return{error:b,code:"not_a_file"};let I=Math.ceil(P-(performance.now()-T));if(I<=0)return{error:it(R.file,P),code:"not_a_file"};let v;if(!r||R.input!==void 0)v=qt(R);else if(e.path===void 0)return{error:"remote-armed helper has no path",code:"bad_path"};else v=(s==="win32"?xn:Mn).dirname(e.path);let H=process.env,k=r?aAr(dr?.()??H,s):H,{stdout:w,stderr:F,code:K,error:U,maxBufferExceeded:z,timedOut:Y}=await lt(R.file,R.args,{timeout:I,cwd:v,useToolMemoryCgroup:!1,maxBuffer:Me+1,env:{...Qt(k,R),CLAUDE_CODE_VERSION:{ISSUES_EXPLAINER:"report the issue at https://github.com/anthropics/claude-code/issues",PACKAGE_URL:"@anthropic-ai/claude-code",README_URL:"https://code.claude.com/docs/en/overview",VERSION:"2.1.295",FEEDBACK_CHANNEL:"https://github.com/anthropics/claude-code/issues",BUILD_TIME:"2026-10-08T16:50:59Z",GIT_SHA:"07e8f67ea3282bf154a9e05673a0942b1b173cef",HOOKS_WORKER_URL:"/$bunfs/root/src/plugins/functionHooks/hooks-worker/hooks-worker.js",DD_SOURCEMAP_GROUP:"darwin"}.VERSION},extendEnv:!1,stdin:R.input===void 0?"ignore":"pipe",input:R.input});if(F)t(`policyHelper stderr: ${F}`,{level:"debug"});if(z)return{error:`${F.length>w.length?"stderr":"stdout"} exceeded ${Me} bytes`,code:"oversize"};if(Y)return{error:`timed out after ${P}ms`,code:"timed_out"};let J=(le)=>{if(w)t(`policyHelper stdout: ${w}`,{level:"debug"});return le};if(K!==0)return J({error:`exited with code ${K}: ${F||w||U||""}`,code:"exit_nonzero"});if(Buffer.byteLength(w,"utf8")>Me)return{error:`stdout exceeded ${Me} bytes`,code:"oversize"};let ue=mt(w,!1);if(ue===null||typeof ue!=="object")return J({error:"stdout is not a JSON object",code:"parse_failed"});let te=Hi().safeParse(ue);if(!te.success)return J({error:`invalid envelope: ${te.error.message}`,code:"envelope_invalid"});let Z={},X=[];if(te.data.managedSettings!==void 0){let le=qn(te.data.managedSettings,"policyHelper");if("error"in le)return J({error:le.error,code:"schema_rejected"});if(X=le.warnings,r)X=F5e(X),delete le.settings.claudeMd;Z.managedSettings=le.settings}if(te.data.claudeMd!==void 0)Z.claudeMd=te.data.claudeMd;if(te.data.appendSystemPrompt!==void 0)Z.appendSystemPrompt=te.data.appendSystemPrompt;return{output:Z,warnings:X}}
function pr(){return O()==="windows"?"win32":"posix"}
function gr(e,n,r,s){if(e.script==null&&e.interpreter==null)return Qi(e.path,n,r,s);if(!r)return"inline scripts are not supported on the singular policyHelper key";if(e.path!=null)return Rko;return jxs(e,n==="win32"?"windows":"linux")}
function Qi(e,n,r,s){let h=n==="win32"?xn:Mn;if(typeof e!=="string")return"path must be a string";if(!h.isAbsolute(e))return`path must be absolute: ${e}`;if(n==="win32"){if(r&&!vko.test(e))return`path must end in .exe or .ps1 on Windows: ${e}`;if(!r&&!e.toLowerCase().endsWith(".exe"))return`path must end in .exe on Windows: ${e}`;if(Cko(e))return`${Ako}: ${e}`}if(s&&(n==="win32"?bko(e):Wtn(e)||wko(e)))return`path must not be a UNC, network-automount (/net, /Network/Servers, or macOS /.vol /.file /.nofollow /.resolve) or kernel magic-link (/proc, /dev/fd) path when delivered via remote managed settings: ${e}`;if(!Eko(e,n,{rejectDriveRelative:n==="win32"}))return n==="win32"?`path must be a drive-qualified (C:\\...) or UNC path in normalized form (no "." or ".." segments, no doubled or trailing separators, no ":" or trailing "."/space in a component, no device-namespace prefix): ${e}`:`path must be in normalized form (no "." or ".." segments, no doubled or trailing separators): ${e}`;return null}
async function dAr(e,n,r){let s=await e.read([{key:Fe.userSettings(),offset:0,length:Tf+1}]);if(!s.ok)return{kind:"failing",code:s.error.code,failureClass:"failureClass"in s.error?s.error.failureClass:void 0};let h=s.value.items[0];if(!h.found)return qi(e.hostFiles,n);if(h.totalBytes>Tf)return{kind:"oversize"};let y=ct(h.value),R=r!==void 0&&r.contentHash===y?r.parsed:OE(Rbe(h.value),n);return{kind:"seeded",contentHash:y,size:h.totalBytes,parsed:R}}
async function qi(e,n){let r;try{r=await e.stat(of.home(n))}catch(s){return{kind:"failing",code:l(s)}}if(r.ok)return r.value.kind==="absent"?{kind:"absent"}:{kind:"failing",code:r.value.kind};return Dut(r.error)?{kind:"absent"}:Be(r.error)}
async function FEo(e,n){if(!j())return;let r=e.epoch;try{let s=await n.read(),h=s.kind==="seeded"?s.parsed:s.kind==="absent"&&n.whenAbsent==="seedAbsence"?rZ():void 0;if(h===void 0){t(`settings: ${n.label} not re-seeded (${Ljn(s)}); the file read serves`);return}if(n.source!=="userSettings"&&e.walkReadDiffers(n.path,h)){t(`settings: ${n.label} not re-seeded (the file read already saw different content this generation); the file read serves`);return}if(s.kind==="absent")$5e(n.path);return e.seedParsedFile(n.path,n.source,h,r)?e.retainLayer(n.path,h):void 0}catch(s){t(`settings: ${n.label} not re-seeded: ${l(s)}; the file read serves`,{level:"warn"});return}}
function $Eo(e,n){return{source:"userSettings",path:n,read:()=>dAr(e,n),label:"user settings",whenAbsent:"seedAbsence"}}
function Ljn(e){if(e.kind!=="failing")return e.kind;return e.code===Rxr?"the backend does not serve this space":`backend read failed: ${e.code}`}
function Be(e){return{kind:"failing",code:e.telemetryCode??e.code,failureClass:e.failureClass}}
async function uAr(e,n,r){return FEo(n,$Eo(e,r))}
class yr{firedSites=new Set;fire(e){if(this.firedSites.has(e))return;this.firedSites.add(e),i("tengu_dead_probe_legacy_local_settings",{site:d(e)})}reset(){this.firedSites.clear()}}
var c$s=new q(()=>new yr);
function M(){let e={store:$i(),cwd:ql(),projectConfigDir:Yr()??void 0,allowedSources:m1e(),onLegacyLocalSettingsRead:(n)=>c$s.of(B().host).fire(n),parentManaged:Yut(),hostManagedProvider:a.CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST,flagInline:ay(),flagPath:PO(),flagExpectedContent:Qv()??HZ(),flagInlineJson:Qv()!==void 0,coworkPlugins:Von(),canonicalGitRoot:mP,mdm:()=>$8(),hkcu:()=>a5e(),helper:()=>ar(),helperArmedFromRemote:()=>cr(),helperMergesOutput:()=>ur(),helperWarnings:()=>Ot(),wslInherits:()=>HW(),helperArmedFromUserWritableBase:()=>Jen(),credentialHelperKeysFrom:(n)=>d$s(n),launchFolderKeysFrom:(n)=>u$s(n)};return e.file=()=>Fdt(e),e}
function pAr(e){if(!M$())return!1;if(e!=="policySettings")return!0;let n=gP();return!((lV(n)||n==="remote")&&$8().userWritable!==!0&&!Jen()&&(NS()===null||Dy()))}
function d$s(e){if(nu())return"none";if(!M$())return xjn(e)?"none":"as_read";if(e!=="policySettings")return"none";return pAr(e)?"machine_admin":"as_read"}
function u$s(e){if(!xjn(e))return"as_read";return e==="projectSettings"&&g_()?"as_read":"none"}
function yn(e){return $W(e,M())}
function JRe(e){return Jko(e,M())}
function ey(){return ose(M())}
function g_(){let e=yn("projectSettings"),n=yn("userSettings");return!!e&&!!n&&he(e)===he(n)}
function Njn(){return Fdt(M())}
function me(e){if(bT()&&Af.has(e))return null;return U5e(e,M())}
var Oe;
function je(){let e=M(),n=Oe;if(n===void 0)return e;let r=e.canonicalGitRoot;return{...e,cwd:n.cwd,projectConfigDir:n.projectConfigDir,canonicalGitRoot:(s)=>s===n.cwd?n.canonicalRoot:r?.(s)??null}}
function GOt(e=new Map){let n=new s8e;for(let[h,y]of e)n.parsedFiles.set(h,y);let r={...je(),store:n},s=new Map;for(let h of mN){if(h==="projectSettings"&&(()=>{let R=$W(h,r),P=$W("userSettings",r);return!!R&&!!P&&dD(he(R),he(P))})())continue;let y=h==="localSettings"?ose(r):void 0;s.set(h,{settings:U5e(h,r),errors:RTr(h,r),unfiltered:rnn(h,r),...h==="localSettings"&&{legacy:y===void 0?null:OB(y,n).settings??null}})}return s}
var ze;
function is(e){let n=$i().repoTierFreeze,r=Af.has(e)&&n.isHeld()&&n.baselineTier(e)!==void 0,s=!1,h=new Map,y=new Map,R=(P)=>{try{return ho(ie(),P).resolvedPath}catch{return P}};return{before:(P,T,b)=>{let I=r?n.baselineTier(e):void 0;if(I===void 0)return;let v=je(),H=e==="localSettings"?ose(v):void 0,k=H===void 0?void 0:OB(H,new s8e);if(H!==void 0&&k!==void 0)h.set(H,k),y.set(H,R(H));let w=$W(e,v);if(w!==b){s=!0;return}let F=w===void 0?void 0:OB(w,new s8e);if(w!==void 0&&F!==void 0)h.set(w,F),y.set(w,R(w));let K=P===null?null:OE(_(P),$W(e,v)??""),U=K?.settings??null,z=k?.settings?TB(wd(k.settings),U??{},N3):U,Y=[...K?.errors??[],...k?.errors??[]],J=(Z)=>{let X=Z&&mt(_(Z),!1);return typeof X==="object"?JHt(X):X},ue=n.differingTiers(GOt(h)).includes(e),te=(Z)=>Z.filter((X)=>X.file===H||(T||X.preserveOnWrite)&&!("alias"in X));s=ue||!Yn(J(z),J(I.unfiltered))||!Yn(te(Y),te(I.errors))||!Yn(J(F?.settings??null),J(U))},noteRead:({path:P,content:T})=>{let b=h.get(P),I=OE(T,P);if(r&&(b===void 0||!Yn(JHt(I.settings),JHt(b.settings))||!Yn(I.errors,b.errors)))s=!0},after:({path:P,content:T})=>{let b=OE(T,P),I=y.get(P);h.set(P,b);for(let v of h.keys())if(v!==P&&I!==void 0&&y.get(v)===I)h.set(v,b);if(r&&!s&&n.isHeld()){n.rebaseline(GOt(h),[e]);try{ze?.(P,T)}catch(v){c(G(v))}}}}}
function ttn(){return TTr(M())}
function Ye(e,n){return e==="userSettings"&&Ct(n)===cT.default}
function bp(){return tnn(M())}
function ft(){return fg().settings||{}}
var rr=ft;
function fg(){let e=$i(),n=e.mergedSettings;if(n!==null)return n;so("loadSettingsFromDisk_start");let r=xTr(M());return so("loadSettingsFromDisk_end"),e.mergedSettings=r,r}
function kB(){let e={...M(),helper:void 0},{present:n,heldEmpty:r}=xR(e);if(r)return{};if(n.remote)return q8(e).settings;if(n.mdm)return $8().settings;if(n.file)return Njn().settings;let{settings:s}=k0t(e);if(s)return s;let h=a5e();return Object.keys(h.settings).length>0?h.settings:null}
function U8(){let e=ATr({...M(),helper:void 0});return e==="helper"?null:e}
function gP(){let e=$i(),n=e.policy.origin;if(n!==void 0)return n.value;let r=ATr(M());return e.policy.origin={value:r},r}
function p5e(){let e=$i(),n=e.policy.slotAdminOrParentComposed;if(n!==void 0)return n;let r=iCo(M());return e.policy.slotAdminOrParentComposed=r,r}
function ea(e,n,r,s){if(e==="policySettings"||e==="flagSettings")return Promise.resolve({error:null});let h=yn(e);if(!h)return Promise.resolve({error:ql()===null?new OP:null});return bt.run(h,()=>us(e,n,h,r,s))}
var bt=Rh();
async function Or(e,n,r){try{await QC(e,n,r)}catch(s){if(!(s instanceof gg)||!await cs(e))throw s;t(`Writing settings through symlinked path ${e}`),await QC(e,n,{...r,allowSymlink:!0,checkParentDir:!1})}}
async function cs(e){return!1}
async function us(e,n,r,s,h){let y=j()&&h!==void 0&&Ye(e,r),R=s?.legacyRevocation==="skip",P=null,T;using b={[Symbol.dispose]:()=>T?.()};try{let I=is(e),v=!1,H=rnn(e,M(),{includeLegacyLocalSettings:!1}),k=H!==null&&OB(r,M().store).errors.some((U)=>U.preserveOnWrite);if(k)H=null;if(!H){let U=null;try{let z=e==="userSettings"||uH(r)?ho(ie(),r):await tj(ie(),r);U=(await __(z.resolvedPath,Tf)).content}catch(z){if(!W(z))throw z}if(U!==null&&U.trim()!==""){let z=mt(U,!1);if(z===null)return t(`updateSettingsForSource: invalid JSON in settings file at ${r}`,{level:"error"}),{error:Error(`Invalid JSON syntax in settings file at ${r}`)};if(z&&typeof z==="object"){let Y=wd(z);Nue(Y,r),H=Y,v=!0,t(k?`Using raw settings from ${r} so entries this build does not recognize survive the write`:`Using raw settings from ${r} due to validation failure`)}}}I.before(H??null,v,r);let w=n(H??null);if(w===null){if(e==="localSettings"&&!R){let U=await Er(n,I);if(U.error)return{error:U.error};if(U.changed){if(j()&&h!==void 0)T=await mr(h,r);try{fg()}catch(z){c(z)}Sr(e)}}return{error:null}}await ie().mkdir(ce(r));let F=TB(H||{},w,(U,z,Y,J)=>{if(z===void 0&&J&&typeof Y==="string"){delete J[Y];return}if(Array.isArray(z))return z;if(Y==="extraKnownMarketplaces"&&L(U)&&L(z))return T0t(U,z);return});tdt(r);let K=_(F,null,2)+`
`;if(y){let U=await h.write(Fe.userSettings(),K,{publishDiscipline:"followAtomic"});if(!U.ok)throw new x(`settings storageV5 write failed: ${U.error.code}${"failureClass"in U.error?` (${U.error.failureClass})`:""}`,"settings storageV5 write failed")}else{let U=UZ(ce(r));await Or(r,K,{encoding:"utf-8",allowSymlink:e==="userSettings"||U,checkParentDir:(e==="projectSettings"||e==="localSettings")&&!U,stagingDir:Se(ce(r),q$)})}if(I.after({path:r,content:K}),Ir(),y)T=ds(r,K);if(e==="localSettings"&&!R){let U=await Er(n,I);if(U.error&&U.phase!==void 0&&!Cr(w))t(`localSettings: legacy settings.local.json could not be evaluated (${U.phase} failure) but this write contains no removals \u2014 canonical write succeeded, ignoring: ${U.error.message}`,{level:"warn"});else P=U.error}if(e==="localSettings"){let U=oAr(mO("localSettings"),Yr()??we()).then((Y)=>{if(!Y.written)return;if(Y.effective)g("gitignore_global_rule");else if(Y.reason==="already_tracked")f("gitignore_global_rule",Y.reason);else m("gitignore_global_rule",Y.reason??"write_ineffective")}).catch(()=>{return}),z=$i();z.backgroundWorkTail=Promise.all([z.backgroundWorkTail,U]).then(()=>{return})}}catch(I){let v=Error(`Failed to read raw settings from ${r}: ${I}`,{cause:I});return t(v.message,{level:"error"}),{error:v}}if(j()&&h!==void 0&&!y)T=await mr(h,r);try{fg()}catch(I){c(I)}return Sr(e),{error:P}}
async function mr(e,n){let r=yn("userSettings");if(r===void 0||r===n||!Ye("userSettings",r))return;return uAr(e,$i(),r)}
function ds(e,n){if(Buffer.byteLength(n)>Tf)return;let r;try{r=OE(n,e)}catch(h){t(`updateSettingsForSource: written settings not seeded: ${l(h)}`);return}let s=$i();return s.seedParsedFile(e,"userSettings",r,s.epoch),s.retainLayer(e,r)}
function Cr(e){if(e===void 0||Array.isArray(e))return!0;if(e===null||typeof e!=="object")return!1;return Object.values(e).some(Cr)}
function Sr(e){try{$i().changed.emit(e)}catch(n){for(let r of n instanceof AggregateError?n.errors:[n])c(r)}}
async function Er(e,n){let r=ose(M());if(!r)return{changed:!1,error:null};let s;try{let I=uH(r)?ho(ie(),r):await tj(ie(),r);s=(await __(I.resolvedPath,Tf)).content}catch(I){if(W(I))return{changed:!1,error:null};let v=new x(`Failed to read legacy settings.local.json at ${r}: ${I}`,"Failed to read legacy settings.local.json");return t(v.message,{level:"error"}),{changed:!1,error:v,phase:"read"}}n.noteRead({path:r,content:s});let h=mt(s,!1);if(!h||typeof h!=="object"||Array.isArray(h))return{changed:!1,error:null};let y=wd(h);Nue(y,r);let R=y,P;try{P=e(wd(R))}catch(I){let v=new x(`Transform failed against legacy settings.local.json at ${r} (malformed legacy content?): ${I}`,"Transform failed against legacy settings.local.json");return t(v.message,{level:"error"}),{changed:!1,error:v,phase:"transform"}}if(P===null)return{changed:!1,error:null};let T=TB(wd(R),P,(I,v,H,k)=>{if(v===void 0&&k&&typeof H==="string"){delete k[H];return}if(Array.isArray(v))return v;return}),b=GRs(R,T);if(_(b)===_(R))return{changed:!1,error:null};try{tdt(r);let I=UZ(ce(r)),v=_(b,null,2)+`
`;return await Or(r,v,{encoding:"utf-8",allowSymlink:I,checkParentDir:!I,stagingDir:Se(ce(r),q$)}),n.after({path:r,content:v}),Ir(),{changed:!0,error:null}}catch(I){let v=new x(`Failed to revoke from legacy settings.local.json at ${r}: ${I}`,"Failed to revoke from legacy settings.local.json");return t(v.message,{level:"error"}),{changed:!1,error:v}}}
function GRs(e,n){if(Array.isArray(e)&&Array.isArray(n)){let r=new Set(n.map((s)=>_(s)));return e.filter((s)=>r.has(_(s)))}if(L(e)&&L(n)){let r={};for(let s of Object.keys(e)){if(!(s in n)||n[s]===void 0)continue;r[s]=GRs(e[s],n[s])}return r}return e}
export { kRs as kRs };
export { AEo as AEo };
export { Yoe as Yoe };
export { M$ as M$ };
export { sdt as sdt };
export { c5e as c5e };
export { Yen as Yen };
export { Xen as Xen };
export { jOt as jOt };
export { Sue as Sue };
export { CRs as CRs };
export { Mjn as Mjn };
export { odt as odt };
export { qen as qen };
export { kEo as kEo };
export { vEo as vEo };
export { Ken as Ken };
export { kR as kR };
export { bp as bp };
export { ttn as ttn };
export { me as me };
export { p5e as p5e };
export { rr as rr };
export { JRe as JRe };
export { yn as yn };
export { ey as ey };
export { LRs as LRs };
export { U8 as U8 };
export { FRs as FRs };
export { kB as kB };
export { NRs as NRs };
export { oAr as oAr };
export { ea as ea };
export { mP as mP };
export { nAr as nAr };
export { tAr as tAr };
export { ft as ft };
export { ERs as ERs };
export { Pjn as Pjn };
export { Ven as Ven };
export { uAr as uAr };
export { $8 as $8 };
export { a5e as a5e };
export { HW as HW };
export { EEo as EEo };
export { rdt as rdt };
export { l5e as l5e };
export { OW as OW };
export { TE as TE };
