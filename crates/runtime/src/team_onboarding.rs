//! Local source data and prompt generation for `/team-onboarding`.
//!
//! The implementation follows Claude Code 2.1.295's local `KDt`/`Sdo`
//! producer (source SHA-256
//! `ae64b2c8c87be0e990738da478adbbe5cd6a8201cef7953d7684cf59bd780120`): it
//! scans only the current project's recent JSONL files, counts slash-command
//! and MCP tool-use markers, reads that project's `.mcp.json`, and adds local
//! git display metadata. The source-local `teamOnboardingLastUsedAt` global
//! config marker is written before the usage scan; remote guide upload is
//! outside this local provider.

use command_api::builtins::{TeamOnboardingCommandAvailability, TeamOnboardingPromptProvider};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const WINDOW_DAYS: u32 = 30;
const TRANSCRIPT_MAX_BYTES: u64 = 52_428_800;
const MAX_SESSION_DESCRIPTORS: usize = 60;
const SESSION_TITLE_MAX_UTF16_UNITS: usize = 200;

/// The local producer backing the source-shaped `/team-onboarding` command.
#[derive(Debug, Clone)]
pub struct LocalTeamOnboardingPromptProvider {
    lingxi_home: PathBuf,
    cwd: PathBuf,
    availability: TeamOnboardingCommandAvailability,
}

impl LocalTeamOnboardingPromptProvider {
    /// Bind local transcript/config discovery to the host's current project.
    #[must_use]
    pub fn new(
        lingxi_home: impl Into<PathBuf>,
        cwd: impl Into<PathBuf>,
        availability: TeamOnboardingCommandAvailability,
    ) -> Self {
        Self {
            lingxi_home: lingxi_home.into(),
            cwd: cwd.into(),
            availability,
        }
    }
}

#[async_trait::async_trait]
impl TeamOnboardingPromptProvider for LocalTeamOnboardingPromptProvider {
    fn command_availability(&self) -> TeamOnboardingCommandAvailability {
        self.availability
    }

    async fn generate_prompt(&self) -> Result<Utf16JsonProjection, Utf16JsonProjection> {
        let lingxi_home = self.lingxi_home.clone();
        let cwd = self.cwd.clone();
        tokio::task::spawn_blocking(move || build_prompt(&lingxi_home, &cwd))
            .await
            .map_err(|_| error_projection("Could not build the local onboarding prompt."))?
    }
}

fn build_prompt(
    lingxi_home: &Path,
    cwd_path: &Path,
) -> Result<Utf16JsonProjection, Utf16JsonProjection> {
    let mut data = scan_project_usage_after_marker(
        ambient_global_config_path(lingxi_home).as_deref(),
        current_time_millis(),
        || {
            let cwd = cwd_path.to_str().ok_or_else(|| {
                error_projection("Could not scan local usage data for this workspace.")
            })?;
            scan_project_usage(lingxi_home, cwd_path, cwd, WINDOW_DAYS)
        },
    )?;
    data.generated_by = git_output(cwd_path, &["config", "user.name"])
        .map(|value| trim_ecmascript_whitespace(&value).to_owned())
        .filter(|value| !value.is_empty());
    let remote = git_output(cwd_path, &["remote", "get-url", "origin"]);
    data.current_repo = remote
        .as_deref()
        .and_then(repository_display_from_remote)
        .unwrap_or_else(|| cwd_basename(cwd_path));

    let usage_data = format_usage_data(&data);
    let prompt = DEFAULT_SYSTEM_PROMPT
        .replace("{{WINDOW_DAYS}}", &WINDOW_DAYS.to_string())
        .replace("{{GUIDE_TEMPLATE}}", DEFAULT_GUIDE_TEMPLATE)
        .replace("{{USAGE_DATA}}", &usage_data);
    let serialized = serde_json::to_string(&prompt)
        .map_err(|_| error_projection("Could not build the local onboarding prompt."))?;
    Utf16JsonProjection::parse(&serialized)
        .map_err(|_| error_projection("Could not build the local onboarding prompt."))
}

fn ambient_global_config_path(lingxi_home: &Path) -> Option<PathBuf> {
    global_config_path_for_home(
        migrations::global_config::global_config_path(),
        migrations::global_config::lingxi_config_home(),
        lingxi_home,
    )
}

fn global_config_path_for_home(
    candidate_path: Option<PathBuf>,
    config_home: Option<PathBuf>,
    lingxi_home: &Path,
) -> Option<PathBuf> {
    candidate_path.filter(|_| config_home.as_deref() == Some(lingxi_home))
}

fn current_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn scan_project_usage_after_marker<T>(
    config_path: Option<&Path>,
    timestamp_ms: u64,
    scan: impl FnOnce() -> Result<T, Utf16JsonProjection>,
) -> Result<T, Utf16JsonProjection> {
    if let Some(path) = config_path {
        migrations::global_config::set_team_onboarding_last_used_at(path, timestamp_ms)
            .map_err(|_| error_projection("Could not update local onboarding usage metadata."))?;
    }
    scan()
}

#[derive(Debug, Default)]
struct UsageData {
    generated_by: Option<String>,
    current_repo: String,
    window_days: u32,
    session_count: usize,
    slash_commands: Vec<CountedName>,
    mcp_servers: Vec<McpServerUsage>,
    session_descriptors: Vec<SessionDescriptor>,
}

#[derive(Debug)]
struct CountedName {
    name: String,
    count: u64,
}

#[derive(Debug)]
struct McpServerUsage {
    name: String,
    call_count: u64,
    url_origin: Option<String>,
}

#[derive(Debug, Default)]
struct SessionDescriptor {
    pr_numbers: Vec<u64>,
    title: Option<Vec<u16>>,
    first_message: Option<Vec<u16>>,
}

impl SessionDescriptor {
    fn ranking_score(&self) -> u8 {
        u8::from(self.title.is_some()) * 2 + u8::from(!self.pr_numbers.is_empty())
    }

    fn is_empty(&self) -> bool {
        self.title.is_none() && self.pr_numbers.is_empty() && self.first_message.is_none()
    }
}

fn scan_project_usage(
    lingxi_home: &Path,
    cwd_path: &Path,
    cwd: &str,
    window_days: u32,
) -> Result<UsageData, Utf16JsonProjection> {
    let projects = lingxi_home
        .join("projects")
        .join(session::jsonl::project_dir_name(cwd));
    let entries = match std::fs::read_dir(&projects) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(UsageData {
                window_days,
                current_repo: cwd_basename(cwd_path),
                ..UsageData::default()
            });
        }
        Err(_) => return Err(error_projection("Could not read local session data.")),
    };

    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(u64::from(window_days) * 24 * 60 * 60))
        .unwrap_or(UNIX_EPOCH);
    let mut usage = UsageData {
        window_days,
        current_repo: cwd_basename(cwd_path),
        ..UsageData::default()
    };

    for entry in entries {
        let entry = entry.map_err(|_| error_projection("Could not read local session data."))?;
        let path = entry.path();
        if path.extension() != Some(OsStr::new("jsonl")) {
            continue;
        }
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(error_projection("Could not read local session data.")),
        };
        if !metadata.is_file() || metadata.len() > TRANSCRIPT_MAX_BYTES {
            continue;
        }
        let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
        if modified < cutoff {
            continue;
        }
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(error_projection("Could not read local session data.")),
        };
        let transcript = String::from_utf8_lossy(&bytes);
        usage.session_count += 1;
        let mut descriptor = SessionDescriptor::default();
        for line in transcript.split('\n') {
            if line.encode_utf16().count() < 10 {
                continue;
            }
            count_slash_commands(line, &mut usage.slash_commands);
            count_mcp_calls(line, &mut usage.mcp_servers);
            scan_descriptor_line(line, &mut descriptor);
        }
        if !descriptor.is_empty() {
            usage.session_descriptors.push(descriptor);
        }
    }

    cap_session_descriptors(&mut usage.session_descriptors);
    usage
        .slash_commands
        .sort_by(|left, right| right.count.cmp(&left.count));

    let mcp_origins = read_mcp_server_origins(
        cwd_path,
        &usage
            .mcp_servers
            .iter()
            .map(|server| server.name.clone())
            .collect::<Vec<_>>(),
    );
    for server in &mut usage.mcp_servers {
        server.url_origin = mcp_origins.get(&server.name).cloned();
    }
    usage
        .mcp_servers
        .sort_by(|left, right| right.call_count.cmp(&left.call_count));
    Ok(usage)
}

fn cap_session_descriptors(descriptors: &mut Vec<SessionDescriptor>) {
    if descriptors.len() > MAX_SESSION_DESCRIPTORS {
        descriptors.sort_by_key(|descriptor| std::cmp::Reverse(descriptor.ranking_score()));
        descriptors.truncate(MAX_SESSION_DESCRIPTORS);
    }
}

fn count_slash_commands(line: &str, counts: &mut Vec<CountedName>) {
    if !line.contains("\"content\":\"<command-name>/")
        && !line.contains("\"content\":\"<command-message>")
    {
        return;
    }
    const OPEN: &str = "<command-name>/";
    const CLOSE: &str = "</command-name>";
    let mut cursor = 0;
    while let Some(offset) = line[cursor..].find(OPEN) {
        let start = cursor + offset + OPEN.len();
        let Some(end_offset) = line[start..].find(CLOSE) else {
            cursor = start;
            continue;
        };
        let end = start + end_offset;
        let name = &line[start..end];
        if !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b':' | b'-'))
        {
            increment_count(counts, name.to_owned());
        }
        cursor = end + CLOSE.len();
    }
}

fn count_mcp_calls(line: &str, counts: &mut Vec<McpServerUsage>) {
    const TOOL_USE: &str = "\"type\":\"tool_use\"";
    const NAME_PREFIX: &str = "\"name\":\"mcp__";
    if !line.contains(TOOL_USE) || !line.contains(NAME_PREFIX) {
        return;
    }
    let mut cursor = 0;
    while let Some(offset) = line[cursor..].find(NAME_PREFIX) {
        let start = cursor + offset + NAME_PREFIX.len();
        let bytes = line.as_bytes();
        let Some(separator_offset) = bytes[start..].windows(2).position(|pair| pair == b"__")
        else {
            break;
        };
        let separator = start + separator_offset;
        let server_name = &line[start..separator];
        let tool_start = separator + 2;
        let Some(tool_end_offset) = line[tool_start..].find('"') else {
            break;
        };
        let tool_end = tool_start + tool_end_offset;
        if !server_name.is_empty() && tool_end > tool_start {
            increment_mcp_count(counts, server_name.to_owned());
        }
        cursor = tool_end + 1;
    }
}

fn increment_count(counts: &mut Vec<CountedName>, name: String) {
    if let Some(row) = counts.iter_mut().find(|row| row.name == name) {
        row.count += 1;
    } else {
        counts.push(CountedName { name, count: 1 });
    }
}

fn increment_mcp_count(counts: &mut Vec<McpServerUsage>, name: String) {
    if let Some(row) = counts.iter_mut().find(|row| row.name == name) {
        row.call_count += 1;
    } else {
        counts.push(McpServerUsage {
            name,
            call_count: 1,
            url_origin: None,
        });
    }
}

fn scan_descriptor_line(line: &str, descriptor: &mut SessionDescriptor) {
    if line.contains("\"customTitle\":\"") {
        if let Some(title) = captured_json_string_after(line, "\"customTitle\":\"") {
            descriptor.title = Some(title);
        }
    }
    if line.contains("\"type\":\"pr-link\"") {
        if let Some(number) = decimal_after(line, "\"prNumber\":") {
            if !descriptor.pr_numbers.contains(&number) {
                descriptor.pr_numbers.push(number);
            }
        }
    }
    if descriptor.first_message.is_none()
        && line.contains("\"role\":\"user\"")
        && !line.contains("\"content\":\"<command-name>/")
        && !line.contains("\"content\":[")
    {
        if let Some(mut message) = first_user_message_units(line) {
            replace_units(&mut message, &[b'\\' as u16, b'n' as u16], b' ' as u16);
            replace_units(&mut message, &[b'\\' as u16, b'"' as u16], b'"' as u16);
            if message.len() > 3 && message.first() != Some(&(b'<' as u16)) {
                message.truncate(SESSION_TITLE_MAX_UTF16_UNITS);
                descriptor.first_message = Some(message);
            }
        }
    }
}

fn captured_json_string_after(line: &str, marker: &str) -> Option<Vec<u16>> {
    let start = line.find(marker)? + marker.len();
    let value = line[start..].split('"').next()?;
    (!value.is_empty()).then(|| value.encode_utf16().collect())
}

fn decimal_after(line: &str, marker: &str) -> Option<u64> {
    let start = line.find(marker)? + marker.len();
    let digits = line[start..].bytes().take_while(u8::is_ascii_digit).count();
    (digits > 0)
        .then(|| line[start..start + digits].parse().ok())
        .flatten()
}

fn first_user_message_units(line: &str) -> Option<Vec<u16>> {
    let start = line.find("\"role\":\"user\"")?;
    let user_row = line[start..].split('}').next()?;
    let marker = "\"content\":\"";
    let content_start = user_row.rfind(marker)? + marker.len();
    let content = user_row[content_start..].split('"').next()?;
    Some(content.encode_utf16().collect())
}

fn replace_units(units: &mut Vec<u16>, needle: &[u16], replacement: u16) {
    if needle.is_empty() {
        return;
    }
    let mut output = Vec::with_capacity(units.len());
    let mut cursor = 0;
    while cursor < units.len() {
        if units[cursor..].starts_with(needle) {
            output.push(replacement);
            cursor += needle.len();
        } else {
            output.push(units[cursor]);
            cursor += 1;
        }
    }
    *units = output;
}

fn read_mcp_server_origins(cwd: &Path, server_names: &[String]) -> HashMap<String, String> {
    let Ok(bytes) = std::fs::read(cwd.join(".mcp.json")) else {
        return HashMap::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    let Ok(document) = Utf16JsonProjection::parse(&text) else {
        return HashMap::new();
    };
    let Some(servers) = document.get_property("mcpServers") else {
        return HashMap::new();
    };
    server_names
        .iter()
        .filter_map(|name| {
            let config = servers.get_property(name)?;
            let url = config.get_property("url")?.code_units()?;
            let url = String::from_utf16_lossy(url);
            Some((name.clone(), url_origin(&url)?))
        })
        .collect()
}

/// Source `new URL(url).origin`, including WHATWG URL normalization and
/// credential/path/query removal.
fn url_origin(input: &str) -> Option<String> {
    url::Url::parse(input)
        .ok()
        .map(|url| url.origin().ascii_serialization())
}

fn git_output(cwd: &Path, arguments: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(cwd)
        .output()
        .ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .trim_matches(ecmascript_whitespace)
            .to_owned()
    })
}

fn repository_display_from_remote(remote: &str) -> Option<String> {
    let remote = trim_ecmascript_whitespace(remote);
    let path = if let Some((_, remainder)) = remote.split_once("://") {
        let (authority, authority_and_path) = remainder.split_once('/')?;
        if authority.is_empty() {
            return None;
        }
        authority_and_path
    } else if let Some((host, path)) = remote.split_once(':') {
        if host.contains('@') || host.contains('.') || host.contains('[') {
            path
        } else {
            return None;
        }
    } else {
        return None;
    };
    let path = path.split(['?', '#']).next()?.trim_matches('/');
    let mut components = path
        .split('/')
        .filter(|component| !component.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if let Some(last) = components.last_mut() {
        if let Some(stem) = last.strip_suffix(".git") {
            *last = stem.to_owned();
        }
    }
    (!components.is_empty()).then(|| components.join("/"))
}

fn cwd_basename(cwd: &Path) -> String {
    cwd.file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_owned()
}

fn trim_ecmascript_whitespace(value: &str) -> &str {
    value.trim_matches(ecmascript_whitespace)
}

fn ecmascript_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    ) || ('\u{2000}'..='\u{200A}').contains(&character)
}

fn format_usage_data(data: &UsageData) -> String {
    let mut fields = Vec::new();
    if let Some(generated_by) = &data.generated_by {
        fields.push(format!(
            "  \"generatedBy\": {}",
            quote_utf16(&generated_by.encode_utf16().collect::<Vec<_>>())
        ));
    }
    fields.push(format!(
        "  \"currentRepo\": {}",
        quote_str(&data.current_repo)
    ));
    fields.push(format!("  \"windowDays\": {}", data.window_days));
    fields.push(format!("  \"sessionCount\": {}", data.session_count));
    fields.push(format!(
        "  \"slashCommands\": {}",
        format_slash_commands(&data.slash_commands)
    ));
    fields.push(format!(
        "  \"mcpServers\": {}",
        format_mcp_servers(&data.mcp_servers)
    ));
    fields.push(format!(
        "  \"sessionDescriptors\": {}",
        format_session_descriptors(&data.session_descriptors)
    ));
    format!("{{\n{}\n}}", fields.join(",\n"))
}

fn format_slash_commands(rows: &[CountedName]) -> String {
    format_object_array(rows, |row| {
        format!(
            "      \"name\": {},\n      \"count\": {}",
            quote_str(&format!("/{}", row.name)),
            row.count
        )
    })
}

fn format_mcp_servers(rows: &[McpServerUsage]) -> String {
    format_object_array(rows, |row| {
        let mut fields = vec![
            format!("      \"name\": {}", quote_str(&row.name)),
            format!("      \"callCount\": {}", row.call_count),
        ];
        if let Some(origin) = &row.url_origin {
            fields.push(format!("      \"urlOrigin\": {}", quote_str(origin)));
        }
        fields.join(",\n")
    })
}

fn format_session_descriptors(rows: &[SessionDescriptor]) -> String {
    if rows.is_empty() {
        return "[]".to_owned();
    }
    let objects = rows
        .iter()
        .map(|row| {
            let mut fields = vec![format!(
                "      \"prNumbers\": {}",
                format_number_array(&row.pr_numbers)
            )];
            if let Some(title) = &row.title {
                fields.push(format!("      \"title\": {}", quote_utf16(title)));
            }
            if let Some(first_message) = &row.first_message {
                fields.push(format!(
                    "      \"firstMessage\": {}",
                    quote_utf16(first_message)
                ));
            }
            format!("    {{\n{}\n    }}", fields.join(",\n"))
        })
        .collect::<Vec<_>>();
    format!("[\n{}\n  ]", objects.join(",\n"))
}

fn format_number_array(numbers: &[u64]) -> String {
    if numbers.is_empty() {
        return "[]".to_owned();
    }
    format!(
        "[\n{}\n      ]",
        numbers
            .iter()
            .map(|number| format!("        {number}"))
            .collect::<Vec<_>>()
            .join(",\n")
    )
}

fn format_object_array<T>(rows: &[T], format_row: impl Fn(&T) -> String) -> String {
    if rows.is_empty() {
        return "[]".to_owned();
    }
    let objects = rows
        .iter()
        .map(|row| format!("    {{\n{}\n    }}", format_row(row)))
        .collect::<Vec<_>>();
    format!("[\n{}\n  ]", objects.join(",\n"))
}

fn quote_str(value: &str) -> String {
    quote_utf16(&value.encode_utf16().collect::<Vec<_>>())
}

fn quote_utf16(units: &[u16]) -> String {
    let mut output = String::with_capacity(units.len() + 2);
    output.push('"');
    let mut index = 0;
    while index < units.len() {
        let unit = units[index];
        match unit {
            0x0022 => output.push_str("\\\""),
            0x005C => output.push_str("\\\\"),
            0x0008 => output.push_str("\\b"),
            0x0009 => output.push_str("\\t"),
            0x000A => output.push_str("\\n"),
            0x000C => output.push_str("\\f"),
            0x000D => output.push_str("\\r"),
            0x0000..=0x001F => {
                let _ = write!(output, "\\u{unit:04x}");
            }
            0xD800..=0xDBFF
                if index + 1 < units.len() && (0xDC00..=0xDFFF).contains(&units[index + 1]) =>
            {
                let high = u32::from(unit) - 0xD800;
                let low = u32::from(units[index + 1]) - 0xDC00;
                let scalar = 0x1_0000 + (high << 10) + low;
                output.push(char::from_u32(scalar).expect("valid UTF-16 surrogate pair"));
                index += 1;
            }
            0xD800..=0xDFFF => {
                let _ = write!(output, "\\u{unit:04x}");
            }
            _ => output.push(char::from_u32(u32::from(unit)).expect("non-surrogate UTF-16 unit")),
        }
        index += 1;
    }
    output.push('"');
    output
}

fn error_projection(message: &str) -> Utf16JsonProjection {
    message.into()
}

const DEFAULT_GUIDE_TEMPLATE: &str = r###"# Welcome to [Team Name]

## How We Use LingXi

Based on [name]'s usage over the last [N] days:

Work Type Breakdown:
  [Category 1]  [ascii bar]  [N]%
  [Category 2]  [ascii bar]  [N]%
  [Category 3]  [ascii bar]  [N]%
  ...

Top Skills & Commands:
  [/command]  [ascii bar]  [N]x/month
  ...

Top MCP Servers:
  [Server]  [ascii bar]  [N] calls
  ...

## Your Setup Checklist

### Codebases
- [ ] [repo-name] — [repo url]
...

### MCP Servers to Activate
- [ ] [Server] — [what it's for]. [How to get access]
...

### Skills to Know About
- [/command] — [what it does, when the team uses it]
...

## Team Tips

_TODO_

## Get Started

_TODO_

<!-- INSTRUCTION FOR LINGXI: A new teammate just pasted this guide for how the
team uses LingXi. You're their onboarding buddy — warm, conversational,
not lecture-y.

Open with a warm welcome — include the team name from the title. Then: "Your
teammate uses LingXi for [list all the work types]. Let's get you started."

Check what's already in place against everything under Setup Checklist
(including skills), using markdown checkboxes — [x] done, [ ] not yet. Lead
with what they already have. One sentence per item, all in one message.

Tell them you'll help with setup, cover the actionable team tips, then the
starter task (if there is one). Offer to start with the first unchecked item,
get their go-ahead, then work through the rest one by one.

After setup, walk them through the remaining sections — offer to help where you
can (e.g. link to channels), and just surface the purely informational bits.

Don't invent sections or summaries that aren't in the guide. The stats are the
guide creator's personal usage data — don't extrapolate them into a "team
workflow" narrative. -->"###;

const DEFAULT_SYSTEM_PROMPT: &str = r###"You are helping a power user generate an onboarding guide for teammates who are new to LingXi. The guide will live in the team's onboarding docs and can be pasted into LingXi for an interactive walkthrough.

You're co-authoring this with them — collaborative and helpful, like a teammate who's done this before and is happy to share.

## Usage data (last {{WINDOW_DAYS}} days)

This was scanned from the guide creator's local LingXi transcripts:

```json
{{USAGE_DATA}}
```

## Your task

Before anything else — including before thinking through the classification — output exactly this line as your first visible text:

> Looking at how you've used LingXi over the last {{WINDOW_DAYS}} days to put together an onboarding guide for teammates new to LingXi.

This must come before any extended thinking about session descriptors. The guide creator is staring at a blank screen until you do. Classification is step 2, not step 1.

Generate the guide immediately, then ask for revisions. Don't wait for answers first — it's easier for the guide creator to edit a concrete draft than answer abstract questions.

1. **Output the acknowledgment line above.** No thinking, no classification, no tool calls before this. One line, then move on.

2. **Derive the work-type breakdown.** Read the `sessionDescriptors` array — each entry describes one session via its title, any linked code reviews (`prNumbers`), and first user message. Classify each session into one of these task types:

   - **build_feature** — new functionality, scripts, tools, config/CI/env setup
   - **debug_fix** — investigating and fixing bugs
   - **improve_quality** — refactoring, tests, cleanup, code review
   - **analyze_data** — queries, metrics, number crunching
   - **plan_design** — architecture, approach, strategy, understanding unfamiliar code, design review
   - **prototype** — spikes, POCs, throwaway exploration
   - **write_docs** — PRDs, RFCs, READMEs, design docs, copy/doc review

   Categories describe the *type of task*, not the project or domain — a teammate on any project should recognize them. Review sessions belong with whatever's being reviewed: code review is improve_quality, doc review is write_docs, design review is plan_design. Most sessions fit the list; only invent a new category if it's genuinely a different type of task. Pick the top 3-5 with rough percentages. First messages alone are usually enough; titles and code-review links are enrichment. If first messages are uninformative, use tool and MCP counts as a weak hint. If there are ~0 sessions, leave the breakdown as a TODO.

   In the rendered guide, display categories with spaces and title case (e.g. "Build Feature" not "build_feature").

3. **Gather the remaining pieces.** For repos, start with `currentRepo` and check the workspace for sibling repo directories. For MCP server setup, use each entry's `name` (and `urlOrigin` where present) to infer what the server does and how a teammate would get access. Leave the Team Tips and Get Started sections as TODO placeholders — you'll ask for these in Review and fill them in after.

4. **Write the guide to `ONBOARDING.md`** following this template:

```
{{GUIDE_TEMPLATE}}
```

   Fill in real numbers from the usage data (not placeholders). Use `generatedBy` for the name; if it's missing, omit the name. Ascii bar charts: `█` for filled, `░` for empty, 20 chars wide. Keep the HTML comment instruction at the bottom exactly as shown.

5. **Render the guide in a code block, then close out the first turn.** You're co-authoring this guide with the guide creator — frame the follow-up as collaboration, not corrections.

   After the code block, add a `---` horizontal rule and a `**Review**` heading so the guide is visually separated from your questions. Under the heading, number these three questions:

   1. "I went with '[X]' for the team name — let me know if that sounds right." (or if you couldn't tell: "What's the team name? I'll add it in.")
   2. Is there a starter task for someone new to LingXi? (ticket or doc link — optional)
   3. Any team tips you'd tell a new teammate that aren't already in LINGXI.md?

   After they answer, update `ONBOARDING.md` with their team name, tips, and starter task. Then close with this exact line (not numbered, not paraphrased):

   Saved to `ONBOARDING.md`. Drop it in your team docs and channels — when a new teammate pastes it into LingXi, they get a guided onboarding tour from there.

   Apply any edits they come back with to the file."###;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::fs::{self, File, OpenOptions};
    use std::time::Duration;

    fn available() -> TeamOnboardingCommandAvailability {
        TeamOnboardingCommandAvailability {
            workspace_available: Some(true),
            allow_team_onboarding: Some(true),
        }
    }

    #[test]
    fn onboarding_templates_use_the_product_identity_and_memory_file() {
        for template in [DEFAULT_GUIDE_TEMPLATE, DEFAULT_SYSTEM_PROMPT] {
            assert!(!template.contains("Claude"));
            assert!(!template.contains("CLAUDE.md"));
        }
        assert!(DEFAULT_GUIDE_TEMPLATE.contains("## How We Use LingXi"));
        assert!(DEFAULT_SYSTEM_PROMPT.contains("already in LINGXI.md"));
    }

    #[test]
    fn onboarding_marker_is_persisted_before_a_failing_usage_scan() {
        let root = tempfile::tempdir().unwrap();
        let config_path = root.path().join(".lingxi.json");
        fs::write(&config_path, r#"{"unrelated":"\ud800"}"#).unwrap();
        let expected_scan_error = error_projection("scan failed");

        let result = scan_project_usage_after_marker(Some(&config_path), 1_765_432_100_987, || {
            Err::<(), _>(expected_scan_error.clone())
        });

        assert!(result.is_err());
        let source = fs::read_to_string(&config_path).unwrap();
        let projected = Utf16JsonProjection::parse(&source).unwrap();
        assert_eq!(
            projected.number_at("/teamOnboardingLastUsedAt"),
            Some(1_765_432_100_987.0)
        );
        assert_eq!(
            projected
                .get_property("unrelated")
                .and_then(|value| value.code_units())
                .unwrap(),
            &[0xD800]
        );
    }

    #[test]
    fn custom_lingxi_home_never_selects_the_ambient_global_config() {
        let root = tempfile::tempdir().unwrap();
        let ambient_home = root.path().join("ambient/.lingxi");
        let custom_home = root.path().join("mobile-app/private-data");
        let ambient_path = root.path().join("ambient/.lingxi.json");

        let selected_path = global_config_path_for_home(
            Some(ambient_path.clone()),
            Some(ambient_home),
            &custom_home,
        );
        assert_eq!(selected_path, None);
        scan_project_usage_after_marker(selected_path.as_deref(), 1_765_432_100_987, || Ok(()))
            .unwrap();
        assert!(!ambient_path.exists());
    }

    #[tokio::test]
    async fn builds_local_usage_prompt_with_redacted_remote_metadata() {
        let root = tempfile::tempdir().unwrap();
        let lingxi_home = root.path().join("home");
        let cwd = root.path().join("team-project");
        fs::create_dir_all(&cwd).unwrap();
        let git_init = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&cwd)
            .status()
            .expect("git is installed for this local metadata fixture");
        assert!(git_init.success());
        assert!(Command::new("git")
            .args(["config", "user.name", "Local Teammate"])
            .current_dir(&cwd)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                "https://git-user:secret-token@github.example/team/repo.git",
            ])
            .current_dir(&cwd)
            .status()
            .unwrap()
            .success());

        fs::write(
            cwd.join(".mcp.json"),
            r#"{"unrelated":"\ud800","mcpServers":{"local":{"url":"https://user:secret@example.test:8443/path?token=hidden"},"lone-path":{"url":"https://good.test/\ud800"}}}"#,
        )
        .unwrap();
        let project_dir = lingxi_home
            .join("projects")
            .join(session::jsonl::project_dir_name(cwd.to_str().unwrap()));
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(
            project_dir.join("recent.jsonl"),
            concat!(
                "{\"role\":\"user\",\"content\":\"Please fix the parser for the Claude API\"}\n",
                "{\"content\":\"<command-name>/review</command-name> <command-name>/review</command-name> <command-name>/commit</command-name>\"}\n",
                "{\"type\":\"tool_use\",\"name\":\"mcp__local__search\"}\n",
                "{\"type\":\"tool_use\",\"name\":\"mcp__local__read\"}\n",
                "{\"type\":\"tool_use\",\"name\":\"mcp__lone-path__find\"}\n",
                "{\"type\":\"pr-link\",\"prNumber\":42}\n",
                "{\"customTitle\":\"Parser review\"}\n"
            ),
        )
        .unwrap();

        let old_path = project_dir.join("old.jsonl");
        fs::write(
            &old_path,
            "{\"role\":\"user\",\"content\":\"expired session\"}\n",
        )
        .unwrap();
        let old_file = File::open(&old_path).unwrap();
        old_file
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(SystemTime::now() - Duration::from_secs(31 * 24 * 60 * 60)),
            )
            .unwrap();

        let oversized_path = project_dir.join("oversized.jsonl");
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&oversized_path)
            .unwrap()
            .set_len(TRANSCRIPT_MAX_BYTES + 1)
            .unwrap();

        let provider =
            LocalTeamOnboardingPromptProvider::new(lingxi_home.clone(), cwd.clone(), available());
        let prompt = provider.generate_prompt().await.unwrap();
        let prompt = prompt.string_at("").expect("root string prompt");
        assert!(prompt.starts_with("You are helping a power user generate an onboarding guide"));
        assert!(!prompt.contains("{{WINDOW_DAYS}}"));
        assert!(!prompt.contains("{{GUIDE_TEMPLATE}}"));
        assert!(!prompt.contains("{{USAGE_DATA}}"));

        let data = prompt
            .split_once("## Usage data (last 30 days)\n\nThis was scanned from the guide creator's local LingXi transcripts:\n\n```json\n")
            .unwrap()
            .1
            .split_once("\n```")
            .unwrap()
            .0;
        let data: Value = serde_json::from_str(data).unwrap();
        assert_eq!(data["generatedBy"], "Local Teammate");
        assert_eq!(data["currentRepo"], "team/repo");
        assert_eq!(data["windowDays"], WINDOW_DAYS);
        assert_eq!(data["sessionCount"], 1);
        assert_eq!(data["slashCommands"][0]["name"], "/review");
        assert_eq!(data["slashCommands"][0]["count"], 2);
        assert_eq!(data["slashCommands"][1]["name"], "/commit");
        assert_eq!(data["mcpServers"][0]["name"], "local");
        assert_eq!(data["mcpServers"][0]["callCount"], 2);
        assert_eq!(
            data["mcpServers"][0]["urlOrigin"],
            "https://example.test:8443"
        );
        assert_eq!(data["mcpServers"][1]["name"], "lone-path");
        assert_eq!(data["mcpServers"][1]["urlOrigin"], "https://good.test");
        assert_eq!(data["sessionDescriptors"][0]["title"], "Parser review");
        assert_eq!(data["sessionDescriptors"][0]["prNumbers"][0], 42);
        assert_eq!(
            data["sessionDescriptors"][0]["firstMessage"],
            "Please fix the parser for the Claude API"
        );
        assert!(!prompt.contains("secret-token"));
        assert!(!prompt.contains("secret@example.test"));
        assert!(!prompt.contains("token=hidden"));
    }

    #[test]
    fn usage_json_escapes_a_surrogate_created_by_the_native_utf16_slice() {
        let prefix = "a".repeat(199);
        let line = format!(r#"{{"role":"user","content":"{prefix}😀 tail"}}"#);
        let mut message = first_user_message_units(&line).unwrap();
        message.truncate(SESSION_TITLE_MAX_UTF16_UNITS);
        assert_eq!(message.len(), SESSION_TITLE_MAX_UTF16_UNITS);
        assert_eq!(message.last(), Some(&0xD83D));
        let descriptor = SessionDescriptor {
            first_message: Some(message),
            ..SessionDescriptor::default()
        };
        let usage = UsageData {
            current_repo: "repo".into(),
            window_days: WINDOW_DAYS,
            session_count: 1,
            session_descriptors: vec![descriptor],
            ..UsageData::default()
        };
        let serialized = format_usage_data(&usage);
        assert!(serialized.contains(&format!(
            "\"firstMessage\": {}",
            quote_utf16(&[vec![b'a' as u16; 199], vec![0xD83D]].concat())
        )));
        assert!(serialized.ends_with("\n}"));
    }

    #[test]
    fn mcp_origins_strip_credentials_paths_and_default_ports() {
        assert_eq!(
            url_origin("https://user:pass@EXAMPLE.test:443/private?key=x"),
            Some("https://example.test".into())
        );
        assert_eq!(
            url_origin("http://localhost:80/mcp"),
            Some("http://localhost".into())
        );
        assert_eq!(url_origin("custom:opaque"), Some("null".into()));
        assert_eq!(url_origin("/relative/path"), None);
        assert_eq!(
            url_origin("https://example.test:"),
            Some("https://example.test".into())
        );
        assert_eq!(
            url_origin("https://bücher.example/private"),
            Some("https://xn--bcher-kva.example".into())
        );
        assert_eq!(
            url_origin("https://127.1/mcp"),
            Some("https://127.0.0.1".into())
        );
        assert_eq!(
            url_origin("blob:https://example.test/identifier"),
            Some("https://example.test".into())
        );
        assert_eq!(url_origin("data:text/plain,secret"), Some("null".into()));
        assert_eq!(
            repository_display_from_remote("file:///private/team/repo.git"),
            None
        );
    }

    #[test]
    fn descriptor_cap_ranks_only_after_more_than_sixty_rows() {
        let mut descriptors = (0..=MAX_SESSION_DESCRIPTORS)
            .map(|index| SessionDescriptor {
                first_message: Some(vec![index as u16]),
                ..SessionDescriptor::default()
            })
            .collect::<Vec<_>>();
        descriptors[MAX_SESSION_DESCRIPTORS].title = Some("highest rank".encode_utf16().collect());

        cap_session_descriptors(&mut descriptors);

        assert_eq!(descriptors.len(), MAX_SESSION_DESCRIPTORS);
        let expected_title = "highest rank".encode_utf16().collect::<Vec<_>>();
        assert_eq!(descriptors[0].title.as_ref(), Some(&expected_title));
        assert_eq!(descriptors[1].first_message.as_deref(), Some(&[0][..]));
        assert_eq!(
            descriptors.last().unwrap().first_message.as_deref(),
            Some(&[58][..])
        );
    }
}
