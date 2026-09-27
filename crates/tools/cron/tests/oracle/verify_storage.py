#!/usr/bin/env python3
"""Execute pinned upstream CronCreate persistence with deterministic host seams."""
import hashlib
from pathlib import Path
import subprocess
import sys
root = Path(sys.argv[1])
with (root / "package/claude").open("rb") as binary:
    assert hashlib.file_digest(binary,"sha256").hexdigest() == "a506b6d970a4cf44f6abdb53a81ddcd5d3b0ce042a95c502fe9d1f946bdb8807"
s = (root / "chunks/src_168472338.js").read_text()
writer = s[s.index("async function Kxt("):s.index("async function yY(")]
js = '''const process={pid:42}, Date={now:()=>1700000000000};
const I=()=>"abcd1234-0000-0000-0000-000000000000",X=()=>"creator-session",xG=()=>"process-start";
const yn=()=>"/project",PX=()=>true,S=(...p)=>p.join("/"),EOe=async()=>{},D=async()=>{},AI="tmp",b=JSON.stringify;
const bee=()=>"/project/.claude/scheduled_tasks.json",ent=async()=>[];
const FS=async(path,body)=>{if(path!==bee())throw Error("wrong path");require("process").stdout.write(body)};
''' + writer + ';a0e("*/5 * * * *","check",true,true);'
output = subprocess.check_output(["node","-e",js])
fixture = Path(__file__).with_name("session_storage_2_1_270.json")
if "--write-fixture" in sys.argv:
    fixture.write_bytes(output)
else:
    assert output == fixture.read_bytes(), "Upstream persistence bytes differ"
print(hashlib.sha256(output).hexdigest())
