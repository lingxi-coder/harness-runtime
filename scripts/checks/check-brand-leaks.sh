#!/usr/bin/env bash
# 品牌泄漏门 —— 驱动（规则在 scripts/checks/check_brand_leaks.py）。
#
# 与 scripts/checks/check-deps.sh 同形状：只用 python3，无额外工具链。
#
# FAIL-CLOSED 是本脚本的设计要点。姊妹脚本 tools/scripts/check_version.sh
# 演示了反面：它用 `while … done < <(find lingxi-code …)`，目录一改名 find
# 就空转，循环体一次都不执行，脚本打印 OK 并 exit 0 —— set -euo pipefail
# 不传播进程替换的失败。这里不用进程替换，并且显式检查引擎存在。
set -euo pipefail
cd "$(dirname "$0")/../.."

ENGINE="scripts/checks/check_brand_leaks.py"
if [[ ! -f "$ENGINE" ]]; then
    echo "check-brand-leaks: engine missing at $ENGINE" >&2
    exit 1
fi

exec python3 "$ENGINE" "$@"
