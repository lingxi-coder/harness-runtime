import json
import re
from pathlib import Path

folder = Path(__file__).parent
rows = [
    json.loads(line)
    for line in (folder / "worker-output.jsonl").read_text(encoding="utf-8").splitlines()
]
assert len(rows) == 3
ping, batch, filtered = rows
assert ping["ok"] and ping["runtime"] == "bun"
assert ping["bun_version"] == "1.4.3"
assert ping["bun_revision"] == "c6da4a4d3010e5553438c60f6bd76d981976867c"
assert ping["protocol_version"] == 4
assert ping["worker_id"] == "native-policy-js-semantics-2.1.295-v4"
assert batch["ok"] and filtered["ok"]
lines = [row["line_units"] for row in batch["rows"]]
assert len(lines) == 3 and all(line is not None for line in lines)
text0 = "".join(chr(unit) for unit in lines[0])
assert re.match(
    r"^20\d\d-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z \[WARN\] "
    r"policyHelper: token=\[REDACTED\]\n$",
    text0,
)
assert "sk-ant-" not in text0
assert lines[1][-1] == 10
assert [92, 117, 100, 56, 48, 48] in [
    lines[1][index : index + 6] for index in range(len(lines[1]) - 5)
]
assert [0xD83D, 0xDE00] in [
    lines[1][index : index + 2] for index in range(len(lines[1]) - 1)
]
assert [92, 110] in [
    lines[1][index : index + 2] for index in range(len(lines[1]) - 1)
]
text2 = "".join(chr(unit) for unit in lines[2])
assert "\\u0001\\u007f\\u2028" in text2
assert lines[2][-1] == 10
assert filtered["rows"] == [{"input_units": batch["rows"][0]["input_units"], "line_units": None}]
print("verified: pinned Bun fingerprint, token redaction, exact filter rejection, multiline D800/paired-pair preservation, and control escaping")
