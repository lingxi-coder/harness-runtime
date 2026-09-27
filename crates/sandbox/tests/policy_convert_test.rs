use sandbox::policy_convert::{
    convert_settings_to_runtime_config, linux_glob_pattern_warnings, SandboxConvertContext,
};
use sandbox::runtime_config::{SandboxSettingsJson, SettingsJson, SettingsPermissions};
use std::path::PathBuf;

/// Default (empty) conversion context — most tests don't exercise seeds.
fn ctx() -> SandboxConvertContext {
    SandboxConvertContext::default()
}

fn settings(allow: Vec<&str>, deny: Vec<&str>) -> SettingsJson {
    SettingsJson {
        permissions: Some(SettingsPermissions {
            allow: allow.into_iter().map(String::from).collect(),
            deny: deny.into_iter().map(String::from).collect(),
            additional_directories: vec![],
        }),
        sandbox: Some(SandboxSettingsJson {
            enabled: Some(true),
            ..Default::default()
        }),
        settings_dir: Some(PathBuf::from("/home/u/.lingxi")),
    }
}

#[test]
fn extracts_edit_allow_into_allow_write() {
    let s = settings(vec!["Edit(./src/**)"], vec![]);
    let cfg = convert_settings_to_runtime_config(&s, &ctx());
    assert!(cfg.filesystem.allow_write.contains(&"./src/**".to_string()));
}

#[test]
fn extracts_edit_deny_into_deny_write() {
    let s = settings(vec![], vec!["Edit(//.git/**)"]);
    let cfg = convert_settings_to_runtime_config(&s, &ctx());
    // `//.git/**` resolves to `/.git/**` (one leading slash stripped).
    assert!(cfg.filesystem.deny_write.contains(&"/.git/**".to_string()));
}

#[test]
fn read_allow_does_not_populate_allow_read() {
    // claude-code never maps Read(allow) → allowRead. allowRead comes only from
    // sandbox.filesystem.allowRead (sandbox-adapter.ts:343-347).
    let s = settings(vec!["Read(~/.aws/credentials)"], vec![]);
    let cfg = convert_settings_to_runtime_config(&s, &ctx());
    assert!(
        cfg.filesystem.allow_read.is_empty(),
        "Read(allow) must not seed allow_read, got {:?}",
        cfg.filesystem.allow_read
    );
}

#[test]
fn extracts_read_deny_into_deny_read() {
    let s = settings(vec![], vec!["Read(/secret)"]);
    let cfg = convert_settings_to_runtime_config(&s, &ctx());
    // `/secret` resolves relative to settings_dir = /home/u/.claude.
    assert!(cfg
        .filesystem
        .deny_read
        .contains(&"/home/u/.lingxi/secret".to_string()));
}

#[test]
fn extracts_webfetch_domain_into_allowed_domains() {
    let s = settings(vec!["WebFetch(domain:anthropic.com)"], vec![]);
    let cfg = convert_settings_to_runtime_config(&s, &ctx());
    assert!(cfg
        .network
        .allowed_domains
        .contains(&"anthropic.com".to_string()));
}

#[test]
fn bash_rules_are_ignored_in_this_layer() {
    let s = settings(vec!["Bash(curl:*)"], vec!["Bash(rm:*)"]);
    let cfg = convert_settings_to_runtime_config(&s, &ctx());
    // Bash rules contribute no filesystem paths; only the `.` seed is present
    // in allow_write (claude-code seeds cwd as writable), deny_write empty.
    assert_eq!(cfg.filesystem.allow_write, vec![".".to_string()]);
    assert!(cfg.filesystem.deny_write.is_empty());
}

#[test]
fn passes_through_sandbox_subsection_values() {
    let mut s = settings(vec![], vec![]);
    s.sandbox = Some(SandboxSettingsJson {
        enabled: Some(true),
        fail_if_unavailable: Some(true),
        excluded_commands: Some(vec!["bazel".into(), "make".into()]),
        ..Default::default()
    });
    let cfg = convert_settings_to_runtime_config(&s, &ctx());
    assert!(cfg.enabled);
    assert!(cfg.fail_if_unavailable);
    assert_eq!(cfg.excluded_commands, vec!["bazel", "make"]);
}

#[test]
fn linux_glob_warning_for_star_in_edit_rule() {
    let s = settings(vec!["Edit(./src/*.rs)"], vec![]);
    let warnings = linux_glob_pattern_warnings(&s);
    assert!(
        warnings.iter().any(|w| w == "Edit(./src/*.rs)"),
        "expected warning for Edit(./src/*.rs), got {warnings:?}"
    );
}

#[test]
fn linux_glob_warning_skips_trailing_double_star() {
    let s = settings(vec!["Edit(./src/**)"], vec![]);
    let warnings = linux_glob_pattern_warnings(&s);
    assert!(
        warnings.is_empty(),
        "expected no warning for trailing /** but got {warnings:?}"
    );
}

#[test]
fn linux_glob_warning_for_brackets() {
    let s = settings(vec![], vec!["Read(./[ab]/file)"]);
    let warnings = linux_glob_pattern_warnings(&s);
    assert!(warnings.iter().any(|w| w == "Read(./[ab]/file)"));
}

#[test]
fn linux_glob_warning_for_question_mark() {
    let s = settings(vec!["Edit(./foo?.txt)"], vec![]);
    let warnings = linux_glob_pattern_warnings(&s);
    assert!(warnings.iter().any(|w| w == "Edit(./foo?.txt)"));
}

#[test]
fn deny_webfetch_domain_populates_denied_domains() {
    let s = settings(
        vec!["WebFetch(domain:good.com)"],
        vec!["WebFetch(domain:evil.com)"],
    );
    let cfg = convert_settings_to_runtime_config(&s, &ctx());
    assert_eq!(cfg.network.denied_domains, vec!["evil.com".to_string()]);
    assert!(cfg
        .network
        .allowed_domains
        .contains(&"good.com".to_string()));
    assert!(!cfg
        .network
        .allowed_domains
        .contains(&"evil.com".to_string()));
}

#[test]
fn network_override_does_not_clobber_derived_denied_domains() {
    use sandbox::runtime_config::NetworkRestrictionConfig;
    let mut s = settings(vec![], vec!["WebFetch(domain:evil.com)"]);
    s.sandbox = Some(SandboxSettingsJson {
        enabled: Some(true),
        network: Some(NetworkRestrictionConfig {
            denied_domains: vec!["also-bad.com".into()],
            ..Default::default()
        }),
        ..Default::default()
    });
    let cfg = convert_settings_to_runtime_config(&s, &ctx());
    // Derived deny (evil.com) + override deny (also-bad.com) both present.
    assert!(cfg.network.denied_domains.contains(&"evil.com".to_string()));
    assert!(cfg
        .network
        .denied_domains
        .contains(&"also-bad.com".to_string()));
}

#[test]
fn seeds_allow_write_with_dot_and_lingxi_temp_dir() {
    let s = settings(vec!["Edit(/proj)"], vec![]);
    let c = SandboxConvertContext {
        lingxi_temp_dir: Some("/tmp/claude-501".into()),
        ..Default::default()
    };
    let cfg = convert_settings_to_runtime_config(&s, &c);
    assert_eq!(cfg.filesystem.allow_write[0], ".");
    assert_eq!(cfg.filesystem.allow_write[1], "/tmp/claude-501");
    // Rule-derived path comes AFTER the seeds.
    assert!(cfg
        .filesystem
        .allow_write
        .iter()
        .skip(2)
        .any(|p| p.ends_with("/proj")));
}

#[test]
fn default_ctx_seeds_only_dot_no_temp() {
    let s = settings(vec![], vec![]);
    let cfg = convert_settings_to_runtime_config(&s, &ctx());
    assert_eq!(cfg.filesystem.allow_write, vec![".".to_string()]);
    assert!(cfg.filesystem.deny_write.is_empty());
}

#[test]
fn denies_settings_managed_and_skills_paths() {
    let s = settings(vec![], vec![]);
    let c = SandboxConvertContext {
        settings_file_paths: vec!["/home/u/.lingxi/settings.json".into()],
        managed_drop_in_dir: Some("/Library/managed-settings.d".into()),
        skills_dirs: vec!["/proj/.lingxi/skills".into()],
        ..Default::default()
    };
    let cfg = convert_settings_to_runtime_config(&s, &c);
    assert!(cfg
        .filesystem
        .deny_write
        .contains(&"/home/u/.lingxi/settings.json".to_string()));
    assert!(cfg
        .filesystem
        .deny_write
        .contains(&"/Library/managed-settings.d".to_string()));
    assert!(cfg
        .filesystem
        .deny_write
        .contains(&"/proj/.lingxi/skills".to_string()));
}

#[test]
fn additional_dirs_union_session_and_settings_dedup() {
    let mut s = settings(vec![], vec![]);
    if let Some(p) = s.permissions.as_mut() {
        p.additional_directories = vec!["a".into(), "b".into()];
    }
    let c = SandboxConvertContext {
        additional_md_dirs: vec!["b".into(), "c".into()],
        ..Default::default()
    };
    let cfg = convert_settings_to_runtime_config(&s, &c);
    let aw = &cfg.filesystem.allow_write;
    assert!(aw.contains(&"a".to_string()));
    assert!(aw.contains(&"b".to_string()));
    assert!(aw.contains(&"c".to_string()));
    // `b` appears exactly once (Set dedup).
    assert_eq!(aw.iter().filter(|p| p.as_str() == "b").count(), 1);
}

#[test]
fn policy_convert_does_not_seed_bare_git_paths() {
    // Regression guard: bare-git repo files are owned by the posix prepare
    // layer, NOT this pure conversion (sandbox-adapter.ts:257-280).
    let s = settings(vec![], vec![]);
    let cfg = convert_settings_to_runtime_config(&s, &ctx());
    for forbidden in ["HEAD", "objects", "refs", "hooks", "config"] {
        assert!(
            !cfg.filesystem
                .deny_write
                .iter()
                .any(|p| p.ends_with(forbidden)),
            "convert must not seed bare-git path {forbidden}: {:?}",
            cfg.filesystem.deny_write
        );
    }
}

/// Locks the feed the desktop now exercises end-to-end: a `WebFetch(domain:...)`
/// allow, an `Edit(/src/**)` allow, and a `Read(/secret)` deny — across the SAME
/// `SettingsJson` the engine-desktop `*_from_settings_tiers` helpers assemble —
/// reach the runtime config with paths resolved relative to `settings_dir`.
#[test]
fn webfetch_and_edit_rules_reach_runtime_config() {
    let s = SettingsJson {
        permissions: Some(SettingsPermissions {
            allow: vec![
                "WebFetch(domain:example.com)".to_string(),
                "Edit(/src/**)".to_string(),
            ],
            deny: vec!["Read(/secret)".to_string()],
            additional_directories: vec![],
        }),
        sandbox: Some(SandboxSettingsJson {
            enabled: Some(true),
            ..Default::default()
        }),
        settings_dir: Some(PathBuf::from("/proj")),
    };
    // The default ctx (only `.` seeded into allow_write) is what
    // `sandbox_runtime_config_from_settings_tiers` passes (sans lingxi_temp_dir).
    let cfg = convert_settings_to_runtime_config(&s, &SandboxConvertContext::default());

    assert!(
        cfg.network
            .allowed_domains
            .contains(&"example.com".to_string()),
        "WebFetch domain allow must reach allowed_domains: {:?}",
        cfg.network.allowed_domains
    );
    // `/src/**` resolves relative to settings_dir = /proj.
    assert!(
        cfg.filesystem
            .allow_write
            .contains(&"/proj/src/**".to_string()),
        "Edit allow must reach allow_write resolved against settings_dir: {:?}",
        cfg.filesystem.allow_write
    );
    // `/secret` resolves relative to settings_dir = /proj.
    assert!(
        cfg.filesystem
            .deny_read
            .contains(&"/proj/secret".to_string()),
        "Read deny must reach deny_read resolved against settings_dir: {:?}",
        cfg.filesystem.deny_read
    );
}

#[test]
fn sandbox_filesystem_absolute_path_kept_as_is_not_settings_relative() {
    use sandbox::runtime_config::FilesystemRestrictionConfig;
    // #30067: an absolute path in sandbox.filesystem.allowWrite stays absolute,
    // unlike permission-rule `/path` which is settings-relative.
    let mut s = settings(vec![], vec![]); // settings_dir = /home/u/.claude
    s.sandbox = Some(SandboxSettingsJson {
        enabled: Some(true),
        filesystem: Some(FilesystemRestrictionConfig {
            allow_write: vec!["/Users/foo/.cargo".into()],
            ..Default::default()
        }),
        ..Default::default()
    });
    let cfg = convert_settings_to_runtime_config(&s, &ctx());
    assert!(cfg
        .filesystem
        .allow_write
        .contains(&"/Users/foo/.cargo".to_string()));
    assert!(!cfg
        .filesystem
        .allow_write
        .contains(&"/home/u/.lingxi/Users/foo/.cargo".to_string()));
}

#[test]
fn sandbox_filesystem_relative_path_resolved_against_settings_dir() {
    use sandbox::runtime_config::FilesystemRestrictionConfig;
    let mut s = settings(vec![], vec![]); // settings_dir = /home/u/.claude
    s.sandbox = Some(SandboxSettingsJson {
        enabled: Some(true),
        filesystem: Some(FilesystemRestrictionConfig {
            deny_read: vec!["secret".into()],
            ..Default::default()
        }),
        ..Default::default()
    });
    let cfg = convert_settings_to_runtime_config(&s, &ctx());
    assert!(cfg
        .filesystem
        .deny_read
        .contains(&"/home/u/.lingxi/secret".to_string()));
}

#[test]
fn sandbox_filesystem_double_slash_legacy_escape() {
    use sandbox::runtime_config::FilesystemRestrictionConfig;
    let mut s = settings(vec![], vec![]);
    s.sandbox = Some(SandboxSettingsJson {
        enabled: Some(true),
        filesystem: Some(FilesystemRestrictionConfig {
            allow_write: vec!["//etc/hosts".into()],
            ..Default::default()
        }),
        ..Default::default()
    });
    let cfg = convert_settings_to_runtime_config(&s, &ctx());
    assert!(cfg
        .filesystem
        .allow_write
        .contains(&"/etc/hosts".to_string()));
}

#[test]
fn allowed_domains_order_subsection_before_webfetch() {
    use sandbox::runtime_config::NetworkRestrictionConfig;
    // claude-code order: sandbox.network.allowedDomains FIRST, then WebFetch allow.
    let mut s = settings(vec!["WebFetch(domain:fetch.example)"], vec![]);
    s.sandbox = Some(SandboxSettingsJson {
        enabled: Some(true),
        network: Some(NetworkRestrictionConfig {
            allowed_domains: vec!["sub.example".into()],
            ..Default::default()
        }),
        ..Default::default()
    });
    let cfg = convert_settings_to_runtime_config(&s, &ctx());
    assert_eq!(
        cfg.network.allowed_domains,
        vec!["sub.example".to_string(), "fetch.example".to_string()]
    );
}

// ── allowManagedDomainsOnly / allowManagedReadPathsOnly enforcement ──────────

#[test]
fn managed_allowed_domains_override_replaces_merged_allowlist() {
    // The merged settings allow a user domain via WebFetch; when the managed-only
    // override is active, only the managed allowlist survives.
    let s = settings(vec!["WebFetch(domain:user-allowed.com)"], vec![]);
    let c = SandboxConvertContext {
        managed_allowed_domains: Some(vec!["managed.example".to_string()]),
        ..SandboxConvertContext::default()
    };
    let cfg = convert_settings_to_runtime_config(&s, &c);
    assert_eq!(
        cfg.network.allowed_domains,
        vec!["managed.example".to_string()],
        "user domain dropped under allowManagedDomainsOnly"
    );
}

#[test]
fn managed_read_paths_override_replaces_allow_read() {
    use sandbox::runtime_config::{FilesystemRestrictionConfig, SandboxSettingsJson};
    let mut s = settings(vec![], vec![]);
    s.sandbox = Some(SandboxSettingsJson {
        enabled: Some(true),
        filesystem: Some(FilesystemRestrictionConfig {
            allow_read: vec!["/user/path".to_string()],
            ..Default::default()
        }),
        ..Default::default()
    });
    let c = SandboxConvertContext {
        managed_read_paths: Some(vec!["/managed/only".to_string()]),
        ..SandboxConvertContext::default()
    };
    let cfg = convert_settings_to_runtime_config(&s, &c);
    assert_eq!(
        cfg.filesystem.allow_read,
        vec!["/managed/only".to_string()],
        "user read path dropped under allowManagedReadPathsOnly"
    );
}

#[test]
fn no_override_keeps_merged_allowlist() {
    // Without the override (None), the merged WebFetch domain is kept as-is.
    let s = settings(vec!["WebFetch(domain:keep.me)"], vec![]);
    let cfg = convert_settings_to_runtime_config(&s, &ctx());
    assert!(cfg.network.allowed_domains.contains(&"keep.me".to_string()));
}

#[test]
fn managed_domain_allowlist_collects_subsection_and_webfetch() {
    use sandbox::policy_convert::managed_domain_allowlist;
    use sandbox::runtime_config::{NetworkRestrictionConfig, SandboxSettingsJson};
    let mut s = settings(
        vec!["WebFetch(domain:from-rule.com)", "Bash(curl:*)"],
        vec![],
    );
    s.sandbox = Some(SandboxSettingsJson {
        enabled: Some(true),
        network: Some(NetworkRestrictionConfig {
            allowed_domains: vec!["from-subsection.com".to_string()],
            ..Default::default()
        }),
        ..Default::default()
    });
    // Subsection domains first, then WebFetch-derived; Bash ignored.
    assert_eq!(
        managed_domain_allowlist(&s),
        vec![
            "from-subsection.com".to_string(),
            "from-rule.com".to_string()
        ]
    );
}

// --- deny-write symlink hardening (parity 2.1.210, sandbox-adapter.ts `SS`) ---

#[cfg(unix)]
#[test]
fn seeded_symlink_settings_deny_write_resolves_escape_target() {
    // A `.lingxi/settings.json` that is a SYMLINK to a path outside the
    // workspace must contribute its REAL target to deny_write, not the symlink
    // path — otherwise a write through the redirect escapes the sandbox. The
    // composition root passes every deny-write seed through
    // `resolve_deny_write_symlink` before it enters the context; this exercises
    // that end-to-end with a real on-disk symlink.
    use sandbox::policy_convert::resolve_deny_write_symlink;
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let dot = root.join(".lingxi");
    std::fs::create_dir_all(&dot).expect("mkdir .lingxi");

    // The escape target the attacker points the settings symlink at.
    let evil = root.join("evil_settings.json");
    std::fs::write(&evil, b"{}").expect("write evil");
    let canonical_evil = std::fs::canonicalize(&evil)
        .expect("canonicalize evil")
        .to_string_lossy()
        .into_owned();

    // `.lingxi/settings.json` → evil target.
    let link = dot.join("settings.json");
    symlink(&evil, &link).expect("symlink");
    let link_str = link.to_string_lossy().into_owned();

    // Seed exactly as the composition root does.
    let seed = resolve_deny_write_symlink(&link_str);

    let s = SettingsJson {
        permissions: None,
        sandbox: Some(SandboxSettingsJson {
            enabled: Some(true),
            ..Default::default()
        }),
        settings_dir: Some(dot.clone()),
    };
    let c = SandboxConvertContext {
        settings_file_paths: vec![seed],
        ..Default::default()
    };
    let cfg = convert_settings_to_runtime_config(&s, &c);

    // deny_write holds the CANONICAL escape target …
    assert!(
        cfg.filesystem.deny_write.contains(&canonical_evil),
        "deny_write must contain the resolved symlink target {canonical_evil:?}, got {:?}",
        cfg.filesystem.deny_write
    );
    // … and NOT the unresolved symlink path, which a redirect would bypass.
    assert!(
        !cfg.filesystem.deny_write.contains(&link_str),
        "deny_write must not keep the unresolved symlink path {link_str:?}"
    );
}

#[cfg(unix)]
#[test]
fn seeded_non_symlink_settings_deny_write_kept_literal() {
    // The common case: a real (non-symlink) settings file passes through
    // unchanged, so the seeded deny-write path is byte-identical to today.
    use sandbox::policy_convert::resolve_deny_write_symlink;

    let tmp = tempfile::tempdir().expect("tempdir");
    let dot = tmp.path().join(".lingxi");
    std::fs::create_dir_all(&dot).expect("mkdir");
    let real = dot.join("settings.json");
    std::fs::write(&real, b"{}").expect("write");
    let real_str = real.to_string_lossy().into_owned();

    assert_eq!(resolve_deny_write_symlink(&real_str), real_str);
}

// ---------------------------------------------------------------------------
// `sandbox.network.strictAllowlist` (2.1.219) — SOURCE-RESTRICTED.
//
// Oracle: "Only honored from user, managed/policy, or CLI (--settings)
// settings — project settings (.claude/settings.json and
// .claude/settings.local.json) are ignored."
// ---------------------------------------------------------------------------

/// A settings blob carrying only `sandbox.network.strictAllowlist`.
fn strict_settings(on: bool) -> SettingsJson {
    let mut net = sandbox::runtime_config::NetworkRestrictionConfig::default();
    net.strict_allowlist = on;
    SettingsJson {
        permissions: None,
        sandbox: Some(SandboxSettingsJson {
            network: Some(net),
            ..Default::default()
        }),
        settings_dir: None,
    }
}

#[test]
fn strict_allowlist_defaults_off() {
    let cfg = convert_settings_to_runtime_config(&strict_settings(false), &ctx());
    assert!(!cfg.network.strict_allowlist);
}

#[test]
fn a_merged_tier_alone_cannot_enable_strict_allowlist() {
    // THE security property. A project `.lingxi/settings.json` reaches the
    // converter through the merged blob; without an honored override the flag
    // must stay off, or project settings would silently gain a setting the
    // oracle refuses them.
    let cfg = convert_settings_to_runtime_config(&strict_settings(true), &ctx());
    assert!(
        !cfg.network.strict_allowlist,
        "an unhonored tier must not enable strictAllowlist"
    );
}

#[test]
fn an_honored_source_enables_strict_allowlist() {
    let c = SandboxConvertContext {
        strict_allowlist_override: Some(true),
        ..SandboxConvertContext::default()
    };
    let cfg = convert_settings_to_runtime_config(&strict_settings(false), &c);
    assert!(cfg.network.strict_allowlist);
}

#[test]
fn an_honored_false_overrides_a_merged_true() {
    // Explicit `false` from an honored source must win over a project tier's
    // `true` — the override is authoritative in both directions.
    let c = SandboxConvertContext {
        strict_allowlist_override: Some(false),
        ..SandboxConvertContext::default()
    };
    let cfg = convert_settings_to_runtime_config(&strict_settings(true), &c);
    assert!(!cfg.network.strict_allowlist);
}

#[test]
fn task_output_root_is_protected_even_when_temp_root_is_writable() {
    let mut context = ctx();
    context.lingxi_temp_dir = Some("/tmp/claude-1/".into());
    context.task_output_dir = Some("/tmp/claude-1/project/session/tasks".into());
    let config = convert_settings_to_runtime_config(&SettingsJson::default(), &context);
    assert!(config
        .filesystem
        .deny_write
        .contains(&"/tmp/claude-1/project/session/tasks".into()));
}

// ===== HP-6: `sandbox.credentials` ==========================================

/// 🚨 The wiring test. Every layer below this is pure and green on its own; the
/// failure this closes is a `sandbox.credentials` block that parses, resolves,
/// and then never reaches the runtime config the sandbox is built from — which
/// is indistinguishable, from the user's side, from the silent-ignore it
/// replaces.
#[test]
fn credentials_reach_the_runtime_config_the_sandbox_is_built_from() {
    use sandbox::credentials::{
        CredentialEnvVar, CredentialFile, CredentialMode, SandboxCredentials,
    };
    use sandbox::runtime_config::{SandboxSettingsJson, SettingsJson};

    let mut s = SettingsJson::default();
    s.sandbox = Some(SandboxSettingsJson {
        enabled: Some(true),
        credentials: Some(SandboxCredentials {
            files: Some(vec![
                CredentialFile {
                    path: "/home/u/.aws/credentials".into(),
                    mode: CredentialMode::Deny,
                    extract: None,
                    on_extract_no_match: None,
                    inject_hosts: None,
                },
                CredentialFile {
                    path: "/home/u/.npmrc".into(),
                    mode: CredentialMode::Mask,
                    extract: None,
                    on_extract_no_match: None,
                    inject_hosts: None,
                },
            ]),
            env_vars: Some(vec![CredentialEnvVar {
                name: "AWS_SECRET_ACCESS_KEY".into(),
                mode: CredentialMode::Mask,
                extract: None,
                on_extract_no_match: None,
                decode: None,
                mask_claims: None,
                inject_hosts: None,
            }]),
            ..SandboxCredentials::default()
        }),
        ..Default::default()
    });

    let cfg = convert_settings_to_runtime_config(&s, &SandboxConvertContext::default());

    let denied = |needle: &str| cfg.filesystem.deny_read.iter().any(|p| p.contains(needle));
    assert!(
        denied(".aws/credentials"),
        "a credential file deny must reach filesystem.deny_read: {:?}",
        cfg.filesystem.deny_read
    );
    assert!(
        denied(".npmrc"),
        "…and so must a MASK, because this port degrades it to a deny rather \
         than leaving the file readable: {:?}",
        cfg.filesystem.deny_read
    );
    assert_eq!(
        cfg.credential_deny_env,
        vec!["AWS_SECRET_ACCESS_KEY"],
        "the env var must reach the list the wrap seam unsets from"
    );
}

/// An absent block must leave the runtime config byte-identical to before this
/// feature existed — the whole point is that nothing changes until a user asks.
#[test]
fn no_credentials_block_changes_nothing() {
    use sandbox::runtime_config::{SandboxSettingsJson, SettingsJson};

    let mut s = SettingsJson::default();
    s.sandbox = Some(SandboxSettingsJson {
        enabled: Some(true),
        ..Default::default()
    });
    let cfg = convert_settings_to_runtime_config(&s, &SandboxConvertContext::default());
    assert!(cfg.credential_deny_env.is_empty());
}
