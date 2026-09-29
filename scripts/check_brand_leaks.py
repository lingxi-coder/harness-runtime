#!/usr/bin/env python3
"""品牌泄漏门 — 规则引擎（由 scripts/check-brand-leaks.sh 驱动）。

产出一个排序后的违规集合并与 baseline 精确比对。用集合而不是计数，是因为
计数只能发现「多了」；集合同时能发现「少了」，而「少了」正是正则退化成零
匹配的症状 —— 这个仓库的 tools/scripts/check_version.sh 就是被这种失效咬过
的活例子（目录改名让它的 find 空转，脚本打印 OK 并 exit 0）。

规则（见 spec §7.1）：
  G1  代码行的字符串字面量里出现品牌 token（needle 集合按区域不同，见 AREAS）
  G2  CLAUDE_* 环境变量读取点，名字不在 KEEP 清单
  G3  反向 —— L1 的路径/文件名常量值出现在 branding crate 之外
  G4  注释自称 claude-code 源码引用却拼作 LINGXI_*（假引用）
  G5  冻结清单里存在目标已消失的条目（死豁免 = 清单在腐烂）
  G6  branding 里存在未被 NAMESPACE_VALUES 覆盖的 pub const（清单覆盖对账）
  G7  Claude 侧 plugin 标识符**到达**本仓库（整行判据，不要求字符串字面量）

用法:
  check_brand_leaks.py                      # 与 baseline 比对，有差异则 exit 1
  check_brand_leaks.py --list               # 打印当前违规集合（含行号，给人看）
  check_brand_leaks.py --update-baseline    # 把当前集合写回 baseline
  check_brand_leaks.py --root DIR           # 针对合成树运行（自测用）
  check_brand_leaks.py --selftest           # 每规则正/负对照（合成树，不碰真仓库）

**baseline 的键是文件粒度的，不含行号。** `--list` 的输出仍然带行号——那是
给人定位用的——但写进 baseline 并参与比对的键只有 `(rule, path, needle)`。
理由是行号会让 baseline 变成一份对无关改动敏感的清单：在一个文件顶部插入
一行会同时产生 155 条删除和 155 条新增，而 baseline 横跨 528 个文件、其中
163 个在最近 60 个提交里被动过——于是大量与品牌无关的 PR 会把这道门弄红，
而文档里给出的补救手段是 `--update-baseline`，它会把扫描器**当时**产出的
任何东西（包括一个已经退化成零匹配的正则）静默重新冻住。换句话说：让门
频繁误红，等于把「重新生成 baseline」变成例行公事，而重新生成正是洗白退化
的那条通道。

去掉行号并不削弱「集合也能发现消失」这一原始理由：一个正则退化成零匹配
时，它命中过的每个文件都会从集合里消失，比对照样报红。丢掉的只是「同一
文件内命中从 7 处降到 1 处」这种粒度。

⚠️ **这段话原先把那一类退化的兜底记在
`branding/tests/brand_gate_test.rs::gate_still_finds_the_classes_that_must_exist_today`
头上 —— 那个文件和那个测试在本仓库里不存在**（`git ls-files | grep brand_gate`
零命中；那个名字只活在 `docs/superpowers/plans/2026-08-23-…` 这份从未落地的
计划里）。一条正确的规则配一个不存在的理由，比没有理由更危险：读的人会以为
已经有东西看着了。真实状态是：
  * 「某条规则整体退化成零匹配」由集合比对兜底（条目消失 => removed => exit 1）；
  * 「同一文件内命中数下降」**今天没有任何东西兜底**，这是已知残余；
  * 每条规则「至少能开火一次」由本文件的 `--selftest` 在合成树上兜底 ——
    它不依赖真实仓库恰好含有某类内容，所以「绿」不会与「探针根本没跑」混淆。

本门自身的三个产出物（引擎源码、baseline、frozen 清单）被排除在扫描之外
（见 SELF_ARTIFACTS）。这不是降噪，是正确性：引擎源码里的品牌 token 是
needle 定义本身，不是泄漏；baseline 的内容按定义就是品牌 token 命中的列表；
frozen 清单存在的意义就是逐字写出那些标识符。更关键的是，若不排除
baseline，自扫描没有不动点 —— baseline 里记录着 baseline 自身的行号，重写
baseline 会移动那些行号，下一次扫描又会得到不同的自我引用命中集合，
永不收敛，门也就永远不可能报绿。

文件枚举走 `git ls-files`（见下），这意味着 --update-baseline 必须在
`git add` 之后运行，而不是之前：ls-files 看不见未暂存的文件，如果先
生成 baseline 再暂存新文件，baseline 记录的是暂存前那棵树，暂存后
再跑门就会发现「新增」条目——即使内容从未变过。这个顺序坑已经咬过
两次（一次是本门自身的测试文件），下一个人不该再摔第三次。
"""
import argparse
import os
import re
import subprocess
import sys

# --- 规则数据 -------------------------------------------------------------

# D5 保留清单：第三方进程写入的入站契约 + 逐字送给 Anthropic 的值。
# 这些 CLAUDE_* 名字是 G2 的合法例外。
KEEP_CLAUDE_ENV = {
    "CLAUDE_AGENT_SDK_CLIENT_APP",
    "CLAUDE_AGENT_SDK_MCP_NO_PREFIX",
    "CLAUDE_AGENT_SDK_VERSION",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EXTRA_BODY",
    "CLAUDE_CODE_EXTRA_METADATA",
    "CLAUDE_CODE_OAUTH_TOKEN",
    # 下面十七个是 2026-09-16 逐个回 oracle 2.1.274 核过的。判据不是「二进制里
    # 出现了这个字符串」——那只证明它被写下过——而是**它被当成环境变量读出来**。
    # 上游有一张带类型的 env 注册表，键就是环境变量名，值是该名字的 schema
    # （`O.bool()` / `O.triBool()` / `O.str()` / `O.int()` / `O.enum([...])`），
    # 读取发生在一个按键取值的惰性 getter 里：
    #
    #     for(let[E,r] of Object.entries(t)){
    #       Object.defineProperty(_,E,{get:()=>{let n=process.env[E];
    #         if(n!==s)e=r.parse(n),s=n;return e}})}
    #
    # 所以「名字出现在这张表里」就等于「process.env 里的这个名字会被读」。
    # 十六个在表里（各自的 schema 记在下面），CLAUDE_CODE_COORDINATOR_EXTRA_TOOLS
    # 不在表里而是直接 `process.env.<NAME>` 读——两种都是被读取。
    #
    # 它们因此属于上面说的「第三方进程写入的入站契约」：从 claude-code 迁过来
    # 的用户会设这些名字，端口读同名才是对的，改名等于单方面断掉一条上游协议面。
    # ⛔ 往这里加名字前必须先在 oracle 里确认它真的被读，不要凭 `CLAUDE_` 前缀
    # 就放行——这份清单的价值全在于它是被核过的。
    "CLAUDE_CODE_BASH_EDIT_DIFF",                          # O.triBool()
    "CLAUDE_CODE_COORDINATOR_EXTRA_TOOLS",                 # process.env.<NAME>
    "CLAUDE_CODE_COORDINATOR_FORCE_WORKER_INHERIT_MODEL",  # O.bool()
    "CLAUDE_CODE_DISABLE_CRON",                            # O.bool()
    "CLAUDE_CODE_DISABLE_EXPLORE_INHERIT_CAP",             # O.bool()
    "CLAUDE_CODE_DISABLE_EXPLORE_PLAN_AGENTS",             # O.bool()
    "CLAUDE_CODE_EVAL_CONFINED",                           # O.bool()
    "CLAUDE_CODE_LOOP_KEEPALIVE",                          # O.triBool()
    "CLAUDE_CODE_LOOP_PERSISTENT",                         # O.bool()
    "CLAUDE_CODE_PLUGIN_DIR_WATCH",                        # O.triBool()
    "CLAUDE_CODE_SAFE_MODE",                               # O.bool()
    "CLAUDE_CODE_SESSION_KIND",                            # O.str()
    "CLAUDE_CODE_SIMPLE",                                  # O.bool()
    "CLAUDE_CODE_STOP_HOOK_BLOCK_CAP",                     # O.int()
    "CLAUDE_CODE_SUBAGENT_MODEL_FORCE",                    # O.bool()
    "CLAUDE_CODE_THRIFTY_SONIC",                           # O.triBool()
    "CLAUDE_JOB_DIR",                                      # O.str()
}

# L1 常量的当前值。G3 断言它们不出现在 branding 之外。
# Plan A 不改这些值 —— 门只是把现状冻成 baseline。
#
# 必须用带边界的正则而不是子串匹配：裸的 `".lingxi" in line` 会命中 132 处
# `.lingxi_home` 字段访问，把 G3 淹没成噪声。负向 lookahead 排除标识符字符、
# 点、连字符，于是 `.lingxi_home`、`com.lingxi.code`、`.lingxi-plugin`
# （单列）都不会误伤，而 `.lingxi/`、`.lingxi"`、`.lingxi'` 照常命中。
L1_PATTERNS = [
    (r"\.lingxi(?![A-Za-z0-9_.\-])", ".lingxi"),
    (r"\.lingxi\.json", ".lingxi.json"),
    (r"\.lingxi-plugin", ".lingxi-plugin"),
    (r"LINGXI\.local\.md", "LINGXI.local.md"),
    (r"LINGXI\.md", "LINGXI.md"),
    (r"LINGXI_CONFIG_DIR", "LINGXI_CONFIG_DIR"),
    (r"Application Support/LingXi", "MANAGED_DIR_MACOS"),
    (r"Program Files\\LingXi", "MANAGED_DIR_WINDOWS"),
    (r"/etc/lingxi", "/etc/lingxi"),
]
L1_COMPILED = [(re.compile(p), label) for p, label in L1_PATTERNS]

# G3 的两族伴生规则。两族都是**实测出来的盲区**，不是推测，证据在
# `GATE-BLIND-SPOT-case-variants.md`（大小写）与 Plan B 总纲 Q9 的裁定
# （连字符家族）。加它们的理由是同一个：**门的 baseline 就是 S3 的工作清单**，
# 门看不见的字符串没有人会被告知去改。
#
# 一、大小写伴生。上面九条 L1 一条都没有用 `re.IGNORECASE`，于是
# `LINGXI\.md` 不匹配 `lingxi.md`。最贵的一处是
# `permission/src/classifier.rs` 的 `is_self_modification_path`：它拿已经
# lowercase 过的路径比 `"lingxi.md"`，翻转后会**静默地不再把「改自己的记忆
# 文件」判成自我修改**（Task 7 已按 Ruling A 路由掉，这里留作动机记录）。
#
# ⛔ **不要改成给九条 L1 全体加 `re.IGNORECASE`。** 那会把 `crates/`、
# `lingxi-cli` 和归 L2 管的 `LingXi` 产品名全部卷进来。只给「换个大小写仍
# 指同一个东西」的那几个 needle 配伴生，实测全树只新增 21 行。
#
# 第三个元素是**规范拼写**：命中恰好等于它时跳过，因为那一处已经由
# 大小写敏感的那条报过了。没有这一步，每一处正常拼写都会被报两次。
L1_CASE_PATTERNS = [
    (r"\.lingxi(?![A-Za-z0-9_.\-])", ".lingxi (case)", ".lingxi"),
    (r"LINGXI\.local\.md", "LINGXI.local.md (case)", "LINGXI.local.md"),
    (r"LINGXI\.md", "LINGXI.md (case)", "LINGXI.md"),
    (r"LINGXI_CONFIG_DIR", "LINGXI_CONFIG_DIR (case)", "LINGXI_CONFIG_DIR"),
    (r"Application Support/LingXi", "MANAGED_DIR_MACOS (case)", "Application Support/LingXi"),
]

# 二、连字符家族。`\.lingxi(?![A-Za-z0-9_.\-])` 的 lookahead 故意排除后随
# 的连字符——那正是挡住 132 处 `.lingxi_home` 和 `com.lingxi.code` 的东西
# ——而唯一单列了自己 pattern 的连字符 needle 只有 `.lingxi-plugin`。剩下的
# `.lingxi-build-state`(46) `.lingxi-scratch`(7) `.lingxi-tmp`(3)
# `.lingxi-worktree-owner.json`(2) `.lingxi-dependency-ready`(2)
# `.lingxi-opened`(1) `.lingxi-home`(1) `.lingxi-edit-`(1)
# `.lingxi-canvas-surface`(1) 共 64 处，一条规则都没有在看。
#
# Q9 裁定 `branding` 日后要为这一族出常量（连同 `WORKTREE_PATH_SEGMENT`、
# `RESUME_MD_REPO_PATH`）；那件事 B-1 没有做，见 ledger。在那之前，这条
# 规则提供的是**可见性**：它们进 baseline，S3 就拿得到清单。
L1_HYPHEN_PATTERNS = [
    (r"\.lingxi-(?!plugin)[A-Za-z0-9][A-Za-z0-9_.\-]*", ".lingxi-*"),
]

# 三族合成一张表喂给 G3：(编译好的正则, 标签, 规范拼写或 None)。
L1_ALL_COMPILED = (
    [(re.compile(p), label, None) for p, label in L1_PATTERNS]
    + [(re.compile(p, re.IGNORECASE), label, canon) for p, label, canon in L1_CASE_PATTERNS]
    + [(re.compile(p), label, None) for p, label in L1_HYPHEN_PATTERNS]
)

# claude 家族的 needle。
#
# 本分支的动机有**两**类泄漏：60 处运行时 CLAUDE_* 环境变量读取（G2 管），
# 以及 77 处 live `.claude` 字符串字面量。第二类一度**没有任何规则在看**
# —— G1 的 needle 全是 LingXi 家族，G3 是 LingXi 侧的命名空间值，G4 只在
# 注释里开火。于是把 claude 家族接进 G1 的 needle 表。
#
# lookahead 里必须同时排除 `.` 和 `-`，否则会误伤三类**永远不能改名的协议
# 值**（实测计数，全在本仓库里）：
#   us.anthropic.claude-opus / anthropic.claude-3 / eu.anthropic.claude-sonnet
#       —— Bedrock / Vertex 的 wire model id
#   code.claude.com / platform.claude.com / status.claude.com / docs.claude.com
#       —— 官方域名
# `.claude.json`（oracle 的配置文件，对应 L1 的 `.lingxi.json`）会被这条
# lookahead 挡掉，所以它单列一条——与 L1 表里 `.lingxi` / `.lingxi.json`
# 两条并列是同一个理由。
# `CLAUDE.md` / `CLAUDE.local.md` 同理对应 L1 的 `LINGXI.md` /
# `LINGXI.local.md`。不用一条宽的 `CLAUDE` needle：那会把每一个
# `CLAUDE_CODE_*` 字面量、每一个 `claude-opus-*` model id 全部卷进来，
# 而它们分别归 G2 管、或者是不可改的协议值。
CLAUDE_NEEDLES = [
    r"\.claude(?![A-Za-z0-9_.\-])",
    r"\.claude\.json",
    r"CLAUDE\.local\.md",
    r"CLAUDE\.md",
]

# G7 —— Claude 侧 plugin 标识符**到达**本仓库（P0a.0）。
#
# 本分支（local-app plugin）引入的方向与 G3 相反：不是 LingXi 值漏出去，而是
# oracle 的 plugin 标识符进来 —— `.claude-plugin` 目录名、`${CLAUDE_PLUGIN_ROOT}`
# / `${CLAUDE_PLUGIN_DATA}` / `${CLAUDE_PLUGIN_OPTION_<KEY>}` 插值语法、
# `${CLAUDE_PROJECT_DIR}`。实测：改动前这四类全部 exit 0 静默（G1 的 needle 表
# 里没有它们，G2 只认 `env::var(...)` 的**调用**形状，G3 是 LingXi 侧的值）。
#
# ⚠️ **为什么不是把它们塞进 CLAUDE_NEEDLES/G1 就完事**——G1 的判据是「代码行的
# **字符串字面量内**出现 needle」。那个判据对本族是错的，因为这些标识符到达时
# 的主要形态根本不在引号里：
#     hooks/*.sh          `exec $CLAUDE_PLUGIN_ROOT/bin/run`      裸 shell 变量
#     skills/*/SKILL.md   `读 .claude-plugin/plugin.json`          markdown 正文
#     workflows/*.js      `process.env.CLAUDE_PLUGIN_ROOT`         成员访问
#     *.rs                `const CLAUDE_PLUGIN_MANIFEST_DIR`       裸标识符
# 全部实测确认过：只加进 G1 时，上面四种形状**一条都不报**，而带引号的
# `"${CLAUDE_PLUGIN_ROOT}"` 报。也就是说「往 plugins/ 里种一个带引号的字面量
# 看它变红」这个验收动作，验的是引号而不是 needle —— 而 plugin 生态里
# `${CLAUDE_PLUGIN_ROOT}` 恰恰主要出现在 hook 脚本和 SKILL.md 正文里。
# 所以本族自成一条规则：判据是**整行**（仍然跳过注释行，理由同 G1——oracle
# 出处引用只合法地存在于注释里，仓库里有大量 `/// claude CLAUDE_PLUGIN_OPTION_…`
# 这类正确的映射注释）。
#
#   `\.claude-plugin`      与 L1 的 `\.lingxi-plugin` 同构。**必须带前导点。**
#                          去掉点的 `claude-plugin` 会命中
#                          `anthropics/claude-plugins-official`（plugin_marketplace.rs
#                          的 4 处），那是上游 GitHub 仓库名 —— 与 Bedrock model id
#                          同类的不可改协议值。
#   `CLAUDE_PLUGIN_`       族 needle，裸前缀（与 CLIENT_NEEDLES 里的 `LINGXI_`
#                          同一种写法）。覆盖 CLAUDE_PLUGIN_ROOT / _DATA /
#                          _OPTION_<KEY> / 日后新增的任何变体，不必每加一个就
#                          回来改这份文件。
#   `CLAUDE_PROJECT_DIR`   精确字面量，插值模板里的第四个 oracle 占位符。
#
# 与 KEEP_CLAUDE_ENV 核对过——而且**核对本身是可执行的**（见下面的
# `_ARRIVING_VS_KEEP` 判据，不是一句注释）：七个保留名没有一个会被本族命中，
# G7 不会推翻 G2 已裁决的合法例外。
# 不加进 L1_PATTERNS/G3：G3 管的是 LingXi 值反向出现在 branding 之外，方向相反。
ARRIVING_PLUGIN_NEEDLES = [
    r"\.claude-plugin",
    r"CLAUDE_PLUGIN_",
    r"CLAUDE_PROJECT_DIR",
]
ARRIVING_COMPILED = [(re.compile(p), p) for p in ARRIVING_PLUGIN_NEEDLES]

# G2 的保留清单与 G7 的 needle 必须不相交，否则一个名字会同时「被 G2 明确
# 放行」和「被 G7 报红」。这不是注释，是启动时就跑的判据 —— 有人往
# KEEP_CLAUDE_ENV 里加一个 `CLAUDE_PLUGIN_*` 名字时，门会当场炸而不是
# 沉默地自相矛盾。
_ARRIVING_VS_KEEP = sorted(
    name for name in KEEP_CLAUDE_ENV
    if any(rx.search(name) for rx, _ in ARRIVING_COMPILED)
)
if _ARRIVING_VS_KEEP:
    # 用 raise 而不是 `assert`：`python3 -O` 会把 assert 整条剥掉，而一道门的
    # 自洽性判据不该随解释器开关消失。
    raise SystemExit(
        "check_brand_leaks: KEEP_CLAUDE_ENV 与 ARRIVING_PLUGIN_NEEDLES 相交于 "
        f"{_ARRIVING_VS_KEEP} —— G2 放行的名字会被 G7 报红，先裁决归谁管"
    )

# G1 的 needle 集合按区域不同。一张全局表会立刻淹没在按 spec §1.1 判定为
# 保留的东西上（clients/ 的 336 处 LingXi、1335 处 com.lingxi、483 处 灵犀），
# 而把它们全塞进豁免清单会让清单失去信号价值 —— 一份两千条的豁免清单等于
# 没有清单。
FULL_NEEDLES = [r"\.lingxi", r"LINGXI", r"LingXi", r"Lingxi", r"灵犀"] + CLAUDE_NEEDLES

# clients/ 只查命名空间面。com.lingxi.* / LingxiCode* /
# LingXiAccessibilityService / 灵犀 按 spec §1.1 保留，不进 needle 表也就
# 不需要豁免条目。
#
# lookahead 里必须含 `.`：否则 `com.lingxi.code` 会被当成 dot-dir 命中，
# 而它是 spec §1.1 明确保留的 Android package / iOS bundle id。
CLIENT_NEEDLES = [
    r"\.lingxi(?![A-Za-z0-9_.\-])",
    r"LINGXI_",
    r"X-LingXi-Ide-Authorization",
] + CLAUDE_NEEDLES

AREAS = [
    # (路径前缀, needle 正则列表)
    ("crates/", FULL_NEEDLES),
    ("clients/", CLIENT_NEEDLES),
    (".github/", FULL_NEEDLES),
    ("scripts/", FULL_NEEDLES),
    ("tools/", FULL_NEEDLES),
    ("skills/", FULL_NEEDLES),
    # spec §7.2 点名了 npm/。今天这个前缀下还没有 tracked 文件，列在这里是
    # 为了让它一出现就被扫到，而不是悄悄落进 fail-open 的默认分支。
    ("npm/", FULL_NEEDLES),
]

# AREAS 是**允许表**，而允许表的默认分支决定它 fail-open 还是 fail-closed。
# 原来的 `needles_for` 对没列出的前缀返回 None，G1 就整块跳过、没有任何
# 信号——一个新顶层目录（或一个被挪走的旧目录）会让 G1 无声地失去覆盖。
# 默认改成全集：宁可多冻几条 baseline，也不要一片静默的盲区。
DEFAULT_NEEDLES = FULL_NEEDLES

# 完全不扫的区域。
#
# `docs/` 与 `.omo/plans/`（两处：仓库根与 crates/ 下）是**历史归档**：
# 它们记录的是当时写下的事实，改写它们等于伪造档案，所以它们贡献的条目
# 永远不可能被合法地清掉——一条永远不会归零的 baseline 条目只有噪声价值。
# `skills/*.md` 反过来**要扫**：skills/create-local-app/SKILL.md 指示 agent
# 去写 `LINGXI.md`，那是一份活的契约，不是档案。
EXCLUDED_PREFIXES = (
    "third_party/",
    "docs/",
    "crates/docs/",
    ".omo/",
    "crates/.omo/",
)

# 本门自己的产出物 —— 显式列出具体路径，不用 glob：这张表本身就是一个
# 正确性声明，必须小而可审计。
#   check_brand_leaks.py         规则引擎自身。它里面的每个品牌 token 都是
#                                 needle 的定义，不是泄漏。
#   brand_leak_baseline.txt      生成物，其内容按定义就是一份品牌 token
#                                 命中列表 —— 扫描它会让自身成为下一轮的
#                                 命中源，而重写它又会移动那些命中的行号，
#                                 自扫描没有不动点，门永远无法收敛到绿。
#   brand_frozen_identities.txt  frozen 清单，逐字写出标识符正是它的本职。
#   accepted_divergences.json    同一形状的第四份：它逐条记录「上游用
#                                 CLAUDE.md / ~/.claude，本端口用自己的」这类
#                                 决定，而要写清楚一条 divergence 就必须把两边
#                                 的拼写都写出来。这是 needle 的定义侧，不是
#                                 泄漏侧；扫描它只会把这份清单变成下一轮的命中源。
SELF_ARTIFACTS = frozenset({
    "scripts/check_brand_leaks.py",
    "scripts/brand_leak_baseline.txt",
    "scripts/brand_frozen_identities.txt",
    "crates/test-harness/src/parity/fixtures/accepted_divergences.json",
})

# 每种文件的行注释前导符。G1 只看代码行 —— oracle 出处引用只合法地存在于
# 注释里。
COMMENT_PREFIXES = {
    ".rs": ("//",),
    ".kt": ("//",),
    ".swift": ("//",),
    ".ts": ("//",),
    ".tsx": ("//",),
    ".js": ("//",),
    ".jsx": ("//",),
    ".mjs": ("//",),
    ".py": ("#",),
    ".sh": ("#",),
    ".yml": ("#",),
    ".yaml": ("#",),
    ".toml": ("#",),
}

# 环境变量读取的形状。用于 G2。
#
# **这个仓库主要不用 `std::env::var` 直接读**，而是穿过一层薄包装
# （`env_truthy` / `is_env_truthy` / `env_on` / `on` / `truthy` / `is_set` …）。
# 只认标准库形状的版本对这些包装完全是盲的，而下一波 parity 移植最可能
# 用的恰恰就是这个惯用法。下面这张名单是把
#     git grep -hoP '[A-Za-z_][A-Za-z0-9_:.]*\s*[(\[]\s*"CLAUDE_[A-Z0-9_]+"'
# 的结果逐个核对**函数定义**得到的（`git grep -P`，绝不用 `-E` 配 `\b`
# ——ERE 不实现 `\b` 且静默返回零行、退出码 0）。核对结果里被**排除**的：
#   std::env::set_var / remove_var / EnvGuard::set / EnvGuard::unset  写，不是读
#   mk / launch_env_key_allowed / cfg.contains                        测试脚手架
#   .env("CLAUDE_CODE_MCP_SERVER_NAME", …)                            往子进程
#       **写**环境（mcp/src/headers_helper.rs:99-100）。它是出站契约，与
#       KEEP 清单里的 CLAUDE_CODE_ENTRYPOINT 同族，不是本规则说的「读取点」。
#
# 交替分支必须**长的在前**，否则 `env_truthy` 会遮住 `is_env_truthy`
# ——两者都能匹配时以最左起点为准，而 `is_env_truthy` 的起点更靠左，
# 所以实际不会漏；把长的写在前面是为了让命中的 needle 名字可读。
_ENV_HELPERS = (
    r"env::var(?:_os)?|std::env::var(?:_os)?|getenv"
    r"|ProcessInfo\.processInfo\.environment"
    r"|is_env_truthy|cache_env_truthy|env_var_truthy|env_truthy"
    r"|env_present|env_on|truthy|is_set|on"
)
ENV_READ = re.compile(
    r'(?:' + _ENV_HELPERS + r')'
    r'\s*[(\[]\s*"(CLAUDE_[A-Z0-9_]+)"'
)

# 第二种形状：CLAUDE_ 名字在**第二个**实参上。
# `dual_env(lingxi, claude)`（http-client/src/tls_config.rs:121）先试 LINGXI_
# 名再回落到 CLAUDE_CODE_ 名，四个 TLS 变量全走这条路——第一实参位置的
# 正则对它是盲的。
ENV_READ_SECOND_ARG = re.compile(
    r'dual_env\s*\(\s*"[^"]*"\s*,\s*"(CLAUDE_[A-Z0-9_]+)"'
)

# G4 —— 假 oracle 引用。
#
# 判据必须窄。宽版本（「注释里同时出现 claude-code 和 LINGXI_」）会命中 30 处，
# 而其中绝大多数是**正确的**映射注释，形如
#     // `LINGXI_SESSION_KIND == "bg"` (claude-code `CLAUDE_CODE_SESSION_KIND`);
# 那是把港口名和 oracle 名并列写出，完全正确。
#
# B18 的真实形状是 LINGXI_ token 出现在**被当作 oracle 源码呈现的片段内部**，
# 即 JS 的 `process.env.X` / `env.X` 表达式里 —— claude-code 是 TypeScript，
# 它的源码不可能出现 LINGXI_。已实测：这条窄规则命中 21 行 / 15 文件，与独立
# 勘察给出的 B18 计数逐一吻合，且命中已知样例
# platforms/posix/src/secure_storage/helpers.rs:63。
FAKE_ORACLE_CITATION = re.compile(r"(?:process\.env\.|env\.)(LINGXI_[A-Z0-9_]+)")

# 字符串字面量。**必须同时认单引号** —— TypeScript 里普遍用单引号，而
# clients/shared/src/lockfile.ts:35 的 `join(homedir(), '.lingxi', 'bridge')`
# 正是这个门最该抓到的那个缺陷（B16）。只认双引号的版本会漏掉它。
STRING_LITERAL = re.compile(r'"([^"\\]|\\.)*"' r"|'([^'\\]|\\.)*'")

# branding crate 的**源码**。G1 与 G3 都对它豁免：这些常量的**定义处**就在
# 这里，一个 crate 因为持有自己拥有的常量而被判泄漏是没有道理的。
# （在此之前 G3 豁免而 G1 不豁免，是一处没有理由的不对称。）
#
# 前缀到 `src/` 为止，**不是** `branding/`：豁免的理由只覆盖 `src/lib.rs`
# —— 常量的定义处。写成 `crates/branding/` 会顺带把
# `branding/tests/brand_gate_test.rs` 也移出扫描面，而那是一份普通测试文件，
# 没有任何理由不被扫。豁免范围超出它自己写下的理由，正是本分支反复遇到的
# 「看起来没问题、实际什么都没证明」的形状。
BRANDING_PREFIX = "crates/branding/src/"


# baseline 文件顶部的说明块。`#` 开头的行在比对时被跳过（见 main），所以
# 这段文字不参与集合比较——改动它不会让门报红，也不需要重新生成 baseline。
BASELINE_HEADER = """\
# 品牌泄漏 baseline —— 由 `scripts/check_brand_leaks.py --update-baseline` 生成。
# 手改没有意义：下一次 --update-baseline 会整体重写本文件。
# 以 `#` 开头的行与空行不参与比对。
#
# 键是 `(rule, path, needle)`，**不含行号**（理由见引擎的模块 docstring）。
#
# ⚠️ 一条条目断言的是「该文件里该 needle **至少命中一次**」，而不是你以为的
# 那一次命中。已知的一处不对应：
#
#     G2\tcrates/core/src/host/uds_inbox.rs\tCLAUDE_CODE_TMPDIR
#
# 这一条钉住的是**测试**里 uds_inbox.rs:1116 的
# `std::env::var_os("CLAUDE_CODE_TMPDIR")` 存档/还原，**不是** uds_inbox.rs:123
# 那处真正的读取。真读取写成
# `for key in ["XDG_RUNTIME_DIR", "CLAUDE_CODE_TMPDIR", "LINGXI_TMPDIR"]`，
# 属于「名字先进数组、再在别处迭代读」的形态，`ENV_READ` 的判据对它是盲的
# （spec §7.3 把这个形态列为已知残余）。后果：**删掉那个测试会让这条条目一起
# 消失，那处真读取就变得完全不可见**——而门只会打印「baseline 少了一条」，
# 一个看起来像「修好了」的信号。改 uds_inbox.rs 的测试时，请一并确认 :123
# 那处读取仍被某条规则看着。
"""


def text_lines(text):
    """按 `\n` 切分，且**只**按 `\n` 切分。

    ⛔ 不要换回 `str.splitlines()`。它额外在 `\x0b \x0c \x1c \x1d \x1e \x85
    \u2028 \u2029` 上切分，而这些字符在本仓库的注释里真实存在（描述 oracle
    正则的地方）。后果有两个，第二个是 fail-open：

    1. 该字符之后每一行报出的行号都偏大。S2 的分类表因此把 `traits/` 与
       `permission/` 的若干行记错，评审当作「行号漂移」重新定位了一遍。
    2. 含该字符的字符串字面量会被切成两半，两半都不是完整的引号跨度，
       `STRING_LITERAL` 一个都匹配不到 —— **G1 对这一行彻底静默**。G3 不
       要求字面量跨度所以仍开火，但 G1 独有的 needle（`LingXi`、`Lingxi`、
       `X-LingXi-Ide-Authorization`）就此无人接管。

    钉住它的是 `--selftest` 的 `u2028` 用例。（此处原先引的 Rust 测试
    `gate_counts_lines_and_keeps_g1_across_unicode_line_separators` 在本仓库
    里同样不存在 —— 见模块 docstring 里的同一条更正。）
    """
    lines = text.split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    return [l[:-1] if l.endswith("\r") else l for l in lines]


def tracked_files(root):
    """root 下被 git 跟踪的文件。fail-closed：零文件即错误。"""
    out = subprocess.run(
        ["git", "-C", root, "ls-files"],
        capture_output=True, text=True, check=True,
    ).stdout
    out = text_lines(out)
    files = [
        f for f in out
        if not f.startswith(EXCLUDED_PREFIXES) and f not in SELF_ARTIFACTS
    ]
    if not files:
        raise SystemExit("check_brand_leaks: git ls-files returned nothing — refusing to report clean")
    return files


def is_comment_line(path, line):
    ext = os.path.splitext(path)[1]
    prefixes = COMMENT_PREFIXES.get(ext)
    if not prefixes:
        return False
    stripped = line.lstrip()
    return any(stripped.startswith(p) for p in prefixes)


def needles_for(path):
    for prefix, pats in AREAS:
        if path.startswith(prefix):
            return pats
    return DEFAULT_NEEDLES


def parse_frozen(frozen_path):
    """读 L3 冻结清单，返回 [(lineno, path, literal), ...]。

    格式：`<path>:<literal>  # 理由`

    两种「空豁免」必须显式拒绝，否则一条豁免会**永远为真**而没人发现：
      1. 没有冒号（或冒号后为空）=> literal 是 ""，而 `"" in text` 恒为真，
         这条豁免既不豁免任何东西、G5 也永远不会报它腐烂。
      2. 注释切分必须用 `" #"` 而不是裸 `#`：被冻结的字面量本身可能含 `#`
         （颜色值、URL fragment、shell 注释样本），裸 `#` 会把它拦腰截断，
         截断后的前缀照样能 `in` 命中，于是又是一条永远为真的豁免。
    """
    entries = []
    if not os.path.exists(frozen_path):
        return entries
    with open(frozen_path, encoding="utf-8") as fh:
        for lineno, raw in enumerate(fh, start=1):
            stripped = raw.strip()
            if not stripped or stripped.startswith("#"):
                continue
            entry = stripped.split(" #", 1)[0].strip()
            if not entry:
                continue
            path, sep, literal = entry.partition(":")
            if not sep or not literal:
                raise SystemExit(
                    f"check_brand_leaks: {frozen_path}:{lineno}: malformed frozen entry "
                    f"{stripped!r} — expected `<path>:<literal>  # reason`. "
                    "An entry with an empty literal is vacuously satisfied "
                    "(`\"\" in text` is always true) and would never be reported as rotten."
                )
            entries.append((lineno, path, literal))
    return entries


def exemptions_by_path(entries):
    """把冻结条目折成 {path: [literal, ...]}，供 G1/G3 的豁免路径用。"""
    by_path = {}
    for _, path, literal in entries:
        by_path.setdefault(path, []).append(literal)
    return by_path


def span_is_exempt(line, start, end, literals):
    """命中区间 [start, end) 是否落在本行某个冻结字面量的出现范围内。

    spec §7.1 把 G1 定义成「代码行字符串字面量里出现品牌 token，**且不在 L3
    豁免清单**」。豁免必须按**位置**判定而不是「本行含某个冻结字面量」：后者
    会让一行里恰好挨着的另一处真泄漏也被顺带放行。
    """
    for lit in literals:
        pos = line.find(lit)
        while pos != -1:
            if pos <= start and end <= pos + len(lit):
                return True
            pos = line.find(lit, pos + 1)
    return False


def scan(root, exempt=None):
    """产出 (rule_id, path, lineno, needle) 四元组的集合。"""
    exempt = exempt or {}
    findings = set()
    for path in tracked_files(root):
        full = os.path.join(root, path)
        try:
            with open(full, encoding="utf-8") as fh:
                lines = text_lines(fh.read())
        except (UnicodeDecodeError, FileNotFoundError, IsADirectoryError):
            continue

        pats = needles_for(path)
        in_branding = path.startswith(BRANDING_PREFIX)
        frozen_here = exempt.get(path, ())

        for i, line in enumerate(lines, start=1):
            comment = is_comment_line(path, line)

            # G1 — 代码行的字符串字面量里的品牌 token
            if pats and not comment and not in_branding:
                for lit in STRING_LITERAL.finditer(line):
                    text = lit.group(0)
                    for pat in pats:
                        # **每个匹配都要看，不能只看第一个。** 见 span_is_exempt
                        # 的 docstring：豁免按位置判定，正是为了不让同一行里挨着
                        # 的另一处真泄漏被顺带放行。可只把第一个匹配递给它，就把
                        # 这个保证原样还了回去——一个落在冻结字面量里的首匹配会
                        # 让整条 pattern 对这一行静默失效。
                        for m in re.finditer(pat, text):
                            if frozen_here and span_is_exempt(
                                line, lit.start() + m.start(), lit.start() + m.end(), frozen_here
                            ):
                                continue
                            findings.add(("G1", path, i, pat))
                            break

            # G2 — 非保留的 CLAUDE_* 环境变量读取
            for rx in (ENV_READ, ENV_READ_SECOND_ARG):
                for m in rx.finditer(line):
                    name = m.group(1)
                    if name not in KEEP_CLAUDE_ENV:
                        findings.add(("G2", path, i, name))

            # G3 — L1 常量值出现在 branding 之外
            if not in_branding and not comment:
                for rx, label, canonical in L1_ALL_COMPILED:
                    # 同 G1：遍历全部匹配，首匹配被豁免不等于本行干净。
                    for m in rx.finditer(line):
                        # 大小写伴生规则：命中就是规范拼写时跳过，那一处已由
                        # 大小写敏感的那条报过了。
                        if canonical is not None and m.group(0) == canonical:
                            continue
                        if frozen_here and span_is_exempt(line, m.start(), m.end(), frozen_here):
                            continue
                        findings.add(("G3", path, i, label))
                        break

            # G7 — Claude 侧 plugin 标识符到达（整行判据，见 ARRIVING_PLUGIN_NEEDLES）
            if not comment:
                for rx, needle in ARRIVING_COMPILED:
                    # 同 G1/G3：遍历全部匹配，首匹配被豁免不等于本行干净。
                    for m in rx.finditer(line):
                        if frozen_here and span_is_exempt(line, m.start(), m.end(), frozen_here):
                            continue
                        findings.add(("G7", path, i, needle))
                        break

            # G4 — 假 oracle 引用（LINGXI_ 出现在被呈现为 TS 源码的表达式里）
            if comment:
                fake = FAKE_ORACLE_CITATION.search(line)
                if fake:
                    findings.add(("G4", path, i, fake.group(1)))

    return findings


BRANDING_LIB = "crates/branding/src/lib.rs"
BRANDING_CONST = re.compile(r"^pub const ([A-Z][A-Z0-9_]*): &str")
# 故意不属于本产品命名空间的常量。
G6_ALLOWLIST = {"LEGACY_GLOBAL_CONFIG_FILE"}


def check_namespace_coverage(root):
    """G6 —— branding 里存在未被 NAMESPACE_VALUES 覆盖的 pub const。

    这是 NAMESPACE_VALUES 参数化唯一堵不住的洞，而且没有任何 Rust 测试能堵住它：
    在同一个 crate 里把列表写两遍是自指的。所以由门来做名字层面的对账。
    """
    findings = set()
    full = os.path.join(root, BRANDING_LIB)
    with open(full, encoding="utf-8") as fh:
        lines = text_lines(fh.read())

    declared, listed, in_list = [], set(), False
    for i, line in enumerate(lines, start=1):
        m = BRANDING_CONST.match(line)
        if m:
            declared.append((m.group(1), i))
        if line.startswith("pub const NAMESPACE_VALUES"):
            in_list = True
            continue
        if in_list:
            if line.strip() == "];":
                in_list = False
            else:
                listed.add(line.strip().rstrip(","))

    for name, lineno in declared:
        if name in G6_ALLOWLIST or name in listed:
            continue
        findings.add(("G6", BRANDING_LIB, lineno, f"{name} not in NAMESPACE_VALUES"))
    return findings


def check_frozen(root, frozen_path, entries):
    """G5 — 冻结清单里目标已消失的条目。"""
    findings = set()
    if not entries:
        return findings
    frozen_rel = os.path.relpath(frozen_path, root)
    for lineno, path, literal in entries:
        full = os.path.join(root, path)
        if not os.path.exists(full):
            findings.add(("G5", frozen_rel, lineno, f"missing file {path}"))
            continue
        with open(full, encoding="utf-8", errors="replace") as target:
            if literal not in target.read():
                findings.add(("G5", frozen_rel, lineno, f"literal gone: {literal}"))
    return findings


def fmt_list(findings):
    """`--list` 的人类可读形态：带行号。"""
    return ["\t".join((r, p, str(n), needle)) for r, p, n, needle in sorted(findings)]


def fmt_key(findings):
    """baseline 的键：文件粒度，**不含行号**（理由见模块 docstring）。"""
    return sorted({"\t".join((r, p, needle)) for r, p, _, needle in findings})


def line_index(findings):
    """(rule, path, needle) 键 -> 命中过的行号排序列表。

    baseline 的键必须不含行号（见模块 docstring 的理由），但 §19.3 要求
    **报红时**输出点名文件与行号，给人一个可以直接跳转的位置。用同一批
    `findings`（4 元组，含行号）反查，不需要重新扫描。一个键可能对应多个
    行——同一 needle 在同一文件命中多次很常见（实测：G7 的 CLAUDE_PLUGIN_ 在
    plugin/src/manager.rs 命中 1113/1115/1132/1138 四行）——全部列出而不是只取
    第一个，
    否则会看起来像「只有一处」而误导排查范围。
    """
    idx = {}
    for r, p, ln, needle in findings:
        idx.setdefault("\t".join((r, p, needle)), []).append(ln)
    for key in idx:
        idx[key].sort()
    return idx


# --- 自测：合成树上的每规则正对照 ---------------------------------------
#
# 为什么必须是**合成树**而不是「在真仓库上跑一遍看它非空」：后者的绿分不清
# 「规则会开火」和「真仓库恰好含有这类内容」两件事，规则退化成零匹配时，只要
# baseline 同步更新过一次就再也看不出来。合成树把每条规则的输入自己写死，
# 于是「绿」只可能来自规则真的开火。
#
# 每个用例同时声明**必须出现**和**必须不出现**。只有前者的用例会被一条过宽的
# 规则轻易满足 —— 本仓库反复吃亏的形状正是「探针没跑也算绿」。
SELFTEST_FILES = {
    # (path, content)
    "crates/st/g1.rs": 'fn f() { let p = ".claude"; }\n',
    "crates/st/g2.rs": (
        'fn f() { let a = std::env::var("CLAUDE_CODE_UNKEPT_NAME");\n'
        '         let b = std::env::var("CLAUDE_CODE_ENTRYPOINT"); }\n'
    ),
    "crates/st/g3.rs": 'fn f() { let p = ".lingxi/state"; }\n',
    "crates/st/g4.rs": '// upstream: process.env.LINGXI_FAKE_CITATION\n',
    "crates/branding/src/lib.rs": (
        'pub const ST_UNLISTED: &str = "x";\n'
        'pub const NAMESPACE_VALUES: &[&str] = &[\n];\n'
    ),
    # G7 的四种**不带引号**的到达形态。这四条正是 P0a.0 写作阶段漏掉的洞：
    # 只把 needle 加进 G1（判据 = 字符串字面量内）时，它们一条都不报。
    "crates/st/hook.sh": '#!/bin/sh\nexec $CLAUDE_PLUGIN_ROOT/bin/run\n',
    "crates/st/SKILL.md": 'Read .claude-plugin/plugin.json first.\n',
    "crates/st/wf.js": 'const r = process.env.CLAUDE_PROJECT_DIR;\n',
    "crates/st/ident.rs": 'const CLAUDE_PLUGIN_MANIFEST_DIR: &str = "manifest";\n',
    # G7 的两个**必须静默**的对照。
    "crates/st/neg_comment.rs": '// oracle spells it CLAUDE_PLUGIN_ROOT / CLAUDE_PROJECT_DIR\n',
    "crates/st/neg_reponame.rs": 'fn f() { let r = "anthropics/claude-plugins-official"; }\n',
    # text_lines 的 U+2028 用例：若换回 str.splitlines()，这一行会被切成两半,
    # 后面那行的行号会偏移 1，且 G1 需要的完整引号跨度会断掉。
    "crates/st/u2028.rs": (
        'fn a() { let s = "seg sep"; }\n'
        'fn b() { let p = ".claude"; }\n'
    ),
}

SELFTEST_CASES = [
    # (用例名, 必须出现的 (rule, path, needle-substring[, 行号]), 必须不出现的同形项)
    # 第四个元素是**可选的行号**。凡是规则真正守的东西是位置而不是存在性的
    # 用例，必须填它——见 U+2028 那一条的说明。
    ("G1 fires on a .claude string literal",
     [("G1", "crates/st/g1.rs", r"\.claude(?!")], []),
    ("G2 fires on a non-KEEP CLAUDE_* read, and KEEP is honoured",
     [("G2", "crates/st/g2.rs", "CLAUDE_CODE_UNKEPT_NAME")],
     [("G2", "crates/st/g2.rs", "CLAUDE_CODE_ENTRYPOINT")]),
    ("G3 fires on a LingXi namespace value outside branding",
     [("G3", "crates/st/g3.rs", ".lingxi")], []),
    ("G4 fires on a fake oracle citation",
     [("G4", "crates/st/g4.rs", "LINGXI_FAKE_CITATION")], []),
    ("G6 fires on a branding const missing from NAMESPACE_VALUES",
     [("G6", "crates/branding/src/lib.rs", "ST_UNLISTED")], []),
    ("G7 fires on a BARE $CLAUDE_PLUGIN_ROOT in a hook script",
     [("G7", "crates/st/hook.sh", "CLAUDE_PLUGIN_")], []),
    ("G7 fires on .claude-plugin in markdown prose (no quotes)",
     [("G7", "crates/st/SKILL.md", r"\.claude-plugin")], []),
    ("G7 fires on process.env.CLAUDE_PROJECT_DIR in a workflow",
     [("G7", "crates/st/wf.js", "CLAUDE_PROJECT_DIR")], []),
    ("G7 fires on a bare CLAUDE_PLUGIN_* Rust identifier",
     [("G7", "crates/st/ident.rs", "CLAUDE_PLUGIN_")], []),
    ("G7 stays silent on comment-line oracle citations",
     [], [("G7", "crates/st/neg_comment.rs", "")]),
    ("G7 stays silent on anthropics/claude-plugins-official (no leading dot)",
     [], [("G7", "crates/st/neg_reponame.rs", "")]),
    # ⚠️ 这一条**必须钉行号**。只断言「命中存在」的版本对 splitlines() 的回退
    # 完全不敏感：切成三行之后命中仍然存在，只是行号从 2 变成 3——实测确认过
    # 那个版本的 mutant 全绿。行号才是这条规则真正守的东西。
    ("text_lines does not split on U+2028 (G1 still spans the quotes, line 2)",
     [("G1", "crates/st/u2028.rs", r"\.claude(?!", 2)], []),
]


def _selftest_matches(findings, rule, path, needle_sub, lineno=None):
    return [
        f for f in findings
        if f[0] == rule and f[1] == path and needle_sub in f[3]
        and (lineno is None or f[2] == lineno)
    ]


def run_selftest():
    """在合成树上跑每规则正对照 + 负对照。返回 0/1。"""
    import shutil
    import tempfile

    tmp = tempfile.mkdtemp(prefix="brandgate-selftest-")
    try:
        for rel, content in SELFTEST_FILES.items():
            full = os.path.join(tmp, rel)
            os.makedirs(os.path.dirname(full), exist_ok=True)
            with open(full, "w", encoding="utf-8") as fh:
                fh.write(content)
        frozen = os.path.join(tmp, "frozen.txt")
        with open(frozen, "w", encoding="utf-8") as fh:
            fh.write("crates/st/vanished.rs:.lingxi  # 目标文件不存在 => G5\n")

        for cmd in (["init", "-q"], ["add", "-A"]):
            subprocess.run(["git", "-C", tmp] + cmd, check=True,
                           capture_output=True, text=True)

        entries = parse_frozen(frozen)
        findings = scan(tmp, exemptions_by_path(entries))
        findings |= check_frozen(tmp, frozen, entries)
        findings |= check_namespace_coverage(tmp)

        cases = list(SELFTEST_CASES) + [
            ("G5 fires on a frozen entry whose target file is gone",
             [("G5", "frozen.txt", "missing file")], []),
        ]

        failures = []
        # 「探针根本没跑」的守卫：合成树按构造必然产出命中，一个空集合意味着
        # 枚举/读取那一层坏了，而不是「没有违规」。
        if not findings:
            failures.append("selftest produced ZERO findings — the scanner never fired")

        for name, must, must_not in cases:
            for spec in must:
                rule, path, needle = spec[0], spec[1], spec[2]
                lineno = spec[3] if len(spec) > 3 else None
                if not _selftest_matches(findings, rule, path, needle, lineno):
                    at = f"{path}:{lineno}" if lineno is not None else path
                    near = sorted(_selftest_matches(findings, rule, path, needle))
                    failures.append(
                        f"{name}: expected {rule} at {at} matching {needle!r}, got none"
                        + (f" (same needle seen at lines {[f[2] for f in near]})" if near else "")
                    )
            for spec in must_not:
                rule, path, needle = spec[0], spec[1], spec[2]
                lineno = spec[3] if len(spec) > 3 else None
                hit = _selftest_matches(findings, rule, path, needle, lineno)
                if hit:
                    failures.append(f"{name}: expected NO {rule} at {path}, got {sorted(hit)}")
            if not failures or not any(f.startswith(name) for f in failures):
                print(f"  ok   {name}")

        for f in failures:
            print(f"  FAIL {f}")
        if failures:
            print(f"selftest: {len(failures)} failure(s)")
            return 1
        print(f"selftest: {len(cases)} cases ok ({len(findings)} findings on the synthetic tree)")
        return 0
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root")
    ap.add_argument("--baseline")
    ap.add_argument("--frozen")
    ap.add_argument("--update-baseline", action="store_true")
    ap.add_argument("--list", action="store_true")
    ap.add_argument("--selftest", action="store_true")
    args = ap.parse_args()

    if args.selftest:
        return run_selftest()

    here = os.path.dirname(os.path.abspath(__file__))
    root = os.path.abspath(args.root if args.root else os.path.join(here, ".."))
    baseline_path = args.baseline or os.path.join(here, "brand_leak_baseline.txt")
    frozen_path = args.frozen or os.path.join(here, "brand_frozen_identities.txt")

    # `--root` 只在自测里出现（合成树）。它和 `--update-baseline` 一起用时，
    # 输出路径过去仍然取自 `__file__`——于是
    # `--root /tmp/scratch --update-baseline` 会拿一棵人造树的扫描结果**覆盖
    # 真 baseline**。拒绝这个组合，除非调用方显式说了 baseline 该写去哪。
    if args.update_baseline and args.root is not None and args.baseline is None:
        ap.error(
            "--update-baseline with an explicit --root would overwrite the real "
            "baseline from a foreign tree; pass --baseline explicitly to say "
            "where the regenerated baseline should go"
        )

    frozen_entries = parse_frozen(frozen_path)
    findings = scan(root, exemptions_by_path(frozen_entries))
    findings |= check_frozen(root, frozen_path, frozen_entries)
    findings |= check_namespace_coverage(root)

    if args.list:
        print("\n".join(fmt_list(findings)))
        return 0

    current = fmt_key(findings)

    if args.update_baseline:
        with open(baseline_path, "w", encoding="utf-8") as fh:
            fh.write(BASELINE_HEADER)
            fh.write("\n".join(current) + "\n")
        print(f"baseline updated: {len(current)} entries")
        return 0

    if not os.path.exists(baseline_path):
        raise SystemExit(f"check_brand_leaks: baseline missing at {baseline_path}")

    # `#` 开头的是 BASELINE_HEADER 的说明文字，不是条目。条目键的第一段永远是
    # `G1`..`G6`，所以按前缀过滤不会误吃任何真条目。
    with open(baseline_path, encoding="utf-8") as fh:
        expected = [
            l for l in text_lines(fh.read()) if l.strip() and not l.startswith("#")
        ]

    added = sorted(set(current) - set(expected))
    removed = sorted(set(expected) - set(current))

    if added or removed:
        if added:
            locations = line_index(findings)
            print(f"NEW brand leaks ({len(added)}):")
            for line in added:
                rule, path, needle = line.split("\t", 2)
                lns = locations.get(line)
                loc = f"{path}:" + ",".join(str(n) for n in lns) if lns else path
                print(f"  + {rule}\t{loc}\t{needle}")
        if removed:
            print(f"baseline entries no longer found ({len(removed)}):")
            print("  这可能是修好了（好事，跑 --update-baseline），")
            print("  也可能是扫描器退化了（坏事）。确认是哪一种再更新 baseline。")
            for line in removed:
                print(f"  - {line}")
        return 1

    print(f"OK: brand leak set matches baseline ({len(current)} known entries)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
