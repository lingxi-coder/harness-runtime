#!/usr/bin/env python3
"""Compare production tool template bytes with the artifact-derived 2.1.319 oracle."""
import difflib
import json
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE = json.loads((ROOT / "scripts/tests/fixtures/tool_prompts_319.json").read_text())


def strings(source):
    pattern = r'r(?P<hashes>#{0,10})"[\s\S]*?"(?P=hashes)|"(?:[^"\\]|\\[\s\S])*?"'
    for match in re.finditer(pattern, source):
        literal = match.group()
        if literal.startswith("r"):
            yield literal[literal.index('"') + 1:literal.rindex('"')]
            continue
        literal = re.sub(r"\\\n\s*", "", literal)
        literal = re.sub(r"\\u\{([0-9a-fA-F]+)\}", lambda m: chr(int(m[1], 16)), literal)
        literal = literal.replace("\n", "\\n").replace("\r", "\\r")
        try:
            yield json.loads(literal)
        except (ValueError, TypeError):
            continue


def source(path):
    path = ROOT / path
    text = path.read_text()
    # This is only a production-source reader. Expand the same static UTF-8
    # files Rust's include_str! reads; upstream oracle bytes stay independent.
    return re.sub(
        r'include_str!\(\s*"([^"\n]+)"\s*\)',
        lambda match: json.dumps((path.parent / match[1]).read_text(), ensure_ascii=False),
        text,
    )


def constant(path, name):
    text = source(path)
    start = re.search(r"\bconst " + name + r"\s*:\s*&str\s*=", text)
    assert start, name
    return next(strings(text[start.end():]))


def normalize(tool, output):
    replacements = {
        "Read": [("This tool allows Claude Code to read images (eg PNG, JPG, etc). When reading an image file the contents are presented visually as Claude Code is a multimodal LLM.",
                  "This tool reads images (eg PNG, JPG, etc). When reading an image file, the contents are presented visually to the selected model.")],
        "Grep": [("as a Bash command", "through a registered shell tool"), ("via Bash", "via a registered shell tool"),
                 ("file links in Claude Code.", "file links in LingXi.")],
        "WebSearch": [(" URLs. US-only.", " URLs."), ("Allows Claude to search", "Allows the assistant to search"),
                      ("beyond Claude's knowledge cutoff", "beyond the model's knowledge cutoff"),
                      ("Domain filtering is supported to include or block specific websites", "Domain filtering can include or block specific websites when supported by the provider"),
                      ("Web search is only available in the US", "Unsupported providers or search options return an error")],
        "WebFetch": [("curl via Bash", "curl via a registered shell tool"),
                     ("For GitHub URLs, prefer using the gh CLI via Bash instead (e.g., gh pr view, gh issue view, gh api).",
                      "For GitHub URLs, prefer an authenticated GitHub tool; when available, this can be an MCP integration or the gh CLI through a registered shell tool.")],
    }
    for old, new in replacements.get(tool, []):
        output = output.replace(old, new)
    return output


def production(tool, lean):
    file_root = "crates/tools/file/src/"
    constants = {
        "Read": (file_root + "read.rs", "READ_PROMPT_LONG", "READ_PROMPT_SHORT"),
        "Write": (file_root + "write.rs", "WRITE_PROMPT_LONG", "WRITE_PROMPT_SHORT"),
        "Edit": (file_root + "edit.rs", "EDIT_DESCRIPTION", "EDIT_PROMPT_SHORT"),
        "TodoWrite": ("crates/tools/task/src/todo_write.rs", "TODO_WRITE_PROMPT_FULL", "TODO_WRITE_PROMPT_SIMPLE"),
    }
    if tool in constants:
        path, long_name, short_name = constants[tool]
        return constant(path, short_name if lean else long_name)
    if tool == "Glob":
        path = file_root + "glob.rs"
        if lean:
            return constant(path, "GLOB_PROMPT_SHORT")
        return constant(path, "GLOB_DESCRIPTION_BASE") + constant(path, "GLOB_DESCRIPTION_AGENT_BULLET")
    if tool == "Grep":
        path = file_root + "grep.rs"
        if lean:
            return constant(path, "GREP_PROMPT_SHORT")
        return "".join(constant(path, name) for name in ["GREP_DESCRIPTION_HEAD", "GREP_DESCRIPTION_AGENT_BULLET", "GREP_DESCRIPTION_TAIL"])
    if tool == "WebSearch":
        text = source("crates/tools/web/src/web_search.rs")
        name = "web_search_description_concise" if lean else "web_search_description"
        start = text.index("fn " + name + "(")
        raw = next(value for value in strings(text[start:]) if "Search the web." in value or "- Allows the assistant" in value)
        return raw.replace("{month_year}", "November 2026")
    if tool == "WebFetch":
        text = source("crates/tools/web/src/web_fetch.rs")
        if lean:
            return next(value for value in strings(text) if value.startswith("Fetches a URL,")).replace("{artifact_exception}", "")
        description = next(value for value in strings(text) if value.startswith("\n- Fetches content from a specified URL"))
        prefix = next(value for value in strings(text) if value.startswith("IMPORTANT: WebFetch WILL FAIL"))
        return prefix.replace("{artifact_bullet}", "").replace("{description}", description)
    raise AssertionError(tool)


checked = 0
for case in FIXTURE["cases"]:
    if case["tool"] == "Agent":
        continue
    expected = normalize(case["tool"], case["output"])
    actual = production(case["tool"], case["lean"])
    if actual != expected:
        diff = "".join(difflib.unified_diff(expected.splitlines(True), actual.splitlines(True), fromfile="native", tofile="production"))
        raise AssertionError(f"{case['tool']} lean={case['lean']} differs:\n{diff}")
    checked += 1

agent_case = next(case for case in FIXTURE["cases"] if case["tool"] == "Agent" and case["lean"] and not case["fork"])
agent_guidance = agent_case["output"].split("\n\n## When to use\n\n", 1)[1].split("\n\n- The agent", 1)[0]
assert source("crates/tools/agent/src/templates/when_to_use_319.md").rstrip("\n") == agent_guidance
bash_source = source("crates/tools/shell/src/prompt.rs")
for case in FIXTURE["bashCwdCases"]:
    assert case["output"] in list(strings(bash_source)), case
for case in FIXTURE["rewriteCases"]:
    actual = constant(f"crates/tools/file/src/{case['tool'].lower()}.rs", case["tool"].upper() + "_PROMPT_SHORT_REWRITE")
    assert actual == normalize(case["tool"], case["output"]), case["tool"]
for case in FIXTURE["taskCases"]:
    name = {"TaskGet":"TASK_GET_PROMPT", "TaskUpdate":"TASK_UPDATE_PROMPT", "TaskCreate":"TASK_CREATE_PROMPT", "TaskList":"TASK_LIST_PROMPT"}[case["tool"]]
    if case.get("swarms"):
        name += "_SWARM"
    actual = constant("crates/tools/task/src/task.rs", name)
    assert actual == case["output"], "".join(difflib.unified_diff(case["output"].splitlines(True),actual.splitlines(True),fromfile=case["tool"],tofile=name))
for case in FIXTURE["otherToolCases"]:
    if case["tool"] == "TaskStop":
        actual = constant("crates/tools/task/src/task.rs", "TASK_STOP_PROMPT")
    else:
        actual = next(value for value in strings(source("crates/tools/file/src/notebook_edit.rs")) if value.startswith("Replaces, inserts, or deletes a single cell"))
    assert actual == case["output"], "".join(difflib.unified_diff(case["output"].splitlines(True),actual.splitlines(True),fromfile=case["tool"],tofile="production"))
monitor_path = "crates/tools/task/src/monitor.rs"
for case in FIXTURE["monitorCases"]:
    suffix = "FOREGROUND" if case["backgroundDisabled"] else "BACKGROUND"
    names = ["CJR_HEAD", "CJR_ONE_SHOT_" + suffix, "CJR_MID", "CJR_UNBOUNDED_" + suffix, "CJR_TAIL", "WS_DESCRIPTION"]
    actual = "".join(constant(monitor_path, name) for name in names)
    if case["pushNotifications"]:
        push = next(value for value in strings(source(monitor_path)) if value.startswith("\n\nWhen an event lands"))
        actual += push.replace("{PUSH_NOTIFICATION_NAME}", "PushNotification")
    assert actual == case["output"], "".join(difflib.unified_diff(case["output"].splitlines(True),actual.splitlines(True),fromfile="Monitor",tofile="production"))
for case in FIXTURE["cronCases"]:
    expected = case["output"].replace(".claude/scheduled_tasks.json", "the branded project session-cron store").replace("Claude", "LingXi")
    path = {"CronCreate":"schedule_cron", "CronDelete":"cron_delete", "CronList":"cron_list"}[case["tool"]]
    text = source(f"crates/tools/cron/src/{path}.rs")
    if case["tool"] == "CronCreate":
        text = text[text.index("fn build_prompt_text()"):text.index("#[async_trait]",text.index("fn build_prompt_text()"))]
        literals = list(strings(text))
        durability = next(value for value in literals if value.startswith("## Durability" if case["durable"] else "## Session-only"))
        durability = durability.replace("{product_name}", "LingXi")
        durable_runtime = next(value for value in literals if value.startswith("Durable jobs persist")) if case["durable"] else ""
        expiry = next(value for value in literals if value.startswith("Recurring tasks auto-expire")).replace("{days}", "7")
        template = next(value for value in literals if value.startswith("Schedule a prompt to be enqueued"))
        actual = template.format(durability=durability,monitor="",durable_runtime=durable_runtime,expiry=expiry)
        monitor = "\n## Not for live watching\n\nCronCreate re-runs a prompt at fixed wall-clock intervals. To watch a log file, process, or command output and be notified the moment something changes, use the Monitor tool instead — Monitor streams events as they happen; cron polls on a schedule.\n"
        expected = expected.replace(monitor, "")
        # Current create/list result schemas do not advertise a next-run value.
        next_run = " The result of this tool and of CronList gives each job's actual next run time: when you tell the user when a job will run, give that time, not the minute in the cron expression."
        assert next_run in expected
        expected = expected.replace(next_run, "")
    else:
        prefix = "Cancel a cron job previously scheduled" if case["tool"] == "CronDelete" else "List all cron jobs scheduled via"
        actual = next(value for value in strings(text) if value.startswith(prefix) and ("branded project" in value) == case["durable"] and (case["tool"] != "CronDelete" or "Removes it" in value))
    assert actual == expected, "".join(difflib.unified_diff(expected.splitlines(True),actual.splitlines(True),fromfile=case["tool"],tofile="production"))
bash_fixture = json.loads((ROOT / "scripts/tests/fixtures/bash_prompt_319.json").read_text())
git_source = bash_source[bash_source.index("fn commit_and_pr_instructions("):]
git_source = git_source[:git_source.index("// =====")]
git_template = max(strings(git_source), key=len)
commit = constant("crates/tools/shell/src/prompt.rs", "COMMIT_ATTRIBUTION")
pr = constant("crates/tools/shell/src/prompt.rs", "PR_ATTRIBUTION")
mention = constant("crates/tools/shell/src/prompt.rs", "GITHUB_MENTION_GUIDANCE")
for explicit in [False, True]:
    actual = git_template.replace("{TASK_TOOL}", "TaskCreate").replace("{GITHUB_MENTION_GUIDANCE}", mention)
    values = {
        "COMMIT_STEP_SUFFIX": " ending with:\n   " + commit if explicit else ".",
        "COMMIT_HEREDOC_SUFFIX": "\n\n   " + commit if explicit else "",
        "PR_BODY_SUFFIX": "\n\n" + pr if explicit else "",
        "PRECOMMIT_SUGGESTION": "",
    }
    for key, value in values.items():
        actual = actual.replace("{" + key + "}", value)
    expected = next(row["output"] for row in bash_fixture["cases"] if not row["lean"] and not row["monitor"] and row["attribution"] == ("explicit" if explicit else "none"))
    expected = expected[expected.index("# Committing changes with git"):].replace("Generated with [Claude Code]", "Generated with [LingXi]")
    assert actual == expected, f"Bash git template explicit={explicit} differs"
print(f"Verified {checked} production tool templates, 2 lean rewrites, 6 task, 6 cron, 2 other tool and 4 Monitor variants, 2 full Bash git templates, Agent delegation guidance and 2 Bash cwd branches against the pinned upstream oracle")
