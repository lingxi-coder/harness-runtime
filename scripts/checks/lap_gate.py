#!/usr/bin/env python3
"""Local App plugin 项目的门契约引擎（驱动在 scripts/checks/lap-gate.sh）。

与 scripts/checks/check_deps.py / scripts/checks/check_brand_leaks.py 同形状：只用 python3，
规则是模块常量，有 --list 模式，main() 经 sys.exit 返回。

存在的理由，一句话：**写代码的 agent、修代码的 agent、验证的 agent 必须调用
同一份字节**。把判据写进 prompt 里，三方各自转述一遍，转述之间的分歧就藏进了
「我跑过了，绿的」。判据落在脚本里，转述就没有生存空间。

本引擎的每一条断言都遵守一条规则：**失败信息必须点名那个具体的东西**。
「测试失败」不算，「测试 foo::bar 在 baseline 里 passed=114，本次运行里这个
二进制根本没有 test result 行」才算。退出码只带一个 bit，本仓库已经被
「绿得毫无意义」和「红得毫无意义」各咬过一次。
"""

import argparse
import json
import os
import re
import shutil
import tempfile
import subprocess
import sys
from pathlib import Path

# --- 规则数据 -------------------------------------------------------------

# cargo test 的输出形状。这些正则是本引擎唯一的输入契约。
RE_RUNNING = re.compile(r"^\s+Running (?P<desc>.+?) \(target/[^)]*/deps/(?P<bin>[A-Za-z0-9_]+)-[0-9a-f]+\)\s*$")
RE_DOCTEST = re.compile(r"^\s+Doc-tests (?P<crate>[A-Za-z0-9_-]+)\s*$")
RE_RESULT = re.compile(
    r"^test result: (?P<verdict>ok|FAILED)\. (?P<passed>\d+) passed; (?P<failed>\d+) failed; "
    r"(?P<ignored>\d+) ignored"
)
RE_TESTLINE = re.compile(r"^test (?P<name>\S+) \.\.\. (?P<outcome>ok|FAILED|ignored)")
RE_FAILURES_HDR = re.compile(r"^failures:$")
RE_FAILURE_ITEM = re.compile(r"^\s{4}(?P<name>[A-Za-z0-9_:]+)\s*$")

# precheck 的硬阈值。低于这个数字，本仓库的构建不报 ENOSPC，而是伪装成随机的
# `could not compile` / `failed to write query cache`——报错形状和 diff 对不上，
# 会让一整轮排查跑去追一个不存在的代码问题。
MIN_FREE_GIB = 8

# 需要 repeatRuns 的 crate。bridge-server 的 e2e 会踩进程全局静态，
# LOOP_KA_TEST_SERIAL 是进程内 Mutex 而不是环境变量：它在一个二进制里串行化
# 线程，跨二进制、以及在 nextest 下什么都不做。所以它的判据是**多次运行的
# 计数一致**，不是一次运行的退出码。
REPEAT_RUN_CRATES = {"bridge-server"}


# --- 解析 -----------------------------------------------------------------


def parse_run(text):
    """把一份 cargo test 输出解析成 {binaries, redlist, result_lines}。"""
    binaries = {}
    redlist = []
    order = []
    cur = None
    in_failures = False
    for line in text.splitlines():
        m = RE_RUNNING.match(line)
        if m:
            cur = "%s|%s" % (m.group("bin"), m.group("desc"))
            order.append(cur)
            in_failures = False
            continue
        m = RE_DOCTEST.match(line)
        if m:
            cur = "%s|Doc-tests" % m.group("crate")
            order.append(cur)
            in_failures = False
            continue
        m = RE_RESULT.match(line)
        if m and cur is not None:
            binaries[cur] = {
                "passed": int(m.group("passed")),
                "failed": int(m.group("failed")),
                "ignored": int(m.group("ignored")),
            }
            in_failures = False
            continue
        if RE_FAILURES_HDR.match(line):
            in_failures = True
            continue
        if in_failures:
            m = RE_FAILURE_ITEM.match(line)
            if m:
                redlist.append("%s::%s" % (cur or "?", m.group("name")))
            elif line.strip():
                in_failures = False
            continue
        m = RE_TESTLINE.match(line)
        if m and m.group("outcome") == "FAILED":
            redlist.append("%s::%s" % (cur or "?", m.group("name")))
    return {
        "binaries": binaries,
        "redlist": sorted(set(redlist)),
        "started": order,
    }


def _resolved(path, what):
    """路径一律相对**调用者的 cwd**解析。

    驱动脚本会 `cd` 到 crates/，所以一个相对路径在这里的含义和调用者
    敲进去时的含义并不相同——这正是一条「文件不存在」的报错能把人送去查
    错方向的地方。失败信息必须同时点名原样和解析后的绝对路径。"""
    p = Path(os.environ.get("LAP_GATE_CWD", ".")) / path
    if not p.is_file():
        fail(
            "%s missing: %r resolved to %s — a gate cannot read evidence that was never written"
            % (what, path, p.resolve())
        )
    return p


def target_to_package(repo):
    """target 名 -> package 名。

    cargo 的输出里,一个测试二进制是按 **target** 命名的
    (`commands_test|tests/commands_test.rs`),而任务的范围是按 **package** 划的
    (`-p client`)。两者不是一回事:client 的集成测试 target 叫
    `commands_test`、`events_test`……名字里没有 package 的影子。拿 package 名去
    子串匹配 binary key,只会匹配到 lib unittests,把同一个 package 的十几个集成
    测试二进制判成「范围外」。所以这张表必须问 cargo 要,不能猜。"""
    out = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        capture_output=True, text=True, cwd=repo,
    )
    if out.returncode != 0:
        fail("cargo metadata failed, cannot attribute test binaries to packages: %s" % out.stderr.strip()[:200])
    table = {}
    for pkg in json.loads(out.stdout)["packages"]:
        for t in pkg["targets"]:
            table[t["name"].replace("-", "_")] = pkg["name"]
    return table


def load_run(path):
    p = _resolved(path, "run file")
    text = p.read_text(encoding="utf-8", errors="replace")
    if not text.strip():
        fail("run file %s is EMPTY — zero bytes is not a passing run" % path)
    return parse_run(text)


def fail(msg):
    print("LAP-GATE FAIL: %s" % msg, file=sys.stderr)
    raise SystemExit(1)


def ok(msg):
    print("LAP-GATE OK: %s" % msg)


# --- 子命令 ---------------------------------------------------------------


def cmd_parse(args):
    run = load_run(args.run)
    if not run["started"]:
        fail(
            "%s names no test binary at all (no 'Running …(target/…/deps/…)' line) — "
            "the run did not execute tests, whatever it exited with" % args.run
        )
    run["binaryPackage"] = {}
    if args.no_attribute:
        ok("parsed %s -> %s (%d binaries, attribution SKIPPED: --only will be unavailable)"
           % (args.run, args.out, len(run["binaries"])))
        Path(os.environ.get("LAP_GATE_CWD", ".")).joinpath(args.out).write_text(
            json.dumps(run, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        return 0
    table = target_to_package(args.workspace_dir)
    unattributed = []
    for key in list(run["binaries"]) + run["started"]:
        target = key.split("|", 1)[0]
        pkg = table.get(target)
        if pkg is None:
            unattributed.append(key)
        else:
            run["binaryPackage"][key] = pkg
    if unattributed:
        fail(
            "%d test binary/binaries could not be attributed to a package: %s — "
            "an unattributable binary silently escapes every per-task scope filter"
            % (len(unattributed), ", ".join(sorted(set(unattributed))[:6]))
        )
    Path(os.environ.get("LAP_GATE_CWD", ".")) .joinpath(args.out).write_text(json.dumps(run, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    ok(
        "parsed %s -> %s (%d binaries started, %d reported a result, %d red)"
        % (args.run, args.out, len(run["started"]), len(run["binaries"]), len(run["redlist"]))
    )
    return 0


def cmd_precheck(args):
    problems = []
    dirty = subprocess.run(
        ["git", "status", "--porcelain", "--untracked-files=no"],
        capture_output=True, text=True, cwd=args.repo,
    ).stdout.strip()
    if dirty:
        problems.append(
            "working tree is dirty at a task boundary; these tracked files are modified:\n    "
            + "\n    ".join(dirty.splitlines())
        )
    free_gib = shutil.disk_usage(args.repo).free / (1024 ** 3)
    if free_gib < MIN_FREE_GIB:
        problems.append(
            "%.1f GiB free, need %d GiB — below this the build fails as a bogus "
            "'could not compile', not as ENOSPC" % (free_gib, MIN_FREE_GIB)
        )
    if args.baseline and not Path(args.baseline).is_file():
        problems.append("baseline missing at %s — green is meaningless against an unmeasured start" % args.baseline)
    for var in ("LINGXI_REUSE_GENERATED_BINDINGS", "LINGXI_DEVICE_ONLY", "BLESS"):
        if os.environ.get(var):
            problems.append(
                "%s=%s is set — it defeats the gate it appears to satisfy "
                "(stale bindings / skipped device leg / re-baselined contract)" % (var, os.environ[var])
            )
    if problems:
        fail("precheck:\n  - " + "\n  - ".join(problems))
    ok("precheck: clean tree, %.1f GiB free, no defeat vars set" % free_gib)
    return 0


def cmd_planted(args):
    """判据 1：种下的失败必须是**行为**失败，且输出点名了那个东西。"""
    p = _resolved(args.planted, "planted evidence (a gate you never broke is not a gate)")
    text = p.read_text(encoding="utf-8", errors="replace")
    if not RE_FAILURES_HDR.search(text) and "\nfailures:\n" not in text:
        hint = ""
        if re.search(r"^error(\[E\d+\])?:", text, re.M):
            hint = " — the file contains a rustc 'error:' line, so this is a COMPILE break, not a planted behavioural failure"
        fail("%s has no 'failures:' block%s" % (args.planted, hint))
    missing = [s for s in args.must_name if s not in text]
    if missing:
        fail(
            "%s never names %s — the run went red, but not demonstrably at the thing this task claims to gate"
            % (args.planted, ", ".join(repr(s) for s in missing))
        )
    ok("planted failure is behavioural and names %s" % ", ".join(repr(s) for s in args.must_name))
    return 0


def cmd_green(args):
    """判据 3/4/5/6：全部相对 baseline，全部点名。"""
    bp = _resolved(args.baseline, "baseline")
    try:
        base = json.loads(bp.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        fail("baseline %s is not valid JSON (%s) — an unparseable baseline compares to nothing" % (bp, exc))
    if not base.get("binaries"):
        fail("baseline %s records ZERO test binaries — every later 'green' would be vacuous" % bp)
    run = load_run(args.run)
    removed = set(args.tests_removed)
    problems = []

    # 每个任务只跑自己的 crate,所以判据 3 必须只对**本次范围内**的 baseline 二进制生效,
    # 否则每一个任务都会因为「没跑别人的二进制」而红。--only 就是这个范围。
    #
    # ⚠️ 范围必须**收窄 baseline**,不能改成「只检查本次运行里出现过的二进制」——
    # 后者会让被截断的二进制自动退出检查,判据 3 就恒真了。
    if args.only:
        attrib = base.get("binaryPackage") or {}
        if not attrib:
            fail("baseline has no binaryPackage attribution — regenerate it with `lap-gate.sh parse`")
        unknown = [o for o in args.only if o not in set(attrib.values())]
        if unknown:
            fail(
                "--only names %s, which no baseline binary belongs to — packages the baseline knows: %s"
                % (", ".join(repr(u) for u in unknown), ", ".join(sorted(set(attrib.values()))))
            )
        # 一个任务**新增**测试二进制是正常的,而 baseline 的归属表是在它存在之前
        # 生成的。所以运行里出现的、baseline 不认识的二进制,要现问 cargo,而不是
        # 当成「范围外」。否则每一个新增测试目标的任务都会因为自己交付的东西而红。
        #
        # ⚠️ 只补 **run** 侧的归属,不补 baseline 侧:baseline 的二进制集合是判据 3
        # 的分母,现场扩充它会让「baseline 有而本次没跑」这条恒真。
        live = None
        for k in run["binaries"]:
            if k not in attrib:
                if live is None:
                    live = target_to_package(args.workspace_dir)
                pkg = live.get(k.split("|", 1)[0])
                if pkg is None:
                    problems.append(
                        "test binary %r is in neither the baseline attribution nor cargo metadata — "
                        "an unattributable binary silently escapes the scope filter" % k
                    )
                else:
                    attrib[k] = pkg
        kept = {k: v for k, v in base["binaries"].items() if attrib.get(k) in args.only}
        if not kept:
            fail(
                "--only %s matches ZERO baseline binaries — the scope filter selected nothing, so every "
                "later assertion would be vacuous. Baseline knows: %s"
                % (args.only, ", ".join(sorted(base["binaries"])[:8]) + " ...")
            )
        base = {
            "binaries": kept,
            "redlist": [r for r in base["redlist"] if attrib.get(r.split("::", 1)[0]) in args.only],
        }
        stray = [k for k in run["binaries"] if attrib.get(k) not in args.only]
        if stray:
            problems.append(
                "run contains %d binary/binaries outside --only %s: %s — the run and the scope disagree"
                % (len(stray), args.only, ", ".join(sorted(stray)[:5]))
            )

    # 3. baseline 记过的每个二进制都必须有 test result 行。
    #    一个 SIGABRT 会截断二进制且**不留 failures: 指名**，本仓库出过
    #    328 个测试只跑了 124 个、输出形状完全成功的情况。
    for name, b in sorted(base["binaries"].items()):
        if name in run["binaries"]:
            continue
        if name in run["started"]:
            problems.append(
                "binary %r STARTED but produced no 'test result:' line (baseline had %d passed) — "
                "truncated test binary, not a pass" % (name, b["passed"])
            )
        else:
            problems.append(
                "binary %r had %d passed in baseline and is MISSING from this run — "
                "it was never started" % (name, b["passed"])
            )

    # 4. 每个二进制的 passed 计数不得下降。
    for name, b in sorted(base["binaries"].items()):
        r = run["binaries"].get(name)
        if r is None or name in removed:
            continue
        if r["passed"] < b["passed"]:
            problems.append(
                "binary %r passed %d, baseline %d — %d test(s) stopped passing "
                "(not in --tests-removed)" % (name, r["passed"], b["passed"], b["passed"] - r["passed"])
            )

    # 5. redlist 只能收缩。新红就是失败，哪怕「跟我无关」。
    new_red = sorted(set(run["redlist"]) - set(base["redlist"]))
    if new_red:
        problems.append(
            "%d test(s) are newly red vs baseline:\n      %s" % (len(new_red), "\n      ".join(new_red))
        )

    # 6. 每个新增测试必须在输出里按名字出现过。
    text = _resolved(args.run, "run file").read_text(encoding="utf-8", errors="replace")
    for t in args.added_test:
        if not re.search(r"^test .*\b%s\b.* \.\.\. ok" % re.escape(t), text, re.M):
            problems.append(
                "added test %r never appears in %s as 'test … %s … ok' — "
                "it was not compiled, not selected, or is #[ignore]d" % (t, args.run, t)
            )

    if problems:
        fail("green:\n  - " + "\n  - ".join(problems))
    total = sum(b["passed"] for b in run["binaries"].values())
    ok(
        "green: %d binaries all reported, %d passed (baseline %d), redlist %d <= %d"
        % (
            len(run["binaries"]),
            total,
            sum(b["passed"] for b in base["binaries"].values()),
            len(run["redlist"]),
            len(base["redlist"]),
        )
    )
    return 0


def cmd_fmt(args):
    """判据 9：任务交付的文件必须是 rustfmt 干净的。

    这条是 P-1.0 落地之后补的:它带着 20 处 rustfmt diff 进了仓库,而我那八条判据
    里没有一条看格式——计划把 `cargo fmt --all -- --check` 放在 phase 边界。
    只在边界上查,等于让第一个撞到边界的人去重排**别人的**文件,而那时已经分不清
    是谁弄脏的了。

    ⚠️ 必须区分「本任务弄脏的」和「本来就脏的」。本分支上 `cargo fmt --all --check`
    对一批不属于当前任务的文件已经是红的;不区分的话,任何碰到既有脏文件的任务
    都永远过不了,而正确的做法是让它**看见**自己没弄脏它。
    做法:同时对工作区版本和 HEAD 版本跑 rustfmt --check。

    ⛔ 不要用 `cargo fmt --all` 来修。它会重排整个工作区,包括**别的写者尚未提交的
    文件**;本会话已经因为在别人正在编辑的文件上做写-还原而弄崩过一次构建。
    只对自己拥有的文件跑 `rustfmt`。"""
    repo = Path(args.repo)
    if not args.file:
        fail("fmt: no --file given — a formatting check over zero files is vacuously clean")
    rustfmt = shutil.which("rustfmt")
    if rustfmt is None:
        fail("fmt: rustfmt not on PATH — cannot verify formatting, and an unverifiable check must not pass")
    introduced, inherited, checked = [], [], 0
    for rel in args.file:
        path = repo / rel
        if not path.is_file():
            fail("fmt: %s does not exist (resolved %s) — a task cannot deliver a file that is not there"
                 % (rel, path))
        checked += 1
        now = subprocess.run([rustfmt, "--edition", "2021", "--check", str(path)],
                             capture_output=True, text=True)
        if now.returncode == 0 and not now.stdout.strip():
            continue
        blob = subprocess.run(["git", "show", "HEAD:%s" % rel], capture_output=True, text=True, cwd=repo)
        if blob.returncode != 0:
            introduced.append((rel, "new file"))
            continue
        with tempfile.NamedTemporaryFile("w", suffix=".rs", delete=False) as fh:
            fh.write(blob.stdout)
            tmp = fh.name
        was = subprocess.run([rustfmt, "--edition", "2021", "--check", tmp], capture_output=True, text=True)
        os.unlink(tmp)
        if was.returncode == 0 and not was.stdout.strip():
            introduced.append((rel, "clean at HEAD, unclean now"))
        else:
            inherited.append(rel)
    problems = []
    for rel, why in introduced:
        problems.append("%s is not rustfmt-clean (%s) — this task introduced it" % (rel, why))
    for rel in inherited:
        problems.append(
            "%s is not rustfmt-clean, and was ALREADY unclean at HEAD — inherited, but this task owns "
            "the file, so clean it here rather than leaving it for whoever hits the phase boundary. "
            "⛔ run rustfmt on THIS FILE ONLY, never `cargo fmt --all`: other writers have uncommitted "
            "files in this checkout" % rel
        )
    if problems:
        fail("fmt:\n  - " + "\n  - ".join(problems))
    ok("fmt: %d owned file(s) rustfmt-clean" % checked)
    return 0


def cmd_owned(args):
    """判据 7：这次任务的提交只能碰它自己声明拥有的路径。"""
    out = subprocess.run(
        ["git", "diff", "--name-only", args.range],
        capture_output=True, text=True, cwd=args.repo,
    )
    if out.returncode != 0:
        fail("git diff --name-only %s failed: %s" % (args.range, out.stderr.strip()))
    touched = [p for p in out.stdout.splitlines() if p.strip()]
    if not touched:
        fail("range %s touches ZERO files — an empty commit range is not a completed task" % args.range)
    strays = [p for p in touched if not any(p == o or p.startswith(o.rstrip("/") + "/") for o in args.owns)]
    if strays:
        fail(
            "%d path(s) outside this task's owned set:\n      %s\n    owned set was:\n      %s"
            % (len(strays), "\n      ".join(strays), "\n      ".join(args.owns))
        )
    ok("ownership: %d file(s), all inside the declared owned set" % len(touched))
    return 0


def cmd_identical(args):
    """判据 8：repeatRuns 的多次运行必须导出**逐字节相同**的计数。"""
    if len(args.runs) < 2:
        fail("--identical needs at least 2 runs; got %d" % len(args.runs))
    derived = []
    for r in args.runs:
        run = load_run(r)
        derived.append(json.dumps({"binaries": run["binaries"], "redlist": run["redlist"]}, sort_keys=True))
    if len(set(derived)) != 1:
        lines = []
        first = json.loads(derived[0])
        for i, d in enumerate(derived[1:], start=1):
            cur = json.loads(d)
            for name in sorted(set(first["binaries"]) | set(cur["binaries"])):
                a = first["binaries"].get(name)
                b = cur["binaries"].get(name)
                if a != b:
                    lines.append("run[0] %r=%s vs run[%d] %r=%s" % (name, a, i, name, b))
            for t in sorted(set(cur["redlist"]) ^ set(first["redlist"])):
                lines.append("redlist differs at %s" % t)
        fail(
            "%d runs are NOT count-identical — this suite shares process globals, so one green run "
            "proves nothing:\n      %s" % (len(args.runs), "\n      ".join(lines[:20]))
        )
    ok("identical: %d runs derived byte-identical counts" % len(args.runs))
    return 0


def cmd_tasks(args):
    """加载时校验任务清单。**没有种雷方案的任务在这里就被拒绝**——
    一个你打不破的门不是门,而「我给它写了个门」是本仓库最常见的假交付形状。"""
    root = Path(os.environ.get("LAP_GATE_CWD", ".")) / args.dir
    files = sorted(root.glob("tasks-phase-*.json"))
    if not files:
        fail("no tasks-phase-*.json under %s — refusing to report a clean task set from an empty enumeration" % root.resolve())
    problems = []
    seen = {}
    for f in files:
        try:
            doc = json.loads(f.read_text(encoding="utf-8"))
        except json.JSONDecodeError as exc:
            problems.append("%s is not valid JSON: %s" % (f.name, exc)); continue
        tasks = doc.get("tasks")
        if not tasks:
            problems.append("%s declares ZERO tasks" % f.name); continue
        for t in tasks:
            tid = t.get("id") or "<no id>"
            where = "%s/%s" % (f.name, tid)
            if tid in seen:
                problems.append("%s: duplicate task id, already defined in %s" % (where, seen[tid]))
            seen[tid] = f.name
            pf = t.get("plantedFailure") or {}
            if not pf.get("edit"):
                problems.append("%s: no plantedFailure.edit — a gate you cannot break is not a gate" % where)
            if not pf.get("mustNameInOutput"):
                problems.append("%s: plantedFailure names nothing the output must contain — "
                                "red-somewhere is not evidence of red-at-this-thing" % where)
            if not t.get("owns"):
                problems.append("%s: no owned paths — criterion 7 cannot be evaluated" % where)
            if not t.get("gate"):
                problems.append("%s: no gate reference — it is not traceable to a section 19 row" % where)
            for c in t.get("crates", []):
                if c in REPEAT_RUN_CRATES and t.get("repeatRuns", 1) < 3:
                    problems.append("%s: owns %s but repeatRuns=%s — that suite shares process globals, "
                                    "so one green run proves nothing" % (where, c, t.get("repeatRuns", 1)))
    # 依赖必须解析得到,否则调度器会静默跳过一个任务。
    for f in files:
        try:
            doc = json.loads(f.read_text(encoding="utf-8"))
        except json.JSONDecodeError:
            continue
        for t in doc.get("tasks", []):
            for d in t.get("deps", []):
                if d not in seen:
                    problems.append("%s/%s: depends on %r, which no descriptor defines" % (f.name, t.get("id"), d))
    if problems:
        fail("task descriptors:\n  - " + "\n  - ".join(problems))
    ok("task descriptors: %d file(s), %d task(s), every one has a planted failure that names something"
       % (len(files), len(seen)))
    return 0


# --- 自检 -----------------------------------------------------------------

# 本引擎自己的「种雷」证据。一个从没被看着变红过的门，和没有门是同一件事——
# 而本引擎是**其余每一个门的判据来源**，所以它的诚实性必须可复现，不能只活在
# 某一次会话的记录里。
#
# 每一行是 (子命令参数, 期望, 这一行在防什么)。期望是 "red" 的那些，全部取自
# 本仓库真实吃过的亏：SIGABRT 截断的二进制、静默变少的计数、拿编译错误当种雷
# 证据、红在了别的地方、新增测试根本没跑、共享进程全局的套件只绿一次。
SELFTEST = [
    (["green", "--run", "truncated.txt", "--baseline", "base.json"], "red",
     "SIGABRT 截断的二进制：启动了、没有 test result 行、零个报告出来的失败"),
    (["green", "--run", "shrunk.txt", "--baseline", "base.json"], "red",
     "两个二进制都 ok，但总数悄悄少了一个"),
    (["green", "--run", "newred.txt", "--baseline", "base.json"], "red",
     "别处新红。「跟我无关」不是理由"),
    (["green", "--run", "base.txt", "--baseline", "base.json",
      "--added-test", "brand_gate_rejects_claude_plugin"], "red",
     "声称新增的测试根本没出现在输出里"),
    (["planted", "--planted", "compilebreak.txt", "--must-name", "manifest::parses"], "red",
     "拿编译错误冒充种雷证据"),
    (["planted", "--planted", "planted_ok.txt",
      "--must-name", "workflow_dir_scan_rejects_mjs_near_miss"], "red",
     "确实红了，但没红在它声称守的那个东西上"),
    (["identical", "--runs", "base.txt", "shrunk.txt"], "red",
     "共享进程全局的套件：两次运行计数不一致"),
    (["green", "--run", "base.txt", "--baseline", "base.json"], "green",
     "对照：run 对自己的 baseline"),
    (["planted", "--planted", "planted_ok.txt", "--must-name", "manifest::parses"], "green",
     "对照：真红在了声称的那个东西上"),
    (["identical", "--runs", "base.txt", "base.txt", "base.txt"], "green",
     "对照：自己和自己计数一致"),
]


def cmd_selftest(args):
    import tempfile
    fixtures = Path(__file__).resolve().parents[1] / "tests/fixtures/lap_gate"
    if not fixtures.is_dir():
        fail("fixture dir missing at %s — the self-test cannot prove anything without its planted cases" % fixtures)
    engine = str(Path(__file__).resolve())
    bad = []
    with tempfile.TemporaryDirectory() as tmp:
        for f in fixtures.glob("*.txt"):
            shutil.copy(f, Path(tmp) / f.name)
        r = subprocess.run(
            [sys.executable, engine, "parse", "--run", "base.txt", "--out", "base.json", "--no-attribute"],
            capture_output=True, text=True, env={**os.environ, "LAP_GATE_CWD": tmp},
        )
        if r.returncode != 0:
            fail("self-test could not build its own baseline: %s" % r.stderr.strip())
        for argv, expect, why in SELFTEST:
            r = subprocess.run(
                [sys.executable, engine] + argv,
                capture_output=True, text=True, env={**os.environ, "LAP_GATE_CWD": tmp},
            )
            got = "green" if r.returncode == 0 else "red"
            mark = "ok  " if got == expect else "BAD "
            if got != expect:
                bad.append("%s: expected %s, got %s — %s" % (" ".join(argv), expect, got, why))
            print("  %s[%-5s] %s" % (mark, expect, why))
    if bad:
        fail("self-test: %d case(s) behaved wrongly:\n  - %s" % (len(bad), "\n  - ".join(bad)))
    ok("self-test: %d planted cases, every one behaved as declared" % len(SELFTEST))
    return 0


def cmd_list(args):
    print("lap-gate subcommands and the criterion each enforces:")
    print("  parse      --run F --out J          turn a cargo test log into a baseline/run JSON")
    print("  precheck   [--baseline J]           clean tree, >= %d GiB free, no defeat env vars" % MIN_FREE_GIB)
    print("  planted    --planted F --must-name  (1) behavioural failure whose output NAMES the thing")
    print("  green      --run F --baseline J     (3) every baseline binary reported")
    print("                                      (4) per-binary passed count non-decreasing")
    print("                                      (5) redlist(run) subset of redlist(baseline)")
    print("                                      (6) every added test appears as '... ok'")
    print("  fmt        --file F --file F        (9) owned files are rustfmt-clean")
    print("  owned      --range R --owns P...    (7) commits touch only declared paths")
    print("  identical  --runs F F F             (8) count identity across runs")
    print("  tasks      --dir D                     reject any task with no planted failure")
    print("  selftest                            run the engine's own planted cases (10, both directions)")
    print("crates requiring repeatRuns: %s" % ", ".join(sorted(REPEAT_RUN_CRATES)))
    return 0


def main():
    ap = argparse.ArgumentParser(prog="lap-gate")
    ap.add_argument("--repo", default=str(Path(__file__).resolve().parents[2]),
                    help="git root (for git status / git diff)")
    # cargo 的工作区根是 crates/,不是 git 根。两者不同,而 `cargo metadata`
    # 在 git 根跑会以 "could not find Cargo.toml" 失败——一条把排查引向错误方向的报错。
    ap.add_argument("--workspace-dir", default=str(Path(__file__).resolve().parents[2]),
                    help="cargo workspace root (for cargo metadata)")
    sub = ap.add_subparsers(dest="cmd")

    p = sub.add_parser("parse"); p.add_argument("--run", required=True); p.add_argument("--out", required=True)
    p.add_argument("--no-attribute", action="store_true",
                   help="skip cargo-metadata package attribution (fixtures / offline)")
    p.set_defaults(fn=cmd_parse)

    p = sub.add_parser("precheck"); p.add_argument("--baseline")
    p.set_defaults(fn=cmd_precheck)

    p = sub.add_parser("planted"); p.add_argument("--planted", required=True)
    p.add_argument("--must-name", action="append", default=[])
    p.set_defaults(fn=cmd_planted)

    p = sub.add_parser("green"); p.add_argument("--run", required=True); p.add_argument("--baseline", required=True)
    p.add_argument("--tests-removed", action="append", default=[])
    p.add_argument("--added-test", action="append", default=[])
    p.add_argument("--only", action="append", default=[],
                   help="restrict the BASELINE to binaries belonging to this cargo package")
    # (repeatable)
    p.set_defaults(fn=cmd_green)

    p = sub.add_parser("fmt"); p.add_argument("--file", action="append", default=[])
    p.set_defaults(fn=cmd_fmt)

    p = sub.add_parser("owned"); p.add_argument("--range", required=True)
    p.add_argument("--owns", action="append", default=[], required=True)
    p.set_defaults(fn=cmd_owned)

    p = sub.add_parser("identical"); p.add_argument("--runs", nargs="+", required=True)
    p.set_defaults(fn=cmd_identical)

    p = sub.add_parser("tasks"); p.add_argument("--dir", default="docs/local-apps/harness")
    p.set_defaults(fn=cmd_tasks)

    p = sub.add_parser("selftest"); p.set_defaults(fn=cmd_selftest)

    p = sub.add_parser("list"); p.set_defaults(fn=cmd_list)

    args = ap.parse_args()
    if not getattr(args, "fn", None):
        ap.print_help()
        return 2
    return args.fn(args)


if __name__ == "__main__":
    sys.exit(main())
