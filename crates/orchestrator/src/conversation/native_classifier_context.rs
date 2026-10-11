//! Build the source-preserving Native classifier context from the live host
//! permission snapshot and current workspace facts.

use lingxi_core::host::permission_gate::{
    PermissionPolicyRuleFact, PermissionPolicySnapshot,
};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_llm_client::protocol::JsonPayload;
use lingxi_llm_client::{NativeAnthropicRequestContext, NativeAnthropicServerClassifierContext};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

const GIT_COMMAND_TIMEOUT: Duration = Duration::from_millis(300);
const GIT_STATUS_MAX_BYTES: usize = 1024 * 1024;

/// Build request-local Native facts only when the live permission gate is in
/// Auto or Plan and all trusted AutoMode source tiers were resolved.
pub(crate) async fn request_context(
    snapshot: Option<PermissionPolicySnapshot>,
) -> Option<NativeAnthropicRequestContext> {
    let snapshot = snapshot?;
    if !matches!(snapshot.permission_mode.as_str(), "auto" | "plan")
        || (snapshot.permission_mode == "auto"
            && (snapshot.auto_mode_disabled || snapshot.denial_circuit_broken))
    {
        return None;
    }
    let auto_mode = snapshot.classifier_auto_mode_settings.as_ref()?;
    let cwd = snapshot.working_directory.as_deref()?;
    let diskless = snapshot.classifier_host_capabilities.diskless;
    let git = git_state(cwd, diskless).await;
    let identity = if diskless {
        None
    } else {
        identity_from_environment(
            std::env::var("GITHUB_ACTOR").ok().as_deref(),
            std::env::var("USER").ok().as_deref(),
            std::env::var("USERNAME").ok().as_deref(),
            git.user_email.as_deref(),
        )
    };
    let projection = classifier_projection(&snapshot, auto_mode, cwd, git.value, identity)?;
    let source = projection.to_json_string().ok()?;
    // Keep the source context intact if it exceeds Native's budget. Native
    // prunes it through a tiered projection; that exact pruning order is still
    // unverified here, so dropping the whole safeguard object would be worse.
    let payload = JsonPayload::parse(&source).ok()?;
    let classifier_context = NativeAnthropicServerClassifierContext::new(payload).ok()?;
    Some(NativeAnthropicRequestContext {
        server_classifier: Some(classifier_context),
        ..NativeAnthropicRequestContext::default()
    })
}

pub(crate) fn supports_native_classifier_route(
    protocol: &llm_runtime::ProtocolFamily,
    first_party_anthropic: bool,
) -> bool {
    match protocol {
        llm_runtime::ProtocolFamily::AnthropicMessages => first_party_anthropic,
        llm_runtime::ProtocolFamily::FoundryClaude => true,
        _ => false,
    }
}

fn classifier_projection(
    snapshot: &PermissionPolicySnapshot,
    auto_mode: &lingxi_core::host::permission_gate::PermissionClassifierAutoModeSettings,
    cwd: &Path,
    git_state: Option<Utf16JsonProjection>,
    user_identity: Option<String>,
) -> Option<Utf16JsonProjection> {
    let root_facts = snapshot.rule_roots.as_ref()?;
    let lingxi_home = root_facts
        .iter()
        .find(|root| root.source == "userSettings")?
        .path
        .clone();
    let filesystem_roots = permission::filesystem::FsRoots {
        cwd: cwd.to_path_buf(),
        home: snapshot.home_directory.clone(),
        lingxi_home,
    };
    let primary = path_projection(cwd);
    let primary_directory = trusted_directory(
        cwd,
        &primary,
        snapshot.classifier_host_capabilities.diskless,
        &filesystem_roots,
    );
    let additional = snapshot
        .additional_working_directories
        .iter()
        .map(|directory| {
            let spelling = path_projection(&directory.path);
            trusted_directory_with_source(
                &directory.path,
                &spelling,
                &directory.source,
                snapshot.classifier_host_capabilities.diskless,
                &filesystem_roots,
            )
        })
        .collect();
    let network = snapshot
        .classifier_host_capabilities
        .trusted_network_directories
        .iter()
        .map(|path| path_projection(path))
        .collect();

    let mut fields = vec![
        ("v".to_owned(), number_projection(1)),
        (
            "permission_mode".to_owned(),
            string_projection(&snapshot.permission_mode),
        ),
        (
            "platform".to_owned(),
            string_projection(super::node_platform_name(std::env::consts::OS)),
        ),
        ("live_cwd".to_owned(), path_projection(cwd)),
    ];
    if !snapshot.classifier_host_capabilities.diskless {
        if let Some(home) = snapshot.home_directory.as_deref() {
            fields.push(("home_dir".to_owned(), path_projection(home)));
        }
    }
    fields.extend([
        (
            "rule_roots".to_owned(),
            object_projection(
                root_facts
                    .iter()
                    .map(|root| (root.source.clone(), path_projection(&root.path)))
                    .collect(),
            ),
        ),
        (
            "trusted_directories".to_owned(),
            object_projection(vec![
                ("primary".to_owned(), primary_directory),
                ("additional".to_owned(), array_projection(additional)),
                ("network".to_owned(), array_projection(network)),
                (
                    "block_reads_outside_working_directories".to_owned(),
                    bool_projection(snapshot.block_reads_outside_working_directories),
                ),
            ]),
        ),
        (
            "rules".to_owned(),
            object_projection(vec![
                ("allow".to_owned(), rules_projection(&snapshot.allow_rules)),
                ("deny".to_owned(), rules_projection(&snapshot.deny_rules)),
                ("ask".to_owned(), rules_projection(&snapshot.ask_rules)),
            ]),
        ),
        (
            "auto_mode".to_owned(),
            object_projection(vec![
                (
                    "allow".to_owned(),
                    array_projection(auto_mode.allow.clone()),
                ),
                (
                    "soft_deny".to_owned(),
                    array_projection(auto_mode.soft_deny.clone()),
                ),
                (
                    "hard_deny".to_owned(),
                    array_projection(auto_mode.hard_deny.clone()),
                ),
                (
                    "environment".to_owned(),
                    array_projection(auto_mode.environment.clone()),
                ),
            ]),
        ),
        (
            "artifact_consent_holdback".to_owned(),
            bool_projection(artifact_consent_holdback(
                std::env::var("CLAUDE_CODE_AUTO_MODE_ARTIFACT_CONSENT_RULE")
                    .ok()
                    .as_deref(),
                telemetry::flag_bool("tengu_umber_sluice", false),
            )),
        ),
        (
            "case_insensitive_paths".to_owned(),
            bool_projection(cfg!(windows)),
        ),
        (
            "restricted".to_owned(),
            bool_projection(snapshot.restricted),
        ),
        (
            "is_remote_mode".to_owned(),
            bool_projection(
                lingxi_core::host::env::is_env_truthy(
                    std::env::var(branding::REMOTE_ENV).ok().as_deref(),
                ) || snapshot.classifier_host_capabilities.remote_workspace,
            ),
        ),
        (
            "classify_all_shell".to_owned(),
            bool_projection(auto_mode.classify_all_shell),
        ),
        (
            "user_identity".to_owned(),
            option_string_projection(user_identity.as_deref()),
        ),
    ]);
    if let Some(git_state) = git_state {
        fields.push(("git_state".to_owned(), git_state));
    }
    Some(object_projection(fields))
}

fn rules_projection(rules: &[PermissionPolicyRuleFact]) -> Utf16JsonProjection {
    array_projection(
        rules
            .iter()
            .map(|rule| {
                object_projection(vec![
                    ("rule".to_owned(), rule.rule.clone()),
                    ("source".to_owned(), string_projection(&rule.source)),
                ])
            })
            .collect(),
    )
}

fn trusted_directory(
    path: &Path,
    spelling: &Utf16JsonProjection,
    diskless: bool,
    roots: &permission::filesystem::FsRoots,
) -> Utf16JsonProjection {
    object_projection(vec![
        ("path".to_owned(), spelling.clone()),
        (
            "resolved".to_owned(),
            resolved_spellings(path, spelling, diskless, roots),
        ),
    ])
}

fn trusted_directory_with_source(
    path: &Path,
    spelling: &Utf16JsonProjection,
    source: &str,
    diskless: bool,
    roots: &permission::filesystem::FsRoots,
) -> Utf16JsonProjection {
    object_projection(vec![
        ("path".to_owned(), spelling.clone()),
        (
            "resolved".to_owned(),
            resolved_spellings(path, spelling, diskless, roots),
        ),
        ("source".to_owned(), string_projection(source)),
    ])
}

fn resolved_spellings(
    path: &Path,
    original: &Utf16JsonProjection,
    diskless: bool,
    roots: &permission::filesystem::FsRoots,
) -> Utf16JsonProjection {
    if diskless {
        return array_projection(vec![original.clone()]);
    }
    let mut spellings = vec![original.clone()];
    for resolved in
        permission::filesystem::native_permission_directory_path_spellings_with_roots(path, roots)
    {
        let projection = path_projection(&resolved);
        if !spellings.contains(&projection) {
            spellings.push(projection);
        }
    }
    array_projection(spellings)
}

fn path_projection(path: &Path) -> Utf16JsonProjection {
    #[cfg(windows)]
    let code_units: Vec<u16> = {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str().encode_wide().collect()
    };
    #[cfg(not(windows))]
    let code_units: Vec<u16> = path.to_string_lossy().encode_utf16().collect();
    let display = String::from_utf16_lossy(&code_units);
    Utf16JsonProjection::root_string(display, code_units)
        .expect("path display matches its source code units")
}

fn string_projection(value: &str) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(value.to_owned(), value.encode_utf16().collect())
        .expect("Rust strings are valid UTF-16 sequences")
}

fn option_string_projection(value: Option<&str>) -> Utf16JsonProjection {
    value.map(string_projection).unwrap_or_else(null_projection)
}

fn bool_projection(value: bool) -> Utf16JsonProjection {
    Utf16JsonProjection::parse(if value { "true" } else { "false" })
        .expect("boolean literal is valid JSON")
}

fn number_projection(value: usize) -> Utf16JsonProjection {
    Utf16JsonProjection::parse(&value.to_string()).expect("integer is valid JSON")
}

fn null_projection() -> Utf16JsonProjection {
    Utf16JsonProjection::parse("null").expect("null is valid JSON")
}

fn array_projection(values: Vec<Utf16JsonProjection>) -> Utf16JsonProjection {
    Utf16JsonProjection::array(values).expect("projection array is well formed")
}

fn object_projection(fields: Vec<(String, Utf16JsonProjection)>) -> Utf16JsonProjection {
    Utf16JsonProjection::object(fields).expect("classifier object keys are unique")
}

fn artifact_consent_holdback(environment: Option<&str>, umber_sluice: bool) -> bool {
    environment.map_or(umber_sluice, |value| {
        !lingxi_core::host::env::is_env_truthy(Some(value))
    })
}

fn identity_from_environment(
    github_actor: Option<&str>,
    user: Option<&str>,
    username: Option<&str>,
    git_email: Option<&str>,
) -> Option<String> {
    let raw = github_actor
        .or(user)
        .or(username)
        .map(ToOwned::to_owned)
        .or_else(|| {
            git_email.map(|email| email.split('@').next().unwrap_or_default().to_owned())
        })?;
    let normalized: String = raw
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        })
        .take(64)
        .collect();
    (!normalized.is_empty()).then_some(normalized)
}

struct GitState {
    value: Option<Utf16JsonProjection>,
    user_email: Option<String>,
}

#[derive(Debug)]
enum GitCommandResult {
    Output(String),
    NonZero,
    Timeout,
    SpawnError,
}

async fn git_command(cwd: &Path, arguments: &[&str]) -> GitCommandResult {
    let mut command = tokio::process::Command::new("git");
    command
        .args(permission::auto_mode_producers::GIT_HARDENING_FLAGS)
        .arg("-C")
        .arg(cwd)
        .arg("--no-optional-locks")
        .args(arguments)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(GIT_COMMAND_TIMEOUT, command.output()).await {
        Err(_) => GitCommandResult::Timeout,
        Ok(Err(_)) => GitCommandResult::SpawnError,
        Ok(Ok(output)) if !output.status.success() => GitCommandResult::NonZero,
        Ok(Ok(output)) => {
            let text = String::from_utf8_lossy(&output.stdout);
            GitCommandResult::Output(text.trim_end_matches(['\r', '\n']).to_owned())
        }
    }
}

async fn git_state(cwd: &Path, diskless: bool) -> GitState {
    if diskless {
        return GitState {
            value: None,
            user_email: None,
        };
    }
    let root = match git_command(cwd, &["rev-parse", "--show-toplevel"]).await {
        GitCommandResult::Output(root) if !root.is_empty() => PathBuf::from(root),
        GitCommandResult::Timeout => {
            return GitState {
                value: Some(git_state_value(
                    cwd,
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some("timeout"),
                )),
                user_email: None,
            };
        }
        GitCommandResult::SpawnError => {
            return GitState {
                value: Some(git_state_value(
                    cwd,
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some("error"),
                )),
                user_email: None,
            };
        }
        GitCommandResult::NonZero | GitCommandResult::Output(_) => {
            return GitState {
                value: Some(git_state_value(
                    cwd,
                    None,
                    None,
                    None,
                    None,
                    Some(visibility_value(None, None, Vec::new())),
                    Some("not_a_repo"),
                )),
                user_email: git_config_value(cwd, &["config", "--get", "user.email"]).await,
            };
        }
    };
    let branch = git_configured_branch(cwd).await;
    let default_branch = match git_command(
        &root,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )
    .await
    {
        GitCommandResult::Output(value) => {
            value.strip_prefix("origin/").unwrap_or(&value).to_owned()
        }
        _ => String::new(),
    };
    let (status, status_error) = match git_command(
        &root,
        &["status", "--porcelain", "--untracked-files=normal"],
    )
    .await
    {
        GitCommandResult::Output(value) => (Some(status_value(&value)), None),
        GitCommandResult::Timeout => (None, Some("timeout")),
        GitCommandResult::SpawnError | GitCommandResult::NonZero => (None, Some("error")),
    };
    let remote_names = match git_command(&root, &["remote"]).await {
        GitCommandResult::Output(value) => value
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    };
    let mut remotes = Vec::new();
    let mut origin_remote = None;
    for name in remote_names {
        let fetch_urls = git_config_values(&root, &format!("remote.{name}.url")).await;
        let push_urls = git_config_values(&root, &format!("remote.{name}.pushurl")).await;
        if name == "origin" {
            origin_remote = fetch_urls
                .first()
                .and_then(|url| remote_visibility_entry(&name, url));
        }
        for url in fetch_urls.iter().chain(push_urls.iter()) {
            if let Some(entry) = remote_visibility_entry(&name, url) {
                remotes.push(entry);
            }
        }
    }
    remotes.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.remote.cmp(&right.remote))
    });
    let mut seen_remotes = HashSet::new();
    remotes.retain(|remote| seen_remotes.insert((remote.host.clone(), remote.remote.clone())));
    let branch_remote = if let Some(branch) = branch.as_deref() {
        git_config_value(
            &root,
            &["config", "--get", &format!("branch.{branch}.remote")],
        )
        .await
    } else {
        None
    };
    let push_default = git_config_value(&root, &["config", "--get", "remote.pushDefault"]).await;
    let push_remote = branch_remote
        .or(push_default)
        .or_else(|| Some("origin".to_owned()));
    let user_email = git_config_value(&root, &["config", "--get", "user.email"]).await;
    let visibility = visibility_value(origin_remote, push_remote, remotes);
    GitState {
        value: Some(git_state_value(
            cwd,
            Some(path_projection(&root)),
            branch.as_deref().map(string_projection),
            (!default_branch.is_empty()).then(|| string_projection(&default_branch)),
            status,
            Some(visibility),
            status_error,
        )),
        user_email,
    }
}

async fn git_configured_branch(cwd: &Path) -> Option<String> {
    match git_command(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).await {
        GitCommandResult::Output(value) if !value.is_empty() => Some(value),
        _ => None,
    }
}

async fn git_config_value(cwd: &Path, arguments: &[&str]) -> Option<String> {
    match git_command(cwd, arguments).await {
        GitCommandResult::Output(value) if !value.is_empty() => Some(value),
        _ => None,
    }
}

async fn git_config_values(cwd: &Path, key: &str) -> Vec<String> {
    match git_command(cwd, &["config", "--null", "--get-all", key]).await {
        GitCommandResult::Output(value) => value
            .split('\0')
            .filter(|item| !item.is_empty())
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

#[derive(Clone)]
struct RemoteVisibilityEntry {
    name: String,
    host: String,
    remote: String,
}

fn remote_visibility_entry(name: &str, url: &str) -> Option<RemoteVisibilityEntry> {
    let redacted = permission::auto_mode_producers::redact_remote_url(url);
    if redacted.starts_with('(') {
        return None;
    }
    let (host, remote) = if let Some((_, rest)) = redacted.split_once("://") {
        let (host, remote) = rest.split_once('/')?;
        (host.to_owned(), remote.to_owned())
    } else {
        let remote = redacted.strip_prefix("/").unwrap_or(&redacted);
        let (host, remote) = remote.split_once(':').or_else(|| remote.split_once('/'))?;
        (host.to_owned(), remote.trim_start_matches('/').to_owned())
    };
    if host.is_empty() || remote.is_empty() {
        return None;
    }
    Some(RemoteVisibilityEntry {
        name: name.to_owned(),
        host,
        remote,
    })
}

fn visibility_entry_projection(entry: &RemoteVisibilityEntry) -> Utf16JsonProjection {
    object_projection(vec![
        ("name".to_owned(), string_projection(&entry.name)),
        ("host".to_owned(), string_projection(&entry.host)),
        ("remote".to_owned(), string_projection(&entry.remote)),
        ("visibility".to_owned(), string_projection("unknown")),
    ])
}

fn origin_projection(entry: &RemoteVisibilityEntry) -> Utf16JsonProjection {
    object_projection(vec![
        ("host".to_owned(), string_projection(&entry.host)),
        ("remote".to_owned(), string_projection(&entry.remote)),
        ("visibility".to_owned(), string_projection("unknown")),
    ])
}

fn visibility_value(
    origin: Option<RemoteVisibilityEntry>,
    push_remote: Option<String>,
    remotes: Vec<RemoteVisibilityEntry>,
) -> Utf16JsonProjection {
    object_projection(vec![
        (
            "origin".to_owned(),
            origin
                .as_ref()
                .map(origin_projection)
                .unwrap_or_else(null_projection),
        ),
        (
            "push_remote".to_owned(),
            option_string_projection(push_remote.as_deref()),
        ),
        (
            "remotes".to_owned(),
            array_projection(remotes.iter().map(visibility_entry_projection).collect()),
        ),
        ("visibility_cache".to_owned(), array_projection(Vec::new())),
    ])
}

fn status_value(porcelain: &str) -> Utf16JsonProjection {
    let rows: Vec<_> = porcelain.lines().filter(|line| !line.is_empty()).collect();
    let truncated = porcelain.len() > GIT_STATUS_MAX_BYTES;
    let porcelain = if truncated {
        let mut end = GIT_STATUS_MAX_BYTES;
        while !porcelain.is_char_boundary(end) {
            end -= 1;
        }
        &porcelain[..end]
    } else {
        porcelain
    };
    let staged = rows
        .iter()
        .filter(|line| {
            line.as_bytes()
                .first()
                .is_some_and(|status| !matches!(*status, b' ' | b'?'))
        })
        .count();
    let modified = rows
        .iter()
        .filter(|line| line.as_bytes().get(1).is_some_and(|status| *status != b' '))
        .count();
    let untracked_normal = rows.iter().filter(|line| line.starts_with("??")).count();
    let counts = object_projection(vec![
        ("staged".to_owned(), number_projection(staged)),
        ("modified".to_owned(), number_projection(modified)),
        ("untracked".to_owned(), null_projection()),
        (
            "untracked_normal".to_owned(),
            number_projection(untracked_normal),
        ),
    ]);
    object_projection(vec![
        ("clean".to_owned(), bool_projection(rows.is_empty())),
        ("counts".to_owned(), counts),
        ("porcelain".to_owned(), string_projection(porcelain)),
        ("truncated".to_owned(), bool_projection(truncated)),
    ])
}

#[allow(clippy::too_many_arguments)]
fn git_state_value(
    cwd: &Path,
    root: Option<Utf16JsonProjection>,
    branch: Option<Utf16JsonProjection>,
    default_branch: Option<Utf16JsonProjection>,
    status: Option<Utf16JsonProjection>,
    visibility: Option<Utf16JsonProjection>,
    error: Option<&str>,
) -> Utf16JsonProjection {
    let mut fields = vec![
        ("cwd".to_owned(), path_projection(cwd)),
        ("root".to_owned(), root.unwrap_or_else(null_projection)),
        ("branch".to_owned(), branch.unwrap_or_else(null_projection)),
        (
            "default_branch".to_owned(),
            default_branch.unwrap_or_else(null_projection),
        ),
        ("status".to_owned(), status.unwrap_or_else(null_projection)),
    ];
    if let Some(visibility) = visibility {
        fields.push(("visibility".to_owned(), visibility));
    }
    if let Some(error) = error {
        fields.push(("error".to_owned(), string_projection(error)));
    }
    object_projection(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admitted_plan_snapshot(cwd: &Path, home: &Path) -> PermissionPolicySnapshot {
        let rule_sources = [
            "userSettings",
            "projectSettings",
            "localSettings",
            "flagSettings",
            "policySettings",
            "cliArg",
            "command",
            "session",
            "toolsNarrowing",
            "mcpServerPolicy",
            "hostCredential",
        ];
        PermissionPolicySnapshot {
            permission_mode: "plan".to_owned(),
            allow_rules: vec![PermissionPolicyRuleFact {
                rule: Utf16JsonProjection::parse(r#""\ud800""#)
                    .expect("isolated UTF-16 unit is a valid source JSON string"),
                source: "userSettings".to_owned(),
            }],
            deny_rules: Vec::new(),
            ask_rules: Vec::new(),
            rule_roots: Some(
                rule_sources
                    .into_iter()
                    .map(
                        |source| lingxi_core::host::permission_gate::PermissionPolicyRuleRoot {
                            source: source.to_owned(),
                            path: if source == "userSettings" {
                                home.join(".lingxi")
                            } else {
                                cwd.to_path_buf()
                            },
                        },
                    )
                    .collect(),
            ),
            home_directory: Some(home.to_path_buf()),
            working_directory: Some(cwd.to_path_buf()),
            additional_working_directories: Vec::new(),
            block_reads_outside_working_directories: false,
            restricted: false,
            classify_all_shell: false,
            classifier_auto_mode_settings: Some(Default::default()),
            classifier_host_capabilities: Default::default(),
            auto_mode_disabled: false,
            bypass_permissions_available: false,
            bypass_killswitch_active: false,
            bypass_permissions_confirmed: false,
            consecutive_denials: 0,
            total_denials: 0,
            denial_circuit_broken: false,
        }
    }

    #[test]
    fn route_admission_uses_selected_protocol_and_first_party_identity() {
        use llm_runtime::ProtocolFamily;

        assert!(supports_native_classifier_route(
            &ProtocolFamily::AnthropicMessages,
            true
        ));
        assert!(!supports_native_classifier_route(
            &ProtocolFamily::AnthropicMessages,
            false
        ));
        assert!(supports_native_classifier_route(
            &ProtocolFamily::FoundryClaude,
            false
        ));
        assert!(!supports_native_classifier_route(
            &ProtocolFamily::OpenAiResponses,
            true
        ));
    }

    #[tokio::test]
    async fn admitted_plan_snapshot_builds_native_context_with_source_units() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("cwd");
        let home = temp.path().join("home");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let snapshot = admitted_plan_snapshot(&cwd, &home);
        let auto_mode = snapshot
            .classifier_auto_mode_settings
            .as_ref()
            .unwrap()
            .clone();
        let source = classifier_projection(&snapshot, &auto_mode, &cwd, None, None)
            .unwrap()
            .to_json_string()
            .unwrap();
        assert!(source.contains(r#""rule":"\ud800""#));
        let expected_order = [
            "\"v\"",
            "\"permission_mode\"",
            "\"platform\"",
            "\"live_cwd\"",
            "\"home_dir\"",
            "\"rule_roots\"",
            "\"trusted_directories\"",
            "\"rules\"",
            "\"auto_mode\"",
            "\"artifact_consent_holdback\"",
            "\"case_insensitive_paths\"",
            "\"restricted\"",
            "\"is_remote_mode\"",
            "\"classify_all_shell\"",
            "\"user_identity\"",
        ];
        let positions: Vec<_> = expected_order
            .iter()
            .map(|needle| source.find(needle).expect("field is present"))
            .collect();
        assert!(positions.windows(2).all(|window| window[0] < window[1]));
        assert!(request_context(Some(snapshot)).await.is_some());
    }

    #[test]
    fn artifact_holdback_uses_native_negated_env_or_flag_semantics() {
        assert!(!artifact_consent_holdback(None, false));
        assert!(artifact_consent_holdback(None, true));
        assert!(!artifact_consent_holdback(Some("true"), false));
        assert!(artifact_consent_holdback(Some("false"), false));
    }

    #[test]
    fn identity_uses_nullish_environment_order_then_git_email_prefix() {
        assert_eq!(
            identity_from_environment(
                Some("octo.dev"),
                Some("user"),
                Some("name"),
                Some("git@example.com")
            ),
            Some("octo.dev".into())
        );
        assert_eq!(
            identity_from_environment(None, Some("user"), Some("name"), Some("git@example.com")),
            Some("user".into())
        );
        assert_eq!(
            identity_from_environment(None, None, None, Some("git@example.com")),
            Some("git".into())
        );
        assert_eq!(
            identity_from_environment(Some("!!!"), Some("user"), None, None),
            None
        );
    }

    #[tokio::test]
    async fn non_repository_git_state_matches_native_null_and_empty_shape() {
        let directory = tempfile::tempdir().unwrap();
        let git = git_state(directory.path(), false).await;
        let source = git
            .value
            .expect("non-repository fact object")
            .to_json_string()
            .unwrap();
        let value = serde_json::from_str::<serde_json::Value>(&source).unwrap();
        assert_eq!(value["root"], serde_json::Value::Null);
        assert_eq!(value["branch"], serde_json::Value::Null);
        assert_eq!(value["default_branch"], serde_json::Value::Null);
        assert_eq!(value["status"], serde_json::Value::Null);
        assert_eq!(value["visibility"]["origin"], serde_json::Value::Null);
        assert_eq!(value["visibility"]["push_remote"], serde_json::Value::Null);
        assert_eq!(value["visibility"]["remotes"], serde_json::json!([]));
        assert_eq!(
            value["visibility"]["visibility_cache"],
            serde_json::json!([])
        );
        assert_eq!(value["error"], "not_a_repo");
        assert!(value.get("push_remote").is_none());
        assert!(value.get("status").is_some_and(serde_json::Value::is_null));
    }

    #[test]
    fn git_status_projection_preserves_native_counts_and_normal_untracked_nullability() {
        let status = status_value(" M changed\nA  staged\n?? untracked\n");
        let value =
            serde_json::from_str::<serde_json::Value>(&status.to_json_string().unwrap()).unwrap();
        assert_eq!(value["clean"], false);
        assert_eq!(value["counts"]["staged"], 1);
        assert_eq!(value["counts"]["modified"], 2);
        assert_eq!(value["counts"]["untracked"], serde_json::Value::Null);
        assert_eq!(value["counts"]["untracked_normal"], 1);
        assert_eq!(value["porcelain"], " M changed\nA  staged\n?? untracked\n");
    }

    #[test]
    fn native_visibility_origin_omits_remote_name_but_remotes_keep_it() {
        let entry = RemoteVisibilityEntry {
            name: "origin".to_owned(),
            host: "github.com".to_owned(),
            remote: "owner/repo".to_owned(),
        };
        let value = serde_json::from_str::<serde_json::Value>(
            &visibility_value(Some(entry.clone()), Some("origin".to_owned()), vec![entry])
                .to_json_string()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(value["origin"]["host"], "github.com");
        assert_eq!(value["origin"]["remote"], "owner/repo");
        assert!(value["origin"].get("name").is_none());
        assert_eq!(value["remotes"][0]["name"], "origin");
    }

    #[test]
    fn additional_directory_spellings_keep_source_then_resolved_home_path() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("cwd");
        let home = temp.path().join("home");
        let lingxi_home = home.join(".lingxi");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(home.join("alias")).unwrap();
        let roots = permission::filesystem::FsRoots {
            cwd,
            home: Some(home.clone()),
            lingxi_home,
        };
        let raw = Path::new("~/alias");
        let actual = resolved_spellings(raw, &path_projection(raw), false, &roots);
        let value =
            serde_json::from_str::<serde_json::Value>(&actual.to_json_string().unwrap()).unwrap();
        assert_eq!(value[0], "~/alias");
        let expected = serde_json::from_str::<serde_json::Value>(
            &path_projection(&home.join("alias"))
                .to_json_string()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(value[1], expected);
    }

    #[cfg(unix)]
    #[test]
    fn resolved_directory_spellings_keep_original_then_proven_symlink_landing() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let alias = temp.path().join("alias");
        symlink(&target, &alias).unwrap();

        let spellings = permission::filesystem::native_permission_directory_path_spellings(&alias);
        assert_eq!(spellings.first(), Some(&alias));
        assert_eq!(spellings.get(1), Some(&target));
    }

    #[cfg(unix)]
    #[test]
    fn resolved_directory_spellings_keep_missing_tail_after_parent_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let alias = temp.path().join("alias");
        symlink(&target, &alias).unwrap();
        let missing = alias.join("not-created");

        let spellings =
            permission::filesystem::native_permission_directory_path_spellings(&missing);
        assert_eq!(spellings.first(), Some(&missing));
        let resolved_target = std::fs::canonicalize(&target).unwrap();
        assert_eq!(
            spellings.get(1),
            Some(&resolved_target.join("not-created"))
        );
    }
}
