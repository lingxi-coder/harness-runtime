// Extracted source closure for 2.1.295; source module src_186481843.js sha256 bea88c0dbea195f15d8f4d43ec260c17ba966a741e4291f74664bc06e281a4ab
import { E0s } from "./src_186455978.mjs";
import { Le } from "./src_186092222.mjs";
import { q, B } from "./src_186096608.mjs";
import { ie, tPe } from "./src_186240709.mjs";
import { join as U, delimiter as Ot, resolve as X, delimiter as v, isAbsolute as Et } from "path";
import { Ee } from "./src_186220896.mjs";
import { Y3 } from "./src_186448966.mjs";
import { homedir as At } from "os";
import { $t } from "./src_186066580.mjs";
import { O } from "./src_186477637.mjs";
import { D } from "./src_186447803.mjs";
import { WZ, cy } from "./src_186068928.mjs";
import { readdir as nt, readlink as Dt, access as l } from "fs/promises";
import { constants as et } from "fs";
function Yk(){return!0}
function op(){return typeof Bun<"u"&&Bun.isStandaloneExecutable===!0}
function iGn(t){return!t.isDirectory()&&!t.isFile()&&!t.isSymbolicLink()&&!t.isFIFO()&&!t.isSocket()&&!t.isBlockDevice()&&!t.isCharacterDevice()}
function Y(t){return g(t,Bun.which(t))}
function g(t,o){return o}
var pc=async(t)=>Y(t);
var vN=Y;
function ipe(t,o){let _=Lrn(o);return _===""?null:$To(t,_)}
function Lrn(t){return t.split(v).map((o)=>aGn(o)).filter((o)=>st(o)).join(v)}
function aGn(t,o="darwin"){return o==="win32"?t.replaceAll('"',""):t}
function st(t){if(!Et(t))return!1;return!0}
function $To(t,o){return g(t,Bun.which(t,{PATH:o}))}
function y(){return!1}
function V(){return}
function w(){return!1}
function Tt(){if(ie().existsSync(U(Ee(),".config.json")))return U(Ee(),".config.json");return dxr()}
function dxr(){let t=`.claude${Y3()}.json`;return U(process.env.CLAUDE_CONFIG_DIR||At(),t)}
function Ul(){return A().getGlobalClaudeFile()}
async function Lt(){return null}
async function c(t){try{return!!await pc(t)}catch{return!1}}
async function ct(){let t=[];if(await c("npm"))t.push("npm");if(await c("yarn"))t.push("yarn");if(await c("pnpm"))t.push("pnpm");return t}
async function pt(){let t=[];if(await c("bun"))t.push("bun");if(await c("deno"))t.push("deno");if(await c("node"))t.push("node");return t}
var uxr=["git","node","npm","npx","yarn","pnpm","bun","deno","tsc","python","python3","py","pip","uv","poetry","ruby","gem","bundle","rake","dotnet","msbuild","nuget","cl","nmake","cmake","ninja","make","gcc","g++","clang","cargo","rustc","go","java","javac","mvn","gradle","docker"];
var Rt=1000;
async function St(t){try{return/appinstaller/i.test(await Dt(t))}catch{return!1}}
async function UTo(t){if(t===""||WZ(t)||cy(t))return!1;return!await tPe(X(t))}
async function Nrn(t,o){let _=O()==="windows",r=new Set(t),s=_?(process.env.PATHEXT??".COM;.EXE;.BAT;.CMD").toLowerCase().split(";").filter(Boolean):[],n=D((process.env.PATH??"").split(Ot).map((e)=>e.replace(/^"|"$/g,"")).filter(Boolean));await Promise.all(n.map(async(e)=>{if(!await UTo(e))return;let C;try{C=await nt(e,{withFileTypes:!0})}catch{return}let R=_&&/[\\/]microsoft[\\/]windowsapps[\\/]?$/i.test(e);for(let T of C){if(T.isDirectory())continue;let S=T.name;if(_){let L=S.toLowerCase(),i=L.lastIndexOf(".");if(i<=0||!s.includes(L.slice(i)))continue;S=L.slice(0,i)}if(!r.has(S)||o.has(S))continue;if(_){if(R&&await St(U(e,T.name)))continue}else{let L=U(e,T.name);if((T.isSymbolicLink()||iGn(T))&&await tPe(X(L)))continue;try{await l(L,et.X_OK)}catch{continue}}o.add(S)}}))}
async function It(){let t=new Set;try{await $t(Nrn(uxr,t),Rt,"build tool PATH scan timed out")}catch{}return uxr.filter((o)=>t.has(o))}
function Ut(t){try{if(!t.isWslEnvironment())return!1;let o=vN("npm");if(o===null)return!1;return o.startsWith("/mnt/c/")}catch(o){return!1}}
function it(){return process.env.__CFBundleIdentifier==="com.conductor.app"}
var vse=["pycharm","intellij","webstorm","phpstorm","rubymine","clion","goland","rider","datagrip","appcode","dataspell","aqua","gateway","fleet","jetbrains","androidstudio"];
function BTo(t){let o=t.toLowerCase();return o.includes("windsurf")||o.includes("devin.app")||o.includes("devin desktop")||o.includes("devin-desktop")||/appdata[\\/]local[\\/](programs[\\/])?devin[\\/]/.test(o)}
function xt(){if(process.env.CURSOR_TRACE_ID)return"cursor";let t=process.env.VSCODE_GIT_ASKPASS_MAIN?.toLowerCase()??"";if(t.includes("cursor"))return"cursor";if(BTo(t))return"windsurf";if(t.includes("antigravity"))return"antigravity";let o=process.env.__CFBundleIdentifier?.toLowerCase();if(o?.includes("vscodium"))return"codium";if(o?.includes("windsurf")||o?.includes("devin"))return"windsurf";if(o?.includes("com.google.android.studio"))return"androidstudio";if(o){for(let _ of vse)if(o.includes(_))return _}if(process.env.VisualStudioVersion)return"visualstudio";if(process.env.TERMINAL_EMULATOR==="JetBrains-JediTerm")return"pycharm";if(process.env.TERM==="xterm-ghostty")return"ghostty";if(process.env.TERM?.includes("kitty"))return"kitty";if(process.env.TERM_PROGRAM){if(/^devin([ -]desktop)?$/i.test(process.env.TERM_PROGRAM))return"windsurf";return process.env.TERM_PROGRAM}if(process.env.TMUX)return"tmux";if(process.env.STY)return"screen";if(process.env.KONSOLE_VERSION)return"konsole";if(process.env.GNOME_TERMINAL_SERVICE)return"gnome-terminal";if(process.env.XTERM_VERSION)return"xterm";if(process.env.VTE_VERSION)return"vte-based";if(process.env.TERMINATOR_UUID)return"terminator";if(process.env.KITTY_WINDOW_ID)return"kitty";if(process.env.ALACRITTY_LOG)return"alacritty";if(process.env.TILIX_ID)return"tilix";if(process.env.WT_SESSION)return"windows-terminal";if(process.env.SESSIONNAME&&process.env.TERM==="cygwin")return"cygwin";if(process.env.MSYSTEM)return process.env.MSYSTEM.toLowerCase();if(process.env.ConEmuANSI||process.env.ConEmuPID||process.env.ConEmuTask)return"conemu";if(process.env.WSL_DISTRO_NAME)return`wsl-${process.env.WSL_DISTRO_NAME}`;if(Z())return"ssh-session";if(process.env.TERM){let _=process.env.TERM;if(_.includes("alacritty"))return"alacritty";if(_.includes("rxvt"))return"rxvt";if(_.includes("termite"))return"termite";return process.env.TERM}if(!process.stdout.isTTY)return"non-interactive";return null}
class h{sources;globalClaudeFile;packageManagers;runtimes;detectedBuildTools;wslEnvironment;npmFromWindowsPath;deploymentEnvironment;primedWslInteropExists;primedHypervisorUuid;primedDockerenvExists;fallbackWslInteropExists;fallbackHypervisorUuid;fallbackDockerenvExists;constructor(t){this.sources=t}getGlobalClaudeFile(){return this.globalClaudeFile??=Tt()}seedGlobalClaudeFile(t){if(this.globalClaudeFile===void 0)return this.globalClaudeFile=t,"seeded";return this.globalClaudeFile===t?"unchanged":"conflict"}getPackageManagers(){return this.packageManagers??=ct()}getRuntimes(){return this.runtimes??=pt()}getDetectedBuildTools(){return this.detectedBuildTools??=It()}isWslEnvironment(){return this.wslEnvironment??=this.primedWslInteropExists??!1}isNpmFromWindowsPath(){return this.npmFromWindowsPath??=Ut(this)}hypervisorUuid(){return this.primedHypervisorUuid??(this.fallbackHypervisorUuid??=this.sources.readHypervisorUuidSync()??"")}isDockerenvPresent(){return this.primedDockerenvExists??(this.fallbackDockerenvExists??=this.sources.dockerenvExistsSync())}detectDeploymentEnvironment(){return this.deploymentEnvironment??=lt(this)}async prime(){this.primedWslInteropExists;return}}
var Nt=new q(()=>new h({wslInteropExistsSync:y,readHypervisorUuidSync:V,dockerenvExistsSync:w}));
function A(){return Nt.of(B().host)}
function lGn(){return A().isDockerenvPresent()}
function k(){return A().detectDeploymentEnvironment()}
function lt(t){if(Le(process.env.CODESPACES))return"codespaces";if(process.env.GITPOD_WORKSPACE_ID)return"gitpod";if(Le(process.env.CODER)||process.env.CODER_WORKSPACE_NAME)return"coder";if(Le(process.env.DEVPOD)||process.env.DEVPOD_WORKSPACE_UID)return"devpod";if(process.env.DAYTONA_WS_ID)return"daytona";if(Le(process.env.GOOGLE_CLOUD_WORKSTATIONS))return"gcp-cloud-workstations";if(process.env.C9_PID||process.env.C9_USER)return"aws-cloud9";if(process.env.REPL_ID||process.env.REPL_SLUG)return"replit";if(process.env.PROJECT_DOMAIN)return"glitch";if(Le(process.env.VERCEL))return"vercel";if(process.env.RAILWAY_ENVIRONMENT_NAME||process.env.RAILWAY_SERVICE_NAME)return"railway";if(Le(process.env.RENDER))return"render";if(Le(process.env.NETLIFY))return"netlify";if(process.env.DYNO)return"heroku";if(process.env.FLY_APP_NAME||process.env.FLY_MACHINE_ID)return"fly.io";if(Le(process.env.CF_PAGES))return"cloudflare-pages";if(process.env.DENO_DEPLOYMENT_ID)return"deno-deploy";if(process.env.AWS_LAMBDA_FUNCTION_NAME)return"aws-lambda";if(process.env.AWS_EXECUTION_ENV==="AWS_ECS_FARGATE")return"aws-fargate";if(process.env.AWS_EXECUTION_ENV==="AWS_ECS_EC2")return"aws-ecs";if(t.hypervisorUuid().startsWith("ec2"))return"aws-ec2";if(process.env.K_SERVICE)return"gcp-cloud-run";if(process.env.GOOGLE_CLOUD_PROJECT)return"gcp";if(process.env.WEBSITE_SITE_NAME||process.env.WEBSITE_SKU)return"azure-app-service";if(process.env.AZURE_FUNCTIONS_ENVIRONMENT)return"azure-functions";if(process.env.APP_URL?.includes("ondigitalocean.app"))return"digitalocean-app-platform";if(process.env.SPACE_CREATOR_USER_ID)return"huggingface-spaces";if(Le(process.env.GITHUB_ACTIONS))return"github-actions";if(Le(process.env.GITLAB_CI))return"gitlab-ci";if(process.env.CIRCLECI)return"circleci";if(process.env.BUILDKITE)return"buildkite";if(Le(!1))return"ci";if(process.env.KUBERNETES_SERVICE_HOST)return"kubernetes";if(t.isDockerenvPresent())return"docker";if(p.platform==="darwin")return"unknown-darwin";if(p.platform==="linux")return"unknown-linux";if(p.platform==="win32")return"unknown-win32";return"unknown"}
function Z(){return!!(process.env.SSH_CONNECTION||process.env.SSH_CLIENT||process.env.SSH_TTY)}
var p={probeInternalNetworkAccess:Lt,isCI:Le(!1),platform:["win32","darwin"].includes("darwin")?"darwin":"linux",arch:"arm64",nodeVersion:process.version,terminal:xt(),isSSH:Z,getPackageManagers(){return A().getPackageManagers()},getRuntimes(){return A().getRuntimes()},isRunningWithBun:Yk,isWslEnvironment(){return A().isWslEnvironment()},isNpmFromWindowsPath(){return A().isNpmFromWindowsPath()},isConductor:it,detectDeploymentEnvironment:k};
function Frn(){let t=process.env.CLAUDE_CODE_HOST_PLATFORM;if(t==="win32"||t==="darwin"||t==="linux")return t;return p.platform}
var kse=["us","eu","apac","jp","au","us-gov","global"];
var P={};
var u={};
var b={};
var B9e=["5m","1h"];
var H={};
var G={};
var m={};
var F={};
var d={};
var Wx={...P,...d,...m,...b,...F,...H,...u,...G};
function Jxe(t,o){let _=Object.create(o);for(let[r,s]of Object.entries(t)){let n=_,e;Object.defineProperty(_,r,{get:()=>{let C=process.env[r];if(C!==n)e=s.parse(C),n=C;return e},enumerable:!0,configurable:!0})}return Object.defineProperties(_,{set:{value:(r,s)=>{process.env[r]=E0s(s)}},unset:{value:(r)=>{delete process.env[r]}}}),_}
var a=Jxe(Wx,p);
export { a as a };
export { Jxe as Jxe };
export { Ul as Ul };
export { vN as vN };
export { op as op };
export { Frn as Frn };
export { lGn as lGn };
export { ipe as ipe };
export { B9e as B9e };
export { kse as kse };
