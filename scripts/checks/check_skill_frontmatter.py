#!/usr/bin/env python3
"""Plugin skill frontmatter 门（驱动在 scripts/checks/check-skill-frontmatter.sh）。

与 scripts/checks/check_deps.py / check_brand_leaks.py / check_i18n_pairing.py 同形状：
只用 python3，规则是模块常量，有 --list 模式，`sys.exit(main())`。

## 为什么这是一道门而不是一次人工检查

写这个门之前，我已经**手工抓到三次** description 超限：batch 3 的
`local-app-data`（193）和 `local-app-background`（185），batch 4 的
`template-selection`（181）。三次都是在内容 agent 报告「全部 ≤180」之后
我自己重新量出来的——agent 不是在撒谎，是它用 `len()` 量的。

**`len()` 和显示列宽只在纯 ASCII 下相等。** 一条 179 个字符的中文 description
是 358 列，packer 会拒绝它，而任何按字符数做的检查都会说它没问题。本仓库的
i18n 真源是中文（`clients/translations/zh-Hans.json`），所以「以后有人用中文写
skill description」不是假设性风险。

判据必须落在**门**里，不能落在「我记得每次都量一遍」上。

## 三条规则

- **F1 name == 目录名。** registry identity 来自目录，frontmatter 的 name 来自
  文件；两者不一致时 listing 和 invocation 会指向不同的东西（§7.2.1）。
- **F2 description ≤ 180 显示列。** packer 的硬上限；超了它会点名 skill 拒绝。
  用 east-asian-width 量，不用 len()。
- **F3 frontmatter 必须存在且可解析**，且 name/description 都非空。

## fail-closed

枚举为零时**报错**，不报 OK。`tools/scripts/check_version.sh` 演示了反面：
目录一改名 `find` 就空转，脚本打印 OK 并 exit 0。一道扫不到东西的门和没有门
是同一件事，但它看起来像通过了。
"""

import argparse
import re
import sys
import unicodedata
from pathlib import Path

# packer 的硬上限（设计文档 §7.2.1）。超限时 packer 点名 skill/FQN 拒绝。
MAX_DESCRIPTION_COLUMNS = 180

# 门要扫的 skill 根目录，相对仓库根。
SKILL_ROOTS = [
    "crates/plugins/lingxi-local-app/skills",
    "crates/runtime/src/mobile/device_skills",
]

# Agent 用同一套 frontmatter 判据,但形状不同:agent 是 `agents/<name>.md`
# 单文件,identity 来自**文件名**;skill 是 `skills/<name>/SKILL.md`,identity
# 来自**目录名**。两者都由 `plugin/src/discovery.rs` 的 `glob_md()` 递归发现。
AGENT_ROOTS = ["crates/plugins/lingxi-local-app/agents"]

# Prose that asserts a tool does not exist. Matched per SENTENCE against the
# agent's own granted tool names, so "no `LocalAppActOnUi` — that is operator's
# job" (a scope statement about a tool the agent was NOT granted) does not trip
# it; only "<granted tool> ... has no backing Host tool" does.
DENIES_EXISTENCE = re.compile(
    r"(has no backing|no backing Host tool|do(?:es)? not exist|there is no Host tool|"
    r"is not reachable from any tool|anywhere in the repo|zero hits|0 hits)",
    re.I,
)

# ⛔ 这三个字段出现在 plugin agent 的 frontmatter 里就是错的,而且**两种错法不同**:
# `hooks` / `permissionMode` 让 `validate_plugin_agent_frontmatter` 失败,
# 于是 `PluginManagerError::Validation` **让整个 plugin 装载失败**
# (`plugin/src/manager.rs:605-609`)——不是跳过这一个 agent,是全部。
# `mcpServers` 则是解析后被清空并 warn(`manager.rs:620-627`),静默降级。
# 前者炸得很响,后者不响,所以后者更需要门。
FORBIDDEN_AGENT_FIELDS = ["permissionMode", "mcpServers", "hooks"]

FRONTMATTER = re.compile(r"\A---\r?\n(.*?)\r?\n---\r?\n", re.S)


def display_columns(text):
    """显示列宽。CJK / 全角字符算两列。

    ⛔ 不要换成 len()。两者只在纯 ASCII 下相等,而这正是本门存在的理由。"""
    return sum(2 if unicodedata.east_asian_width(c) in ("W", "F") else 1 for c in text)


def scalar(block, key):
    m = re.search(r"^%s:\s*(.*)$" % re.escape(key), block, re.M)
    if not m:
        return None
    return m.group(1).strip().strip('"').strip("'")


def main():
    ap = argparse.ArgumentParser(prog="check-skill-frontmatter")
    here = Path(__file__).resolve()
    ap.add_argument("--repo", default=str(here.parents[2]))
    ap.add_argument("--list", action="store_true")
    args = ap.parse_args()

    repo = Path(args.repo)
    skills = []
    for root in SKILL_ROOTS:
        base = repo / root
        if not base.is_dir():
            print("check-skill-frontmatter: skill root missing at %s — refusing to report a clean "
                  "result from a root that does not exist" % base, file=sys.stderr)
            return 1
        skills.extend(sorted(p for p in base.glob("*/SKILL.md")))

    if not skills:
        print("check-skill-frontmatter: enumerated ZERO skills under %s — refusing to report clean "
              "from an empty enumeration" % ", ".join(SKILL_ROOTS), file=sys.stderr)
        return 1

    agents = []
    for root in AGENT_ROOTS:
        base = repo / root
        if not base.is_dir():
            print("check-skill-frontmatter: agent root missing at %s — refusing to report a clean "
                  "result from a root that does not exist" % base, file=sys.stderr)
            return 1
        # rglob, not glob: matches discovery.rs's glob_md, which recurses —
        # a nested reorg the plugin loader still finds must not go unseen here.
        agents.extend(sorted((p, base) for p in base.rglob("*.md")))

    if not agents:
        print("check-skill-frontmatter: enumerated ZERO agents under %s — refusing to report clean "
              "from an empty enumeration" % ", ".join(AGENT_ROOTS), file=sys.stderr)
        return 1

    problems = []
    rows = []
    for path, agent_base in agents:
        stem = path.stem
        # `stem` is the IDENTITY (discovery.rs keys agents by file name, not by
        # relative path); `rel` is only for the message, so a nested agent the
        # rglob above now finds is reported at a path that actually exists.
        rel = path.relative_to(agent_base)
        text = path.read_text(encoding="utf-8", errors="replace")
        m = FRONTMATTER.match(text)
        if not m:
            problems.append("agents/%s: no YAML frontmatter block" % rel)
            continue
        block = m.group(1)
        name = scalar(block, "name")
        if not name:
            problems.append("agents/%s: frontmatter has no non-empty `name`" % rel)
        elif name != stem:
            problems.append(
                "agents/%s: frontmatter name is %r but the file is %r.md — agent identity comes "
                "from the FILE NAME" % (rel, name, stem)
            )
        if not scalar(block, "description"):
            problems.append("agents/%s: frontmatter has no non-empty `description`" % rel)
        for field in FORBIDDEN_AGENT_FIELDS:
            if re.search(r"^%s\s*:" % re.escape(field), block, re.M):
                how = ("makes validate_plugin_agent_frontmatter fail, which fails the WHOLE plugin load"
                       if field in ("hooks", "permissionMode")
                       else "is parsed then silently cleared with only a tracing warning")
                problems.append(
                    "agents/%s declares `%s`, which a plugin agent must never set — it %s"
                    % (rel, field, how)
                )

        # An agent that GRANTS a tool must not also tell itself the tool does
        # not exist. The 2026-09-02 create-flow fix corrected exactly this
        # sentence in `tester.md` and left the identical sentence standing in
        # `designer.md`, `operator.md` and `verifier.md` — three of the four
        # agents the build workflow orders to resolve a selection handle were
        # still reading "there is no such tool anywhere in the repo" in their
        # own system prompt. Nothing caught it: the frontmatter was right, the
        # tool was registered, and the contradiction lived only in prose.
        body = text[m.end():]
        for tool in re.findall(r"^\s*-\s*([A-Za-z][A-Za-z0-9_]*)\s*$", block, re.M):
            if tool not in body:
                continue
            for sentence in re.split(r"(?<=[.。])\s+", body):
                if tool not in sentence:
                    continue
                if DENIES_EXISTENCE.search(sentence):
                    problems.append(
                        "agents/%s grants `%s` in its frontmatter but its prose says the tool "
                        "does not exist: %r — an agent told its own granted tool is missing will "
                        "skip the step that needs it"
                        % (rel, tool, " ".join(sentence.split())[:160])
                    )
                    break

    for path in skills:
        directory = path.parent.name
        text = path.read_text(encoding="utf-8", errors="replace")
        m = FRONTMATTER.match(text)
        if not m:
            problems.append("%s: no YAML frontmatter block at the top of the file" % directory)
            continue
        block = m.group(1)
        name = scalar(block, "name")
        desc = scalar(block, "description")
        if not name:
            problems.append("%s: frontmatter has no non-empty `name`" % directory)
        elif name != directory:
            problems.append(
                "%s: frontmatter name is %r but the directory is %r — registry identity comes from "
                "the directory and listing comes from the file, so these must agree"
                % (directory, name, directory)
            )
        if not desc:
            problems.append("%s: frontmatter has no non-empty `description`" % directory)
            continue
        cols = display_columns(desc)
        rows.append((directory, cols, len(desc)))
        if cols > MAX_DESCRIPTION_COLUMNS:
            problems.append(
                "%s: description is %d display columns, over the packer's limit of %d "
                "(%d characters — these differ for any non-ASCII text, which is exactly the case "
                "a character count would miss)"
                % (directory, cols, MAX_DESCRIPTION_COLUMNS, len(desc))
            )

    if args.list:
        for directory, cols, chars in sorted(rows):
            print("  %-24s %3d columns  %3d chars" % (directory, cols, chars))
        return 0

    if problems:
        print("SKILL-FRONTMATTER FAIL:\n  - " + "\n  - ".join(problems), file=sys.stderr)
        return 1
    widest = max(rows, key=lambda r: r[1])
    print("OK: %d skills + %d agents; every name matches its directory/file, no agent declares a "
          "forbidden field, widest description is %s at %d/%d columns"
          % (len(rows), len(agents), widest[0], widest[1], MAX_DESCRIPTION_COLUMNS))
    return 0


if __name__ == "__main__":
    sys.exit(main())
