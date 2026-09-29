//! `/skill-doctor` — show which live skills are unused and costing context.
//!
//! Claude Code 2.1.220 changed this command from its earlier filesystem-only
//! view. The production path now consumes the merged command registry so
//! plugin and MCP prompt skills appear, reports their one-line prompt-listing
//! cost, and scans the last seven days of local transcript usage attribution.
//! The filesystem loader remains only as an embedding/test fallback when no
//! live registry was supplied.
//!
//! ## Why this handler bypasses [`lingxi_core::host::OrchestratorHandle`]
//!
//! `OrchestratorHandle` (see `platform-api/src/orchestrator.rs`) has no
//! command-dispatch-history or skill-invocation-history surface — nothing
//! resembling claude-code's `t.options.commands` (the live, already-merged
//! command list with per-command `source`/`pluginInfo`) or its usage-tracking
//! store (`U_l`). Extending the trait for a single new command is
//! disproportionate, and there is no existing seam to reuse. Instead — like
//! [`crate::builtins::skills::SkillsHandler::with_all_roots`] — this handler is
//! constructed with explicit filesystem roots and loads its own view of "what
//! commands/skills are on disk" directly through [`crate::markdown_loader`],
//! bypassing both the handle and the live [`crate::CommandRegistry`].
//!
//! Usage is read from the shared `crate::skill_usage` store. The registry
//! dispatcher records every successful user/project/local Markdown expansion,
//! so interactive and headless invocations feed the same counters this handler
//! reports.

use crate::markdown_loader::{
    build_markdown_command, build_skill_command, load_command_markdown_files,
    load_skill_markdown_files_with_roots,
};
use crate::model::{
    BuiltinCommandHandler, CommandResult, CommandSource, SlashCommand, SlashCommandKind,
};
use crate::parser::ParsedSlashCommand;
use crate::registry::CommandRegistry;
use crate::skill_usage::{read_skill_usage, SkillUsageRecord};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

const DESCRIPTION: &str = "Show which loaded skills are unused and costing context";
const HEADER: &str = "Skills loaded this session";
const EMPTY_TABLE: &str = "  (no skills loaded)";
const ALL_USED: &str = "All loaded skills have been used at least once.";

/// Seconds per day, for turning a unix-timestamp delta into whole days.
const SECONDS_PER_DAY: i64 = 86_400;

/// Explicit filesystem roots this handler was constructed with. Mirrors
/// [`crate::builtins::skills::SkillsHandler`]'s private `SkillsRoots`.
#[derive(Debug, Clone, Default)]
struct SkillDoctorRoots {
    cwd: PathBuf,
    lingxi_home: PathBuf,
    managed_dir: Option<PathBuf>,
    additional_skill_dirs: Vec<PathBuf>,
}

/// `/skill-doctor` handler — renders unused-skill/command diagnostics.
#[derive(Clone)]
pub struct SkillDoctorHandler {
    roots: SkillDoctorRoots,
    registry: Option<Arc<RwLock<CommandRegistry>>>,
}

impl SkillDoctorHandler {
    /// Construct a handler pinned to explicit `(cwd, lingxi_home, managed_dir,
    /// additional_skill_dirs)` roots — the same four parameters as
    /// [`crate::builtins::skills::SkillsHandler::with_all_roots`], passed by the
    /// composition root.
    #[must_use]
    pub fn new(
        cwd: PathBuf,
        lingxi_home: PathBuf,
        managed_dir: Option<PathBuf>,
        additional_skill_dirs: Vec<PathBuf>,
    ) -> Self {
        Self {
            roots: SkillDoctorRoots {
                cwd,
                lingxi_home,
                managed_dir,
                additional_skill_dirs,
            },
            registry: None,
        }
    }

    /// Construct the production handler over the composition root's live,
    /// already-merged command registry.
    #[must_use]
    pub fn with_registry(
        registry: Arc<RwLock<CommandRegistry>>,
        cwd: PathBuf,
        lingxi_home: PathBuf,
        managed_dir: Option<PathBuf>,
        additional_skill_dirs: Vec<PathBuf>,
    ) -> Self {
        let mut handler = Self::new(cwd, lingxi_home, managed_dir, additional_skill_dirs);
        handler.registry = Some(registry);
        handler
    }
}

#[async_trait]
impl BuiltinCommandHandler for SkillDoctorHandler {
    // Upstream's `load:()=>Promise.resolve({call:async(e,t)=>{...Bcf(e,t)}})`
    // never reads its raw-args parameter `e`; `args` is ignored here too.
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        let loaded = match self.registry.as_ref() {
            Some(registry) => load_registry_entries(&self.roots, registry, now_unix).await,
            None => load_entries(&self.roots, now_unix).await,
        };
        match loaded {
            Ok(mut entries) => {
                sort_entries(&mut entries);
                CommandResult::Done {
                    display: Some(render_skill_doctor(&entries)),
                }
            }
            // TS: `catch(n){return ke(...),{type:"text",value:`Couldn't compute
            // skill usage. Run with --debug for details. (${he(n)})`}}`
            Err(e) => CommandResult::Done {
                display: Some(format!(
                    "Couldn't compute skill usage. Run with --debug for details. ({e})"
                )),
            },
        }
    }

    fn name(&self) -> &str {
        "skill-doctor"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }
}

/// One row of the rendered table.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillDoctorEntry {
    name: String,
    source: String,
    owner: SkillOwner,
    listing_tokens: Option<u64>,
    week_tokens: Option<u64>,
    usage_count: u64,
    days_since_use: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SkillOwner {
    Settings,
    Plugin(String),
    Mcp(String),
}

/// Record one real dispatch of a Markdown/skill-kind command: increments its
/// usage count and stamps `last_used_unix` to now.
pub fn record_skill_usage(lingxi_home: &Path, name: &str) -> Result<(), String> {
    crate::skill_usage::record_skill_usage(lingxi_home, name)
}

/// TS `a.source==="bundled"||a.source==="builtin"||a.source==="policySettings"||a.source==="plugin"`
/// → `continue` (skip). Kept: user, project, and (forward-compatibility, not
/// currently produced by either loader) local.
fn is_countable_source(source: CommandSource) -> bool {
    matches!(
        source,
        CommandSource::Settings(lingxi_core::types::SettingsScope::User)
            | CommandSource::Settings(lingxi_core::types::SettingsScope::Project)
            | CommandSource::Settings(lingxi_core::types::SettingsScope::Local)
    )
}

fn source_label(source: CommandSource) -> &'static str {
    match source {
        CommandSource::Builtin => "builtin",
        CommandSource::Settings(lingxi_core::types::SettingsScope::User) => "user",
        CommandSource::Settings(lingxi_core::types::SettingsScope::Project) => "project",
        CommandSource::Settings(lingxi_core::types::SettingsScope::Local) => "local",
        CommandSource::Plugin => "plugin",
        CommandSource::Settings(lingxi_core::types::SettingsScope::Managed) => "managed",
        CommandSource::Mcp => "mcp",
        CommandSource::Bundled => "bundled",
    }
}

/// `lingxi_home` is `~/.lingxi`; its parent is the upward-walk stop boundary
/// (`home`) the loaders need. Same convention as
/// `skill_api::listing::project_skills_dirs`'s `lingxi_home.parent()`.
fn derive_home(lingxi_home: &Path) -> PathBuf {
    lingxi_home
        .parent()
        .map_or_else(|| lingxi_home.to_path_buf(), Path::to_path_buf)
}

fn days_between(last_used_unix: i64, now_unix: i64) -> u64 {
    let delta = now_unix.saturating_sub(last_used_unix);
    if delta <= 0 {
        0
    } else {
        u64::try_from(delta / SECONDS_PER_DAY).unwrap_or(0)
    }
}

fn make_entry(
    name: String,
    display_name: String,
    source: String,
    owner: SkillOwner,
    listing_tokens: Option<u64>,
    usage: &HashMap<String, SkillUsageRecord>,
    weekly_tokens: &HashMap<String, u64>,
    now_unix: i64,
) -> SkillDoctorEntry {
    let record = usage.get(&name).or_else(|| usage.get(&display_name));
    let usage_count = record.map_or(0, |r| r.count);
    let days_since_use = record
        .and_then(|r| r.last_used_unix)
        .map(|last| days_between(last, now_unix));
    SkillDoctorEntry {
        name: display_name.clone(),
        source,
        owner,
        listing_tokens,
        week_tokens: weekly_tokens
            .get(&name)
            .or_else(|| weekly_tokens.get(&display_name))
            .copied(),
        usage_count,
        days_since_use,
    }
}

/// Enumerate the union of custom `.lingxi/commands/**.md` markdown commands
/// and directory-format `.lingxi/skills/<name>/SKILL.md` skills, filtered to
/// `CommandSource::{User, Project, Local}`, joined with usage stats.
///
/// Both loaders already return `managed, user, project[, additional]` order
/// (`managed > user > project` priority); after dropping `Managed` the
/// remaining order is `user` before `project`, so a first-wins dedup-by-name
/// (mirroring [`crate::builtins::custom_commands::load_and_register_custom_commands`]'s
/// own `HashSet` guard) reproduces the same "user overrides project"
/// precedence without needing the live [`crate::CommandRegistry`].
async fn load_entries(
    roots: &SkillDoctorRoots,
    now_unix: i64,
) -> Result<Vec<SkillDoctorEntry>, String> {
    let home = derive_home(&roots.lingxi_home);
    let usage = read_skill_usage(&roots.lingxi_home)?;
    let weekly_tokens = read_weekly_attribution_tokens(&roots.lingxi_home, now_unix);

    // `load_command_markdown_files` takes `managed_dir: &Path` (not
    // `Option`); when no managed root is configured, point it at a directory
    // that cannot exist so the loader's `read_dir` fails closed (returns
    // nothing) rather than accidentally resolving a real path.
    let no_managed_dir = roots.lingxi_home.join(".skill-doctor-no-managed-dir");
    let managed_for_commands = roots.managed_dir.clone().unwrap_or(no_managed_dir);

    let command_files =
        load_command_markdown_files(&roots.cwd, &roots.lingxi_home, &managed_for_commands, &home)
            .await;
    let skill_files = load_skill_markdown_files_with_roots(
        &roots.cwd,
        &roots.lingxi_home,
        roots.managed_dir.as_deref(),
        &home,
        &roots.additional_skill_dirs,
    )
    .await;

    let mut seen: HashSet<String> = HashSet::new();
    let mut entries = Vec::new();
    for file in &skill_files {
        if !is_countable_source(file.source) {
            continue;
        }
        let cmd = build_skill_command(file, file.source);
        if seen.insert(cmd.name.clone()) {
            let listing_tokens =
                (!cmd.disable_model_invocation).then(|| estimate_listing_tokens(&cmd));
            entries.push(make_entry(
                cmd.name.clone(),
                cmd.name,
                source_label(file.source).to_string(),
                SkillOwner::Settings,
                listing_tokens,
                &usage,
                &weekly_tokens,
                now_unix,
            ));
        }
    }
    for file in &command_files {
        if !is_countable_source(file.source) {
            continue;
        }
        let cmd = build_markdown_command(file, file.source);
        if seen.insert(cmd.name.clone()) {
            let listing_tokens =
                (!cmd.disable_model_invocation).then(|| estimate_listing_tokens(&cmd));
            entries.push(make_entry(
                cmd.name.clone(),
                cmd.name,
                source_label(file.source).to_string(),
                SkillOwner::Settings,
                listing_tokens,
                &usage,
                &weekly_tokens,
                now_unix,
            ));
        }
    }

    Ok(entries)
}

async fn load_registry_entries(
    roots: &SkillDoctorRoots,
    registry: &Arc<RwLock<CommandRegistry>>,
    now_unix: i64,
) -> Result<Vec<SkillDoctorEntry>, String> {
    let usage = read_skill_usage(&roots.lingxi_home)?;
    let weekly_tokens = read_weekly_attribution_tokens(&roots.lingxi_home, now_unix);
    let commands: Vec<SlashCommand> = registry
        .read()
        .await
        .list_all()
        .into_iter()
        .cloned()
        .collect();
    let mut entries = Vec::new();

    for command in commands {
        let (display_name, source, owner) = match &command.kind {
            SlashCommandKind::Markdown { .. }
                if matches!(
                    command.source,
                    CommandSource::Settings(lingxi_core::types::SettingsScope::User)
                        | CommandSource::Settings(lingxi_core::types::SettingsScope::Project)
                        | CommandSource::Settings(lingxi_core::types::SettingsScope::Local)
                ) =>
            {
                (
                    command.name.clone(),
                    source_label(command.source).to_string(),
                    SkillOwner::Settings,
                )
            }
            SlashCommandKind::Plugin { .. } => {
                let (plugin, unqualified) = split_qualified_name(&command.name);
                (
                    unqualified.to_string(),
                    plugin.to_string(),
                    SkillOwner::Plugin(plugin.to_string()),
                )
            }
            SlashCommandKind::Mcp { .. } => {
                let (server, unqualified) = split_qualified_name(&command.name);
                (
                    unqualified.to_string(),
                    server.to_string(),
                    SkillOwner::Mcp(server.to_string()),
                )
            }
            _ => continue,
        };

        let listing_tokens =
            (!command.disable_model_invocation).then(|| estimate_listing_tokens(&command));
        entries.push(make_entry(
            command.name,
            display_name,
            source,
            owner,
            listing_tokens,
            &usage,
            &weekly_tokens,
            now_unix,
        ));
    }

    Ok(entries)
}

fn split_qualified_name(name: &str) -> (&str, &str) {
    name.split_once(':').unwrap_or(("", name))
}

fn estimate_listing_tokens(command: &SlashCommand) -> u64 {
    let mut chars = command.name.chars().count() + command.description.chars().count() + 4;
    if let Some(when) = command.when_to_use.as_deref() {
        chars = chars.saturating_add(when.chars().count() + 3);
    }
    u64::try_from(chars.saturating_add(3) / 4)
        .unwrap_or(u64::MAX)
        .max(1)
}

const WEEK_SECONDS: i64 = 7 * SECONDS_PER_DAY;
const MAX_TRANSCRIPT_BYTES: u64 = 64 * 1024 * 1024;

/// Sum assistant-message token usage attributed to a skill during the last
/// seven days. Transcript corruption is isolated to the affected line/file;
/// `/skill-doctor` remains useful even after an interrupted JSONL write.
fn read_weekly_attribution_tokens(config_home: &Path, now_unix: i64) -> HashMap<String, u64> {
    let mut totals = HashMap::new();
    let projects = config_home.join("projects");
    scan_transcript_dir(&projects, now_unix, &mut totals);
    totals
}

fn scan_transcript_dir(path: &Path, now_unix: i64, totals: &mut HashMap<String, u64>) {
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            scan_transcript_dir(&path, now_unix, totals);
            continue;
        }
        if !file_type.is_file() || path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.len() > MAX_TRANSCRIPT_BYTES {
            continue;
        }
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            accumulate_attributed_usage(&value, now_unix, totals);
        }
    }
}

fn accumulate_attributed_usage(value: &Value, now_unix: i64, totals: &mut HashMap<String, u64>) {
    if value.get("type").and_then(Value::as_str) != Some("assistant") {
        return;
    }
    let Some(timestamp) = value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_unix)
    else {
        return;
    };
    if now_unix.saturating_sub(timestamp) > WEEK_SECONDS || timestamp > now_unix {
        return;
    }
    let Some(skill) = find_string_field(value, "attributionSkill") else {
        return;
    };
    let Some(usage) = value
        .pointer("/message/usage")
        .or_else(|| value.get("usage"))
    else {
        return;
    };
    let tokens = [
        "input_tokens",
        "output_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
        "inputTokens",
        "outputTokens",
        "cacheCreationInputTokens",
        "cacheReadInputTokens",
    ]
    .into_iter()
    .filter_map(|key| usage.get(key).and_then(Value::as_u64))
    .fold(0_u64, u64::saturating_add);
    if tokens > 0 {
        let entry = totals.entry(skill.to_string()).or_default();
        *entry = entry.saturating_add(tokens);
    }
}

fn find_string_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    match value {
        Value::Object(map) => map
            .get(key)
            .and_then(Value::as_str)
            .or_else(|| map.values().find_map(|child| find_string_field(child, key))),
        Value::Array(items) => items.iter().find_map(|child| find_string_field(child, key)),
        _ => None,
    }
}

fn parse_rfc3339_unix(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || bytes.get(10).is_none_or(|b| *b != b'T' && *b != b't')
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return None;
    }
    let number =
        |range: std::ops::Range<usize>| value.get(range).and_then(|part| part.parse::<i64>().ok());
    let year = number(0..4)?;
    let month = number(5..7)?;
    let day = number(8..10)?;
    let hour = number(11..13)?;
    let minute = number(14..16)?;
    let second = number(17..19)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let suffix = &value[19..];
    let offset = if suffix.ends_with('Z') || suffix.ends_with('z') {
        0
    } else {
        let tz_start = suffix.rfind(['+', '-'])?;
        let sign = if suffix.as_bytes().get(tz_start) == Some(&b'-') {
            -1
        } else {
            1
        };
        let tz = suffix.get(tz_start + 1..)?;
        let (hours, minutes) = tz.split_once(':')?;
        sign * (hours.parse::<i64>().ok()? * 3600 + minutes.parse::<i64>().ok()? * 60)
    };

    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(
        days.saturating_mul(86_400)
            .saturating_add(hour * 3600 + minute * 60 + second)
            .saturating_sub(offset),
    )
}

/// TS `r.sort((a,l)=>(l.daysSinceUse??1/0)-(a.daysSinceUse??1/0))` — descending
/// by `daysSinceUse`, `null`/never sorting as `+Infinity` (first). Rust's
/// `sort_by` is stable, matching `Array.prototype.sort`.
fn sort_entries(entries: &mut [SkillDoctorEntry]) {
    entries.sort_by(|a, b| {
        let key = |e: &SkillDoctorEntry| e.days_since_use.unwrap_or(u64::MAX);
        key(b).cmp(&key(a))
    });
}

fn plural(n: u64, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

/// Port of `Fcf`.
fn format_days_since_use(days: Option<u64>) -> String {
    match days {
        None => "never".to_string(),
        Some(0) => "today".to_string(),
        Some(n) => format!("{n} {}", plural(n, "day")),
    }
}

fn render_table(entries: &[SkillDoctorEntry]) -> String {
    if entries.is_empty() {
        return EMPTY_TABLE.to_string();
    }
    let name_width = entries
        .iter()
        .map(|e| e.name.chars().count())
        .max()
        .unwrap_or(0)
        .max(5);
    let source_width = entries
        .iter()
        .map(|e| e.source.chars().count())
        .max()
        .unwrap_or(0)
        .max(6);
    let context_width = entries
        .iter()
        .map(|e| format_token_count(e.listing_tokens).chars().count())
        .max()
        .unwrap_or(0)
        .max(7);
    let week_width = entries
        .iter()
        .map(|e| format_token_count(e.week_tokens).chars().count())
        .max()
        .unwrap_or(0)
        .max(9);

    let mut rows = vec![format!(
        "  {skill:<name_width$}  {source:<source_width$}  {context:>context_width$}  {week:>week_width$}  {uses:>4}  last used",
        skill = "skill",
        source = "source",
        context = "context",
        week = "7d tokens",
        uses = "uses",
    )];
    rows.extend(entries.iter().map(|e| {
        format!(
            "  {name:<name_width$}  {source:<source_width$}  {context:>context_width$}  {week:>week_width$}  {count:>3}\u{00D7}  {days}",
            name = e.name,
            source = e.source,
            context = format_token_count(e.listing_tokens),
            week = format_token_count(e.week_tokens),
            count = e.usage_count,
            days = format_days_since_use(e.days_since_use),
        )
    }));
    rows.join("\n")
}

fn format_token_count(tokens: Option<u64>) -> String {
    tokens.map_or_else(|| "-".to_string(), |value| value.to_string())
}

fn render_skill_doctor(entries: &[SkillDoctorEntry]) -> String {
    let mut lines: Vec<String> = Vec::with_capacity(12);
    lines.push(HEADER.to_string());
    lines.push(String::new());
    lines.push(render_table(entries));

    if !entries.is_empty() {
        lines.push(String::new());
        lines.push(
            "  context = this skill's one-line listing in the system prompt, included every turn"
                .to_string(),
        );
        lines.push(
            "  (dash = not in the current listing, costs nothing; full SKILL.md loads only when it runs)"
                .to_string(),
        );
        lines.push(
            "  7d tokens = tokens attributed to the skill over the last 7 days of sessions on this machine"
                .to_string(),
        );
    }

    let settings_unused = entries
        .iter()
        .filter(|entry| {
            matches!(entry.owner, SkillOwner::Settings)
                && entry.usage_count == 0
                && entry.listing_tokens.is_some()
        })
        .count();
    if settings_unused > 0 {
        let n = u64::try_from(settings_unused).unwrap_or(u64::MAX);
        lines.push(String::new());
        lines.push(format!(
            "{n} {} loaded but never invoked. Each one adds to the system prompt every turn. Disable in /skills, or remove from .lingxi/skills.",
            plural(n, "skill")
        ));
    }

    append_owner_warning(entries, true, &mut lines);
    append_owner_warning(entries, false, &mut lines);

    if !entries.is_empty() && entries.iter().all(|entry| entry.usage_count > 0) {
        lines.push(String::new());
        lines.push(ALL_USED.to_string());
    }
    lines.join("\n")
}

fn append_owner_warning(entries: &[SkillDoctorEntry], plugin: bool, lines: &mut Vec<String>) {
    let mut owners: HashMap<&str, Vec<&SkillDoctorEntry>> = HashMap::new();
    for entry in entries {
        let name = match &entry.owner {
            SkillOwner::Plugin(name) if plugin => Some(name.as_str()),
            SkillOwner::Mcp(name) if !plugin => Some(name.as_str()),
            _ => None,
        };
        if let Some(name) = name {
            owners.entry(name).or_default().push(entry);
        }
    }

    let mut unused_owners = Vec::new();
    let mut unused_count = 0_usize;
    for (owner, owned) in owners {
        if owned.iter().any(|entry| entry.usage_count > 0) {
            continue;
        }
        let count = owned
            .iter()
            .filter(|entry| entry.listing_tokens.is_some())
            .count();
        if count > 0 {
            unused_count = unused_count.saturating_add(count);
            unused_owners.push(owner);
        }
    }
    if unused_count == 0 {
        return;
    }
    unused_owners.sort_unstable();
    let n = u64::try_from(unused_count).unwrap_or(u64::MAX);
    let owner_list = unused_owners.join(", ");
    lines.push(String::new());
    if plugin {
        lines.push(format!(
            "{n} plugin {} loaded but never invoked, from {owner_list}. Each one adds to the system prompt every turn. Plugin skills can't be turned off individually — disable {} in /plugin.",
            plural(n, "skill"),
            if unused_owners.len() == 1 {
                "the plugin"
            } else {
                "those plugins"
            }
        ));
    } else {
        lines.push(format!(
            "{n} MCP {} loaded but never invoked, from {owner_list}. Each one adds to the system prompt every turn. MCP skills live on the server, not on disk — turning {} off in /mcp also removes {} tools.",
            plural(n, "skill"),
            if unused_owners.len() == 1 {
                "that server"
            } else {
                "those servers"
            },
            if unused_owners.len() == 1 {
                "its"
            } else {
                "their"
            }
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CommandFrontmatter, SlashCommandKind};
    use lingxi_core::types::{McpConnectionId, PluginId};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "lingxi-skill-doctor-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp root");
        dir
    }

    fn args() -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "skill-doctor".into(),
            raw_args: String::new(),
            positional_args: vec![],
        }
    }

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn plugin_skill(name: &str, plugin_id: PluginId) -> SlashCommand {
        SlashCommand {
            name: name.to_string(),
            description: format!("Run {name}"),
            source: CommandSource::Plugin,
            kind: SlashCommandKind::Plugin {
                plugin_id,
                file_path: PathBuf::from("SKILL.md"),
                frontmatter: CommandFrontmatter::default(),
                prompt_template: "body".to_string(),
            },
            loaded_from: Some("plugin".to_string()),
            ..SlashCommand::default()
        }
    }

    fn mcp_skill(name: &str) -> SlashCommand {
        SlashCommand {
            name: name.to_string(),
            description: format!("Run {name}"),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            kind: SlashCommandKind::Mcp {
                connection_id: McpConnectionId::new(),
                prompt_name: name.split_once(':').unwrap().1.to_string(),
                arguments: Vec::new(),
            },
            loaded_from: Some("mcp".to_string()),
            ..SlashCommand::default()
        }
    }

    #[tokio::test]
    async fn handler_name_and_description() {
        let h = SkillDoctorHandler::new(PathBuf::from("."), PathBuf::from("."), None, Vec::new());
        assert_eq!(h.name(), "skill-doctor");
        assert_eq!(h.description(), DESCRIPTION);
    }

    #[tokio::test]
    async fn empty_state_matches_binary_copy() {
        let root = tmp_root("empty");
        let cwd = root.join("repo");
        let lingxi_home = root.join("home").join(".lingxi");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&lingxi_home).unwrap();

        let h = SkillDoctorHandler::new(cwd, lingxi_home, None, Vec::new());
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s, "Skills loaded this session\n\n  (no skills loaded)");
            }
            other => panic!("expected Done display, got {other:?}"),
        }
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn never_used_project_skill_renders_never_and_unused_line() {
        let root = tmp_root("never-used");
        let cwd = root.join("repo");
        let lingxi_home = root.join("home").join(".lingxi");
        fs::create_dir_all(cwd.join(".git")).unwrap();
        fs::create_dir_all(&lingxi_home).unwrap();
        write(
            &cwd.join(".lingxi")
                .join("skills")
                .join("demo")
                .join("SKILL.md"),
            "---\ndescription: Demo skill\n---\nBody\n",
        );

        let h = SkillDoctorHandler::new(cwd, lingxi_home, None, Vec::new());
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => {
                assert!(
                    s.contains("demo"),
                    "table should list the discovered skill: {s}"
                );
                assert!(
                    s.contains("project"),
                    "source column should read project: {s}"
                );
                assert!(s.contains("never"), "unrecorded usage renders never: {s}");
                assert!(
                    s.contains(
                        "1 skill loaded but never invoked. Each one adds to the system prompt every turn. Disable in /skills, or remove from .lingxi/skills."
                    ),
                    "unused-count line: {s}"
                );
            }
            other => panic!("expected Done display, got {other:?}"),
        }
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn recorded_usage_marks_all_used_and_shows_today() {
        let root = tmp_root("recorded");
        let cwd = root.join("repo");
        let lingxi_home = root.join("home").join(".lingxi");
        fs::create_dir_all(cwd.join(".git")).unwrap();
        fs::create_dir_all(&lingxi_home).unwrap();
        write(
            &cwd.join(".lingxi").join("commands").join("foo.md"),
            "# Foo\n\nDo the foo",
        );
        record_skill_usage(&lingxi_home, "foo").expect("record usage");

        let h = SkillDoctorHandler::new(cwd, lingxi_home, None, Vec::new());
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => {
                assert!(s.contains("foo"), "table should list foo: {s}");
                assert!(
                    s.contains("today"),
                    "just-recorded usage renders today: {s}"
                );
                assert_eq!(
                    s.lines().last(),
                    Some(ALL_USED),
                    "all-used line when every entry has usage: {s}"
                );
            }
            other => panic!("expected Done display, got {other:?}"),
        }
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn live_registry_includes_plugin_and_mcp_skills_and_groups_warnings() {
        let root = tmp_root("live-registry");
        let cwd = root.join("repo");
        let lingxi_home = root.join("home").join(".lingxi");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&lingxi_home).unwrap();

        let plugin_id = PluginId::new();
        let mut registry = CommandRegistry::new();
        registry.register_plugin_commands(
            plugin_id,
            vec![
                plugin_skill("reviewer:scan", plugin_id),
                plugin_skill("reviewer:verify", plugin_id),
            ],
        );
        registry.register_command(mcp_skill("github:review_pr"));
        record_skill_usage(&lingxi_home, "reviewer:scan").unwrap();
        let registry = Arc::new(RwLock::new(registry));

        let handler =
            SkillDoctorHandler::with_registry(registry, cwd, lingxi_home, None, Vec::new());
        let display = match handler.handle(&args()).await {
            CommandResult::Done {
                display: Some(display),
            } => display,
            other => panic!("expected Done display, got {other:?}"),
        };

        assert!(display.contains("scan"));
        assert!(display.contains("verify"));
        assert!(display.contains("reviewer"));
        assert!(display.contains("review_pr"));
        assert!(display.contains("github"));
        assert!(
            !display.contains("plugin skill loaded but never invoked"),
            "one used skill suppresses the plugin-level warning: {display}"
        );
        assert!(
            display.contains("1 MCP skill loaded but never invoked, from github."),
            "unused MCP owner is diagnosed: {display}"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn weekly_attribution_uses_assistant_timestamp_and_all_token_classes() {
        let now = parse_rfc3339_unix("2026-07-29T12:00:00Z").unwrap();
        let recent = serde_json::json!({
            "type": "assistant",
            "timestamp": "2026-07-28T12:00:00Z",
            "message": {
                "attributionSkill": "reviewer:scan",
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 20,
                    "cache_creation_input_tokens": 30,
                    "cache_read_input_tokens": 40
                }
            }
        });
        let stale = serde_json::json!({
            "type": "assistant",
            "timestamp": "2026-07-20T12:00:00Z",
            "attributionSkill": "reviewer:scan",
            "usage": {"input_tokens": 999}
        });
        let mut totals = HashMap::new();
        accumulate_attributed_usage(&recent, now, &mut totals);
        accumulate_attributed_usage(&stale, now, &mut totals);
        assert_eq!(totals.get("reviewer:scan"), Some(&100));
        assert_eq!(
            parse_rfc3339_unix("1970-01-01T00:00:00Z"),
            Some(0),
            "timestamp parser anchors the seven-day window correctly"
        );
    }

    #[tokio::test]
    async fn corrupt_usage_log_falls_back_to_locked_error_string() {
        let root = tmp_root("corrupt");
        let cwd = root.join("repo");
        let lingxi_home = root.join("home").join(".lingxi");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&lingxi_home).unwrap();
        write(&lingxi_home.join("skill_usage.json"), "{not json");

        let h = SkillDoctorHandler::new(cwd, lingxi_home, None, Vec::new());
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => {
                assert!(
                    s.starts_with("Couldn't compute skill usage. Run with --debug for details. ("),
                    "fallback error string: {s}"
                );
            }
            other => panic!("expected Done display, got {other:?}"),
        }
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn sort_puts_never_used_first_then_descending_days() {
        let mut entries = vec![
            SkillDoctorEntry {
                name: "a".into(),
                source: "project".into(),
                owner: SkillOwner::Settings,
                listing_tokens: Some(1),
                week_tokens: None,
                usage_count: 1,
                days_since_use: Some(2),
            },
            SkillDoctorEntry {
                name: "b".into(),
                source: "project".into(),
                owner: SkillOwner::Settings,
                listing_tokens: Some(1),
                week_tokens: None,
                usage_count: 0,
                days_since_use: None,
            },
            SkillDoctorEntry {
                name: "c".into(),
                source: "project".into(),
                owner: SkillOwner::Settings,
                listing_tokens: Some(1),
                week_tokens: None,
                usage_count: 3,
                days_since_use: Some(10),
            },
        ];
        sort_entries(&mut entries);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["b", "c", "a"]);
    }

    #[test]
    fn format_days_matches_locked_labels() {
        assert_eq!(format_days_since_use(None), "never");
        assert_eq!(format_days_since_use(Some(0)), "today");
        assert_eq!(format_days_since_use(Some(1)), "1 day");
        assert_eq!(format_days_since_use(Some(2)), "2 days");
    }

    #[test]
    fn record_skill_usage_increments_and_stamps() {
        let root = tmp_root("record-fn");
        let lingxi_home = root.join("home").join(".lingxi");
        fs::create_dir_all(&lingxi_home).unwrap();

        record_skill_usage(&lingxi_home, "foo").unwrap();
        record_skill_usage(&lingxi_home, "foo").unwrap();
        let usage = read_skill_usage(&lingxi_home).expect("read back");
        assert_eq!(usage.get("foo").map(|r| r.count), Some(2));
        assert!(usage.get("foo").unwrap().last_used_unix.is_some());
        fs::remove_dir_all(root).ok();
    }
}
