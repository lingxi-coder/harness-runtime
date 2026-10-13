// Extracted source closure for 2.1.295; source module src_188508971.js sha256 c685a2dfd00fecdd0a777d544e9594bcb380d21e037f2c95d70a04a24f5b30be
import { q, B, De } from "./src_186096608.mjs";
import { O } from "./src_186477637.mjs";
import { a, Jxe } from "./src_186481843.mjs";
import { KTr } from "./src_186760168.mjs";
import { Ge } from "./src_186068928.mjs";
import { kRs, AEo, Yoe, M$, sdt, c5e, Yen, Xen, jOt, Sue, CRs, Mjn, odt, qen, kEo, vEo, Ken, kR, bp } from "./src_188306029.mjs";
import { Le, _s } from "./src_186092222.mjs";
import { vtn, rxe, k5e } from "./src_187673145.mjs";
import { zwo } from "./src_188489849.mjs";
import { Ya } from "./src_186559853.mjs";
import { t, ie, Y } from "./src_186240709.mjs";
import { fN } from "./src_188508318.mjs";
import { toolServerShape as __native_dynamic_0 } from "./src_206963702.mjs";
import { join as g, resolve as y, dirname as _t, basename as ae, parse as Et, relative as Ct, sep as de, isAbsolute as ue } from "path";
import { realpathSync as pt, readFileSync as ft, lstatSync as dt } from "fs";
import { Bt, l } from "./src_186056234.mjs";
import { D } from "./src_186447803.mjs";
import { p } from "./src_186562918.mjs";
import { u, U, T, oe } from "./src_186578912.mjs";
import { cn } from "./src_186448966.mjs";
var at="tengu_lively_beaver";
class re{reader=null;register(e){let n=this.reader;return this.reader=e,n}}
var se=new re;
function AOt(){let e=se.reader;if(!e)return!1;try{return e(at,!1)===!0}catch{return!1}}
var Hct="account_on_hold";
var TOt="https://claude.ai/restricted";
var ne=2048;
var ct=/^https:\/\/[a-z0-9.-]+\/[A-Za-z0-9/._~%-]*(?:\?[A-Za-z0-9._~%=&-]*)?(?<![.?])$/;
function lt(e){try{return e===new URL(cn().CLAUDE_AI_ORIGIN).origin}catch{return!1}}
function y$e(e){if(typeof e!=="string"||e.length>ne)return TOt;let n;try{n=new URL(e)}catch{return TOt}if(n.protocol!=="https:"||n.username||n.password||n.port||n.hash)return TOt;let r=n.hostname;return(r==="claude.ai"||r.endsWith(".claude.ai")||r==="anthropic.com"||r.endsWith(".anthropic.com")||lt(n.origin))&&n.href.length<=ne&&ct.test(n.href)?n.href:TOt}
var ut=p(()=>u({error:U(["invalid_grant","access_denied"]),error_description:T(Hct),error_uri:oe().optional()}));
function _en(e){if(!AOt())return null;let n=e;if(typeof n==="string"){if(!n.includes(Hct))return null;try{n=Y(n)}catch{return null}}let r=ut().safeParse(n);return r.success?{url:y$e(r.data.error_uri)}:null}
var ht=["ANTHROPIC_FEDERATION_RULE_ID","ANTHROPIC_ORGANIZATION_ID"];
class fe{precedenceSource={filled:!1};authType={filled:!1};profileBaseUrl={filled:!1};accountInfo={filled:!1};federationIdentity={filled:!1};profileStoreDenyPaths={filled:!1};settingsBearerRejected=!1;primedFiles=void 0}
var R=new q(()=>new fe);
function CE(){let e=R.of(B().host);if(e.precedenceSource.filled)return e.precedenceSource.value;let n=Ot();return e.precedenceSource={filled:!0,value:n},n}
function Ot(){let e=R$(),n=_e();if(n){if(e===null)return null;let r=W(e,n);return r==="oidc_federation"||r==="user_oauth"?"profile-explicit":null}if(Ee())return"env-quad";if(e!==null){let r=W(e,BRe(e));if(r==="oidc_federation"||r==="user_oauth")return"profile-implicit"}return null}
function ben(){R.of(B().host).accountInfo={filled:!1}}
function zBn(){return CE()!==null}
function Lct(){let e=R.of(B().host);if(e.authType.filled)return e.authType.value;let n=gt();return e.authType={filled:!0,value:n},n}
function gt(){let e=CE();if(e===null)return null;if(e==="env-quad")return"oidc_federation";let n=R$();if(n===null)return null;let r=e==="profile-explicit"?process.env.ANTHROPIC_PROFILE?.trim()??"default":BRe(n),i=W(n,r);return i==="oidc_federation"||i==="user_oauth"?i:null}
function wen(){return CE()==="profile-implicit"&&Lct()==="user_oauth"}
function VBn(){let e=R.of(B().host);if(e.profileStoreDenyPaths.filled)return e.profileStoreDenyPaths.value;let{value:n,complete:r}=mt();if(r)e.profileStoreDenyPaths={filled:!0,value:n};return n}
function mt(){let e=R$();if(e===null)return{value:null,complete:!0};let n={dirs:[e],files:[]};try{let r=y(e);n=ce(r)?{dirs:[g(r,"configs"),g(r,"credentials")],files:[g(r,"active_config")]}:{dirs:[r],files:[]};let c=Owo(e),s=j(e,c);if(typeof s!=="string"||!s.trim())return{value:n,complete:!0};let d=D(ue(s)?[y(s)]:[y(r,s),y(s)]).filter((_)=>!n.dirs.some((h)=>_===h||_.startsWith(h+de))&&!n.files.includes(_)&&!ce(_));return{value:{dirs:n.dirs,files:[...n.files,...d.filter((_)=>!St(_))]},complete:!0}}catch(r){return t(`WIF profile store: could not resolve the active profile's credentials path (${l(r)}); denying the store root only`,{level:"warn"}),{value:n,complete:!1}}}
function BRe(e){return L(g(e,"active_config"))?.trim()||"default"}
function Owo(e){return process.env.ANTHROPIC_PROFILE?.trim()||BRe(e)}
function W(e,n){let r=L(g(e,"configs",`${n}.json`));if(r===null)return null;let i;try{i=JSON.parse(r)}catch{return null}let c=i?.authentication?.type??null;if(c==="user_oauth"){if(!L(j(e,n,i))?.trim())return null}return c}
function j(e,n,r){if(r===void 0){let i=L(g(e,"configs",`${n}.json`));if(i!==null)try{r=JSON.parse(i)}catch{}}return r?.authentication?.credentials_path??g(e,"credentials",`${n}.json`)}
function R$(){return pe()?.dir??null}
function pe(){if(De())return null;let e=process.env,n=e.ANTHROPIC_CONFIG_DIR?.trim();if(n)return{dir:n,space:"userNamed"};let r=e.XDG_CONFIG_HOME?.trim();if(r)return{dir:g(r,"anthropic"),space:"home"};let i=e.HOME?.trim();return i?{dir:g(i,".config","anthropic"),space:"home"}:null}
function _e(){return process.env.ANTHROPIC_PROFILE?.trim()}
function Ee(){return ht.every((e)=>process.env[e]?.trim())}
function ce(e){let n=le(e);if(n===Et(n).root)return!0;let r=Ct(n,le(ie().cwd())),i=r.split(de)[0];return r===""||i!==".."&&!ue(r)}
function le(e){let n=y(e),r="";for(;;)try{n=g(pt.native(n),r);break}catch{let i=_t(n);if(i===n){n=g(n,r);break}r=r?g(ae(n),r):ae(n),n=i}return n.toLowerCase()}
function St(e){try{return dt(e).isDirectory()}catch{return!1}}
function L(e){let n=R.of(B().host).primedFiles;if(n!==void 0&&n.has(e))return n.get(e)??null;try{return ft(e,"utf-8")}catch(r){if(Bt(r))return null;throw r}}
var ven="ssh-placeholder";
var Fct=["CLAUDE_BG_DISPATCHER_SUBSCRIPTION_TYPE","CLAUDE_BG_DISPATCHER_RATE_LIMIT_TIER"];
function Hwo(){if(a.CLAUDE_CODE_SESSION_KIND!=="bg")return{subscriptionType:void 0,rateLimitTier:void 0};return{subscriptionType:a.CLAUDE_BG_DISPATCHER_SUBSCRIPTION_TYPE||void 0,rateLimitTier:a.CLAUDE_BG_DISPATCHER_RATE_LIMIT_TIER||void 0}}
var Rt=__native_dynamic_0;
var xW=Jxe(Rt,null);
function qBn(){return bp().map((e)=>e.sandbox?.bwrapPath).find((e)=>e!=null)}
function kTs(){return bp().map((e)=>e.sandbox?.socatPath).find((e)=>e!=null)}
var Dt=/[ \t\n\v\f\r'"]/;
function ge(e){let n=e.HTTP_PROXY||e.http_proxy||e.CLAUDE_CODE_HTTP_PROXY,r=e.HTTPS_PROXY||e.https_proxy||e.CLAUDE_CODE_HTTPS_PROXY,i=KTr(e);if(!n&&!r)return{};let c=Oe(n),s=Oe(r);if(!s.host)s=c;let d={},_=(h,E)=>{if(E&&e[h]===void 0)d[h]=E};if(_("YARN_HTTP_PROXY",n),_("YARN_HTTPS_PROXY",r),_("npm_config_proxy",n),_("npm_config_https_proxy",r),_("npm_config_noproxy",i),_("GLOBAL_AGENT_HTTP_PROXY",n),_("GLOBAL_AGENT_HTTPS_PROXY",r),_("GLOBAL_AGENT_NO_PROXY",i),_("ELECTRON_GET_USE_PROXY","1"),_("DOCKER_HTTP_PROXY",n),_("DOCKER_HTTPS_PROXY",r),s.host)_("CLOUDSDK_PROXY_TYPE","http"),_("CLOUDSDK_PROXY_ADDRESS",s.host),_("CLOUDSDK_PROXY_PORT",s.port),_("CLOUDSDK_PROXY_USERNAME",s.user),_("CLOUDSDK_PROXY_PASSWORD",s.pass);if(_("FSSPEC_GCS",'{"session_kwargs": {"trust_env": true}}'),s.host){let h=e.JAVA_TOOL_OPTIONS;if(!h?.includes("-Dhttps.proxyHost=")){let E=It(c,s,i);d.JAVA_TOOL_OPTIONS=h?`${h} ${E}`:E}}return d}
function Oe(e){if(!e)return{host:"",port:"",user:"",pass:""};try{let n=new URL(e);if(!n.hostname)return{host:"",port:"",user:"",pass:""};return{host:n.hostname.startsWith("[")&&n.hostname.endsWith("]")?n.hostname.slice(1,-1):n.hostname,port:n.port||(n.protocol==="https:"?"443":"80"),user:decodeURIComponent(n.username),pass:decodeURIComponent(n.password)}}catch{return{host:"",port:"",user:"",pass:""}}}
function It(e,n,r){let i=[],c=(s,d)=>{if(d&&!Dt.test(d))i.push(`-D${s}=${d}`)};if(c("http.proxyHost",e.host),c("http.proxyPort",e.port),c("https.proxyHost",n.host),c("https.proxyPort",n.port),c("http.proxyUser",e.user),c("http.proxyPassword",e.pass),c("https.proxyUser",n.user),c("https.proxyPassword",n.pass),r)c("http.nonProxyHosts",Pt(r));return i.push("-Djdk.http.auth.tunneling.disabledSchemes="),i.push("-Djdk.http.auth.proxying.disabledSchemes="),i.join(" ")}
function Pt(e){return e.split(/[,\s]+/).map((n)=>n.trim()).filter(Boolean).flatMap((n)=>{if(n.startsWith("."))return[`*${n}`];return Lt(n)??[n]}).join("|")}
function Lt(e){let n=e.match(/^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})\/(\d{1,2})$/);if(!n)return;let r=[Number(n[1]),Number(n[2]),Number(n[3]),Number(n[4])],i=Number(n[5]);if(r.some((E)=>E>255)||i<8||i>24)return[e];let c=Math.floor(i/8),s=i%8;if(s===0)return[`${r.slice(0,c).join(".")}.*`];let d=2**(8-s);if(d>16)return[e];let _=r[c]-r[c]%d,h=[];for(let E=_;E<_+d;E++)h.push(`${[...r.slice(0,c),E].join(".")}.*`);return h}
var Q4e=["CLAUDE_CODE_SESSION_KIND","CLAUDE_BG_SOURCE","CLAUDE_BG_ISOLATION","CLAUDE_BG_BACKEND","CLAUDE_CODE_SESSION_NAME","CLAUDE_CODE_RESUME_INTERRUPTED_TURN","CLAUDE_CODE_RESUME_INTERRUPTED_TURN_MAX_AGE_MS","CLAUDE_CODE_RESUME_PROMPT","CLAUDE_CODE_RESUME_REASON","CLAUDE_CODE_RESUME_SOURCE_ALIVE","CLAUDE_BG_POST_CLEAR_RESPAWN","CLAUDE_BG_SESSION_PERMISSION_RULES","CLAUDE_BG_MEMORY_TOGGLED_OFF","CLAUDE_BG_AUTO_MEMORY_OFF","CLAUDE_BG_WORKSPACE_TRUSTED","CLAUDE_BG_CARRIED_PROMPTS_SHA256","CLAUDE_CODE_RELAUNCH_HOME_TRUST",...Fct];
class Ie{scrubEnabledLatched=void 0;scrubSandboxAvailableLatched=void 0;scrubPathsLatched=void 0;scriptCallCounts=new Map;scriptCapsLatched=void 0;setScrubEnabledLatched(e){this.scrubEnabledLatched=e}setScrubSandboxAvailableLatched(e){this.scrubSandboxAvailableLatched=e}setScrubPathsLatched(e){this.scrubPathsLatched=e}setScriptCapsLatched(e){this.scriptCapsLatched=e}reset(){this.scrubEnabledLatched=void 0,this.scrubSandboxAvailableLatched=void 0,this.scrubPathsLatched=void 0,this.scriptCallCounts.clear(),this.scriptCapsLatched=void 0}}
var A=Ge(new Ie,(e)=>{e.reset(),kRs()});
class Pe{getAgentProxyEnv=void 0;getGhStandinDir=void 0;settingsEnvForChildren={};hipaaTierNoticeGiven=!1}
var I=new q(()=>new Pe);
function Jd(){let e=A.scrubEnabledLatched;if(e!==void 0)return e;let n=Le(process.env.CLAUDE_CODE_SUBPROCESS_ENV_SCRUB);return A.setScrubEnabledLatched(n),n}
function ECr(){if(Jd())return!0;if(_s(process.env.CLAUDE_CODE_SUBPROCESS_ENV_SCRUB))return!1;return process.env.CLAUDE_CODE_ENTRYPOINT==="local-agent"}
function Goe(){if(A.scrubSandboxAvailableLatched!==void 0)return A.scrubSandboxAvailableLatched;return!1}
var Ut="3";
function kCr(){return a.CLAUDE_BG_BACKEND==="daemon"&&a.FORCE_COLOR===Ut}
function CCr(){if(!kCr())return!1;let{settingsEnvForChildren:e}=I.of(B().host);return!Object.hasOwn(e,b$e(e,"FORCE_COLOR"))}
function b$e(e,n){if(O()!=="windows"||Object.hasOwn(e,n))return n;let r=n.toUpperCase();return Object.keys(e).find((i)=>i.toUpperCase()===r)??n}
function ACr(e){return e.startsWith("CLAUDE_CODE_ARTIFACT")&&e.endsWith("_BASE_URL")}
function TCr(e){let n=e.toUpperCase();return!jOt(e)&&Sue(e)&&!CRs.has(n)&&!Mjn(n)}
var Nt=new Set(vtn.map((e)=>e.toUpperCase()));
function RCr(){if(!POt())return[];let e=I.of(B().host);if(!e.hipaaTierNoticeGiven)e.hipaaTierNoticeGiven=!0,t("HIPAA: withholding this CLI's Anthropic credentials from child processes and config ${VAR} expansion");return AEo}
function POt(){return Ya("hipaa")}
var Te=new Set(AEo.map((e)=>e.toUpperCase()));
function w$e(e){let n=e.replace(/^INPUT_/,"");return ACr(n)||rxe(n)||n.startsWith("OTEL_")||n==="CLAUDE_CODE_OTEL_DIAG_STDERR"}
function xCr(){let e=process.env,n=(i)=>e.ANTHROPIC_UNIX_SOCKET!==void 0&&(i==="CLAUDE_CODE_OAUTH_TOKEN"||i==="ANTHROPIC_API_KEY")&&e[i]===ven,r=(i)=>!Object.keys(e).some((c)=>c!==i&&c.toUpperCase()===i);return[...vtn.filter((i)=>!n(i)),...Object.keys(e).filter((i)=>w$e(i.toUpperCase())),...k5e(e),...zwo(),...RCr().filter((i)=>!(n(i)&&r(i))),"CLAUDE_CODE_SUBSCRIPTION_TYPE","CLAUDE_CODE_RATE_LIMIT_TIER","CLAUDE_CODE_PLUGIN_ATTRIBUTION","CLAUDE_CODE_SKILL_ATTRIBUTION","CLAUDE_CODE_BRIDGE_CHILD_AUTO_DEFAULT","CLAUDE_CODE_BRIDGE_CHILD_ARTIFACT","CLAUDE_CODE_BRIDGE_CHILD_MACHINE_SETTINGS","CLAUDE_CODE_CONFIG_PROBE","CLAUDE_CODE_HOST_PROMPT_SUPERSEDES_RECORD","CLAUDE_CODE_MCP_SERVE_SETTINGS","CLAUDE_CODE_REMOTE_TOOLS_FORWARD",...Q4e]}
var yt=new Set(["BUN_GC_TIMER_INTERVAL","BUN_GC_TIMER_DISABLE","BUN_GC_RUNS_UNTIL_SKIP_RELEASE_ACCESS","BUN_IDLE_GC_SECONDS"]);
var vt=new Set(["MIMALLOC_PURGE_HOLES_MIN_INTERVAL"]);
function me(e){let n=e.toUpperCase();return n.startsWith("BUN_JSC_")||yt.has(n)||vt.has(n)}
function hs(){let e=I.of(B().host),n=e.getAgentProxyEnv?.()??{},r=Object.keys(n).length>0,{settingsEnvForChildren:i}=e,c=Object.keys(i).length>0,s=process.env;if(c){s={...process.env};for(let[f,S]of Object.entries(i))s[b$e(s,f)]=S}let d=a.CLAUDE_CODE_REMOTE===!0,_=d?ge(r?{...s,...n}:s):{},h=Object.keys(_).length>0,E=ECr(),m=Object.keys(process.env).some((f)=>Nt.has(f.toUpperCase()))||Object.keys(process.env).some((f)=>w$e(f.toUpperCase()))||process.env.CLAUDE_CODE_SUBSCRIPTION_TYPE!==void 0||process.env.CLAUDE_CODE_RATE_LIMIT_TIER!==void 0,P=k5e(process.env),ve=zwo(),z=POt()&&Object.keys(process.env).some((f)=>Te.has(Yoe(f))),J=!1;J=Q4e.some((f)=>process.env[f]!==void 0);let be=Object.keys(process.env).some((f)=>/^(INPUT_)?(OTEL_|CLAUDE_CODE_OTEL_DIAG_STDERR$)/i.test(f)),xe=a.CLAUDE_CODE_PLUGIN_ATTRIBUTION!==void 0||a.CLAUDE_CODE_SKILL_ATTRIBUTION!==void 0,He=a.CLAUDE_CODE_QUESTION_EXTENDED||a.CLAUDE_CODE_QUESTION_OPTIONAL_DESCRIPTIONS,$e=a.CLAUDE_CODE_SDK_HAS_OAUTH_REFRESH,Fe=a.CLAUDE_CODE_SDK_READS_SESSION_STATE,Be=M$(),ke=a.CLAUDE_CODE_REMOTE_TOOLS_HOST_ALLOWS_UNATTENDED,Me=Object.keys(s).some((f)=>f.toUpperCase()==="CLAUDE_CODE_REMOTE_TOOLS_FORWARD"),We=a.CLAUDE_CODE_BRIDGE_CHILD_AUTO_DEFAULT||a.CLAUDE_CODE_BRIDGE_CHILD_ARTIFACT||a.CLAUDE_CODE_BRIDGE_CHILD_MACHINE_SETTINGS,je=a.CLAUDE_CODE_CONFIG_PROBE,Ye=a.CLAUDE_CODE_MCP_STARTUP_WAIT_MS!==void 0,Ke=a.CLAUDE_CODE_MCP_APPS_HOST,Ve=a.CLAUDE_CODE_SLEEP_COMPACT,Xe=a.CLAUDE_CODE_ARTIFACT_SHARE!==void 0,ze=a.CLAUDE_CODE_AUTOUPDATER_DISABLED_BY_HOST,Je=Object.keys(s).some((f)=>f.toUpperCase()==="CLAUDE_CODE_ARTIFACT_VERSIONS"),qe=a.CLAUDE_CHROME_TAB_GROUP_KEY!==void 0,Qe=a.CLAUDE_CODE_HOST_PROMPT_SUPERSEDES_RECORD,Ze=xW.CLAUDE_CODE_MCP_SERVE_SETTINGS!==void 0,F=!1;F=Object.keys(s).some(Se);let Q=d&&Object.keys(s).some(me);if(!r&&!h&&!E&&!J&&!m&&!P.length&&!ve.length&&!z&&!be&&!c&&!xe&&!He&&!$e&&!Fe&&!Be&&!ke&&!Me&&!We&&!je&&!Ye&&!Ke&&!Ve&&!Xe&&!ze&&!Je&&!qe&&!Qe&&!Ze&&!F&&!Q)return process.env;let Z={...n,..._},C={...s};for(let f of Object.keys(Z)){let S=b$e(C,f);if(S!==f)delete C[S]}Object.assign(C,Z);let et=new Set([...xCr(),"CLAUDE_CODE_SUBSCRIPTION_TYPE","CLAUDE_CODE_RATE_LIMIT_TIER","CLAUDE_CODE_QUESTION_EXTENDED","CLAUDE_CODE_QUESTION_OPTIONAL_DESCRIPTIONS","CLAUDE_CODE_SDK_HAS_OAUTH_REFRESH","CLAUDE_CODE_SDK_READS_SESSION_STATE","CLAUDE_CODE_REMOTE_TOOLS_PIN_STORED_LOGIN","CLAUDE_CODE_REMOTE_TOOLS_HOST_ALLOWS_UNATTENDED","CLAUDE_CODE_BRIDGE_CHILD_AUTO_DEFAULT","CLAUDE_CODE_BRIDGE_CHILD_ARTIFACT","CLAUDE_CODE_BRIDGE_CHILD_MACHINE_SETTINGS","CLAUDE_CODE_CONFIG_PROBE","CLAUDE_CODE_MCP_STARTUP_WAIT_MS","CLAUDE_CODE_MCP_APPS_HOST","CLAUDE_CODE_SLEEP_COMPACT","CLAUDE_CODE_ARTIFACT_SHARE","CLAUDE_CODE_ARTIFACT_VERSIONS","CLAUDE_CODE_AUTOUPDATER_DISABLED_BY_HOST","CLAUDE_CHROME_TAB_GROUP_KEY"].map((f)=>f.toUpperCase()));for(let f of Object.keys(C))if(et.has(f.toUpperCase())||w$e(f.toUpperCase())||z&&f.includes("-")&&Te.has(Yoe(f)))delete C[f];for(let f of Q4e)delete C[f];if(CCr())delete C[b$e(C,"FORCE_COLOR")];for(let f of Object.keys(C))if(f.startsWith("OTEL_"))delete C[f];if(delete C.CLAUDE_CODE_OTEL_DIAG_STDERR,delete C.CLAUDE_CODE_PLUGIN_ATTRIBUTION,delete C.CLAUDE_CODE_SKILL_ATTRIBUTION,F){for(let f of Object.keys(C))if(Se(f))delete C[f]}if(Q){for(let f of Object.keys(C))if(me(f))delete C[f]}if(!E)return C;let tt=bt(),nt=sdt().length>0;for(let[f,S]of Object.entries(C)){if(S===fN&&Object.hasOwn(n,f))continue;if((f==="CLAUDE_CODE_OAUTH_TOKEN"||f==="ANTHROPIC_API_KEY")&&S===ven&&C.ANTHROPIC_UNIX_SOCKET!==void 0)continue;if(tt.has(Yoe(f))||Yen.test(f)||Xen(f)||nt&&TCr(f)){delete C[f];continue}if(S===void 0||jOt(f))continue;let N=typeof S==="string"?S:String(S);if(odt(f)){let w=qen(f,N);if(w.value!==N)C[f]=w.value;if(w.cut){let x=kEo(f,w.value);if(x!==void 0&&!vEo(C,x.name))C[x.name]=x.value}continue}let b=Ken(f,N);if(b===void 0){if(kR(N))delete C[f]}else if(b!==N)if(b==="")delete C[f];else C[f]=b}return C}
function Se(e){return e.startsWith("CLAUDE_CODE_RELAUNCH_PROACTIVITY_")}
var K;
function bt(){let e=sdt(),n=e.length>0;if(K?.armed!==n)K={armed:n,names:new Set([...c5e,...e].map((r)=>r.toUpperCase()))};return K.names}
export { hs as hs };
export { Jd as Jd };
export { Goe as Goe };
export { VBn as VBn };
export { qBn as qBn };
export { kTs as kTs };
export { zBn as zBn };
export { CE as CE };
export { wen as wen };
export { Hct as Hct };
export { _en as _en };
export { TOt as TOt };
export { Hwo as Hwo };
export { ben as ben };
