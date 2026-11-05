// Evaluate only hash-pinned pure permission functions from the official artifact.
import { readFileSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import path from "node:path";
import vm from "node:vm";
import { fileURLToPath } from "node:url";
const binary = readFileSync(process.argv[2]);
const sha256 = createHash("sha256").update(binary).digest("hex");
if (sha256 !== "677a2ea7bf576cedd20e191d973914ed8a41796f53a63b1923381ddc4d5b5e7d") throw Error("unexpected baseline");
const ranges = [];
function range(start, end) {
 const from = binary.indexOf(start, 257482802), to = binary.indexOf(end, from);
 if (from < 0 || to < from) throw Error("missing oracle anchor: " + start);
 ranges.push({start: from, end: to, sha256: createHash("sha256").update(binary.subarray(from,to)).digest("hex")});
 return binary.subarray(from,to).toString("utf8");
}
const context = vm.createContext({dt:path.posix,Ie:"/",process:{platform:"darwin"}});
vm.runInContext(`const ae=()=>false;`+
 range('var l={Task:"Agent"', 'export{td,') +
 range('var Ab=new RegExp(', 'function Tb(')+
 `const Cye=(()=>{const O=fn=>{let m;return()=>{if(!m){m={exports:{}};fn(m.exports,m)}return m.exports}};`+range('var Cye=O(function(', 'var tn=Oe(Cye(),1);')+`return Cye})();`+
 `const tn={default:Cye()}; const Ao=s=>s.toLowerCase().replace(/ı/g,"i").replace(/ſ/g,"s");`+
 range('function mo(e){return dt.join(Ie,e)}', 'function go(e){')+
 range('function eg(e){try{return jl.default()', 'class Bl{')+
 `const vdo=eg;const jl=tn;const mi=()=>{};`+
 range('function Dn(e){let n=e.replace(/', 'var ph=')+
 `;globalThis.api={parse:yr,serialize:fn,wildcard:d7,rebase:Fc,preprocess:(e,a)=>es(Dn(e),a),ignore:Cye()};`,context,{timeout:1000});
const {api}=context;
const parseInputs=["Task", "Task(general-purpose)","Agent", "AgentOutputTool", "BashOutputTool","AgentOutput","BashOutput","KillShell","Brief","Eval","Bash()","Bash(*)","Bash(x)y","Bash)x(y)",String.raw`Bash\(name(x)`,String.raw`Bash(echo \(x\))`,"(foo)"];
const parses=parseInputs.map(input=>({input,expected:api.parse(input)}));
const serializations=[{toolName:"Read",ruleContent:""},{toolName:"Read",ruleContent:"a(b)"}].map(input=>({input,expected:api.serialize(input)}));
const wildcardInputs=[
 ["*","abc"],["git *","git"],["git *","git add"],["cat /a/**/b","cat /a/b"],
 ["\0ESCAPED_STAR\0","*"],["x\0GLOBSTAR\0y","x/y"],["a\0*b","a\0xxb"],
 ["\u0085*\u0085","x"],["\uFEFF*\uFEFF","x"],
 ["git  *","git\tstatus",false,true], ["GIT *","git status",true,true],
 [String.raw`echo \*`,"echo *"], ["*","\n"],
].map(input=>({input,expected:api.wildcard(...input)}));
const prefixInputs=["git:*","git\nfoo:*","git\rfoo:*","git\u2028foo:*",":*"];
const prefixes=prefixInputs.map(input=>({input,expected:vm.runInContext("jHe",context)(input)}));
const rebases=[
 ["/proj","/secrets/**","/proj/secrets"],["/proj","/secrets/**","/proj/secrets/nested"],
 ["/proj","/s*/**/*.key","/proj/secrets"],["/proj","/**/secrets/*.key","/proj/x"],
 ["/proj","/**/secrets/*.key","/proj/secrets"],["/proj","/secrets/","/proj/secrets"],
 ["/proj","/secrets/*.key","/else"],["/proj","/src/**","/proj/vendor"],
 ["/proj","/a/b/**","/proj/a"],["/proj","/a/**/b/**","/proj/a/b"]
].map(([patternRoot,pattern,rootPath])=>({input:{patternRoot,pattern,rootPath},expected:api.rebase({patternRoot,pattern,rootPath})}));
const preprocess=[" /**", "a\\\\\\", "#secret","!#secret","a[","a\\"," ","!","/a//b/**","\uFEFF.env","foo/**","[a--c]","[z-a]","[z-ab]"].flatMap(input=>[false,true].map(allow=>({input,allow,expected:api.preprocess(input,allow)})));
const fileMatches=[
 ["{src,lib}/a.txt", "src/a.txt"], ["{src,lib}/a.txt", "{src,lib}/a.txt"], ["src/[A-\u0080].txt", "src/A.txt"], ["src/[A-\u0080].txt", "src/\u0080.txt"], ["src/[A-\u0080].txt", "src/\u0081.txt"], ["src/[!A-\u0080].txt", "src/\u0080.txt"], ["src/[!A-\u0080].txt", "src/\u0081.txt"], ["[Z-a]", "A"], ["[Z-a]", "z"], ["[!a]", "b"], ["[!a]", "a"], ["[^a]", "b"], ["[σ-τ]", "ς"], ["Secret", "secret"], ["Secret", "SECRET/data"], ["σ", "ς"], ["K", "K"], ["s", "ſ"], ["I", "ı"],
 ["#secret", "#secret"], ["a\\", "a\\"], ["\uFEFF.env", ".env"], ["a[", "a["], ["[z-a]", "z"],
 ["[a--c]", "c"], ["[a--c]", "[a--c]"], ["[z-a]", "[z-a]"], ["[z-ab]", "[z-ab]"], ["[z-ab]", "b"],
].map(([pattern,input])=>{const prepared=api.preprocess(pattern,false);return {pattern,input,expected:prepared!==null&&api.ignore().add(prepared).test(input).ignored}});
const result={fileMatches,version:"2.1.318",sha256,ranges,scope:"Pure rule parser/public aliases, wildcard, gitignore validation and cwd rebasing; eval/REPL feature gate fixed off, POSIX path implementation. No live permission UI/model/network execution.",parses,serializations,wildcards:wildcardInputs,prefixes,rebases,preprocess};
const fixture=fileURLToPath(new URL('../../crates/permission/tests/fixtures/permission_rules_2_1_318.json',import.meta.url));
const output=JSON.stringify(result,null,2)+"\n";
if(process.argv.includes("--write")){writeFileSync(fixture,output);console.log("wrote "+fixture)}
else if(readFileSync(fixture,"utf8")!==output)throw Error("permission rule fixture differs from official artifact");
else console.log("Claude Code 2.1.318 permission rules oracle matches "+sha256);
