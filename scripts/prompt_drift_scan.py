#!/usr/bin/env python3
"""Report model-facing prompt text that no longer exists in an oracle build.

Why this exists: two model-facing surfaces were found byte-locked to a stale
Claude Code version (the Workflow tool description at 2.1.245, the memory
section at 2.1.220) only because someone happened to look. Nothing enumerated
them, so drift was invisible until read by hand.

🚨 Four traps make a naive version of this scan useless. Each cost a wrong
conclusion before being fixed, and each is handled below:

  1. The binary stores non-ASCII ESCAPED and not uniformly (`\\u2014`, `\\xD7`).
     Unescape the corpus, and unescape the port's Rust `\\u{2014}` too.
  2. Upstream joins prompt arrays with "\\n" at RENDER time, so multi-line
     paragraphs never appear contiguously in source. Only single-line literals
     are comparable; longer text needs the assembled text, not chunk source.
  3. Tool names are INTERPOLATED (`${mt}` = Agent, `${oo}` = Skill, `${Ts}` =
     AskUserQuestion). "Use the Agent tool" is absent from source while being
     present in the product. Compare with those spans wildcarded.
  4. The port rebrands (LINGXI.md, ~/.lingxi, LingXi). Reverse it before
     comparing or every branded line reads as drift.

A "MISS" is a hypothesis, not a finding: read the divergence point before
believing it.
"""
import io, os, re, sys, glob

TOOL_NAMES = ["AskUserQuestion", "Agent", "Skill", "Task", "Workflow", "Bash",
              "Read", "Edit", "Write", "Grep", "Glob", "TodoWrite"]
BRAND = [("LINGXI.md", "CLAUDE.md"), ("~/.lingxi", "~/.claude"),
         (".lingxi/", ".claude/"), ("LingXi", "Claude Code"),
         ("lingxi-cli", "claude")]

def unescape_js(t):
    t = re.sub(r"\\u\{([0-9a-fA-F]+)\}", lambda m: chr(int(m.group(1), 16)), t)
    t = re.sub(r"\\u([0-9a-fA-F]{4})", lambda m: chr(int(m.group(1), 16)), t)
    t = re.sub(r"\\x([0-9a-fA-F]{2})", lambda m: chr(int(m.group(1), 16)), t)
    for a, b in [("\\`", "`"), ("\\n", "\n"), ("\\t", "\t"), ("\\'", "'"),
                 ('\\"', '"'), ("\\$", "$"), ("\\\\", "\\")]:
        t = t.replace(a, b)
    return t

def unescape_rust(t):
    t = re.sub(r"\\u\{([0-9a-fA-F]+)\}", lambda m: chr(int(m.group(1), 16)), t)
    for a, b in [('\\"', '"'), ("\\\\", "\\"), ("\\'", "'")]:
        t = t.replace(a, b)
    return t

def unbrand(t):
    for a, b in BRAND:
        t = t.replace(a, b)
    return t

def build_corpus(chunk_dir):
    parts = []
    for f in sorted(os.listdir(chunk_dir)):
        if f.endswith(".js"):
            raw = io.open(chunk_dir + "/" + f, encoding="utf-8", errors="replace").read()
            t = unescape_js(raw)
            parts.append(t.encode("utf-8", "surrogatepass").decode("utf-8", "replace"))
    return "\n".join(parts)

def present(text, corpus):
    """Exact hit, or a hit once interpolated tool names are wildcarded.

    A leading list marker is stripped first: the port stores `" - foo"` where
    upstream stores `"foo"` and adds the marker when joining the bullet array,
    so the raw literal misses while the product text matches.
    """
    text = re.sub(r"^\s*[-*\u2022]\s+", "", text)
    if text in corpus:
        return True
    pat = re.escape(text)
    for name in TOOL_NAMES:
        pat = pat.replace(re.escape(name), r"(?:" + re.escape(name) + r"|\$\{\w+\})")
    return re.search(pat, corpus) is not None

def longest_prefix(t, corpus):
    lo, hi = 0, len(t)
    while lo < hi:
        mid = (lo + hi + 1) // 2
        if t[:mid] in corpus: lo = mid
        else: hi = mid - 1
    return lo

def main():
    chunk_dir = sys.argv[1] if len(sys.argv) > 1 else os.path.expanduser(
        "~/.claude/oracle-chunks/2.1.267")
    patterns = sys.argv[2:] or ["orchestrator/src/prompt/*.rs"]
    corpus = build_corpus(chunk_dir)
    # Positive control: the scan must be able to FIND something.
    control = "Execute a workflow script that orchestrates multiple subagents deterministically."
    if control not in corpus:
        print("FATAL: positive control missing — the corpus or unescaping is broken")
        return 2
    print(f"corpus {len(corpus):,} chars; control OK")
    misses = checked = 0
    for pat in patterns:
        for f in sorted(glob.glob(pat)):
            s = io.open(f, encoding="utf-8").read()
            ti = s.find("mod tests")
            prod = s if ti < 0 else s[:ti]
            for m in re.finditer(r'"((?:[^"\\\n]|\\.){90,})"', prod):
                lit = m.group(1)
                if "\\n" in lit or lit.count("{") > 2:
                    continue
                t = unbrand(unescape_rust(lit))
                if "{" in t:
                    continue
                checked += 1
                if present(t, corpus):
                    continue
                misses += 1
                p = longest_prefix(t, corpus)
                line = prod[:m.start()].count("\n") + 1
                print(f"\nMISS {f}:{line}  ({p}/{len(t)} chars matched)")
                print(f"  diverges at: {t[p:p+120]!r}")
    print(f"\nchecked {checked} literals; {misses} candidate(s) — read each before believing it")
    return 0

if __name__ == "__main__":
    sys.exit(main())
