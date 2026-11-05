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
 [String.raw`[^a\]`, "a"], [String.raw`[^a\]`, String.raw`[^a\]`],
 [String.raw`[\c]`, "\\"], [String.raw`[\c]`, "c"], [String.raw`[\c]`, "C"],
 [String.raw`[\ca]`, "\u0001"], [String.raw`[\c0]`, "\u0010"],
 [String.raw`[\ca-z]`, "b"], [String.raw`[\c0-z]`, "b"], [String.raw`[\c_-z]`, "b"],
 [String.raw`[a\-z]`, "b"], [String.raw`[a\-z]`, "-"],
];
// A class-set escape is not a scalar range endpoint in legacy RegExp.
// Include both endpoint positions, complements and non-ASCII whitespace.
for (const escape of ["d", "D", "s", "S", "w", "W"]) {
 for (const body of [`\\${escape}-z`, `a-\\${escape}`, `!-\\${escape}`, `\\${escape}`, `\\${escape}-!`, `\\${escape}--\\${escape}`]) {
  for (const input of ["f", "-", "0", "z", "_", "\t", "\u00a0", "\u2028", "\u0085", "ſ", "K", "σ"]) {
   fileMatches.push([`x[${body}]y`, `x${input}y`]);
  }
 }
 // Wildcards are rewritten before node-ignore parses character classes.
 for (const wildcard of ["?", "*", String.raw`\?`, String.raw`\*`]) {
  for (const body of [`\\${escape}${wildcard}`, `${wildcard}\\${escape}`]) {
   for (const input of ["0", "f", "]", "0]", "f]", "^]", "[]", "0a]", "0/]", "\\]"]) {
    fileMatches.push([`x[${body}]y`, `x${input}y`]);
   }
  }
 }
}
for (const pattern of [String.raw`x[\d\-?]y`, String.raw`x[\d**]y`, String.raw`x[\x80?]y`, String.raw`x[\u0080?]y`, String.raw`x[?\-?]y`, "x[\\d*\r]y", "x[\\d**\r]y", "x[\\d*\u2028]y", "x[\\d\\ \u00a0?]y"]) {
 for (const input of ["x0y", "x0]y", "xf]y", "x^]y", "x\u0080]y", "x]y", "x\ry", "x\u2028y", "x ]y"]) {
  fileMatches.push([pattern, input]);
 }
}
for (const pattern of [String.raw`x[\x80]y`, String.raw`x[\u0080]y`, String.raw`x[\s\x80]y`, String.raw`x[\x61]y`, String.raw`x[\141]y`, String.raw`x[\t]y`]) {
 for (const input of ["x0y", "x8y", "xxy", "xuy", "xay", "x\u0080y", "x\ty", "x y"]) {
  fileMatches.push([pattern, input]);
 }
}
const matchedFiles=fileMatches.map(([pattern,input])=>{const prepared=api.preprocess(pattern,false);return {pattern,input,expected:prepared!==null&&api.ignore().add(prepared).test(input).ignored}});
const result={fileMatches:matchedFiles,version:"2.1.318",sha256,ranges,scope:"Pure rule parser/public aliases, wildcard, gitignore validation and cwd rebasing; eval/REPL feature gate fixed off, POSIX path implementation. No live permission UI/model/network execution.",parses,serializations,wildcards:wildcardInputs,prefixes,rebases,preprocess};
const fixture=fileURLToPath(new URL('../../crates/permission/tests/fixtures/permission_rules_2_1_318.json',import.meta.url));
const output=JSON.stringify(result,null,2)+"\n";
if(process.argv.includes("--write")){writeFileSync(fixture,output);console.log("wrote "+fixture)}
else if(readFileSync(fixture,"utf8")!==output)throw Error("permission rule fixture differs from official artifact");
else console.log("Claude Code 2.1.318 permission rules oracle matches "+sha256);
