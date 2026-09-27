#!/usr/bin/env python3
"""Verify the pinned official Claude Code binary's /loop cron oracle.

Usage: python3 verify_latest.py /tmp/lingxi-loop-oracle-2.1.270
The directory must contain package/claude and extracted chunks/*.js. No source
from the upstream binary is checked into this repository. Requires Node.js.
"""
import hashlib
import json
import mmap
import os
from pathlib import Path
import subprocess
import sys

VERSION = "2.1.270"
BINARY_SHA256 = "a506b6d970a4cf44f6abdb53a81ddcd5d3b0ce042a95c502fe9d1f946bdb8807"
root = Path(sys.argv[1])
with (root / "package/claude").open("rb") as binary:
    assert hashlib.file_digest(binary, "sha256").hexdigest() == BINARY_SHA256
chunks = root / "chunks"
prompt_source = (chunks / "src_168489904.js").read_text()
calendar_source = (chunks / "src_168472338.js").read_text()
assert f"// Version: {VERSION}" in prompt_source
assert f"// Version: {VERSION}" in calendar_source
with (root / "package/claude").open("rb") as binary:
    with mmap.mmap(binary.fileno(), 0, access=mmap.ACCESS_READ) as embedded:
        for source in [prompt_source, calendar_source]:
            assert embedded.find(source.encode()) >= 0, "Oracle source is not embedded verbatim in pinned binary"

prompt_fn = prompt_source[prompt_source.index("function aIn("):prompt_source.index("var lIn=")]
prompt = subprocess.check_output([
    "node", "-e", 'const QO=()=>false,Ym="CronCreate",cw="CronDelete",See=7;'
    + prompt_fn + '\nprocess.stdout.write(aIn(true));'
])
# Model-facing tool-store paths are upstream literals; no normalization.
fixture = Path(__file__).resolve().parents[1] / "fixtures/cron_create_prompt_2_1_263.txt"
assert prompt == fixture.read_bytes(), "CronCreate prompt bytes differ"
# All local durable/Monitor prompt branches, independently evaluated upstream.
contracts_source = prompt_source[prompt_source.index("function sIn("):prompt_source.index("export{")]
contracts = subprocess.check_output([
    "node", "-e", 'let monitor=false;const QO=()=>monitor,Ym="CronCreate",cw="CronDelete",da="Monitor",See=7;'
    + contracts_source + ';const cases=[];for(const durable of [false,true])for(monitor of [false,true])cases.push({durable,monitor,create:aIn(durable),description:sIn(durable),durableDescription:iIn(durable),delete:cIn(durable),list:dIn(durable)});process.stdout.write(JSON.stringify(cases,null,2)+"\\n");'
])
contracts_fixture = fixture.with_name("cron_contracts_2_1_270.json")
if "--write" in sys.argv:
    contracts_fixture.write_bytes(contracts)
assert contracts == contracts_fixture.read_bytes(), "Cron tool local branch bytes differ"

human_source = calendar_source[calendar_source.index("var M="):calendar_source.index("function sle(")]
human_inputs = ["* * * * *", "*/1 * * * *", "*/5 * * * *", "*/4294967296 * * * *", "*/1000000000000000000000 * * * *", "0 * * * *", "7 * * * *", "00 * * * *", "0 */1 * * *", "7 */2 * * *", "0 */4294967296 * * *", "0 0 * * *", "30 12 * * *", "59 23 * * *", "0 9 * * 0", "0 9 * * 7", "0 9 * * 6", "0 9 * * 1-5", "0 9 * * 0,6", "30 14 28 2 *", "bad", "\ufeff* * * * *\ufeff", "*\u0085* * * *"]
human = subprocess.check_output(["node", "-e", human_source + ';process.stdout.write(JSON.stringify(' + json.dumps(human_inputs) + '.map(cron=>({cron,text:cS(cron)})),null,2)+"\\n");'], env={**os.environ,"TZ":"UTC"})
# fixtures path: tools/cron/tests/fixtures -> workspace root is parents[4].
human_fixture = fixture.parents[4] / "cron/src/bundled/human_schedule_2_1_270.json"
if "--write" in sys.argv:
    human_fixture.write_bytes(human)
assert human == human_fixture.read_bytes(), "Cron cadence text differs"
calendar_fn = calendar_source[calendar_source.index("var x="):calendar_source.index("var M=")]
cases = [
    ["0 0 29 2 *", "2025-03-01T00:00:00Z", "2028-02-29T08:00:00.000Z"],
    ["* * * * *", "2026-11-01T08:59:00Z", "2026-11-01T10:00:00.000Z"],
    ["30 1 * * *", "2026-11-01T08:45:00Z", "2026-11-02T09:30:00.000Z"],
    ["* * * * *", "2026-03-08T09:59:00Z", "2026-03-08T10:00:00.000Z"],
    ["30 2 * * *", "2026-03-08T09:00:00Z", "2026-03-09T09:30:00.000Z"],
]
output = subprocess.check_output([
    "node", "-e", calendar_fn + '\nconst cases=' + json.dumps(cases)
    + ';process.stdout.write(JSON.stringify(cases.map(([cron,at])=>Nje(JO(cron),new Date(at))?.toISOString())));'
], env={**os.environ, "TZ": "America/Los_Angeles"})
assert json.loads(output) == [case[2] for case in cases], "Calendar oracle differs"
parser_output = subprocess.check_output([
    "node", "-e", calendar_fn
    + ';process.stdout.write(JSON.stringify([JO("\\ufeff* * * * *\\ufeff")!==null,JO("*\\u0085* * * *")===null,JO("*/4294967296 * * * *").minute]));'
])
assert json.loads(parser_output) == [True, True, [0]], "Parser oracle differs"

print(json.dumps({"version": VERSION, "binarySha256": BINARY_SHA256,
                  "promptSha256": hashlib.sha256(prompt).hexdigest(),
                  "sourceSha256": {"src_168489904.js": hashlib.sha256(prompt_source.encode()).hexdigest(), "src_168472338.js": hashlib.sha256(calendar_source.encode()).hexdigest()},
                  "toolPromptVariants": 4, "humanScheduleCases": len(human_inputs), "parserCases": 3, "calendarCases": len(cases), "timezone": "America/Los_Angeles",
                  "normalizations": []}, indent=2))
