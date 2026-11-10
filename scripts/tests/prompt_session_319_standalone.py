#!/usr/bin/env python3
"""Compile real prompt assemblers with rustc when Cargo's workspace lock is busy."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
from prompt_session_319_fixture import HOST_AVAILABILITY, expected_host_body


ROOT = Path(__file__).resolve().parents[2]
PROMPT = ROOT / "crates/orchestrator/src/prompt"
SOURCE_RECORDS = []


def read_source(path):
    text = path.read_text()
    SOURCE_RECORDS.append({"path": str(path), "kind": "module", "sha256": hashlib.sha256(text.encode()).hexdigest()})
    return text


def closing_brace(text, opening):
    depth = 0
    cursor = opening
    while cursor < len(text):
        if text.startswith("//", cursor):
            end = text.find("\n", cursor)
            cursor = len(text) if end < 0 else end + 1
            continue
        if text.startswith("/*", cursor):
            nested = 1
            cursor += 2
            while nested:
                if text.startswith("/*", cursor):
                    nested += 1
                    cursor += 2
                elif text.startswith("*/", cursor):
                    nested -= 1
                    cursor += 2
                else:
                    cursor += 1
            continue
        raw = re.match(r'(?:b)?r(#{0,255})"', text[cursor:])
        if raw:
            end = text.find('"' + raw.group(1), cursor + len(raw.group()))
            assert end >= 0, "unterminated raw Rust string"
            cursor = end + 1 + len(raw.group(1))
            continue
        if text[cursor] == '"':
            cursor += 1
            while cursor < len(text):
                if text[cursor] == "\\":
                    cursor += 2
                elif text[cursor] == '"':
                    cursor += 1
                    break
                else:
                    cursor += 1
            continue
        if text[cursor] == "'":
            character = re.match(r"'(?:\\u\{[0-9A-Fa-f]+\}|\\.|[^'\\\n])'", text[cursor:])
            if character:
                cursor += len(character.group())
                continue
        if text[cursor] == "{":
            depth += 1
        elif text[cursor] == "}":
            depth -= 1
            if depth == 0:
                return cursor + 1
        cursor += 1
    raise AssertionError("unterminated Rust item")


def extract(path, text, name, kind="fn", attributes=False):
    pattern = r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?(?:const[ \t]+)?" + kind + r"[ \t]+" + re.escape(name) + r"\b"
    match = re.search(pattern, text)
    assert match, "missing %s %s in %s" % (kind, name, path)
    start = match.start()
    if attributes:
        while start:
            previous = text.rfind("\n", 0, max(0, start - 1)) + 1
            attribute = text[previous:start].strip()
            if not (attribute.startswith("#[") and attribute.endswith("]")):
                break
            start = previous
    opening = text.find("{", match.end())
    end = closing_brace(text, opening)
    value = text[start:end]
    SOURCE_RECORDS.append({"path": str(path), "kind": kind, "name": name,
                           "start": len(text[:start].encode()), "endExclusive": len(text[:end].encode()),
                           "sha256": hashlib.sha256(value.encode()).hexdigest()})
    return value


def rust_string(value):
    hashes = "###"
    while '"' + hashes in value:
        hashes += "#"
    return "r" + hashes + '"' + value + '"' + hashes


def module_path(name, path, relocated=None):
    text = read_source(path)
    for relative in re.findall(r'include_str!\(\s*"([^"]+)"\s*\)', text):
        target = path.parent / relative
        if target.exists():
            read_source(target.resolve())
    for template_name in re.findall(r'template!\("([^"]+)"\)', text):
        read_source(path.parent / "templates/claude-code-2.1.319" / (template_name + ".md"))
    imported = path
    if relocated:
        def child_module(match):
            child_name = match.group(1)
            child = path.parent / path.stem / (child_name + ".rs")
            if child.exists():
                read_source(child)
            return '#[path = %s]\nmod %s;' % (rust_string(str(child)), child_name)
        text = re.sub(r"(?m)^mod (\w+);", child_module, text)
        text = re.sub(r'include_str!\(\s*"([^"]+)"\s*\)', lambda match: 'include_str!(%s)' % rust_string(str((path.parent / match.group(1)).resolve())), text)
        imported = relocated / (name + "-source.rs")
        imported.write_text(text)
        SOURCE_RECORDS.append({"path": str(imported), "kind": "module-location-adapter", "sha256": hashlib.sha256(text.encode()).hexdigest(), "scope": "Only module/include paths become absolute; all declarations and implementations remain unchanged."})
    return "#[path = %s] pub mod %s;\n" % (rust_string(str(imported)), name)


def cached_dependencies():
    target = Path(os.environ.get("CARGO_TARGET_DIR", str(ROOT / "target")))
    directory = target / "debug/deps"
    def candidates(name):
        return sorted(directory.glob("lib" + name + "-*.rlib"), key=lambda value: value.stat().st_mtime, reverse=True)
    def serde_core_fingerprint(path, name):
        fingerprint = target / "debug/.fingerprint" / (name + "-" + path.stem.split("-")[-1]) / ("lib-" + name + ".json")
        if not fingerprint.exists():
            return None
        return next((entry[3] for entry in json.loads(fingerprint.read_text()).get("deps", []) if entry[1] == "serde_core"), None)
    serde = candidates("serde")
    json_candidates = candidates("serde_json")
    pair = next(((left, right) for left in serde for right in json_candidates if serde_core_fingerprint(left, "serde") == serde_core_fingerprint(right, "serde_json") is not None), None)
    assert pair, "Build matching serde/serde_json dependencies before the standalone replay"
    sha2_version = re.search(r'(?m)^sha2\s*=\s*"([^"]+)"', (ROOT / "crates/core/Cargo.toml").read_text()).group(1)
    sha2 = next((path for path in candidates("sha2") if re.search(
        r'/sha2-' + re.escape(sha2_version) + r'(?:\.|/)',
        (directory / (path.stem.removeprefix("lib") + ".d")).read_text(),
    )), None)
    assert sha2, "Build the core crate's sha2 dependency before the standalone replay"
    libraries = {"serde": pair[0], "serde_json": pair[1], "sha2": sha2, "thiserror": candidates("thiserror")[0]}
    arguments = ["-L", "dependency=" + str(directory)]
    for name, path in libraries.items():
        arguments += ["--extern", name + "=" + str(path)]
    return arguments


def run(work, policy_tests=False):
    work.mkdir(parents=True, exist_ok=True)
    fixture_path = ROOT / "crates/orchestrator/tests/fixtures/prompt_session_2_1_319.json"
    fixture = json.loads(fixture_path.read_text())
    assert fixture["version"] == "2.1.319"
    assert fixture["binary_sha256"] == "1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd"
    assert fixture["host_line_allowlist"] == [HOST_AVAILABILITY]
    mod_path = PROMPT / "mod.rs"
    mod_source = read_source(mod_path)
    effort_path = ROOT / "crates/core/src/host/effort.rs"
    effort_source = read_source(effort_path)
    fork_path = ROOT / "crates/core/src/host/fork_subagent.rs"
    fork_source = read_source(fork_path)
    sessions_path = ROOT / "crates/core/src/host/live_sessions.rs"
    sessions_source = read_source(sessions_path)
    env_meta_path = PROMPT / "env_meta.rs"
    env_meta_source = read_source(env_meta_path)
    mode_path = ROOT / "crates/core/src/host/session_context.rs"
    mode_source = read_source(mode_path)
    source = """extern crate self as lingxi_core;
extern crate self as lingxi_llm_client;
extern crate self as llm_runtime;
extern crate self as branding;
pub const DOT_DIR: &str = ".lingxi";
pub const MODEL_CAPABILITIES_ENV: &str = "LINGXI_MODEL_CAPABILITIES";
pub const DISABLE_ATTACHMENTS_ENV: &str = "LINGXI_DISABLE_ATTACHMENTS";
pub const SIMPLE_ENV: &str = "LINGXI_SIMPLE";
pub mod providers {pub mod anthropic {pub mod system_prompt {pub type PromptText = ();}}}
pub mod model {pub mod context_window {
pub fn context_window_for_model(_: &str, _: &[String]) -> u64 {panic!("countdown is outside this padded/default replay matrix")}
}}
pub mod host {
pub mod session_context {use serde::{Deserialize,Serialize};
"""
    source += extract(mode_path, mode_source, "ConversationMode", "enum", attributes=True) + "\n"
    source += "impl ConversationMode {\n" + extract(mode_path, mode_source, "as_str") + "\n"
    source += extract(mode_path, mode_source, "from_str") + "\n}\n}\n"
    source += """
pub mod effort {
"""
    source += extract(effort_path, effort_source, "javascript_whitespace") + "\n"
    source += extract(effort_path, effort_source, "trim_js_whitespace") + "\n}\n"
    source += module_path("env", ROOT / "crates/core/src/host/env.rs")
    source += module_path("model_capabilities", ROOT / "crates/core/src/host/model_capabilities.rs")
    source += module_path("prompt_policy", ROOT / "crates/core/src/host/prompt_policy.rs", work)
    source += "pub mod fork_subagent {\n"
    gate_source = fork_source[:fork_source.index("pub fn is_in_fork_child")]
    for kind, name in re.findall(r"(?m)^(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(fn|struct|enum)\s+(\w+)", gate_source):
        source += extract(fork_path, fork_source, name, kind, attributes=True) + "\n"
    source += "}\n"
    source += "pub mod live_sessions {use std::sync::Mutex; use super::env::{is_env_truthy,is_env_defined_falsy}; static STEER:Mutex<String> = Mutex::new(String::new());\n"
    source += extract(sessions_path, sessions_source, "subagent_steer_is_default") + "\n}\n}\n"
    source += "pub mod prompt {use std::path::PathBuf;use std::sync::Arc;use lingxi_core::host::prompt_policy::{resolve_prompt_policy,MemoryPromptBinding,PromptTarget,ResolvedPromptPolicy,SystemPromptBinding};\n"
    for name, kind in [("ActiveOutputStyle", "struct"), ("PromptSectionScope", "enum"), ("PromptSection", "struct"), ("PromptSectionSlot", "struct"),
                       ("RenderedPromptSections", "struct"), ("SystemPromptContext", "struct"), ("GitStatus", "struct"),
                       ("FileTree", "struct"), ("FileTreeEntry", "struct")]:
        source += extract(mod_path, mod_source, name, kind, attributes=True) + "\n"
    source += "#[derive(Debug,Clone)]pub struct MemoryFile;\nimpl PromptSection {\n"
    for name in ["new", "id_units", "id_equals_ascii", "id_base_units", "id_base_equals_ascii"]:
        source += extract(mod_path, mod_source, name) + "\n"
    source += "}\n"
    for name in ["locked_templates", "body_sections", "env_block", "memory_section", "bg_session", "portable_background", "total_tokens"]:
        source += module_path(name, PROMPT / (name + ".rs"))
    silent_path = PROMPT / "silent_turn.rs"
    silent_source = read_source(silent_path)
    source += "pub mod silent_turn {\n"
    source += extract(silent_path, silent_source, "is_enabled") + "\n"
    source += extract(silent_path, silent_source, "enabled_with_override") + "\n}\n"
    source += "pub mod env_meta {\n"
    source += extract(env_meta_path, env_meta_source, "marketing_name_for_model") + "\n"
    source += extract(env_meta_path, env_meta_source, "knowledge_cutoff_for_model") + "\n}\n"
    source += "use locked_templates::{HEADER,SECTION_SEP};\n"
    for name in ["assemble_system_prompt", "assemble_system_prompt_with_style", "named_prompt_sections_with_style", "prompt_section_slots", "render_prompt_sections", "render_prompt_sections_scoped", "resolve_context_prompt_policy", "prompt_policy_is_lean", "body_render_options", "named_prompt_sections_for_mode", "profile_header", "output_style_section", "push_section_separator"]:
        source += extract(mod_path, mod_source, name) + "\n"
    if policy_tests:
        source += "#[cfg(test)]\n" + module_path("policy_dispatch_tests", PROMPT / "policy_dispatch_tests.rs")
    source += "}\nfn main() {let mut failures = 0;\n"
    for case in fixture["cases"]:
        name = case["name"]
        target = work / (name + ".expected.txt")
        upstream = case["expected_body"]
        assert hashlib.sha256(upstream.encode()).hexdigest() == case["expected_body_sha256"]
        (work / (name + ".upstream-expected.txt")).write_text(upstream)
        target.write_text(expected_host_body(upstream))
        assert hashlib.sha256(target.read_bytes()).hexdigest() == case["expected_host_body_sha256"]
        values = case["input"]
        memory = "None" if values["memory_dir"] is None else "Some(std::path::PathBuf::from(%s))" % rust_string(values["memory_dir"])
        tools = "vec![" + ",".join(rust_string(tool) + ".to_owned()" for tool in values["tool_names"]) + "]"
        source += "{let model = %s; let ctx = prompt::SystemPromptContext {prompt_policy:None,native_system_turns:None,cwd:std::path::PathBuf::from(\"/repo\"),platform:\"darwin\".into(), model:model.into(),model_marketing_name:prompt::env_meta::marketing_name_for_model(model).map(str::to_owned),knowledge_cutoff:prompt::env_meta::knowledge_cutoff_for_model(model).map(str::to_owned),shell:\"zsh\".into(),os_version:\"Darwin 27.2.0\".into(),git_status:None,in_worktree:false,file_tree:prompt::FileTree::default(),memory_files:Vec::new(),tool_names:%s,skills_available:%s,is_interactive:%s,memory_dir:%s,exclude_dynamic_sections:%s};\n" % (rust_string(values["model"]), tools, str(values["skills_available"]).lower(), str(values["is_interactive"]).lower(), memory, str(values["exclude_dynamic_sections"]).lower())
        source += "let mut sections = prompt::named_prompt_sections_for_mode(&ctx,None,host::session_context::ConversationMode::Code); assert_eq!(sections[0].id,\"header\"); sections.remove(0);let actual = prompt::render_prompt_sections(&sections);let expected=std::fs::read_to_string(%s).unwrap();std::fs::write(%s,&actual).unwrap();\n" % (rust_string(str(target)), rust_string(str(work / (name + ".actual.txt"))))
        source += "if actual == expected {println!(\"PASS {}: {} bytes\",%s,actual.len());} else {failures+=1;let at=actual.as_bytes().iter().zip(expected.as_bytes()).position(|(a,b)|a!=b).unwrap_or_else(||actual.len().min(expected.len()));println!(\"FAIL {} at byte {}: actual {} bytes expected {} bytes; sections {:?}\",%s,at,actual.len(),expected.len(),sections.iter().map(|s|s.id.as_str()).collect::<Vec<_>>());}}\n" % (rust_string(name), rust_string(name))
    source += "if failures > 0 {std::process::exit(1);}println!(\"6 complete native 2.1.319 bodies matched the real Rust assembler\");}\n"
    (work / "assembler-replay.rs").write_text(source)
    manifest = {"version": fixture["version"], "fixture": str(fixture_path), "fixture_sha256": hashlib.sha256(fixture_path.read_bytes()).hexdigest(), "sources": SOURCE_RECORDS,
                "host_line_allowlist": fixture["host_line_allowlist"],
                "host_stubs": ["empty instruction-file inventory", "unused provider PromptText opaque type", "initial empty session-steer state", "branding DOT_DIR=.lingxi", "context-window lookup panics if an out-of-matrix countdown branch executes"],
                "compiled_real_modules": ["core model capabilities", "core environment parsing", "body sections", "memory sections", "environment blocks", "background sessions", "total token accounting"],
                "scope": "Complete system body assembly, excluding the product identity header and provider transport projection. Actual assembler function text is extracted unmodified and SHA-recorded."}
    (work / "source-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    dependencies = cached_dependencies()
    subprocess.run(["rustc", "--edition", "2021", "-A", "dead_code", *dependencies, str(work / "assembler-replay.rs"), "-o", str(work / "assembler-replay")], check=True, cwd=ROOT)
    environment = {key: value for key, value in os.environ.items() if not key.startswith(("CLAUDE_CODE_", "LINGXI_"))}
    result = subprocess.run([str(work / "assembler-replay")], env=environment, cwd=ROOT, text=True, capture_output=True)
    print(result.stdout, end="")
    print(result.stderr, end="")
    (work / "result.json").write_text(json.dumps({"exit_code": result.returncode, "stdout": result.stdout, "stderr": result.stderr}, indent=2) + "\n")
    if result.returncode == 0 and policy_tests:
        binary = work / "policy-tests"
        subprocess.run(["rustc", "--edition", "2021", "--test", "-A", "dead_code", *dependencies, str(work / "assembler-replay.rs"), "-o", str(binary)], check=True, cwd=ROOT)
        result = subprocess.run([str(binary), "prompt::policy_dispatch_tests", "--test-threads=1"], env=environment, cwd=ROOT, text=True, capture_output=True)
        print(result.stdout, end="")
        print(result.stderr, end="")
        (work / "policy-result.json").write_text(json.dumps({"exit_code": result.returncode, "stdout": result.stdout, "stderr": result.stderr}, indent=2) + "\n")
    return result.returncode


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work-dir", type=Path)
    parser.add_argument("--policy-tests", action="store_true", help="Also compile and run the actual policy renderer tests")
    args = parser.parse_args()
    if args.work_dir:
        return run(args.work_dir.resolve(), args.policy_tests)
    with tempfile.TemporaryDirectory(prefix="prompt-session-319-") as directory:
        return run(Path(directory), args.policy_tests)


if __name__ == "__main__":
    raise SystemExit(main())
