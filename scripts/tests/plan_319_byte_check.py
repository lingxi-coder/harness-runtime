#!/usr/bin/env python3
"""Check production Plan Mode renderers against the artifact-derived 2.1.319 fixture.

This compiles the production reminder module and extracts its actual tool text
renderers into a small Rust executable. Only the UTF-16 transport container is
substituted; the ECMAScript whitespace helper is production source too. Full
tool/gate integration tests remain `cargo test -p tool-plan --lib`.
"""
from pathlib import Path
import argparse
import hashlib
import json
import re
import subprocess
import tempfile


REPO = Path(__file__).resolve().parents[2]
TOOL = REPO / 'crates/tools/plan/src/plan_mode.rs'
REMINDER = REPO / 'crates/orchestrator/src/prompt/plan_reminder.rs'
EFFORT = REPO / 'crates/core/src/host/effort.rs'
ENV = REPO / 'crates/core/src/host/env.rs'
FIXTURE = REPO / 'scripts/tests/fixtures/claude_2_1_319_plan.json'


def literal(text):
    return 'r########"' + text + '"########'


def function(source, name):
    match = re.search(r'(?m)^(?:pub\s+)?fn\s+' + re.escape(name) + r'\b', source)
    if match is None:
        raise ValueError('Production function missing: ' + name)
    end = source.index('\n}', match.start()) + 2
    return source[match.start():end]


def run():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, help='Optional JSON report path')
    args = parser.parse_args()
    source = TOOL.read_text()
    fixture = json.loads(FIXTURE.read_text())
    constants = '\n'.join(re.findall(
        r'(?:pub\s+)?const\s+\w+: &str =\s*(?:r#".*?"#|"(?:\\.|[^"\\])*")\s*;', source, re.S,
    ))
    effort_source = EFFORT.read_text()
    rust = '''
#![allow(dead_code)]
extern crate self as branding;
extern crate self as lingxi_core;
pub const PLAN_EXPLORE_AGENT_COUNT_ENV: &str = "LINGXI_PLAN_V2_EXPLORE_AGENT_COUNT";
pub const PLAN_AGENT_COUNT_ENV: &str = "LINGXI_PLAN_V2_AGENT_COUNT";
pub const DISABLE_EXPLORE_PLAN_AGENTS_ENV: &str = "LINGXI_DISABLE_EXPLORE_PLAN_AGENTS";
'''
    rust += 'pub mod host { pub mod effort {\n' + '\n'.join(
        function(effort_source, name) for name in ['javascript_whitespace', 'trim_js_whitespace']
    ) + '\n} pub mod env {\n' + function(ENV.read_text(), 'is_env_truthy') + '\n}}\n'
    rust += '''
#[derive(Clone)] pub struct ProbeProjection { units: Vec<u16>, display: String }
impl From<&str> for ProbeProjection {
 fn from(text: &str) -> Self { Self { units:text.encode_utf16().collect(), display: text.to_owned() } }
}
impl ProbeProjection {
 fn string_at(&self, _: &str) -> Option<&str> { Some(&self.display) }
 fn string_units(&self, _: &str) -> Option<Vec<u16>> { Some(self.units.clone()) }
}
type Utf16JsonProjection = ProbeProjection;
mod tool_api { pub mod tool_trait {
 pub(crate) fn source_text_projection_units(units: &[u16]) -> super::super::ProbeProjection {
  super::super::ProbeProjection { units: units.to_vec(), display: String::from_utf16_lossy(units) }
 }
}}
'''
    rust += '#[path = ' + literal(str(REMINDER)) + '] mod plan_reminder;\n'
    rust += constants + '\n' + '\n'.join(function(source, name) for name in [
        'render_enter_plan_mode_prompt', 'render_exit_plan_mode_result',
    ]) + '\n'
    with tempfile.TemporaryDirectory(prefix='lingxi-plan-319-') as temporary:
        root = Path(temporary)
        main = ['fn main() { let out = std::path::Path::new(' + literal(str(root)) + ');']
        checks = []

        def write_check(name, expected, expression, utf16=False):
            filename = str(len(checks)) + '.bin'
            if utf16:
                main.append(f'{{ let projection = {expression}; std::fs::write(out.join({literal(filename)}), projection.units.iter().flat_map(|unit| unit.to_le_bytes()).collect::<Vec<_>>()).unwrap(); }}')
            else:
                main.append(f'std::fs::write(out.join({literal(filename)}), {expression}).unwrap();')
            checks.append({'name': name, 'expected': expected, 'filename': filename})

        for case in fixture['reminders']:
            inp = case['input']
            values = [
                ('LINGXI_DISABLE_EXPLORE_PLAN_AGENTS', '1' if inp.get('agents') is False else None),
                ('LINGXI_PLAN_V2_AGENT_COUNT', str(inp['planCount']) if 'planCount' in inp else None),
                ('LINGXI_PLAN_V2_EXPLORE_AGENT_COUNT', str(inp['exploreCount']) if 'exploreCount' in inp else None),
            ]
            for key, value in values:
                main.append(f'std::env::set_var({literal(key)}, {literal(value)});' if value is not None else f'std::env::remove_var({literal(key)});')
            custom = f'Some({literal(inp["customInstructions"])})' if 'customInstructions' in inp else 'None'
            params = f'''plan_reminder::PlanReminderParams {{
                plan_file_path: "/tmp/plan.md",
                plan_exists: {str(inp.get('planExists', False)).lower()},
                custom_instructions: {custom},
                is_subagent: {str(inp.get('isSubAgent', False)).lower()},
                reminder_type_sparse: {str(inp.get('reminderType') == 'sparse').lower()},
                output_style_is_default: {str(inp.get('outputStyle', 'default') == 'default').lower()}
            }}'''
            write_check('reminder/' + case['name'], case['expected'].encode(), f'plan_reminder::render_plan_mode_reminder(&{params})')
        for case in fixture['exitResults']:
            inp = case['input']
            if inp['planUnits'] is None:
                plan = 'None'
            else:
                units = ','.join(map(str, inp['planUnits']))
                plan = f'Some(&tool_api::tool_trait::source_text_projection_units(&[{units}]))'
            filename = 'None' if 'filePath' not in inp else f'Some({literal(inp["filePath"])})'
            expression = f'render_exit_plan_mode_result({plan}, {str(inp["isAgent"]).lower()}, {filename}, {str(inp.get("hasTaskTool", False)).lower()}, {str(case["outputStyleIsDefault"]).lower()}, {str(inp.get("planWasEdited", False)).lower()})'
            write_check('exit/' + case['name'], b''.join(unit.to_bytes(2, 'little') for unit in case['expectedUnits']), expression, utf16=True)
        for case in fixture['enterPrompts']:
            write_check(f'enter-prompt/style={case["outputStyleIsDefault"]}/hybrid-search={case["hybridSearch"]}', case['expected'].encode(), f'render_enter_plan_mode_prompt({str(case["outputStyleIsDefault"]).lower()}, {str(case["hybridSearch"]).lower()})')
        for name, expected, expression in [
            ('exit-prompt', fixture['exitPrompt'], 'EXIT_PLAN_MODE_V2_TOOL_PROMPT'),
            ('enter-result', fixture['enterResult'], 'ENTER_PLAN_MODE_INSTRUCTIONS'),
            ('enter-confirmation', fixture['enterConfirmation'], 'ENTER_PLAN_MODE_CONFIRMATION'),
        ]:
            write_check(name, expected.encode(), expression)
        main.append('}')
        program = root / 'probe.rs'
        program.write_text(rust + '\n'.join(main) + '\n')
        subprocess.run(['rustc', '--edition=2021', str(program), '-o', str(root / 'probe')], check=True)
        subprocess.run([str(root / 'probe')], check=True)
        report_checks = []

        def record(name, expected, actual):
            first = next((i for i, (a, b) in enumerate(zip(expected, actual)) if a != b), min(len(expected), len(actual)))
            report_checks.append({
                'name': name, 'match': expected == actual,
                'expectedBytes': len(expected), 'actualBytes': len(actual),
                'firstDifferentByte': None if expected == actual else first,
                'expectedSha256': hashlib.sha256(expected).hexdigest(),
                'actualSha256': hashlib.sha256(actual).hexdigest(),
            })

        for check in checks:
            record(check['name'], check['expected'], (root / check['filename']).read_bytes())
        for name, constant in [('EnterPlanMode', 'EMPTY_INPUT_SCHEMA'), ('ExitPlanMode', 'EXIT_INPUT_SCHEMA')]:
            at = source.index('json!(', source.index('static ' + constant)) + len('json!(')
            actual, _ = json.JSONDecoder().raw_decode(source[at:].lstrip())
            compact = lambda value: json.dumps(value, ensure_ascii=False, separators=(',', ':')).encode()
            record('schema/' + name, compact(fixture['inputSchemas'][name]), compact(actual))
        report = {
            'version': fixture['version'], 'fixtureSha256': hashlib.sha256(FIXTURE.read_bytes()).hexdigest(),
            'sources': [{'path': str(path), 'sha256': hashlib.sha256(path.read_bytes()).hexdigest()} for path in [TOOL, REMINDER, EFFORT, ENV]],
            'checks': report_checks,
            'scope': 'Current production reminder module and exact tool renderer functions compiled with the actual JS-whitespace/env helpers. UTF-16 source transport uses a small substitute container. Schema literals compared in insertion order; no full local provider/gate execution.',
        }
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(report, indent=2) + '\n')
        failures = [check['name'] for check in report_checks if not check['match']]
        print(json.dumps({'checks': len(report_checks), 'matched': len(report_checks) - len(failures), 'mismatches': failures}))
        if failures:
            raise SystemExit(1)


if __name__ == '__main__':
    run()
