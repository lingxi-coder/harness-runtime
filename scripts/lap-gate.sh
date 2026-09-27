#!/usr/bin/env bash
# Local App plugin 项目的门契约 —— 驱动（判据在 scripts/lap_gate.py）。
#
# 与 scripts/check-deps.sh、scripts/check-brand-leaks.sh 同形状：只用 python3，
# 无额外工具链，显式检查引擎存在。
#
# 这个脚本存在的唯一目的是让「写 / 评审 / 修 / 验证」四方调用**同一份字节**。
set -euo pipefail

# 判据文件的路径按**调用者的 cwd** 解析，不按本脚本 cd 之后的 cwd。
# 这两者不同，是一条 "file not found" 能把排查送去错误方向的地方。
export LAP_GATE_CWD="${LAP_GATE_CWD:-$PWD}"
cd "$(dirname "$0")/.."

ENGINE="scripts/lap_gate.py"
if [[ ! -f "$ENGINE" ]]; then
    echo "lap-gate: engine missing at $ENGINE" >&2
    exit 1
fi

exec python3 "$ENGINE" "$@"
