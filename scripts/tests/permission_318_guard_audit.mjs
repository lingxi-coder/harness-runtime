// Hash-pinned pure-function probes. No Claude process, commands, files, or network are executed.
import { readFileSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import path from "node:path";
import vm from "node:vm";
import { fileURLToPath } from "node:url";
const binary = readFileSync(process.argv[2]);
const sha256 = createHash("sha256").update(binary).digest("hex");
if (sha256 !== "677a2ea7bf576cedd20e191d973914ed8a41796f53a63b1923381ddc4d5b5e7d") throw Error("unexpected baseline");
const ranges=[];
function range(start,end,after=257482802){
 const from=binary.indexOf(start,after),to=binary.indexOf(end,from);
 if(from<0||to<from)throw Error("missing anchor: "+start);
 ranges.push({start:from,end:to,sha256:createHash("sha256").update(binary.subarray(from,to)).digest("hex")});
 return binary.subarray(from,to).toString("utf8");
}
const windows=vm.createContext({platform:"wsl"});
vm.runInContext(`const P=()=>platform,Q0=e=>/^[\\\\/]\\?\\?[\\\\/]/.test(e),pLr=/~\\d/,CVe=/[.\\s]+$/,OBs=/\\.(?:CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])$/i;const fI=()=>false,Dd=()=>false,MY=()=>false;`+range("function yZ(e,n){return OJt(e,n)","function Apt(e)"),windows);
const windowsPatterns=["file.txt:stream","中:stream","😀:stream",String.raw`\??\C:\file`,"safe.txt"].flatMap(input=>["windows","wsl","linux","macos"].map(platform=>{windows.platform=platform;return {input,platform,expected:vm.runInContext("yZ",windows)(input)}}));
let ticks=0;
const parser=vm.createContext({performance:{now:()=>ticks++},ge:x=>x,kUn:()=>false,fgt:()=>false,FRs:()=>false,LRs:()=>false,Xrn:()=>false,vUn:()=>{},CUn:/$^/});
vm.runInContext(range("var $e=50,Ae=50000,ne=67108864", "export{ly,Pbr,aco}"),parser,{timeout:1000});
const rawParse=vm.runInContext("he",parser);
ticks=0;const zeroBudget=rawParse("echo "+"x ".repeat(200),0)===null;
ticks=0;const normal=rawParse("echo x",50)!==null;
const parserProbe={zeroBudgetAborts:zeroBudget,normalParses:normal,limits:vm.runInContext("({milliseconds:$e,nodes:Ae,bytes:Ce})",parser),scope:"Native hand-written parser with feature predicates fixed off and a deterministic clock; explicit zero-ms budget is a controlled timeout probe."};
const home="/home/user",cwd="/home/user/project";
const removal=vm.createContext({home,cwd,path});
vm.runInContext(`const Q=()=>home;`+range("function wp(t){if(ht(t))", "function nH(t){")+`
const mR={rm:n=>n},No=(_fs,e)=>({resolvedPath:e}),de=()=>({}),L=e=>[...new Set(e)],qw=e=>e.additionalWorkingDirectories??[],qJ=()=>false;
const iv=path.posix.isAbsolute,L2t=path.posix.resolve,l$o=path.posix.normalize,u$o="/",rr=()=>false,ji=e=>e.includes("$")||e.includes("__CMDSUB__"),w0e=e=>e.split("/").some((p,i,a)=>p===".."&&a.slice(0,i).some(x=>x&&x!==".")),G2t=(e,n)=>n(e),Hc=(e,n)=>e===n||e.startsWith(n+"/"),U=(e,n)=>e.filter(n).length;
const bSe=e=>e==="/"||path.posix.dirname(e)==="/"||e.toLowerCase()===home.toLowerCase();
const ub=(cmd,message,reason)=>({behavior:"ask",message,reason});
`+range("function $K(e,n,r,s,h){let y=mR[e]", "var B2t=",268500000),removal);
const nativeRemove=vm.runInContext("$K",removal);
const removals=["~","~/project","~/project/*","~/build/*","~/build"].map(target=>({target,cwd,home,expected:nativeRemove("rm",[target],cwd,undefined,false)}));
const removalsWithWorkingDirs=["/other/deep/*","/other/deep/sub/*","/other/deep"].map(target=>({target,cwd,home,workingDirs:["/other/deep"],expected:nativeRemove("rm",[target],cwd,{additionalWorkingDirectories:["/other/deep"]},false)}));
const env=vm.createContext({});
vm.runInContext(`const Gze=null,b0n=()=>true,_Z="__CMDSUB_OUTPUT__";const w0n=e=>e.replaceAll("__CMDSUB_OUTPUT__","").replaceAll("__TRACKED_VAR__","");`+range('var ZAt=Gze,aVe=new Set(', 'function X0o(e,n){')+range('function CVt(e,n){', 'function dbe(')+range('function zVt(e){', 'var vVt='),env);
const envPrefix=["LANG=C cat safe.txt","LANG=/tmp/locale cat safe.txt","LANG=./locale cat safe.txt","LANG=../locale cat safe.txt","LANG=folder cat safe.txt","LANG=__TRACKED_VAR__/locale cat safe.txt","LANG=__CMDSUB_OUTPUT__ cat safe.txt","SECRET=1 cat safe.txt"].map(command=>({command,expected:vm.runInContext("CVt",env)({command})}));
const wrappers=envPrefix.map(({command})=>({command,expected:vm.runInContext("xw",env)(command)}));
const result={removalsWithWorkingDirs,envPrefix,wrappers,version:"2.1.318",sha256,ranges,windowsPatterns,parserProbe,removals,limitations:["Windows patterns isolate the platform/NT namespace branches; UNC trust predicates are fixed false.","Removal probes inject a POSIX path/critical-directory/filesystem provider with no symlinks or additional working directories and no mounted-drive rule. The artifact's tilde-expansion and removal branch order are executed; executable dispatch and shell expansion are not run."]};
if(!normal||!zeroBudget)throw Error("Native parser budget probe failed");
const filename=fileURLToPath(new URL("../../crates/permission/tests/fixtures/permission_guards_2_1_318.json",import.meta.url));
const output=JSON.stringify(result,null,2)+"\n";
if(process.argv.includes("--write")){writeFileSync(filename,output);console.log("wrote "+filename)}
else if(readFileSync(filename,"utf8")!==output)throw Error("guard fixture differs from official artifact");
else console.log("Claude Code 2.1.318 guard oracle matches "+sha256);
