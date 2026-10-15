#!/usr/bin/env python3
"""Execute the pinned builtin's real hooks; filesystem records are explicit inputs."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess

SHA = "c989596b92bf28f6872d5e44c2baa4686838501f35a4882bc437149b151c2b95"
ROOT = Path(__file__).resolve().parents[2]
OUTPUT = ROOT / "crates/orchestrator/tests/fixtures/instructions-2.1.286/agents.json"
DRIVER = r'''
const fs=require('fs'),vm=require('vm');
const source=fs.readFileSync(process.argv[1],'utf8');
vm.runInThisContext(source.slice(source.indexOf('function M(e,t)'),source.indexOf('var ne=')));
const file=(path,content,kind='project',parent)=>({path,content,kind,...parent?{parent}:{}});
const group=(dir,name,body,imports=[])=>({dir,name,parts:[{path:dir+'/'+name,content:body},...imports]});
const base=[file('/fixture/managed/CLAUDE.md','managed','managed'),file('/fixture/home/.claude/CLAUDE.md','user','user')];
const sets=[
 {name:'fallback',options:{},engine:base,groups:[group('/fixture','AGENTS.md','outer'),group('/fixture/repo','AGENTS.md','inner')]},
 {name:'fallback_yields_to_ancestor',options:{},engine:[...base,file('/fixture/CLAUDE.md','legacy')],groups:[group('/fixture','CLAUDE.md','legacy'),group('/fixture/repo','AGENTS.md','inner')]},
 {name:'both_directory_order',options:{instructionFiles:'claude-md-and-agents-md'},engine:[...base,file('/fixture/CLAUDE.md','outer legacy'),file('/fixture/repo/CLAUDE.md','inner legacy')],groups:[group('/fixture','AGENTS.md','outer agent'),group('/fixture/repo','AGENTS.md','inner agent')]},
 {name:'same_body_is_suppressed',options:{instructionFiles:'claude-md-and-agents-md'},engine:[...base,file('/fixture/repo/CLAUDE.md','same')],groups:[group('/fixture/repo','AGENTS.md',' same ')]},
 {name:'same_candidate_body_kept',options:{},engine:base,groups:[group('/fixture','AGENTS.md','same'),group('/fixture/repo','AGENTS.md','same')]},
 {name:'imports_follow_root',options:{instructionFiles:'claude-md-and-agents-md'},engine:[...base,file('/fixture/repo/CLAUDE.md','legacy')],groups:[group('/fixture','AGENTS.md','outer\n@./shared.md',[{path:'/fixture/shared.md',content:'shared'}]),group('/fixture/repo','AGENTS.md','inner')]},
 {name:'existing_import_path_suppressed',options:{instructionFiles:'claude-md-and-agents-md'},engine:[...base,file('/fixture/repo/CLAUDE.md','legacy\n@./AGENTS.md'),file('/fixture/repo/AGENTS.md','agent','project','/fixture/repo/CLAUDE.md')],groups:[group('/fixture/repo','AGENTS.md','agent')]},
 {name:'managed_only',options:{instructionFiles:'managed-only'},engine:[...base,file('/fixture/repo/CLAUDE.md','legacy')],groups:[]},
 {name:'claude_only',options:{instructionFiles:'claude-md'},engine:base,groups:[group('/fixture/repo','AGENTS.md','ignored')]},
 {name:'legacy_both',options:{projectInstructions:'both'},engine:base,groups:[group('/fixture/repo','AGENTS.md','agent')]},
];
async function run(test){
 const hooks={};Ye((event,filter,fn)=>hooks[event]=fn??filter,test.options);
 const api={session:{root:async()=>'/fixture/repo',cwd:async()=>'/fixture/repo'},fs:{ancestors:async({names})=>test.groups.filter(g=>names.includes(g.name))},telemetry:{log:()=>{},mark:()=>{}},ui:{log:()=>{}},env:{get:async()=>undefined}};
 const next=async x=>x;
 const result=hooks['prompt.context']?await hooks['prompt.context'](api,{instructionFiles:test.engine},next):{instructionFiles:test.engine};
 return {...test,expected:result.instructionFiles};
}
(async()=>{const results=[];for(const s of sets)results.push(await run(s));console.log(JSON.stringify(results));})().catch(e=>{console.error(e);process.exit(1)});
'''


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("oracle", type=Path)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    raw = args.oracle.read_bytes()
    if hashlib.sha256(raw).hexdigest() != SHA:
        raise SystemExit("oracle chunk hash differs")
    result = json.loads(subprocess.check_output(["node", "-e", DRIVER, str(args.oracle)], text=True))
    # Only the user's branding exception is normalized.
    payload = json.loads(json.dumps(result).replace("CLAUDE", "LINGXI").replace(".claude/", ".lingxi/"))
    encoded = json.dumps({"version": "2.1.286", "oracle_sha256": SHA, "cases": payload}, indent=2) + "\n"
    if args.check:
        if OUTPUT.read_text() != encoded:
            raise SystemExit("fixture drift")
    else:
        OUTPUT.parent.mkdir(parents=True, exist_ok=True)
        OUTPUT.write_text(encoded)
    print(f"{len(payload)} AGENTS oracle hook cases verified")


if __name__ == "__main__":
    main()
