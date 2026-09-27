use client_protocol::commands::SkillAdminCommandDto;
use plugin::plugin_source_sha256;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_SKILL_BYTES: usize = 512 * 1024;
const SKILL_FILE: &str = "SKILL.md";
const TRASH_META_FILE: &str = ".lingxi-trash.json";

#[derive(Debug, Clone)]
pub struct SkillsAdminContext {
    pub cwd: PathBuf,
    pub lingxi_home: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct SkillCatalogEnvelope {
    pub entries: Vec<SkillCatalogEntry>,
    pub trash: Vec<SkillCatalogEntry>,
    pub sync_claude_ai_note: &'static str,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SkillCatalogEntry {
    pub id: String,
    pub name: String,
    pub source: String,
    pub root_dir: String,
    pub directory: String,
    pub writable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readonly_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics_json: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trash_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trashed_at: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillDocumentEnvelope {
    pub id: String,
    pub name: String,
    pub source: String,
    pub directory: String,
    pub root_dir: String,
    pub markdown: String,
    pub writable: bool,
    pub revision: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readonly_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics_json: Option<String>,
}

#[derive(Debug)]
pub enum SkillAdminOutcome {
    Catalog(String),
    Document(String),
    Changed {
        catalog_json: String,
        document_json: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SaveSkillPayload {
    name: String,
    markdown: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MoveSkillPayload {
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PurgeSkillPayload {
    confirmed: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrashMetadata {
    original_directory: String,
    original_source: String,
    trashed_at: String,
}

#[derive(Debug)]
struct Inspection {
    markdown: String,
    revision: Option<String>,
    description: Option<String>,
    when_to_use: Option<String>,
    parse_error: Option<String>,
    diagnostics_json: Option<String>,
}

pub fn handle(
    command: SkillAdminCommandDto,
    ctx: &SkillsAdminContext,
) -> Result<SkillAdminOutcome, String> {
    match command.action.as_str() {
        "get_catalog" => Ok(SkillAdminOutcome::Catalog(catalog_json(ctx)?)),
        "get_document" => {
            let target = command
                .target
                .as_deref()
                .ok_or_else(|| "skill document target is required".to_string())?;
            Ok(SkillAdminOutcome::Document(document_json(target, ctx)?))
        }
        "save_document" | "create_skill" => {
            let scope = command
                .scope
                .as_deref()
                .ok_or_else(|| "skill scope is required".to_string())?;
            let payload: SaveSkillPayload = serde_json::from_str(
                command
                    .payload_json
                    .as_deref()
                    .ok_or_else(|| "skill payload is required".to_string())?,
            )
            .map_err(|error| format!("invalid skill payload: {error}"))?;
            let document = save_document(
                ctx,
                scope,
                command.target.as_deref(),
                command.revision.as_deref(),
                payload,
            )?;
            Ok(SkillAdminOutcome::Changed {
                catalog_json: catalog_json(ctx)?,
                document_json: Some(document),
            })
        }
        "move_skill" => {
            let source = command
                .target
                .as_deref()
                .ok_or_else(|| "skill target is required".to_string())?;
            let scope = command
                .scope
                .as_deref()
                .ok_or_else(|| "destination scope is required".to_string())?;
            let payload: MoveSkillPayload = command
                .payload_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(|error| format!("invalid move payload: {error}"))?
                .unwrap_or(MoveSkillPayload { name: None });
            move_skill(
                ctx,
                source,
                scope,
                command.revision.as_deref(),
                payload.name.as_deref(),
            )?;
            let moved_name = payload.name.clone().unwrap_or_else(|| file_name(source));
            let moved_path = destination_dir(&root_for_scope(ctx, scope)?, &moved_name)?;
            Ok(SkillAdminOutcome::Changed {
                catalog_json: catalog_json(ctx)?,
                document_json: Some(document_json(moved_path.to_string_lossy().as_ref(), ctx)?),
            })
        }
        "trash_skill" => {
            let target = command
                .target
                .as_deref()
                .ok_or_else(|| "skill target is required".to_string())?;
            trash_skill(ctx, target, command.revision.as_deref())?;
            Ok(SkillAdminOutcome::Changed {
                catalog_json: catalog_json(ctx)?,
                document_json: None,
            })
        }
        "restore_skill" => {
            let trash_id = command
                .target
                .as_deref()
                .ok_or_else(|| "trash skill target is required".to_string())?;
            let scope = command
                .scope
                .as_deref()
                .ok_or_else(|| "restore scope is required".to_string())?;
            let payload: MoveSkillPayload = command
                .payload_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(|error| format!("invalid restore payload: {error}"))?
                .unwrap_or(MoveSkillPayload { name: None });
            let restored = restore_skill(ctx, trash_id, scope, payload.name.as_deref())?;
            Ok(SkillAdminOutcome::Changed {
                catalog_json: catalog_json(ctx)?,
                document_json: Some(document_json(restored.to_string_lossy().as_ref(), ctx)?),
            })
        }
        "purge_trash_skill" => {
            let trash_id = command
                .target
                .as_deref()
                .ok_or_else(|| "trash skill target is required".to_string())?;
            let payload: PurgeSkillPayload = serde_json::from_str(
                command
                    .payload_json
                    .as_deref()
                    .ok_or_else(|| "purge confirmation payload is required".to_string())?,
            )
            .map_err(|error| format!("invalid purge payload: {error}"))?;
            if !payload.confirmed {
                return Err("purge requires confirmed:true".to_string());
            }
            purge_skill(ctx, trash_id)?;
            Ok(SkillAdminOutcome::Changed {
                catalog_json: catalog_json(ctx)?,
                document_json: None,
            })
        }
        action => Err(format!("unsupported skill admin action: {action}")),
    }
}

pub fn catalog_json(ctx: &SkillsAdminContext) -> Result<String, String> {
    serde_json::to_string(&SkillCatalogEnvelope {
        entries: catalog_entries(ctx)?,
        trash: trash_entries(ctx)?,
        sync_claude_ai_note: "Stored only. Claude.ai cloud sync is not wired on desktop.",
    })
    .map_err(|error| format!("failed to serialize skill catalog: {error}"))
}

pub fn document_json(target: &str, ctx: &SkillsAdminContext) -> Result<String, String> {
    let path = PathBuf::from(target);
    let entry = entry_for_path(&path, ctx)?;
    let inspection = if entry.source == "bundled" {
        inspect_markdown(&bundled_markdown(&entry.name)?)
    } else {
        inspect_markdown(&read_text(&path.join(SKILL_FILE))?)
    };
    serde_json::to_string(&SkillDocumentEnvelope {
        id: entry.id,
        name: entry.name,
        source: entry.source,
        directory: entry.directory,
        root_dir: entry.root_dir,
        markdown: inspection.markdown,
        writable: entry.writable,
        revision: inspection.revision.unwrap_or_else(|| sha256_hex(&[])),
        readonly_reason: entry.readonly_reason,
        parse_error: inspection.parse_error,
        diagnostics_json: inspection.diagnostics_json,
    })
    .map_err(|error| format!("failed to serialize skill document: {error}"))
}

fn catalog_entries(ctx: &SkillsAdminContext) -> Result<Vec<SkillCatalogEntry>, String> {
    let user_root = ctx.lingxi_home.join("skills");
    let mut entries = scan_root(&user_root, "user", true)?;
    for project_root in project_skill_roots(&ctx.cwd, &ctx.lingxi_home) {
        entries.extend(scan_root(&project_root, "project", true)?);
    }
    let managed_root = platform_api::live_sessions::managed_settings_dir()
        .join(branding::DOT_DIR)
        .join("skills");
    entries.extend(scan_root(&managed_root, "managed", false)?);
    entries.extend(bundled_entries()?);
    entries.sort_by(|left, right| {
        left.source
            .cmp(&right.source)
            .then(left.name.cmp(&right.name))
            .then(left.directory.cmp(&right.directory))
    });
    Ok(entries)
}

fn scan_root(root: &Path, source: &str, writable: bool) -> Result<Vec<SkillCatalogEntry>, String> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(error) => return Err(format!("failed to read {}: {error}", root.display())),
    };
    for entry in entries {
        let entry = entry
            .map_err(|error| format!("failed to read entry in {}: {error}", root.display()))?;
        let path = entry.path();
        if !entry.file_type().map(|ty| ty.is_dir()).unwrap_or(false) {
            continue;
        }
        let inspection = inspect_markdown(&read_text(&path.join(SKILL_FILE)).unwrap_or_default());
        out.push(SkillCatalogEntry {
            id: path.to_string_lossy().into_owned(),
            name: entry.file_name().to_string_lossy().into_owned(),
            source: source.to_string(),
            root_dir: root.to_string_lossy().into_owned(),
            directory: path.to_string_lossy().into_owned(),
            writable,
            readonly_reason: (!writable).then(|| format!("{source} skills are read-only.")),
            revision: inspection.revision,
            description: inspection.description,
            when_to_use: inspection.when_to_use,
            parse_error: inspection.parse_error,
            diagnostics_json: inspection.diagnostics_json,
            trash_id: None,
            trashed_at: None,
        });
    }
    Ok(out)
}

fn bundled_entries() -> Result<Vec<SkillCatalogEntry>, String> {
    let mut registry = skill_api::SkillRegistry::new();
    skill_api::builtin::register_desktop(&mut registry);
    let mut names = registry.names();
    names.sort_unstable();
    Ok(names
        .into_iter()
        .map(|name| {
            let skill = registry.get(name).expect("bundled skill exists");
            let markdown = bundled_markdown(name).unwrap_or_default();
            SkillCatalogEntry {
                id: format!("<bundled:{name}>"),
                name: name.to_string(),
                source: "bundled".to_string(),
                root_dir: "<bundled>".to_string(),
                directory: format!("<bundled:{name}>"),
                writable: false,
                readonly_reason: Some("Bundled skills are compiled into the app.".to_string()),
                revision: Some(sha256_hex(markdown.as_bytes())),
                description: Some(skill.description.clone()),
                when_to_use: skill.frontmatter.when_to_use.clone(),
                parse_error: None,
                diagnostics_json: Some("{\"ok\":true,\"issues\":[]}".to_string()),
                trash_id: None,
                trashed_at: None,
            }
        })
        .collect())
}

fn bundled_markdown(name: &str) -> Result<String, String> {
    let mut registry = skill_api::SkillRegistry::new();
    skill_api::builtin::register_desktop(&mut registry);
    let skill = registry
        .get(name)
        .ok_or_else(|| format!("unknown bundled skill: {name}"))?;
    Ok(skill.content.clone())
}

fn project_skill_roots(cwd: &Path, lingxi_home: &Path) -> Vec<PathBuf> {
    let home = lingxi_home.parent();
    let git_root = nearest_git_root(cwd);
    let mut dirs = Vec::new();
    let mut current = Some(cwd);
    while let Some(dir) = current {
        if Some(dir) == home {
            break;
        }
        let candidate = dir.join(branding::DOT_DIR).join("skills");
        if candidate.is_dir() {
            dirs.push(candidate);
        }
        if git_root.as_deref() == Some(dir) {
            break;
        }
        current = dir.parent();
    }
    if dirs.is_empty() {
        dirs.push(cwd.join(branding::DOT_DIR).join("skills"));
    }
    dirs
}

fn nearest_git_root(cwd: &Path) -> Option<PathBuf> {
    let mut current = Some(cwd);
    while let Some(dir) = current {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

fn save_document(
    ctx: &SkillsAdminContext,
    scope: &str,
    current_target: Option<&str>,
    revision: Option<&str>,
    payload: SaveSkillPayload,
) -> Result<String, String> {
    validate_skill_name(&payload.name)?;
    validate_skill_markdown(&payload.markdown, scope)?;
    let root = root_for_scope(ctx, scope)?;
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("failed to create {}: {error}", root.display()))?;
    let directory = destination_dir(&root, &payload.name)?;
    if let Some(current_target) = current_target {
        let current = PathBuf::from(current_target);
        validate_existing_skill_path(&current, ctx)?;
        let current_file = current.join(SKILL_FILE);
        let current_text = read_text(&current_file)?;
        ensure_revision(
            revision,
            sha256_hex(current_text.as_bytes()).as_str(),
            "skill",
        )?;
        if current != directory {
            rename_confined(&current, &directory, &[root.clone()])?;
        }
    } else if directory.exists() {
        let current_text = read_text(&directory.join(SKILL_FILE))?;
        ensure_revision(
            revision,
            sha256_hex(current_text.as_bytes()).as_str(),
            "skill",
        )?;
    }
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("failed to create {}: {error}", directory.display()))?;
    atomic_write(&directory.join(SKILL_FILE), payload.markdown.as_bytes())?;
    document_json(directory.to_string_lossy().as_ref(), ctx)
}

fn move_skill(
    ctx: &SkillsAdminContext,
    source: &str,
    scope: &str,
    revision: Option<&str>,
    next_name: Option<&str>,
) -> Result<(), String> {
    let current = PathBuf::from(source);
    validate_existing_skill_path(&current, ctx)?;
    let current_text = read_text(&current.join(SKILL_FILE))?;
    ensure_revision(
        revision,
        sha256_hex(current_text.as_bytes()).as_str(),
        "skill",
    )?;
    let root = root_for_scope(ctx, scope)?;
    let inherited_name = file_name(source);
    let name = next_name.unwrap_or(inherited_name.as_str());
    validate_skill_name(name)?;
    let destination = destination_dir(&root, name)?;
    rename_confined(&current, &destination, &writable_roots(ctx))?;
    Ok(())
}

fn trash_skill(
    ctx: &SkillsAdminContext,
    target: &str,
    revision: Option<&str>,
) -> Result<(), String> {
    let current = PathBuf::from(target);
    validate_existing_skill_path(&current, ctx)?;
    let current_text = read_text(&current.join(SKILL_FILE))?;
    ensure_revision(
        revision,
        sha256_hex(current_text.as_bytes()).as_str(),
        "skill",
    )?;
    let trash_root = trash_root(&ctx.lingxi_home);
    let trash_dir = trash_root.join(fresh_trash_id().as_str());
    std::fs::create_dir_all(&trash_root)
        .map_err(|error| format!("failed to create trash root: {error}"))?;
    let metadata = TrashMetadata {
        original_directory: current.to_string_lossy().into_owned(),
        original_source: source_for_path(&current, ctx).unwrap_or_else(|| "unknown".to_string()),
        trashed_at: fresh_trash_id(),
    };
    let meta = serde_json::to_vec(&metadata)
        .map_err(|error| format!("failed to serialize trash metadata: {error}"))?;
    atomic_write(&current.join(TRASH_META_FILE), &meta)?;
    if let Err(error) = rename_confined(&current, &trash_dir, std::slice::from_ref(&trash_root)) {
        let _ = std::fs::remove_file(current.join(TRASH_META_FILE));
        return Err(error);
    }
    Ok(())
}

fn restore_skill(
    ctx: &SkillsAdminContext,
    trash_id: &str,
    scope: &str,
    next_name: Option<&str>,
) -> Result<PathBuf, String> {
    validate_skill_name(trash_id)?;
    let trash_dir = trash_root(&ctx.lingxi_home).join(trash_id);
    validate_existing_trash_path(&trash_dir, &ctx.lingxi_home)?;
    let metadata: TrashMetadata =
        serde_json::from_str(&read_text(&trash_dir.join(TRASH_META_FILE))?)
            .map_err(|error| format!("invalid trash metadata: {error}"))?;
    let inherited_name = file_name(&metadata.original_directory);
    let name = next_name.unwrap_or(inherited_name.as_str());
    validate_skill_name(name)?;
    let destination = destination_dir(&root_for_scope(ctx, scope)?, name)?;
    rename_confined(&trash_dir, &destination, &writable_roots(ctx))?;
    let _ = std::fs::remove_file(destination.join(TRASH_META_FILE));
    Ok(destination)
}

fn purge_skill(ctx: &SkillsAdminContext, trash_id: &str) -> Result<(), String> {
    validate_skill_name(trash_id)?;
    let path = trash_root(&ctx.lingxi_home).join(trash_id);
    validate_existing_trash_path(&path, &ctx.lingxi_home)?;
    std::fs::remove_dir_all(&path)
        .map_err(|error| format!("failed to purge {}: {error}", path.display()))
}

fn trash_entries(ctx: &SkillsAdminContext) -> Result<Vec<SkillCatalogEntry>, String> {
    let root = trash_root(&ctx.lingxi_home);
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(error) => return Err(format!("failed to read {}: {error}", root.display())),
    };
    for entry in entries.flatten() {
        if !entry.file_type().map(|ty| ty.is_dir()).unwrap_or(false) {
            continue;
        }
        let directory = entry.path();
        let trash_id = entry.file_name().to_string_lossy().into_owned();
        let metadata: Option<TrashMetadata> = read_text(&directory.join(TRASH_META_FILE))
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok());
        let inspection =
            inspect_markdown(&read_text(&directory.join(SKILL_FILE)).unwrap_or_default());
        out.push(SkillCatalogEntry {
            id: directory.to_string_lossy().into_owned(),
            name: file_name(metadata.as_ref().map_or_else(
                || directory.to_string_lossy().into_owned(),
                |meta| meta.original_directory.clone(),
            )),
            source: "trash".to_string(),
            root_dir: root.to_string_lossy().into_owned(),
            directory: directory.to_string_lossy().into_owned(),
            writable: false,
            readonly_reason: Some("Restore or purge this skill from the trash.".to_string()),
            revision: inspection.revision,
            description: inspection.description,
            when_to_use: inspection.when_to_use,
            parse_error: inspection.parse_error,
            diagnostics_json: inspection.diagnostics_json,
            trash_id: Some(trash_id),
            trashed_at: metadata.map(|meta| meta.trashed_at),
        });
    }
    out.sort_by(|left, right| left.directory.cmp(&right.directory));
    Ok(out)
}

fn entry_for_path(path: &Path, ctx: &SkillsAdminContext) -> Result<SkillCatalogEntry, String> {
    if let Some(rest) = path.to_string_lossy().strip_prefix("<bundled:") {
        let name = rest.trim_end_matches('>');
        return bundled_entries()?
            .into_iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| format!("unknown bundled skill: {name}"));
    }
    let mut entries = catalog_entries(ctx)?;
    entries.extend(trash_entries(ctx)?);
    entries
        .into_iter()
        .find(|entry| entry.directory == path.to_string_lossy())
        .ok_or_else(|| format!("unknown skill path: {}", path.display()))
}

fn validate_existing_skill_path(path: &Path, ctx: &SkillsAdminContext) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "refusing to use symlinked skill path {}",
            path.display()
        ));
    }
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("failed to resolve {}: {error}", path.display()))?;
    for root in writable_roots(ctx) {
        if let Ok(canonical_root) = root.canonicalize() {
            if canonical.parent() == Some(canonical_root.as_path()) {
                return Ok(());
            }
        }
    }
    Err(format!(
        "{} is outside the writable skill roots",
        path.display()
    ))
}

fn validate_existing_trash_path(path: &Path, lingxi_home: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "refusing to use symlinked trash path {}",
            path.display()
        ));
    }
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("failed to resolve {}: {error}", path.display()))?;
    let root = trash_root(lingxi_home)
        .canonicalize()
        .map_err(|error| format!("failed to resolve trash root: {error}"))?;
    if canonical.parent() == Some(root.as_path()) {
        Ok(())
    } else {
        Err(format!(
            "{} is outside the skill trash root",
            path.display()
        ))
    }
}

fn validate_skill_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(std::path::MAIN_SEPARATOR)
        || name.contains('/')
        || name.contains('\\')
    {
        return Err(format!("invalid skill name: {name}"));
    }
    if matches!(name, "trash" | ".git" | branding::DOT_DIR) {
        return Err(format!("reserved skill name: {name}"));
    }
    Ok(())
}

fn validate_skill_markdown(markdown: &str, scope: &str) -> Result<(), String> {
    if markdown.as_bytes().len() > MAX_SKILL_BYTES {
        return Err("SKILL.md exceeds the 512 KiB limit".to_string());
    }
    let source = if scope == "user" {
        skill_api::SkillSource::Settings(protocol::SettingsScope::User)
    } else {
        skill_api::SkillSource::Settings(protocol::SettingsScope::Project)
    };
    skill_api::parse_skill_markdown(
        markdown,
        PathBuf::from(SKILL_FILE),
        source,
        skill_api::LoadedFrom::Skills,
    )
    .map(|_| ())
    .map_err(|error| format!("invalid SKILL.md: {error}"))
}

fn destination_dir(root: &Path, name: &str) -> Result<PathBuf, String> {
    validate_skill_name(name)?;
    Ok(root.join(name))
}

fn rename_confined(
    source: &Path,
    destination: &Path,
    allowed_roots: &[PathBuf],
) -> Result<(), String> {
    if destination.exists() {
        return Err(format!("{} already exists", destination.display()));
    }
    let destination_parent = destination
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", destination.display()))?;
    std::fs::create_dir_all(destination_parent)
        .map_err(|error| format!("failed to create {}: {error}", destination_parent.display()))?;
    let destination_meta = std::fs::symlink_metadata(destination_parent).map_err(|error| {
        format!(
            "failed to inspect {}: {error}",
            destination_parent.display()
        )
    })?;
    if destination_meta.file_type().is_symlink() {
        return Err(format!(
            "refusing to use symlinked destination parent {}",
            destination_parent.display()
        ));
    }
    let canonical_parent = destination_parent.canonicalize().map_err(|error| {
        format!(
            "failed to resolve {}: {error}",
            destination_parent.display()
        )
    })?;
    if !allowed_roots.iter().any(|root| {
        root.canonicalize()
            .map(|canonical_root| canonical_parent.starts_with(&canonical_root))
            .unwrap_or(false)
    }) {
        return Err(format!(
            "{} is outside the allowed skill roots",
            destination.display()
        ));
    }
    std::fs::rename(source, destination).map_err(|error| {
        format!(
            "failed to move {} to {}: {error}",
            source.display(),
            destination.display()
        )
    })
}

fn root_for_scope(ctx: &SkillsAdminContext, scope: &str) -> Result<PathBuf, String> {
    match scope {
        "user" => Ok(ctx.lingxi_home.join("skills")),
        "project" => Ok(ctx.cwd.join(branding::DOT_DIR).join("skills")),
        other => Err(format!("unsupported skill scope: {other}")),
    }
}

fn writable_roots(ctx: &SkillsAdminContext) -> Vec<PathBuf> {
    vec![
        ctx.lingxi_home.join("skills"),
        ctx.cwd.join(branding::DOT_DIR).join("skills"),
    ]
}

fn trash_root(lingxi_home: &Path) -> PathBuf {
    lingxi_home.join("trash").join("skills")
}

fn fresh_trash_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("skill-{now}")
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(".lingxi-write-{}-{nonce}.tmp", std::process::id()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| format!("failed to create {}: {error}", temporary.display()))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("failed to write {}: {error}", temporary.display()))?;
        std::fs::rename(&temporary, path)
            .map_err(|error| format!("failed to replace {}: {error}", path.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn file_name(path: impl AsRef<str>) -> String {
    Path::new(path.as_ref())
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("skill")
        .to_string()
}

fn source_for_path(path: &Path, ctx: &SkillsAdminContext) -> Option<String> {
    let user_root = ctx.lingxi_home.join("skills");
    if path.starts_with(&user_root) {
        return Some("user".to_string());
    }
    let project_root = ctx.cwd.join(branding::DOT_DIR).join("skills");
    if path.starts_with(&project_root) {
        return Some("project".to_string());
    }
    None
}

fn inspect_markdown(markdown: &str) -> Inspection {
    if markdown.is_empty() {
        return Inspection {
            markdown: String::new(),
            revision: None,
            description: None,
            when_to_use: None,
            parse_error: Some("SKILL.md is missing or empty".to_string()),
            diagnostics_json: Some(
                json!({
                    "ok": false,
                    "issues": [{ "code": "missing_or_empty", "severity": "error" }],
                })
                .to_string(),
            ),
        };
    }
    let revision = Some(sha256_hex(markdown.as_bytes()));
    match skill_api::parse_skill_markdown(
        markdown,
        PathBuf::from(SKILL_FILE),
        skill_api::SkillSource::Settings(protocol::SettingsScope::User),
        skill_api::LoadedFrom::Skills,
    ) {
        Ok(skill) => Inspection {
            markdown: markdown.to_string(),
            revision,
            description: Some(skill.description),
            when_to_use: skill.frontmatter.when_to_use,
            parse_error: None,
            diagnostics_json: Some("{\"ok\":true,\"issues\":[]}".to_string()),
        },
        Err(error) => Inspection {
            markdown: markdown.to_string(),
            revision,
            description: None,
            when_to_use: None,
            parse_error: Some(error.to_string()),
            diagnostics_json: Some(
                json!({
                    "ok": false,
                    "issues": [{ "code": "parse_error", "severity": "error", "message": error.to_string() }],
                })
                .to_string(),
            ),
        },
    }
}

fn read_text(path: &Path) -> Result<String, String> {
    match std::fs::read(path) {
        Ok(bytes) => {
            String::from_utf8(bytes).map_err(|_| format!("{} is not valid UTF-8", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(format!("failed to read {}: {error}", path.display())),
    }
}

fn sha256_hex(input: &[u8]) -> String {
    plugin_source_sha256(input)
}

fn ensure_revision(expected: Option<&str>, actual: &str, label: &str) -> Result<(), String> {
    if let Some(expected) = expected {
        if expected != actual {
            return Err(format!(
                "{label} changed on disk (expected revision {expected}, found {actual})"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "lingxi-skills-admin-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp root");
        dir
    }

    fn context(root: &Path) -> SkillsAdminContext {
        let lingxi_home = root.join("home").join(".lingxi");
        let cwd = root.join("repo");
        fs::create_dir_all(&lingxi_home).unwrap();
        fs::create_dir_all(cwd.join(".git")).unwrap();
        fs::create_dir_all(cwd.join(branding::DOT_DIR)).unwrap();
        SkillsAdminContext { cwd, lingxi_home }
    }

    fn write_skill(root: &Path, name: &str, markdown: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(SKILL_FILE), markdown).unwrap();
        dir
    }

    #[cfg(unix)]
    fn symlink_dir(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    #[test]
    fn trash_then_restore_uses_correct_destination_roots() {
        let root = temp_root("trash-restore");
        let ctx = context(&root);
        let user_root = ctx.lingxi_home.join("skills");
        let project_root = ctx.cwd.join(branding::DOT_DIR).join("skills");
        fs::create_dir_all(&user_root).unwrap();
        let dir = write_skill(
            &user_root,
            "alpha",
            "---\nname: alpha\ndescription: alpha\n---\nBody\n",
        );
        let revision = sha256_hex(&fs::read(dir.join(SKILL_FILE)).unwrap());
        trash_skill(&ctx, dir.to_string_lossy().as_ref(), Some(&revision)).unwrap();
        let trash = trash_entries(&ctx).unwrap();
        let trash_id = trash[0].trash_id.clone().unwrap();
        let restored = restore_skill(&ctx, &trash_id, "project", Some("beta")).unwrap();
        assert_eq!(restored, project_root.join("beta"));
        assert!(restored.join(SKILL_FILE).exists());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn save_and_move_reject_external_sources() {
        let root = temp_root("external");
        let ctx = context(&root);
        let outside_root = root.join("outside");
        let outside = write_skill(
            &outside_root,
            "rogue",
            "---\nname: rogue\ndescription: rogue\n---\nBody\n",
        );

        let save_err = save_document(
            &ctx,
            "user",
            Some(outside.to_string_lossy().as_ref()),
            None,
            SaveSkillPayload {
                name: "rogue".to_string(),
                markdown: "---\nname: rogue\ndescription: changed\n---\nBody\n".to_string(),
            },
        )
        .unwrap_err();
        assert!(save_err.contains("outside the writable skill roots"));

        let move_err = move_skill(
            &ctx,
            outside.to_string_lossy().as_ref(),
            "user",
            None,
            Some("moved"),
        )
        .unwrap_err();
        assert!(move_err.contains("outside the writable skill roots"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn damaged_skills_remain_visible_and_stale_revisions_are_rejected() {
        let root = temp_root("damaged-cas");
        let ctx = context(&root);
        let user_root = ctx.lingxi_home.join("skills");
        let damaged = write_skill(&user_root, "damaged", "---\ninvalid: [\n---\nBody\n");
        let entries = catalog_entries(&ctx).expect("catalog");
        let row = entries
            .iter()
            .find(|entry| entry.name == "damaged")
            .expect("damaged skill remains listed");
        assert!(row.parse_error.is_some());

        let valid = "---\nname: damaged\ndescription: repaired\n---\nBody\n";
        let stale_revision = row.revision.clone().expect("damaged bytes have revision");
        fs::write(damaged.join(SKILL_FILE), format!("{valid}\nexternal edit"))
            .expect("external edit");
        let error = save_document(
            &ctx,
            "user",
            Some(damaged.to_string_lossy().as_ref()),
            Some(&stale_revision),
            SaveSkillPayload {
                name: "damaged".to_string(),
                markdown: valid.to_string(),
            },
        )
        .expect_err("stale writes must fail");
        assert!(error.contains("changed on disk"));
        assert!(fs::read_to_string(damaged.join(SKILL_FILE))
            .expect("read external edit")
            .contains("external edit"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    #[cfg(unix)]
    fn symlinked_skill_source_is_rejected() {
        let root = temp_root("symlink");
        let ctx = context(&root);
        let user_root = ctx.lingxi_home.join("skills");
        let real_root = root.join("real");
        fs::create_dir_all(&user_root).unwrap();
        fs::create_dir_all(&real_root).unwrap();
        let real_skill = write_skill(
            &real_root,
            "alpha",
            "---\nname: alpha\ndescription: alpha\n---\nBody\n",
        );
        let link = user_root.join("alpha");
        symlink_dir(&real_skill, &link);

        let err = validate_existing_skill_path(&link, &ctx).unwrap_err();
        assert!(err.contains("symlinked skill path"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn purge_requires_confirmed_true() {
        let root = temp_root("purge");
        let ctx = context(&root);
        let trash_root = trash_root(&ctx.lingxi_home);
        let trash_dir = trash_root.join("skill-1");
        fs::create_dir_all(&trash_dir).unwrap();
        fs::write(
            trash_dir.join(TRASH_META_FILE),
            serde_json::to_vec(&TrashMetadata {
                original_directory: "x".to_string(),
                original_source: "user".to_string(),
                trashed_at: "1".to_string(),
            })
            .unwrap(),
        )
        .unwrap();

        let err = handle(
            SkillAdminCommandDto {
                action: "purge_trash_skill".to_string(),
                operation_id: None,
                target: Some("skill-1".to_string()),
                scope: None,
                revision: None,
                payload_json: Some(json!({ "confirmed": false }).to_string()),
            },
            &ctx,
        )
        .unwrap_err();
        assert_eq!(err, "purge requires confirmed:true");
        assert!(trash_dir.exists());

        handle(
            SkillAdminCommandDto {
                action: "purge_trash_skill".to_string(),
                operation_id: None,
                target: Some("skill-1".to_string()),
                scope: None,
                revision: None,
                payload_json: Some(json!({ "confirmed": true }).to_string()),
            },
            &ctx,
        )
        .unwrap();
        assert!(!trash_dir.exists());
        fs::remove_dir_all(root).ok();
    }
}
