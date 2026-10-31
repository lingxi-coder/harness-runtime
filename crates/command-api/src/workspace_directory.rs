//! Shared Native `/cd` and `/add-dir` path validation.
//! Hosts supply the live session cwd and home explicitly. Arguments are one
//! path string and are never split or interpreted as shell syntax.

use std::path::{Component, Path, PathBuf};
use unicode_normalization::UnicodeNormalization;

/// Native's logical cwd text is NFC even when realpath returned a decomposed
/// filesystem spelling. Keep the Path itself intact for filesystem operations.
#[must_use]
pub fn logical_directory_text(path: &Path) -> String {
    path.to_string_lossy().nfc().collect()
}

/// Outcome of validating a `/add-dir` argument, mirroring claude-code's
/// `AddDirectoryResult` union (minus the `alreadyInWorkingDirectory` arm, which
/// is folded into the idempotent persist step — see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddDirValidation {
    /// No path was supplied.
    EmptyPath,
    /// A NUL byte cannot be passed through the filesystem path boundary.
    InvalidPath {
        /// Original argument containing the invalid character.
        input: String,
    },
    /// The resolved path does not exist or is inaccessible (claude-code maps
    /// ENOENT/ENOTDIR/EACCES/EPERM all to "not found").
    PathNotFound {
        /// Resolved absolute path that could not be accessed.
        absolute: String,
    },
    /// The resolved path exists but is a file, not a directory.
    NotADirectory {
        /// Original argument that resolved to a file.
        input: String,
        /// Parent directory suggested to the caller.
        parent: String,
    },
    /// UNC / `/net/<host>` automount — refused before `stat` (claude-code
    /// `resultType:"networkPath"`).
    NetworkPath {
        /// Original network path that was refused.
        directory_path: String,
    },
    /// The path resolved to an existing directory at `absolute`.
    Success {
        /// Resolved absolute directory path.
        absolute: String,
    },
}

/// Expand a leading `~` / `~/…` against the user's home directory. Any other
/// use of `~` is left verbatim (claude-code `expandPath`).
fn expand_tilde(input: &str, home: Option<&Path>) -> PathBuf {
    if input == "~" {
        return home
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("~"));
    }
    if let Some(rest) = input.strip_prefix("~/") {
        if let Some(home) = home {
            return home.join(rest);
        }
    }
    PathBuf::from(input)
}

/// Lexically normalize `path` (resolve `.`/`..` components and drop the
/// trailing slash) WITHOUT touching the filesystem — mirrors node
/// `path.resolve`'s normalization. Symlinks are intentionally NOT resolved,
/// matching the reference (`resolve`, not `realpath`).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() && !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolve `input` to an absolute, normalized path and validate that it exists
/// and is a directory. Relative inputs resolve against the supplied live
/// session working directory, matching claude-code `resolve(expandPath(...))`.
/// Network paths are refused **before** `stat`, matching `validateDirectoryForWorkspace`.
#[must_use]
pub fn resolve_and_validate(input: &str, cwd: &Path, home: Option<&Path>) -> AddDirValidation {
    resolve_and_validate_inner(input, cwd, home, true)
}

/// Like [`resolve_and_validate`] but does not refuse network paths. `/cd` reuses
/// the expand/stat checks without the working-directory network gate.
#[must_use]
pub fn resolve_existing_directory(
    input: &str,
    cwd: &Path,
    home: Option<&Path>,
) -> AddDirValidation {
    resolve_and_validate_inner(input, cwd, home, false)
}

fn resolve_and_validate_inner(
    input: &str,
    cwd: &Path,
    home: Option<&Path>,
    reject_network: bool,
) -> AddDirValidation {
    if input.is_empty() {
        return AddDirValidation::EmptyPath;
    }
    if reject_network && input.contains('\0') {
        return AddDirValidation::InvalidPath {
            input: input.into(),
        };
    }
    let expanded = expand_tilde(input, home);
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    let absolute = normalize(&absolute);
    let absolute_str = absolute.to_string_lossy().to_string();
    if reject_network {
        let cwd = cwd.to_str();
        if permission::is_network_working_directory_against(input, cwd)
            || permission::is_network_working_directory_against(&absolute_str, cwd)
        {
            return AddDirValidation::NetworkPath {
                directory_path: input.to_string(),
            };
        }
    }
    match std::fs::metadata(&absolute) {
        Ok(meta) if meta.is_dir() => AddDirValidation::Success {
            absolute: absolute_str,
        },
        Ok(_) => {
            let parent = absolute
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            AddDirValidation::NotADirectory {
                input: input.to_string(),
                parent,
            }
        }
        // Match claude-code: any stat error (ENOENT/ENOTDIR/EACCES/EPERM) reads
        // as "not found" rather than crashing the command.
        Err(_) => AddDirValidation::PathNotFound {
            absolute: absolute_str,
        },
    }
}

/// The user-facing message for a NON-success validation result (claude-code
/// `addDirHelpMessage`, minus the terminal-bold markup).
#[must_use]
pub fn help_message(result: &AddDirValidation) -> String {
    match result {
        AddDirValidation::EmptyPath => "Please provide a directory path.".to_string(),
        AddDirValidation::InvalidPath { input } => format!(
            "Path {} contains a null character, so it can't be used as a working directory. Remove the null character from the path or from the settings entry that lists it.",
            serde_json::to_string(input).expect("path text is JSON representable")
        ),
        AddDirValidation::PathNotFound { absolute } => format!("Path {absolute} was not found."),
        AddDirValidation::NotADirectory { input, parent } => format!(
            "{input} is not a directory. Did you mean to add the parent directory {parent}?"
        ),
        AddDirValidation::NetworkPath { directory_path } => {
            permission::network_working_directory_message(directory_path)
        }
        AddDirValidation::Success { absolute } => {
            format!("Added {absolute} as a working directory.")
        }
    }
}

/// Resolve one path string without interpreting shell quoting or splitting spaces.
#[must_use]
pub fn resolve_path(input: &str, cwd: &Path, home: Option<&Path>) -> PathBuf {
    let expanded = expand_tilde(input, home);
    normalize(&if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    })
}

/// Source-aware Native Cd rule decision. Denies see requested symlink spellings;
/// allows see only the canonical path and verified system aliases.
#[must_use]
pub fn check_cd_permission(
    requested: &Path,
    canonical: &Path,
    rules: &[permission::PermissionRule],
    roots: &permission::FsRoots,
) -> crate::cd_permissions::CdCheck {
    let mut deny_paths = directory_spellings(requested);
    if !deny_paths.contains(&canonical.to_path_buf()) {
        deny_paths.push(canonical.to_path_buf());
    }
    let mut allow_paths = vec![canonical.to_path_buf()];
    for (alias, real) in [
        ("/tmp", "/private/tmp"),
        ("/var", "/private/var"),
        ("/etc", "/private/etc"),
        ("/bin", "/usr/bin"),
        ("/lib", "/usr/lib"),
        ("/sbin", "/usr/sbin"),
    ] {
        if std::fs::canonicalize(alias).ok().as_deref() == Some(Path::new(real)) {
            if let Ok(tail) = canonical.strip_prefix(real) {
                let path = Path::new(alias).join(tail);
                if !allow_paths.contains(&path) {
                    allow_paths.push(path);
                }
            }
        }
    }
    check_cd_permission_with_paths(&deny_paths, &allow_paths, rules, roots)
}

/// Native rule decision after the host has resolved the relevant path forms.
/// Caller ordering is retained for first-match citations and allow patterns.
#[must_use]
pub fn check_cd_permission_with_paths(
    deny_paths: &[PathBuf],
    allow_paths: &[PathBuf],
    rules: &[permission::PermissionRule],
    roots: &permission::FsRoots,
) -> crate::cd_permissions::CdCheck {
    use crate::cd_permissions::{format_cd_rule, CdCheck};
    let ordered = rules
        .iter()
        .filter(|rule| rule.value.tool_name == "Cd")
        .collect::<Vec<_>>();
    for rule in ordered
        .iter()
        .copied()
        .filter(|rule| rule.behavior == permission::PermissionBehavior::Deny)
    {
        if rule.value.rule_content.is_none()
            || deny_paths
                .iter()
                .any(|path| cd_rule_matches(path, rule, roots))
        {
            return CdCheck::BlockedByRule {
                rule: format_cd_rule(rule.value.rule_content_text()),
                source_label: cd_rule_source_label(rule.source).into(),
                tool_wide: rule.value.rule_content.is_none(),
            };
        }
    }
    let allowed = ordered
        .into_iter()
        .filter(|rule| rule.behavior == permission::PermissionBehavior::Allow)
        .collect::<Vec<_>>();
    if allowed.is_empty()
        || allowed.iter().any(|rule| {
            rule.value.rule_content.is_none()
                || allow_paths
                    .iter()
                    .any(|path| cd_rule_matches(path, rule, roots))
        })
    {
        CdCheck::Allowed
    } else {
        CdCheck::OutsideAllowedPatterns {
            allowed_patterns: allowed
                .into_iter()
                .filter_map(|rule| rule.value.rule_content_text().map(str::to_owned))
                .collect(),
        }
    }
}

fn cd_rule_matches(
    path: &Path,
    rule: &permission::PermissionRule,
    roots: &permission::FsRoots,
) -> bool {
    let Some(pattern) = rule.value.rule_content_text() else {
        return false;
    };
    let (relative, root) = if let Some(rest) = pattern.strip_prefix("//") {
        (rest, Path::new("/"))
    } else if let Some(rest) = pattern.strip_prefix("~/") {
        let Some(home) = roots.home.as_deref() else {
            return false;
        };
        (rest, home)
    } else if pattern.starts_with('/') {
        let root = match rule.source {
            permission::PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User) => {
                roots.lingxi_home.as_path()
            }
            _ => roots.cwd.as_path(),
        };
        (pattern, root)
    } else {
        (
            pattern.strip_prefix("./").unwrap_or(pattern),
            roots.cwd.as_path(),
        )
    };
    let Ok(path) = path.strip_prefix(root) else {
        return false;
    };
    crate::cd_permissions::matches_cd_pattern(relative, &path.to_string_lossy().replace('\\', "/"))
}

fn cd_rule_source_label(source: permission::PermissionRuleSource) -> &'static str {
    use lingxi_core::types::SettingsScope as Scope;
    use permission::PermissionRuleSource as Source;
    match source {
        Source::Settings(Scope::User) => "user settings",
        Source::Settings(Scope::Project) => "shared project settings",
        Source::Settings(Scope::Local) => "project local settings",
        Source::Settings(Scope::Managed) => "enterprise managed settings",
        Source::FlagSettings => "command line arguments",
        Source::CliArg => "CLI argument",
        Source::Command => "command configuration",
        Source::Session => "current session",
        Source::ToolsNarrowing => "CLI tool narrowing",
        Source::McpServerPolicy => "MCP server policy",
    }
}

fn directory_spellings(requested: &Path) -> Vec<PathBuf> {
    let mut paths = vec![requested.to_path_buf()];
    let mut next = 0;
    while next < paths.len() && next < 64 {
        let path = paths[next].clone();
        for ancestor in path.ancestors() {
            let Ok(link) = std::fs::read_link(ancestor) else {
                continue;
            };
            let resolved = if link.is_absolute() {
                link
            } else {
                ancestor.parent().unwrap_or(Path::new("/")).join(link)
            };
            let resolved = normalize(
                &resolved.join(path.strip_prefix(ancestor).expect("ancestor contains path")),
            );
            if !paths.contains(&resolved) {
                paths.push(resolved);
            }
        }
        next += 1;
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_expansion_is_explicit_and_absent_home_preserves_the_tilde() {
        assert_eq!(
            expand_tilde("~", Some(Path::new("/home/user"))),
            Path::new("/home/user")
        );
        assert_eq!(
            expand_tilde("~/目录 with spaces", Some(Path::new("/home/user"))),
            Path::new("/home/user/目录 with spaces")
        );
        assert_eq!(expand_tilde("~/child", None), Path::new("~/child"));
        assert_eq!(normalize(Path::new("/a/../../b")), Path::new("/b"));
        let raw = Path::new("/workspace/cafe\u{301}");
        assert_eq!(logical_directory_text(raw), "/workspace/caf\u{e9}");
        assert_eq!(raw.to_str(), Some("/workspace/cafe\u{301}"));
        let invalid = resolve_and_validate("/workspace/B\0hidden", Path::new("/workspace/A"), None);
        assert_eq!(help_message(&invalid), "Path \"/workspace/B\\u0000hidden\" contains a null character, so it can't be used as a working directory. Remove the null character from the path or from the settings entry that lists it.");
    }

    #[test]
    fn cd_permissions_preserve_native_allow_and_deny_order_and_roots() {
        use crate::cd_permissions::CdCheck;
        let roots = permission::FsRoots {
            cwd: "/workspace".into(),
            home: Some("/home/user".into()),
            lingxi_home: "/config".into(),
        };
        let rules = permission::permission_rules_from_settings_json(
            r#"{"permissions":{"deny":["Cd(./private/**)"],"allow":["Cd(./allowed/**)"]}}"#,
            permission::PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert!(matches!(
            check_cd_permission(
                Path::new("/workspace/private/目录 with spaces"),
                Path::new("/workspace/private/目录 with spaces"),
                &rules,
                &roots
            ),
            CdCheck::BlockedByRule { .. }
        ));
        assert_eq!(
            check_cd_permission(
                Path::new("/workspace/allowed/目录 with spaces"),
                Path::new("/workspace/allowed/目录 with spaces"),
                &rules,
                &roots
            ),
            CdCheck::Allowed
        );
        assert!(matches!(
            check_cd_permission(
                Path::new("/outside/allowed"),
                Path::new("/outside/allowed"),
                &rules,
                &roots
            ),
            CdCheck::OutsideAllowedPatterns { .. }
        ));
    }

    #[test]
    fn native_307_cd_permission_decisions_match_the_captured_oracle() {
        use crate::cd_permissions::{format_cd_rule, CdCheck};
        use permission::{
            PermissionBehavior as Behavior, PermissionRule, PermissionRuleSource as Source,
            PermissionRuleValue,
        };
        fn source(value: &str) -> Source {
            use lingxi_core::types::SettingsScope as Scope;
            match value {
                "userSettings" => Source::Settings(Scope::User),
                "projectSettings" => Source::Settings(Scope::Project),
                "localSettings" => Source::Settings(Scope::Local),
                "policySettings" => Source::Settings(Scope::Managed),
                "cliArg" => Source::CliArg,
                _ => Source::Session,
            }
        }
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../session/tests/fixtures/workspace_session_2_1_307.json"
        ))
        .unwrap();
        for case in fixture["cd_permission_cases"].as_array().unwrap() {
            let input = &case["input"];
            let canonical = PathBuf::from(input["canonical_path"].as_str().unwrap());
            let mut deny_paths = input["requested_spellings"]
                .as_array()
                .map(|paths| {
                    paths
                        .iter()
                        .map(|path| PathBuf::from(path.as_str().unwrap()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|| vec![PathBuf::from(input["requested_path"].as_str().unwrap())]);
            deny_paths.push(canonical.clone());
            let mut allow_paths = vec![canonical.clone()];
            for pair in input["canonical_equivalences"]
                .as_array()
                .into_iter()
                .flatten()
            {
                if let Ok(tail) = canonical.strip_prefix(pair[0].as_str().unwrap()) {
                    allow_paths.push(Path::new(pair[1].as_str().unwrap()).join(tail));
                }
            }
            let mut rules = Vec::new();
            for (name, behavior) in [("deny", Behavior::Deny), ("allow", Behavior::Allow)] {
                for rule in input[name].as_array().unwrap() {
                    let value = &rule["ruleValue"];
                    let tool = value["toolName"].as_str().unwrap();
                    let spelling = value["ruleContent"]
                        .as_str()
                        .map(|pattern| format!("{tool}({pattern})"))
                        .unwrap_or_else(|| tool.into());
                    rules.push(PermissionRule {
                        value: PermissionRuleValue::from_rule_string(&spelling),
                        behavior,
                        source: source(rule["source"].as_str().unwrap()),
                    });
                }
            }
            let roots = permission::FsRoots {
                cwd: input["current_dir"].as_str().unwrap().into(),
                home: Some("/home/test".into()),
                lingxi_home: "/home/test/.claude".into(),
            };
            let actual = check_cd_permission_with_paths(&deny_paths, &allow_paths, &rules, &roots);
            let expected = &case["expected"];
            match actual {
                CdCheck::Allowed => assert_eq!(expected["result"], "allowed", "{}", case["name"]),
                CdCheck::BlockedByRule {
                    rule,
                    source_label,
                    tool_wide,
                } => {
                    assert_eq!(expected["result"], "blockedByRule", "{}", case["name"]);
                    let denied = &expected["rule"];
                    let pattern = denied["ruleValue"]["ruleContent"].as_str();
                    assert_eq!(rule, format_cd_rule(pattern));
                    assert_eq!(
                        source_label,
                        cd_rule_source_label(source(denied["source"].as_str().unwrap()))
                    );
                    assert_eq!(tool_wide, pattern.is_none());
                }
                CdCheck::OutsideAllowedPatterns { allowed_patterns } => {
                    assert_eq!(
                        expected["result"], "outsideAllowedPatterns",
                        "{}",
                        case["name"]
                    );
                    assert_eq!(
                        serde_json::json!(allowed_patterns),
                        expected["allowedPatterns"]
                    );
                }
            }
        }
    }
}
