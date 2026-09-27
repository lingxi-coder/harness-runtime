#!/usr/bin/env bash
# Plugin skill frontmatter 门 —— 驱动（规则在 scripts/check_skill_frontmatter.py）。
#
# 存在的理由：packer 对 description 有 180 显示列的硬上限，而
# **`len()` 只在纯 ASCII 下等于显示列宽**。写这个门之前，三次超限都是靠人工
# 复量抓到的（193 / 185 / 181），三次内容 agent 都报告过「全部 ≤180」。
set -euo pipefail
cd "$(dirname "$0")/.."

ENGINE="scripts/check_skill_frontmatter.py"
if [[ ! -f "$ENGINE" ]]; then
    echo "check-skill-frontmatter: engine missing at $ENGINE" >&2
    exit 1
fi

exec python3 "$ENGINE" "$@"
