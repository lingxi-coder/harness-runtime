//! Tests for the PowerShell containment maps + normalizers (claude-code `y_`,
//! `hhe`, `Rgg`, `FKn`, `Rtt`, `LKn`), byte-checked against binary 2.1.206.

use super::*;
use crate::mode::PermissionMode;

#[test]
fn normalize_resolves_common_aliases() {
    // `hhe` alias → canonical cmdlet, lowercased (claude-code `y_`).
    assert_eq!(normalize_cmdlet("ls"), "get-childitem");
    assert_eq!(normalize_cmdlet("gc"), "get-content");
    assert_eq!(normalize_cmdlet("cat"), "get-content");
    assert_eq!(normalize_cmdlet("rm"), "remove-item");
    assert_eq!(normalize_cmdlet("del"), "remove-item");
    assert_eq!(normalize_cmdlet("cp"), "copy-item");
    assert_eq!(normalize_cmdlet("mv"), "move-item");
    assert_eq!(normalize_cmdlet("cd"), "set-location");
    assert_eq!(normalize_cmdlet("%"), "foreach-object");
    assert_eq!(normalize_cmdlet("?"), "where-object");
}

#[test]
fn normalize_is_case_insensitive_and_canonical_passthrough() {
    assert_eq!(normalize_cmdlet("Get-Content"), "get-content");
    assert_eq!(normalize_cmdlet("GET-CONTENT"), "get-content");
    // A canonical cmdlet not in the alias table returns itself lowercased.
    assert_eq!(normalize_cmdlet("Remove-Item"), "remove-item");
    // Unknown command → itself (lowercased), unchanged.
    assert_eq!(normalize_cmdlet("Some-CustomCmdlet"), "some-customcmdlet");
}

#[test]
fn normalize_strips_exe_extension_only_without_path_separator() {
    // `dgg = /\.(exe|cmd|bat|com)$/` strips when there is no `/` or `\`.
    assert_eq!(normalize_cmdlet("pwsh.exe"), "pwsh");
    assert_eq!(normalize_cmdlet("gc.cmd"), "get-content"); // strip then alias
    assert_eq!(normalize_cmdlet("foo.bat"), "foo");
    assert_eq!(normalize_cmdlet("bar.com"), "bar");
    // A name WITH a path separator keeps its extension (no strip).
    assert_eq!(normalize_cmdlet("C:\\tools\\foo.exe"), "c:\\tools\\foo.exe");
    assert_eq!(normalize_cmdlet("./foo.exe"), "./foo.exe");
    // A non-exe extension is not stripped.
    assert_eq!(normalize_cmdlet("data.txt"), "data.txt");
}

#[test]
fn fkn_has_all_40_cmdlets() {
    assert_eq!(FKN.len(), 40, "FKn must carry all 40 path-taking cmdlets");
}

#[test]
fn fkn_operation_types_and_path_params() {
    let gc = FKN.get("get-content").expect("get-content in FKn");
    assert_eq!(gc.operation_type, PsOperation::Read);
    assert_eq!(gc.path_params, &["-path", "-literalpath", "-pspath", "-lp"]);

    let rm = FKN.get("remove-item").expect("remove-item in FKn");
    assert_eq!(rm.operation_type, PsOperation::Write);
    assert!(rm.known_switches.contains(&"-recurse"));

    // Out-File / Tee-Object lead with -FilePath.
    assert_eq!(FKN.get("out-file").unwrap().path_params[0], "-filepath");
}

#[test]
fn fkn_special_shapes() {
    // New-Item's -Name is a leaf-only path param.
    let ni = FKN.get("new-item").unwrap();
    assert_eq!(ni.leaf_only_path_params, &["-name"]);
    assert_eq!(ni.positional_skip, 0);

    // Invoke-WebRequest/RestMethod skip 1 positional (the URI) and are optional writes.
    for c in ["invoke-webrequest", "invoke-restmethod"] {
        let e = FKN.get(c).unwrap();
        assert_eq!(e.operation_type, PsOperation::Write);
        assert_eq!(e.positional_skip, 1, "{c} skips the positional URI");
        assert!(e.optional_write, "{c} is an optional write");
        assert_eq!(e.path_params, &["-outfile", "-infile"]);
    }

    // Copy/Move-Item include -Destination as a path param.
    assert!(FKN
        .get("copy-item")
        .unwrap()
        .path_params
        .contains(&"-destination"));
    assert!(FKN
        .get("move-item")
        .unwrap()
        .path_params
        .contains(&"-destination"));

    // Pop-Location takes no path params.
    assert!(FKN.get("pop-location").unwrap().path_params.is_empty());
}

#[test]
fn read_only_set_and_common_params() {
    assert!(RGG.contains("get-childitem"));
    assert!(RGG.contains("test-path"));
    assert!(!RGG.contains("set-content"));
    assert_eq!(RGG.len(), 8);

    assert_eq!(WWI, &["-verbose", "-debug"]);
    assert!(GWI.contains(&"-erroraction"));
    assert!(GWI.contains(&"-ea"));
    assert_eq!(GWI.len(), 14);
}

#[test]
fn is_parameter_matches_rtt() {
    // Known element type wins.
    assert!(is_parameter("anything", Some("Parameter")));
    assert!(!is_parameter("-path", Some("StringConstant")));
    // Fallback: leading parameter-prefix char (hyphen or unicode dashes).
    assert!(is_parameter("-path", None));
    assert!(is_parameter("\u{2013}path", None)); // en dash
    assert!(is_parameter("\u{2015}path", None)); // horizontal bar
    assert!(!is_parameter("path", None));
    assert!(!is_parameter("", None));
}

/// Build a command whose arg element types are all unknown (empty vector ⇒
/// `is_parameter` falls back to the leading-dash heuristic, like a real
/// text-only reconstruction).
fn cmd(name: &str, args: &[&str]) -> PsCommand {
    PsCommand {
        name: name.to_string(),
        args: args.iter().map(|s| (*s).to_string()).collect(),
        element_types: Vec::new(),
        redirections: Vec::new(),
        ..PsCommand::default()
    }
}

/// Build a command with explicit per-arg element types (index 0 = name type).
fn cmd_typed(name: &str, args: &[&str], types: &[&str]) -> PsCommand {
    PsCommand {
        name: name.to_string(),
        args: args.iter().map(|s| (*s).to_string()).collect(),
        element_types: types.iter().map(|s| (*s).to_string()).collect(),
        redirections: Vec::new(),
        ..PsCommand::default()
    }
}

#[test]
fn extract_positional_and_named_paths() {
    // Positional read path.
    let e = extract_paths(&cmd("Get-Content", &["foo.txt"]));
    assert_eq!(e.paths, vec!["foo.txt"]);
    assert_eq!(e.operation_type, PsOperation::Read);
    assert!(!e.has_unvalidatable_path_arg);

    // Named -Path with a skipped -Value.
    let e = extract_paths(&cmd(
        "Set-Content",
        &["-Path", "out.txt", "-Value", "hello"],
    ));
    assert_eq!(e.paths, vec!["out.txt"]);
    assert_eq!(e.operation_type, PsOperation::Write);

    // Alias resolves + positional path.
    assert_eq!(extract_paths(&cmd("gc", &["a.log"])).paths, vec!["a.log"]);
}

#[test]
fn extract_colon_form_and_quote_strip() {
    assert_eq!(
        extract_paths(&cmd("Set-Content", &["-Path:out.txt"])).paths,
        vec!["out.txt"]
    );
    // Surrounding quotes stripped from a -Param:value value.
    assert_eq!(
        extract_paths(&cmd("Set-Content", &["-Path:\"my file.txt\""])).paths,
        vec!["my file.txt"]
    );
    // Unambiguous abbreviation binds to -Path.
    assert_eq!(
        extract_paths(&cmd("Get-Content", &["-pa", "z.txt"])).paths,
        vec!["z.txt"]
    );
}

#[test]
fn extract_switch_ignored_value_param_skipped() {
    // -Recurse is a switch (no value); the positional after it is the path.
    let e = extract_paths(&cmd("Get-ChildItem", &["-Recurse", "src"]));
    assert_eq!(e.paths, vec!["src"]);
    // -Filter is a value param → its value is skipped, not treated as a path.
    let e = extract_paths(&cmd("Get-Content", &["-Filter", "*.rs", "real.txt"]));
    assert_eq!(e.paths, vec!["real.txt"]);
}

#[test]
fn extract_positional_skip_and_optional_write() {
    // Invoke-WebRequest skips the positional URI (positional_skip=1); only -OutFile is a path.
    let e = extract_paths(&cmd(
        "Invoke-WebRequest",
        &["https://example.com/x", "-OutFile", "dl.bin"],
    ));
    assert_eq!(e.paths, vec!["dl.bin"]);
    assert_eq!(e.operation_type, PsOperation::Write);
    assert!(e.optional_write);
    // With no path at all, extraction yields none (caller decides via optional_write).
    let e = extract_paths(&cmd("Invoke-WebRequest", &["https://example.com/x"]));
    assert!(e.paths.is_empty());
    assert!(e.optional_write);
}

#[test]
fn extract_leaf_only_param() {
    // New-Item -Name takes a bare leaf → valid path.
    assert_eq!(
        extract_paths(&cmd("New-Item", &["-Name", "notes.md"])).paths,
        vec!["notes.md"]
    );
    // A leaf value containing a separator is un-validatable, NOT a path.
    let e = extract_paths(&cmd("New-Item", &["-Name", "sub/notes.md"]));
    assert!(e.paths.is_empty());
    assert!(e.has_unvalidatable_path_arg);
}

#[test]
fn extract_unknown_param_and_array_value_flag_unvalidatable() {
    // Unknown parameter → unvalidatable; a -X:value still contributes its value.
    let e = extract_paths(&cmd("Set-Content", &["-Bogus:val"]));
    assert!(e.has_unvalidatable_path_arg);
    assert_eq!(e.paths, vec!["val"]);
    // An array-literal value on a path param is flagged unvalidatable (eGi).
    let e = extract_paths(&cmd("Set-Content", &["-Path:@(a,b)"]));
    assert!(e.has_unvalidatable_path_arg);
}

#[test]
fn extract_element_type_marks_unvalidatable() {
    // A path value whose AST element type is a Variable (not StringConstant/Parameter)
    // is pushed but flags unvalidatable (the `d()` peek).
    // types: [0]=cmd name, [1]=-Path (Parameter), [2]=$var (Variable)
    let e = extract_paths(&cmd_typed(
        "Set-Content",
        &["-Path", "$var"],
        &["StringConstant", "Parameter", "Variable"],
    ));
    assert_eq!(e.paths, vec!["$var"]);
    assert!(e.has_unvalidatable_path_arg);
}

#[test]
fn extract_non_path_cmdlet_is_empty_read() {
    let e = extract_paths(&cmd("Write-Output", &["hello", "world"]));
    assert!(e.paths.is_empty());
    assert_eq!(e.operation_type, PsOperation::Read);
    assert!(!e.has_unvalidatable_path_arg);
}

fn dirs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn format_dir_list_matches_mkn() {
    assert_eq!(format_dir_list(&dirs(&["/a", "/b"])), "'/a', '/b'");
    assert_eq!(format_dir_list(&dirs(&["/a"])), "'/a'");
    // Exactly 5 → all listed.
    assert_eq!(
        format_dir_list(&dirs(&["/1", "/2", "/3", "/4", "/5"])),
        "'/1', '/2', '/3', '/4', '/5'"
    );
    // 6 → first 5 + "and 1 more".
    assert_eq!(
        format_dir_list(&dirs(&["/1", "/2", "/3", "/4", "/5", "/6"])),
        "'/1', '/2', '/3', '/4', '/5', and 1 more"
    );
    assert_eq!(
        format_dir_list(&dirs(&["/1", "/2", "/3", "/4", "/5", "/6", "/7", "/8"])),
        "'/1', '/2', '/3', '/4', '/5', and 3 more"
    );
}

#[test]
fn containment_messages_use_lingxi_brand_and_correct_verb() {
    let m = cmdlet_containment_message("get-content", "/etc/passwd", &dirs(&["/work"]));
    assert_eq!(
        m,
        "get-content targeting '/etc/passwd' was blocked. For security, LingXi may only access files in the allowed working directories for this session: '/work'."
    );
    let r = redirection_containment_message("/etc/x", &dirs(&["/work"]));
    assert_eq!(
        r,
        "Output redirection to '/etc/x' was blocked. For security, LingXi may only write to files in the allowed working directories for this session: '/work'."
    );
    assert_eq!(
        remove_item_protected_message("/etc"),
        "Remove-Item on system path '/etc' is blocked. This path is protected from removal."
    );
}

#[test]
fn expand_tilde_matches_ukn() {
    assert_eq!(expand_tilde("~", Some("/home/u")), "/home/u");
    assert_eq!(expand_tilde("~/proj", Some("/home/u")), "/home/u/proj");
    assert_eq!(expand_tilde("~\\proj", Some("/home/u")), "/home/u\\proj");
    // A `~user` form is NOT expanded (not `~`/`~/`/`~\`).
    assert_eq!(expand_tilde("~bob/x", Some("/home/u")), "~bob/x");
    // No home available → unchanged.
    assert_eq!(expand_tilde("~/proj", None), "~/proj");
    // Non-tilde unchanged.
    assert_eq!(expand_tilde("/abs/path", Some("/home/u")), "/abs/path");
}

#[test]
fn traversal_after_segment_matches_eur() {
    assert!(has_traversal_after_segment("a/../b", false));
    assert!(has_traversal_after_segment("./a/../b", false));
    assert!(has_traversal_after_segment("dir/..", false));
    // A leading `..` (before any real segment) does NOT count.
    assert!(!has_traversal_after_segment("../a", false));
    assert!(!has_traversal_after_segment("../../x", false));
    assert!(!has_traversal_after_segment("a/b/c", false));
    // Windows separators.
    assert!(has_traversal_after_segment("a\\..\\b", true));
    assert!(!has_traversal_after_segment("a\\..\\b", false)); // backslash not a sep off Windows
}

#[test]
fn glob_index_matches_uxe() {
    assert_eq!(glob_index("a*b"), Some(1));
    assert_eq!(glob_index("a?b"), Some(1));
    assert_eq!(glob_index("a[bc]d"), Some(1));
    assert_eq!(glob_index("plain.txt"), None);
    // `[` with no closing `]` is not a glob.
    assert_eq!(glob_index("a[bc"), None);
}

#[test]
fn dotdot_segment_matches_ote() {
    assert!(has_dotdot_segment("a/../b"));
    assert!(has_dotdot_segment("a/.."));
    assert!(has_dotdot_segment(".."));
    assert!(has_dotdot_segment("../a"));
    assert!(has_dotdot_segment("a\\..\\b"));
    // `..` embedded in a name is NOT a segment.
    assert!(!has_dotdot_segment("a..b"));
    assert!(!has_dotdot_segment("...."));
    assert!(!has_dotdot_segment("a/b"));
}

#[test]
fn glob_base_dir_matches_wgg() {
    assert_eq!(glob_base_dir("src/*.rs"), "src/");
    assert_eq!(glob_base_dir("*.rs"), ".");
    assert_eq!(glob_base_dir("/a/b/*.txt"), "/a/b/");
    // No glob → unchanged.
    assert_eq!(glob_base_dir("plain/path.txt"), "plain/path.txt");
}

#[test]
fn casefold_matches_hg() {
    assert_eq!(casefold_path("/Etc/PASSWD"), "/etc/passwd");
    assert_eq!(casefold_path("\u{0131}"), "i"); // dotless i → i
    assert_eq!(casefold_path("\u{017F}"), "s"); // long s → s
}

fn classify(raw: &str, op: PsOperation) -> PsPathClass {
    classify_ps_path(raw, op, false, Some("/home/u"))
}

fn reason_of(c: &PsPathClass) -> &str {
    match c {
        PsPathClass::Blocked { reason, .. } => reason,
        PsPathClass::Proceed { .. } => panic!("expected Blocked, got Proceed"),
    }
}

#[test]
fn classify_string_guards_block_with_exact_reasons() {
    use ps_path_reasons as R;
    assert_eq!(
        reason_of(&classify("~bob/x", PsOperation::Read)),
        R::TILDE_USER
    );
    assert_eq!(reason_of(&classify("a`b", PsOperation::Read)), R::BACKTICK);
    assert_eq!(
        reason_of(&classify("Registry::HKLM", PsOperation::Read)),
        R::PROVIDER_QUALIFIED
    );
    assert_eq!(
        reason_of(&classify("//server/share", PsOperation::Read)),
        R::UNC
    );
    assert_eq!(
        reason_of(&classify("$env:TEMP/x", PsOperation::Read)),
        R::VARIABLE_EXPANSION
    );
    assert_eq!(
        reason_of(&classify("dir/../escape", PsOperation::Read)),
        R::TRAVERSAL
    );
}

#[test]
fn classify_glob_reason_depends_on_operation() {
    use ps_path_reasons as R;
    assert_eq!(
        reason_of(&classify("out*.txt", PsOperation::Write)),
        R::GLOB_WRITE
    );
    assert_eq!(
        reason_of(&classify("out*.txt", PsOperation::Create)),
        R::GLOB_WRITE
    );
    assert_eq!(
        reason_of(&classify("in*.txt", PsOperation::Read)),
        R::GLOB_READ
    );
}

#[test]
fn classify_provider_and_drive_relative() {
    // Off Windows, a `C:foo` drive-relative is caught by the provider guard.
    let r = reason_of(&classify("C:foo", PsOperation::Read)).to_string();
    assert!(r.contains("uses a non-filesystem provider"));
    assert!(r.contains("C:foo"));
    // A URL-like provider prefix.
    assert!(reason_of(&classify("http:notafile", PsOperation::Read))
        .contains("non-filesystem provider"));

    // On Windows, `C:foo` is drive-relative (distinct reason).
    let w = classify_ps_path("C:foo", PsOperation::Read, true, Some("C:\\Users\\u"));
    assert!(reason_of(&w).contains("is drive-relative"));
}

#[test]
fn classify_clean_paths_proceed_normalized() {
    // Backslashes normalized, surrounding quotes stripped.
    assert_eq!(
        classify("src\\file.txt", PsOperation::Read),
        PsPathClass::Proceed {
            normalized: "src/file.txt".to_string()
        }
    );
    assert_eq!(
        classify("'quoted name.txt'", PsOperation::Read),
        PsPathClass::Proceed {
            normalized: "quoted name.txt".to_string()
        }
    );
    // `~/x` (tilde followed by '/') is expanded, NOT a ~user block.
    assert_eq!(
        classify("~/proj/a.txt", PsOperation::Read),
        PsPathClass::Proceed {
            normalized: "/home/u/proj/a.txt".to_string()
        }
    );
    // A leading `../` is not traversal-after-segment → proceeds.
    assert_eq!(
        classify("../sibling.txt", PsOperation::Read),
        PsPathClass::Proceed {
            normalized: "../sibling.txt".to_string()
        }
    );
    // A plain relative path proceeds.
    assert_eq!(
        classify("logs/today.txt", PsOperation::Read),
        PsPathClass::Proceed {
            normalized: "logs/today.txt".to_string()
        }
    );
}

#[test]
fn classify_guard_ordering_var_before_provider() {
    // `$` variable-expansion fires before the provider-prefix guard.
    use ps_path_reasons as R;
    assert_eq!(
        reason_of(&classify("env:$x", PsOperation::Read)),
        R::VARIABLE_EXPANSION
    );
}

fn ps_roots() -> crate::filesystem::FsRoots {
    crate::filesystem::FsRoots {
        cwd: std::path::PathBuf::from("/proj/work"),
        home: Some(std::path::PathBuf::from("/home/u")),
        lingxi_home: std::path::PathBuf::from("/home/u/.lingxi"),
    }
}

#[test]
fn check_ps_path_allows_inside_working_dir() {
    let roots = ps_roots();
    // A relative path resolves under cwd → allowed (read is auto-allowed in-cwd).
    assert!(matches!(
        check_ps_path(
            "notes.txt",
            PsOperation::Read,
            &roots,
            &[],
            false,
            PermissionMode::Default,
        ),
        PsPathOutcome::Allowed { .. }
    ));
    // An absolute path inside cwd → allowed.
    assert!(matches!(
        check_ps_path(
            "/proj/work/sub/a.txt",
            PsOperation::Read,
            &roots,
            &[],
            false,
            PermissionMode::Default,
        ),
        PsPathOutcome::Allowed { .. }
    ));
}

#[test]
fn check_ps_path_blocks_outside_working_dir_with_containment() {
    let roots = ps_roots();
    match check_ps_path(
        "/etc/passwd",
        PsOperation::Read,
        &roots,
        &[],
        false,
        PermissionMode::Default,
    ) {
        PsPathOutcome::AskContainment { resolved } => assert_eq!(resolved, "/etc/passwd"),
        other => panic!("expected AskContainment, got {other:?}"),
    }
}

#[test]
fn check_ps_path_honors_additional_working_dirs() {
    let roots = ps_roots();
    let extra = [std::path::PathBuf::from("/tmp/allowed")];
    // A write inside an additional working dir is auto-allowed only under
    // `AcceptEdits` (the main-pipeline in-cwd write gate); use it here to exercise
    // the in-dir allow path.
    assert!(matches!(
        check_ps_path(
            "/tmp/allowed/f.txt",
            PsOperation::Write,
            &roots,
            &extra,
            false,
            PermissionMode::AcceptEdits,
        ),
        PsPathOutcome::Allowed { .. }
    ));
    // Still blocked outside both cwd and the extra dir (regardless of mode).
    assert!(matches!(
        check_ps_path(
            "/tmp/other/f.txt",
            PsOperation::Write,
            &roots,
            &extra,
            false,
            PermissionMode::AcceptEdits,
        ),
        PsPathOutcome::AskContainment { .. }
    ));
}

/// PERM-03 (claude-code 2.1.238 `w$i`): a path that carried quote characters
/// never auto-allows — the `if(i&&f.allowed)return s(d)` tail degrades an
/// otherwise-allowed in-cwd path to the byte-exact QUOTE_CHARS manual approval.
#[test]
fn check_ps_path_quoted_path_requires_manual_approval() {
    let roots = ps_roots();
    assert_eq!(
        ps_path_reasons::QUOTE_CHARS,
        "Paths containing quote characters cannot be statically validated and require manual approval"
    );
    // Same path unquoted → auto-allowed (read, in cwd).
    assert!(matches!(
        check_ps_path(
            "notes.txt",
            PsOperation::Read,
            &roots,
            &[],
            false,
            PermissionMode::Default,
        ),
        PsPathOutcome::Allowed { .. }
    ));
    // Surrounding quotes → manual approval, not Allowed.
    match check_ps_path(
        "'notes.txt'",
        PsOperation::Read,
        &roots,
        &[],
        false,
        PermissionMode::Default,
    ) {
        PsPathOutcome::AskReason { reason, resolved } => {
            assert_eq!(reason, ps_path_reasons::QUOTE_CHARS);
            assert_eq!(resolved, "/proj/work/notes.txt");
        }
        other => panic!("expected AskReason, got {other:?}"),
    }
    // An INTERIOR quote also counts (2.1.238 `wW` is the GLOBAL strip; 2.1.220's
    // anchored `z2` would have left the quote in place and blocked elsewhere).
    match check_ps_path(
        "no\"tes.txt",
        PsOperation::Read,
        &roots,
        &[],
        false,
        PermissionMode::Default,
    ) {
        PsPathOutcome::AskReason { reason, resolved } => {
            assert_eq!(reason, ps_path_reasons::QUOTE_CHARS);
            assert_eq!(resolved, "/proj/work/notes.txt");
        }
        other => panic!("expected AskReason, got {other:?}"),
    }
    // A quoted path OUTSIDE the working dirs keeps the containment ask — the
    // quote verdict only replaces an `allowed` outcome.
    match check_ps_path(
        "'/etc/passwd'",
        PsOperation::Read,
        &roots,
        &[],
        false,
        PermissionMode::Default,
    ) {
        PsPathOutcome::AskContainment { resolved } => assert_eq!(resolved, "/etc/passwd"),
        other => panic!("expected AskContainment, got {other:?}"),
    }
}

/// PERM-03: quoting does not override an earlier string guard, and in the glob
/// branch it only replaces the read reason on the `..`-segment (`p4e`) arm.
#[test]
fn classify_quoted_glob_and_guard_ordering() {
    use ps_path_reasons as R;
    // A guard that fires before the tail wins over the quote verdict.
    assert_eq!(
        reason_of(&classify("'~bob/x'", PsOperation::Read)),
        R::TILDE_USER
    );
    // Quoted read-glob WITHOUT a `..` segment keeps GLOB_READ.
    assert_eq!(
        reason_of(&classify("'logs/*.txt'", PsOperation::Read)),
        R::GLOB_READ
    );
    // Quoted read-glob WITH a `..` segment takes the quote verdict.
    assert_eq!(
        reason_of(&classify("'../logs/*.txt'", PsOperation::Read)),
        R::QUOTE_CHARS
    );
    // Unquoted, same shape → still GLOB_READ.
    assert_eq!(
        reason_of(&classify("../logs/*.txt", PsOperation::Read)),
        R::GLOB_READ
    );
    // A write glob is GLOB_WRITE regardless of quoting.
    assert_eq!(
        reason_of(&classify("'../out/*.log'", PsOperation::Write)),
        R::GLOB_WRITE
    );
}

#[test]
fn check_ps_path_string_guard_wins_over_containment() {
    let roots = ps_roots();
    // A guard fires before any working-dir resolution.
    match check_ps_path(
        "~bob/secret",
        PsOperation::Read,
        &roots,
        &[],
        false,
        PermissionMode::Default,
    ) {
        PsPathOutcome::AskReason { reason, .. } => assert_eq!(reason, ps_path_reasons::TILDE_USER),
        other => panic!("expected AskReason, got {other:?}"),
    }
    match check_ps_path(
        "out*.log",
        PsOperation::Write,
        &roots,
        &[],
        false,
        PermissionMode::Default,
    ) {
        PsPathOutcome::AskReason { reason, .. } => assert_eq!(reason, ps_path_reasons::GLOB_WRITE),
        other => panic!("expected AskReason, got {other:?}"),
    }
}

fn ctx_of<'a>(roots: &'a crate::filesystem::FsRoots, add: &'a [std::path::PathBuf]) -> PsCtx<'a> {
    ctx_of_mode(roots, add, PermissionMode::Default)
}

fn ctx_of_mode<'a>(
    roots: &'a crate::filesystem::FsRoots,
    add: &'a [std::path::PathBuf],
    mode: PermissionMode,
) -> PsCtx<'a> {
    PsCtx {
        roots,
        additional: add,
        is_windows: false,
        is_macos: false,
        mode,
    }
}

fn one_cmd_stmt(command: PsCommand) -> PsStatement {
    PsStatement {
        commands: vec![PsElement::Command(command)],
        nested_commands: Vec::new(),
        redirections: Vec::new(),
        ..PsStatement::default()
    }
}

fn validate_one(command: PsCommand) -> PsContainmentResult {
    let roots = ps_roots();
    validate_ps_statement(&one_cmd_stmt(command), &ctx_of(&roots, &[]), false)
}

#[test]
fn xgg_allows_in_cwd_read_but_asks_in_cwd_write() {
    // PERM-PS-VRG-01 (main pipeline, default mode): an in-cwd READ is auto-allowed
    // → passthrough; an in-cwd WRITE is NOT (only `read`/`acceptEdits`) → the
    // containment ask.
    assert_eq!(
        validate_one(cmd("Get-Content", &["notes.txt"])),
        PsContainmentResult::Passthrough
    );
    match validate_one(cmd(
        "Set-Content",
        &["-Path", "/proj/work/out.txt", "-Value", "x"],
    )) {
        PsContainmentResult::Ask { message, .. } => assert_eq!(
            message,
            "set-content targeting '/proj/work/out.txt' was blocked. For security, LingXi may only access files in the allowed working directories for this session: '/proj/work'."
        ),
        other => panic!("expected in-cwd write containment ask, got {other:?}"),
    }
}

#[test]
fn vrg_gate_matrix_main_pipeline_and_nested() {
    // PERM-PS-VRG-01 gate matrix.
    let roots = ps_roots();
    let write = || {
        cmd(
            "Set-Content",
            &["-Path", "/proj/work/out.txt", "-Value", "x"],
        )
    };

    // MAIN-PIPELINE, in-cwd READ, default mode → auto-allowed (passthrough).
    assert_eq!(
        validate_ps_statement(
            &one_cmd_stmt(cmd("Get-Content", &["/proj/work/notes.txt"])),
            &ctx_of(&roots, &[]),
            false,
        ),
        PsContainmentResult::Passthrough
    );

    // MAIN-PIPELINE, in-cwd WRITE, default mode → NOT auto-allowed → ask.
    match validate_ps_statement(&one_cmd_stmt(write()), &ctx_of(&roots, &[]), false) {
        PsContainmentResult::Ask { message, .. } => assert_eq!(
            message,
            "set-content targeting '/proj/work/out.txt' was blocked. For security, LingXi may only access files in the allowed working directories for this session: '/proj/work'."
        ),
        other => panic!("expected default-mode in-cwd write ask, got {other:?}"),
    }

    // MAIN-PIPELINE, in-cwd WRITE, acceptEdits mode → auto-allowed (passthrough).
    assert_eq!(
        validate_ps_statement(
            &one_cmd_stmt(write()),
            &ctx_of_mode(&roots, &[], PermissionMode::AcceptEdits),
            false,
        ),
        PsContainmentResult::Passthrough
    );

    // NESTED, in-cwd WRITE, default mode → the vRg gate DOES run on nested path
    // checks (it is universal, verified vs the binary) → ask, not passthrough.
    // (Main pipeline `Write-Output run` has no expression source / control-flow ask,
    // so the nested per-path containment ask is what surfaces.)
    let nested_stmt = PsStatement {
        commands: vec![PsElement::Command(cmd("Write-Output", &["run"]))],
        nested_commands: vec![write()],
        redirections: Vec::new(),
        ..PsStatement::default()
    };
    match validate_ps_statement(&nested_stmt, &ctx_of(&roots, &[]), false) {
        PsContainmentResult::Ask { message, .. } => assert!(
            message.contains("was blocked"),
            "expected nested in-cwd write containment ask, got: {message}"
        ),
        other => panic!("expected nested in-cwd write ask, got {other:?}"),
    }
}

#[test]
fn xgg_asks_with_template_b_outside_cwd() {
    match validate_one(cmd("Get-Content", &["/etc/passwd"])) {
        PsContainmentResult::Ask { message, .. } => assert_eq!(
            message,
            "get-content targeting '/etc/passwd' was blocked. For security, LingXi may only access files in the allowed working directories for this session: '/proj/work'."
        ),
        other => panic!("expected Ask, got {other:?}"),
    }
}

#[test]
fn xgg_string_guard_message_wins() {
    // A tilde-user path asks with the guard reason (not template B).
    match validate_one(cmd("Get-Content", &["~bob/secret"])) {
        PsContainmentResult::Ask { message, .. } => {
            assert_eq!(message, ps_path_reasons::TILDE_USER)
        }
        other => panic!("expected Ask, got {other:?}"),
    }
}

#[test]
fn xgg_remove_item_protected_path_denies() {
    // Remove-Item of a top-level system dir → hard deny (Hwt).
    match validate_one(cmd("Remove-Item", &["/etc"])) {
        PsContainmentResult::Deny { message, .. } => assert_eq!(
            message,
            "Remove-Item on system path '/etc' is blocked. This path is protected from removal."
        ),
        other => panic!("expected Deny, got {other:?}"),
    }
    // Alias `rm` normalizes to remove-item.
    assert!(matches!(
        validate_one(cmd("rm", &["/"])),
        PsContainmentResult::Deny { .. }
    ));
}

// ── PERM-PS-RM-05: normalize/resolve `..` before the protected-path check ──

#[test]
fn xgg_remove_absolute_traversal_into_protected_root_denies() {
    // `Remove-Item /Users/x/proj/../..` → O5r normalizes the absolute path to
    // `/Users` (a protected top-level dir) → hard deny, not the traversal ask.
    let roots = ps_roots();
    match validate_ps_statement(
        &one_cmd_stmt(cmd("Remove-Item", &["/Users/x/proj/../.."])),
        &ctx_of(&roots, &[]),
        false,
    ) {
        PsContainmentResult::Deny { message, .. } => {
            assert!(message.contains("is blocked"), "{message}");
        }
        other => panic!("expected Deny, got {other:?}"),
    }
}

#[test]
fn xgg_remove_relative_traversal_into_protected_root_denies() {
    // `Remove-Item proj/../../..` from cwd `/proj/work` resolves to `/` — yeo's
    // traversal branch reports the cwd-resolved path so the resolved d7t check
    // hard-denies rather than degrading to the traversal ask.
    let roots = ps_roots();
    match validate_ps_statement(
        &one_cmd_stmt(cmd("Remove-Item", &["proj/../../.."])),
        &ctx_of(&roots, &[]),
        false,
    ) {
        PsContainmentResult::Deny { message, .. } => {
            assert!(message.contains("is blocked"), "{message}");
        }
        other => panic!("expected Deny, got {other:?}"),
    }
}

#[test]
fn xgg_remove_relative_traversal_inside_cwd_asks_not_denies() {
    // A traversal that resolves back inside cwd is NOT protected → the traversal
    // ask fires (no over-deny).
    let roots = ps_roots();
    match validate_ps_statement(
        &one_cmd_stmt(cmd("Remove-Item", &["sub/../notes.txt"])),
        &ctx_of(&roots, &[]),
        false,
    ) {
        PsContainmentResult::Ask { message, .. } => {
            assert_eq!(message, ps_path_reasons::TRAVERSAL);
        }
        other => panic!("expected traversal Ask, got {other:?}"),
    }
}

#[test]
fn xgg_remove_recurse_targeting_cwd_asks() {
    // Remove-Item -Recurse of the working directory (or an ancestor) → ask.
    match validate_one(cmd("Remove-Item", &["-Recurse", "/proj/work"])) {
        // /proj/work is itself a top-level-ish path? No: dirname is /proj, not /.
        // It equals cwd → the recurse guard asks about deleting the working dir.
        PsContainmentResult::Ask { message, .. } => {
            assert!(
                message.contains("would delete the working directory"),
                "{message}"
            );
        }
        other => panic!("expected Ask, got {other:?}"),
    }
}

#[test]
fn xgg_write_without_path_asks() {
    // Out-File with no resolvable target path → ask (write-no-path). Out-File is
    // NOT optional_write, unlike Invoke-WebRequest.
    match validate_one(cmd("Out-File", &["-Encoding", "utf8"])) {
        PsContainmentResult::Ask { message, .. } => {
            assert!(
                message.contains("is a write operation but no target path"),
                "{message}"
            );
        }
        other => panic!("expected Ask, got {other:?}"),
    }
}

#[test]
fn xgg_non_path_cmdlet_passes_through() {
    assert_eq!(
        validate_one(cmd("Write-Output", &["hello"])),
        PsContainmentResult::Passthrough
    );
}

#[test]
fn xgg_pipeline_source_before_cmdlet_asks() {
    let roots = ps_roots();
    let stmt = PsStatement {
        commands: vec![
            PsElement::Expression {
                text: "$x".to_string(),
            },
            PsElement::Command(cmd("Set-Content", &["out.txt"])),
        ],
        nested_commands: Vec::new(),
        redirections: Vec::new(),
        ..PsStatement::default()
    };
    match validate_ps_statement(&stmt, &ctx_of(&roots, &[]), false) {
        PsContainmentResult::Ask { message, .. } => {
            assert!(
                message.contains("receives its path from a pipeline expression source"),
                "{message}"
            );
        }
        other => panic!("expected Ask, got {other:?}"),
    }
}

// ── PERM-PS-NEST-04: nested-command control-flow ask + no recurse-cwd check ──

#[test]
fn xgg_nested_control_flow_source_asks() {
    // Main pipeline is a bare expression source (sets claude-code's `i`), and the
    // control-flow body is a nested command. Each nested command that does not
    // otherwise ask ends with the "appears inside a control-flow …" ask.
    let roots = ps_roots();
    let stmt = PsStatement {
        commands: vec![PsElement::Expression {
            text: "$cond".to_string(),
        }],
        nested_commands: vec![cmd("Get-Content", &["notes.txt"])],
        redirections: Vec::new(),
        ..PsStatement::default()
    };
    match validate_ps_statement(&stmt, &ctx_of(&roots, &[]), false) {
        PsContainmentResult::Ask { message, reason } => {
            assert_eq!(
                message,
                "get-content appears inside a control-flow or chain statement where piped expression sources cannot be statically validated and requires manual approval"
            );
            assert_eq!(reason, message);
        }
        other => panic!("expected Ask, got {other:?}"),
    }
}

#[test]
fn xgg_nested_without_pipeline_expression_no_control_flow_ask() {
    // No main-pipeline expression → `i` is false → nested commands do NOT get the
    // control-flow ask; an in-cwd read passes through.
    let roots = ps_roots();
    let stmt = PsStatement {
        commands: vec![PsElement::Command(cmd("Write-Output", &["hi"]))],
        nested_commands: vec![cmd("Get-Content", &["notes.txt"])],
        redirections: Vec::new(),
        ..PsStatement::default()
    };
    assert_eq!(
        validate_ps_statement(&stmt, &ctx_of(&roots, &[]), false),
        PsContainmentResult::Passthrough
    );
}

#[test]
fn xgg_nested_remove_recurse_cwd_asks_via_vrg_gate() {
    // PERM-PS-VRG-01: CC's `vRg` per-path auto-allow gate is UNIVERSAL — it runs on
    // NESTED command paths too (verified vs the 2.1.211 binary). A nested
    // `Remove-Item /proj/work` (write, in-cwd, default mode) therefore ASKS via the
    // vRg containment gate, NOT the (separate, main-pipeline-only) `-Recurse` "would
    // delete the working directory" guard.
    let roots = ps_roots();
    let stmt = PsStatement {
        commands: vec![PsElement::Command(cmd("Write-Output", &["run"]))],
        nested_commands: vec![cmd("Remove-Item", &["-Recurse", "/proj/work"])],
        redirections: Vec::new(),
        ..PsStatement::default()
    };
    match validate_ps_statement(&stmt, &ctx_of(&roots, &[]), false) {
        PsContainmentResult::Ask { message, .. } => assert!(
            !message.contains("would delete the working directory"),
            "nested asks via the vRg containment gate, not the main-only -Recurse guard: {message}"
        ),
        other => panic!("expected nested in-cwd write containment ask, got {other:?}"),
    }
    // Contrast: the SAME command in the main pipeline surfaces the main-only
    // `-Recurse` "would delete the working directory" guard (set before the
    // per-path loop, so it wins).
    match validate_ps_statement(
        &one_cmd_stmt(cmd("Remove-Item", &["-Recurse", "/proj/work"])),
        &ctx_of(&roots, &[]),
        false,
    ) {
        PsContainmentResult::Ask { message, .. } => {
            assert!(
                message.contains("would delete the working directory"),
                "{message}"
            );
        }
        other => panic!("expected main-pipeline recurse ask, got {other:?}"),
    }
}

#[test]
fn xgg_nested_remove_protected_still_denies() {
    // The protected-path hard deny still fires from the nested-command loop.
    let roots = ps_roots();
    let stmt = PsStatement {
        commands: vec![PsElement::Command(cmd("Write-Output", &["run"]))],
        nested_commands: vec![cmd("Remove-Item", &["/etc"])],
        redirections: Vec::new(),
        ..PsStatement::default()
    };
    assert!(matches!(
        validate_ps_statement(&stmt, &ctx_of(&roots, &[]), false),
        PsContainmentResult::Deny { .. }
    ));
}

#[test]
fn xgg_redirection_outside_cwd_asks() {
    let roots = ps_roots();
    let stmt = PsStatement {
        commands: vec![PsElement::Command(cmd("Get-Process", &[]))],
        nested_commands: Vec::new(),
        redirections: vec![PsRedirection {
            target: "/etc/evil".to_string(),
            is_merging: false,
        }],
        ..PsStatement::default()
    };
    match validate_ps_statement(&stmt, &ctx_of(&roots, &[]), false) {
        PsContainmentResult::Ask { message, .. } => assert_eq!(
            message,
            "Output redirection to '/etc/evil' was blocked. For security, LingXi may only write to files in the allowed working directories for this session: '/proj/work'."
        ),
        other => panic!("expected Ask, got {other:?}"),
    }
}

#[test]
fn xgg_merging_and_empty_redirections_skipped() {
    let roots = ps_roots();
    let stmt = PsStatement {
        commands: vec![PsElement::Command(cmd("Get-Process", &[]))],
        nested_commands: Vec::new(),
        redirections: vec![
            PsRedirection {
                target: String::new(),
                is_merging: true,
            },
            PsRedirection {
                target: String::new(),
                is_merging: false,
            },
        ],
        ..PsStatement::default()
    };
    assert_eq!(
        validate_ps_statement(&stmt, &ctx_of(&roots, &[]), false),
        PsContainmentResult::Passthrough
    );
}

#[test]
fn zu_first_deny_wins_then_first_ask() {
    let roots = ps_roots();
    let ctx = ctx_of(&roots, &[]);
    // Two statements: first asks, second denies → Z_u returns the DENY.
    let asking = one_cmd_stmt(cmd("Get-Content", &["/etc/passwd"]));
    let denying = one_cmd_stmt(cmd("Remove-Item", &["/etc"]));
    assert!(matches!(
        validate_ps_statements(&[asking.clone(), denying], &ctx, false),
        PsContainmentResult::Deny { .. }
    ));
    // Two asking statements → first ask wins.
    let a2 = one_cmd_stmt(cmd("Get-Content", &["/var/log/x"]));
    match validate_ps_statements(&[asking, a2], &ctx, false) {
        PsContainmentResult::Ask { message, .. } => assert!(message.contains("/etc/passwd")),
        other => panic!("expected Ask, got {other:?}"),
    }
}

#[test]
fn zu_compound_cd_asks() {
    let roots = ps_roots();
    let stmt = one_cmd_stmt(cmd("Get-Content", &["notes.txt"]));
    match validate_ps_statements(&[stmt], &ctx_of(&roots, &[]), true) {
        PsContainmentResult::Ask { message, reason } => {
            assert!(
                message.contains("Compound command changes working directory"),
                "{message}"
            );
            // PS-CD-03: the decisionReason is DISTINCT from the display message.
            assert_eq!(
                reason,
                "Compound command contains cd with path operation \u{2014} manual approval required to prevent path resolution bypass"
            );
        }
        other => panic!("expected Ask, got {other:?}"),
    }
}

#[test]
fn param_in_list_matches_lkn() {
    let list = &["-path", "-literalpath", "-pspath", "-lp"];
    // Exact match.
    assert!(param_in_list("-path", list));
    // Unambiguous ≥2-char prefix (PowerShell abbreviation): "-pa" → "-path".
    assert!(param_in_list("-pa", list));
    assert!(param_in_list("-li", list));
    // A single-char "-" is length 1 → prefix matching disabled, no exact "-".
    assert!(!param_in_list("-", list));
    // Non-matching prefix.
    assert!(!param_in_list("-zz", list));
}

#[test]
fn null_redirect_targets_skipped() {
    // 2.1.211 `xXt`: `> $null` / `> ${null}` discard idioms are skipped before
    // path validation (case-insensitive, trimmed).
    assert!(is_null_redirect("$null"));
    assert!(is_null_redirect("${null}"));
    assert!(is_null_redirect("  $NULL  "));
    assert!(is_null_redirect("${NULL}"));
    // A real path is not a null redirect.
    assert!(!is_null_redirect("out.txt"));
    assert!(!is_null_redirect("$env:TEMP\\x"));
}

// ===========================================================================
// PERM-PS-CALLER-06 — git-security caller battery tests (2.1.211 `NTU`).
// ===========================================================================

/// A statement built from a list of main-pipeline commands.
fn bstmt(cmds: Vec<PsCommand>) -> PsStatement {
    PsStatement {
        commands: cmds.into_iter().map(PsElement::Command).collect(),
        nested_commands: Vec::new(),
        redirections: Vec::new(),
        ..PsStatement::default()
    }
}

fn bredir(target: &str) -> PsRedirection {
    PsRedirection {
        target: target.to_string(),
        is_merging: false,
    }
}

fn battery(statements: &[PsStatement], compound_cd: bool) -> Option<PsContainmentResult> {
    let roots = ps_roots();
    powershell_git_battery(statements, &ctx_of(&roots, &[]), compound_cd)
}

fn battery_ask_msg(r: &Option<PsContainmentResult>) -> Option<&str> {
    match r {
        Some(PsContainmentResult::Ask { message, .. }) => Some(message.as_str()),
        _ => None,
    }
}

// --- cd-git ----------------------------------------------------------------

#[test]
fn battery_cd_git_positive() {
    // `cd sub; git status` — compound cd + git.
    let stmts = vec![
        bstmt(vec![cmd("cd", &["sub"])]),
        bstmt(vec![cmd("git", &["status"])]),
    ];
    let r = battery(&stmts, /* compound_cd = */ true);
    assert_eq!(battery_ask_msg(&r), Some(BATTERY_CD_GIT));
}

#[test]
fn battery_cd_git_negative_no_git() {
    // Compound cd WITHOUT git → no cd-git ask (and no other in-scope ask fires).
    let stmts = vec![
        bstmt(vec![cmd("cd", &["sub"])]),
        bstmt(vec![cmd("Get-Content", &["notes.txt"])]),
    ];
    assert!(battery(&stmts, true).is_none());
}

#[test]
fn battery_cd_git_negative_git_not_compound() {
    // A lone `git` (not compound) never fires cd-git.
    let stmts = vec![bstmt(vec![cmd("git", &["status"])])];
    assert!(battery(&stmts, false).is_none());
}

// --- git-internal-write ----------------------------------------------------

#[test]
fn battery_git_internal_write_positive_arg() {
    // `New-Item HEAD; git status` — a write cmdlet targets a git-internal path.
    let stmts = vec![
        bstmt(vec![cmd("New-Item", &["HEAD"])]),
        bstmt(vec![cmd("git", &["status"])]),
    ];
    let r = battery(&stmts, false);
    assert_eq!(battery_ask_msg(&r), Some(BATTERY_GIT_INTERNAL_WRITE));
}

#[test]
fn battery_git_internal_write_positive_redirection() {
    // `git status > .git/hooks/evil` — a statement-level redirection (`R5r`/`U`)
    // targets a git-internal path.
    let mut s = bstmt(vec![cmd("git", &["status"])]);
    s.redirections.push(bredir(".git/hooks/evil"));
    let r = battery(&[s], false);
    assert_eq!(battery_ask_msg(&r), Some(BATTERY_GIT_INTERNAL_WRITE));
}

#[test]
fn battery_git_internal_write_negative_no_git() {
    // Write to HEAD but NO git command present → not the git-internal-write ask
    // (S is required). It IS a `.git`? no — HEAD is not under `.git/`, so dotgit
    // does not fire either → passthrough.
    let stmts = vec![bstmt(vec![cmd("New-Item", &["HEAD"])])];
    assert!(battery(&stmts, false).is_none());
}

#[test]
fn battery_git_internal_write_negative_safe_target() {
    // `New-Item notes.txt; git status` — write target is NOT git-internal.
    let stmts = vec![
        bstmt(vec![cmd("New-Item", &["notes.txt"])]),
        bstmt(vec![cmd("git", &["status"])]),
    ];
    assert!(battery(&stmts, false).is_none());
}

// --- xcopy/robocopy + git --------------------------------------------------

#[test]
fn battery_xcopy_robocopy_positive() {
    // `robocopy src dst; git status` — native copier + git.
    let stmts = vec![
        bstmt(vec![cmd("robocopy", &["src", "dst"])]),
        bstmt(vec![cmd("git", &["status"])]),
    ];
    let r = battery(&stmts, false);
    assert_eq!(battery_ask_msg(&r), Some(BATTERY_XCOPY_ROBOCOPY));
}

#[test]
fn battery_xcopy_robocopy_positive_pathed_exe() {
    // Basename-lowercase membership: a path-qualified `xcopy.exe` still matches.
    let stmts = vec![
        bstmt(vec![cmd(
            "C:\\Windows\\System32\\robocopy.exe",
            &["a", "b"],
        )]),
        bstmt(vec![cmd("git", &["status"])]),
    ];
    let r = battery(&stmts, false);
    assert_eq!(battery_ask_msg(&r), Some(BATTERY_XCOPY_ROBOCOPY));
}

#[test]
fn battery_xcopy_robocopy_negative_no_git() {
    // Copier WITHOUT git → no ask (the copier test is gated inside `if(S)`).
    let stmts = vec![bstmt(vec![cmd("robocopy", &["src", "dst"])])];
    assert!(battery(&stmts, false).is_none());
}

// --- archive-extract -------------------------------------------------------

#[test]
fn battery_archive_extract_positive_git() {
    // `tar -xf a.tar; git status` — extract + git.
    let stmts = vec![
        bstmt(vec![cmd("tar", &["-xf", "a.tar"])]),
        bstmt(vec![cmd("git", &["status"])]),
    ];
    let r = battery(&stmts, false);
    assert_eq!(battery_ask_msg(&r), Some(BATTERY_ARCHIVE_GIT));
}

#[test]
fn battery_archive_extract_positive_no_git() {
    // `tar -xf a.tar; Get-Content notes.txt` — extract + other command (no git).
    let stmts = vec![
        bstmt(vec![cmd("tar", &["-xf", "a.tar"])]),
        bstmt(vec![cmd("Get-Content", &["notes.txt"])]),
    ];
    let r = battery(&stmts, false);
    assert_eq!(battery_ask_msg(&r), Some(BATTERY_ARCHIVE_NO_GIT));
}

#[test]
fn battery_archive_extract_negative_single_command() {
    // A lone archive command (not compound) never asks.
    let stmts = vec![bstmt(vec![cmd("tar", &["-xf", "a.tar"])])];
    assert!(battery(&stmts, false).is_none());
}

// --- dotgit-write ----------------------------------------------------------

#[test]
fn battery_dotgit_write_positive_arg() {
    // `Set-Content .git/config ...` — write into `.git/`, NO git command needed.
    let stmts = vec![bstmt(vec![cmd("Set-Content", &[".git/config", "x"])])];
    let r = battery(&stmts, false);
    assert_eq!(battery_ask_msg(&r), Some(BATTERY_DOTGIT_WRITE));
}

#[test]
fn battery_dotgit_write_positive_redirection() {
    // `Write-Output x > .git/hooks/pre-commit` — statement-level redirection into
    // `.git/` (dotgit is `.git`-subtree only, so `.git/hooks/...` matches).
    let mut s = bstmt(vec![cmd("Write-Output", &["x"])]);
    s.redirections.push(bredir(".git/hooks/pre-commit"));
    let r = battery(&[s], false);
    assert_eq!(battery_ask_msg(&r), Some(BATTERY_DOTGIT_WRITE));
}

#[test]
fn battery_dotgit_write_negative_safe_target() {
    // Write to a non-`.git` path → no dotgit ask.
    let stmts = vec![bstmt(vec![cmd("Set-Content", &["config.txt", "x"])])];
    assert!(battery(&stmts, false).is_none());
}

// --- precedence & regression ----------------------------------------------

#[test]
fn battery_cd_git_precedes_git_internal_write() {
    // Both cd-git and git-internal-write would fire; cd-git is pushed first, so it
    // wins the first-ask resolution.
    let stmts = vec![
        bstmt(vec![cmd("cd", &["sub"])]),
        bstmt(vec![cmd("New-Item", &["HEAD"])]),
        bstmt(vec![cmd("git", &["status"])]),
    ];
    let r = battery(&stmts, true);
    assert_eq!(battery_ask_msg(&r), Some(BATTERY_CD_GIT));
}

#[test]
fn battery_git_internal_write_precedes_dotgit() {
    // A `.git/hooks/...` write with git present fires git-internal-write (pushed
    // earlier), not dotgit-write, even though both segment-match.
    let stmts = vec![
        bstmt(vec![cmd("Set-Content", &[".git/hooks/pre-commit", "x"])]),
        bstmt(vec![cmd("git", &["status"])]),
    ];
    let r = battery(&stmts, false);
    assert_eq!(battery_ask_msg(&r), Some(BATTERY_GIT_INTERNAL_WRITE));
}

#[test]
fn battery_plain_safe_command_passes() {
    // A benign single read command triggers no battery ask.
    let stmts = vec![bstmt(vec![cmd("Get-Content", &["notes.txt"])])];
    assert!(battery(&stmts, false).is_none());
    // Even compound-but-benign (two reads, no git/archive/copier/write) passes.
    let stmts2 = vec![
        bstmt(vec![cmd("Get-Content", &["a.txt"])]),
        bstmt(vec![cmd("Get-ChildItem", &["."])]),
    ];
    assert!(battery(&stmts2, false).is_none());
}

// --- helper unit checks ----------------------------------------------------

#[test]
fn battery_ueo_normalizes_backtick_and_drive_relative() {
    let roots = ps_roots();
    let ctx = ctx_of(&roots, &[]);
    // Backtick before a normal char is stripped: ".g`it" → ".git".
    assert_eq!(battery_ueo(".g`it", &ctx), ".git");
    // Drive-relative `C:foo` → `./foo` → `foo`.
    assert_eq!(battery_ueo("C:foo", &ctx), "foo");
    // Backslashes normalized, trailing dot stripped: `.git\\config.` → `.git/config`.
    assert_eq!(battery_ueo(".git\\config.", &ctx), ".git/config");
}

#[test]
fn battery_zbu_and_kbu_segment_matchers() {
    // zbu = git-internal segments; Kbu = `.git` subtree only.
    assert!(battery_zbu("head"));
    assert!(battery_zbu("objects/pack"));
    assert!(battery_zbu(".git/config"));
    assert!(battery_zbu("git~1/refs"));
    assert!(!battery_zbu("notes.txt"));
    assert!(battery_kbu(".git"));
    assert!(battery_kbu(".git/hooks/x"));
    assert!(!battery_kbu("head")); // dotgit does NOT match bare HEAD
    assert!(!battery_kbu("objects/pack"));
}

// ───────────────────────────────────────────────────────────────────────────
// ps-acceptedits — the `zLs` whole-pipeline auto-allow validator.
//
// SAFETY CONTRACT: `ps_accept_edits_validate` performs NO path containment — it
// only ever ALLOWs (structurally safe) or passes through. The out-of-cwd ASK
// that stops a dangerous write comes from `validate_ps_statements`, which the
// policy orchestrator composes ABOVE this allow (covered by the inline policy
// test `ps_acceptedits_policy_test`). These tests pin the STRUCTURAL matrix.
// ───────────────────────────────────────────────────────────────────────────

/// Build a `zLs`-shaped command with explicit `name_type` + element types.
fn zc(name: &str, name_type: &str, args: &[&str], types: &[&str]) -> PsCommand {
    PsCommand {
        name: name.to_string(),
        name_type: name_type.to_string(),
        args: args.iter().map(|s| (*s).to_string()).collect(),
        element_types: types.iter().map(|s| (*s).to_string()).collect(),
        ..PsCommand::default()
    }
}

/// A single-pipeline statement built from `commands`.
fn zstmt(commands: Vec<PsElement>) -> PsStatement {
    PsStatement {
        commands,
        ..PsStatement::default()
    }
}

fn ae(statements: &[PsStatement]) -> PsAcceptEditsResult {
    ps_accept_edits_validate(statements, &[], false)
}

fn ae_passes(statements: &[PsStatement]) -> bool {
    matches!(ae(statements), PsAcceptEditsResult::Passthrough(_))
}

#[test]
fn zls_allows_simple_in_cwd_write() {
    // Set-Content ./f.txt x — a single structurally-safe write → ALLOW.
    let c = zc(
        "Set-Content",
        "cmdlet",
        &["./f.txt", "x"],
        &["StringConstant", "StringConstant", "StringConstant"],
    );
    assert_eq!(
        ae(&[zstmt(vec![PsElement::Command(c)])]),
        PsAcceptEditsResult::Allow
    );
}

#[test]
fn zls_allows_write_piped_to_out_null() {
    // Set-Content ./f.txt x | Out-Null → the out-null sink is safe (ANt) → ALLOW.
    let set = zc(
        "Set-Content",
        "cmdlet",
        &["./f.txt", "x"],
        &["StringConstant", "StringConstant", "StringConstant"],
    );
    let out = zc("Out-Null", "cmdlet", &[], &["StringConstant"]);
    assert_eq!(
        ae(&[zstmt(vec![
            PsElement::Command(set),
            PsElement::Command(out),
        ])]),
        PsAcceptEditsResult::Allow
    );
}

#[test]
fn zls_is_path_agnostic_but_containment_asks_out_of_cwd() {
    // zLs is PURELY structural: Set-Content /etc/passwd x is structurally safe →
    // it ALLOWs. The under-ask is prevented by the SEPARATE containment check,
    // which asks on the same out-of-cwd write (even in acceptEdits mode).
    let c = zc(
        "Set-Content",
        "cmdlet",
        &["/etc/passwd", "x"],
        &["StringConstant", "StringConstant", "StringConstant"],
    );
    assert_eq!(
        ae(&[zstmt(vec![PsElement::Command(c.clone())])]),
        PsAcceptEditsResult::Allow
    );
    let roots = ps_roots();
    assert!(matches!(
        validate_ps_statement(
            &one_cmd_stmt(c),
            &ctx_of_mode(&roots, &[], PermissionMode::AcceptEdits),
            false,
        ),
        PsContainmentResult::Ask { .. }
    ));
}

#[test]
fn zls_passthrough_on_each_security_pattern_feature() {
    let base = || {
        zc(
            "Set-Content",
            "cmdlet",
            &["./f.txt", "x"],
            &["StringConstant", "StringConstant", "StringConstant"],
        )
    };
    for sp in [
        PsSecurityPatterns {
            has_sub_expressions: true,
            ..Default::default()
        },
        PsSecurityPatterns {
            has_script_blocks: true,
            ..Default::default()
        },
        PsSecurityPatterns {
            has_member_invocations: true,
            ..Default::default()
        },
        PsSecurityPatterns {
            has_expandable_strings: true,
            ..Default::default()
        },
    ] {
        let s = PsStatement {
            commands: vec![PsElement::Command(base())],
            security_patterns: sp,
            ..PsStatement::default()
        };
        assert!(ae_passes(&[s]));
    }
}

#[test]
fn zls_passthrough_on_assignment_statement() {
    let s = PsStatement {
        commands: vec![PsElement::Command(zc(
            "Set-Content",
            "cmdlet",
            &["./f.txt", "x"],
            &["StringConstant", "StringConstant", "StringConstant"],
        ))],
        statement_type: "AssignmentStatementAst".to_string(),
        ..PsStatement::default()
    };
    assert!(ae_passes(&[s]));
}

#[test]
fn zls_passthrough_on_stop_parsing_token() {
    let s = zstmt(vec![PsElement::Command(zc(
        "Set-Content",
        "cmdlet",
        &["./f.txt", "x"],
        &["StringConstant", "StringConstant", "StringConstant"],
    ))]);
    // has_stop_parsing = true → passthrough.
    assert!(matches!(
        ps_accept_edits_validate(std::slice::from_ref(&s), &[], true),
        PsAcceptEditsResult::Passthrough(_)
    ));
}

#[test]
fn zls_passthrough_on_splatting_variable() {
    let s = zstmt(vec![PsElement::Command(zc(
        "Set-Content",
        "cmdlet",
        &["./f.txt", "x"],
        &["StringConstant", "StringConstant", "StringConstant"],
    ))]);
    let vars = [PsVariable {
        path: "args".to_string(),
        is_splatted: true,
    }];
    assert!(matches!(
        ps_accept_edits_validate(std::slice::from_ref(&s), &vars, false),
        PsAcceptEditsResult::Passthrough(_)
    ));
}

#[test]
fn zls_passthrough_on_element_type_feature() {
    // A SubExpression-typed arg is folded by Voe's element-type scan even without
    // a `securityPatterns` object → passthrough.
    let c = zc(
        "Set-Content",
        "cmdlet",
        &["./f.txt", "$(danger)"],
        &["StringConstant", "StringConstant", "SubExpression"],
    );
    assert!(ae_passes(&[zstmt(vec![PsElement::Command(c)])]));
}

#[test]
fn zls_passthrough_on_new_item_symlink_types() {
    for ty in ["SymbolicLink", "Junction", "HardLink"] {
        let c = zc(
            "New-Item",
            "cmdlet",
            &["-ItemType", ty, "-Path", "./link"],
            &[
                "StringConstant",
                "Parameter",
                "StringConstant",
                "Parameter",
                "StringConstant",
            ],
        );
        match ae(&[zstmt(vec![PsElement::Command(c)])]) {
            PsAcceptEditsResult::Passthrough(r) => {
                assert!(r.contains("creates a filesystem link"), "{ty}: {r}");
            }
            other => panic!("{ty}: expected passthrough, got {other:?}"),
        }
    }
    // Alias `ni` + `-Type` abbreviation + item-type PREFIX (`sym`) also caught.
    //
    // This must match on the REASON, not merely on Passthrough: `New-Item`/`ni`
    // is never in the write set, so it returns Passthrough unconditionally and a
    // bare `ae_passes` assertion would hold even if the link detection never
    // fired at all.
    let c = zc(
        "ni",
        "cmdlet",
        &["-Type", "sym", "-Path", "./link"],
        &[
            "StringConstant",
            "Parameter",
            "StringConstant",
            "Parameter",
            "StringConstant",
        ],
    );
    match ae(&[zstmt(vec![PsElement::Command(c)])]) {
        PsAcceptEditsResult::Passthrough(r) => {
            assert!(r.contains("creates a filesystem link"), "ni -Type sym: {r}");
        }
        other => panic!("ni -Type sym: expected passthrough, got {other:?}"),
    }

    // ...and the non-link item types must NOT report the link reason, so the
    // assertion above can only pass when the detection actually distinguishes.
    for ty in ["File", "Directory"] {
        let c = zc(
            "New-Item",
            "cmdlet",
            &["-ItemType", ty, "-Path", "./thing"],
            &[
                "StringConstant",
                "Parameter",
                "StringConstant",
                "Parameter",
                "StringConstant",
            ],
        );
        if let PsAcceptEditsResult::Passthrough(r) = ae(&[zstmt(vec![PsElement::Command(c)])]) {
            assert!(
                !r.contains("creates a filesystem link"),
                "{ty} must not be reported as a link: {r}"
            );
        }
    }
}

#[test]
fn zls_passthrough_on_compound_cd_plus_write() {
    // A write FIRST then a cd — the compound-cd guard (which runs before the
    // per-command loop) is what fires, with its distinct message.
    let write = zc(
        "Set-Content",
        "cmdlet",
        &["./f.txt", "x"],
        &["StringConstant", "StringConstant", "StringConstant"],
    );
    let cd = zc(
        "Set-Location",
        "cmdlet",
        &["sub"],
        &["StringConstant", "StringConstant"],
    );
    let stmts = [
        zstmt(vec![PsElement::Command(write)]),
        zstmt(vec![PsElement::Command(cd)]),
    ];
    match ae(&stmts) {
        PsAcceptEditsResult::Passthrough(r) => {
            assert!(r.contains("directory-changing"), "{r}");
        }
        other => panic!("expected passthrough, got {other:?}"),
    }
}

#[test]
fn zls_passthrough_on_non_write_and_unknown_cmdlets() {
    // Get-Process: recognized but not write/out-null/formatting → no handling.
    let c = zc("Get-Process", "cmdlet", &[], &["StringConstant"]);
    match ae(&[zstmt(vec![PsElement::Command(c)])]) {
        PsAcceptEditsResult::Passthrough(r) => {
            assert!(
                r.contains("No mode-specific handling for 'Get-Process'"),
                "{r}"
            );
        }
        other => panic!("expected passthrough, got {other:?}"),
    }
    // Unknown cmdlet → also passthrough (never auto-allowed).
    let c = zc(
        "Some-Custom",
        "cmdlet",
        &["x"],
        &["StringConstant", "StringConstant"],
    );
    assert!(ae_passes(&[zstmt(vec![PsElement::Command(c)])]));
}

#[test]
fn zls_passthrough_on_pipeline_expression_source() {
    // $x | Set-Content ./f.txt — the leading expression source cannot be validated.
    let stmt = zstmt(vec![
        PsElement::Expression {
            text: "$x".to_string(),
        },
        PsElement::Command(zc(
            "Set-Content",
            "cmdlet",
            &["./f.txt"],
            &["StringConstant", "StringConstant"],
        )),
    ]);
    match ae(&[stmt]) {
        PsAcceptEditsResult::Passthrough(r) => assert!(r.contains("expression source"), "{r}"),
        other => panic!("expected passthrough, got {other:?}"),
    }
}

#[test]
fn zls_passthrough_on_application_resolved_name() {
    let c = zc(
        "./evil.sh",
        "application",
        &["x"],
        &["StringConstant", "StringConstant"],
    );
    match ae(&[zstmt(vec![PsElement::Command(c)])]) {
        PsAcceptEditsResult::Passthrough(r) => {
            assert!(r.contains("resolved from a path-like name"), "{r}");
        }
        other => panic!("expected passthrough, got {other:?}"),
    }
}

#[test]
fn zls_passthrough_on_unvalidatable_arg_element_type() {
    // Set-Content -Path $dest — the $dest arg is a Variable (not literal) → the
    // per-command element-type check refuses it.
    let c = zc(
        "Set-Content",
        "cmdlet",
        &["-Path", "$dest"],
        &["StringConstant", "Parameter", "Variable"],
    );
    match ae(&[zstmt(vec![PsElement::Command(c)])]) {
        PsAcceptEditsResult::Passthrough(r) => {
            assert!(r.contains("unvalidatable type (Variable)"), "{r}");
        }
        other => panic!("expected passthrough, got {other:?}"),
    }
}

#[test]
fn zls_passthrough_on_colon_bound_expression_parameter() {
    // Set-Content -Path ./f.txt -Encoding:$x — a colon-bound expression parameter.
    let c = zc(
        "Set-Content",
        "cmdlet",
        &["-Path", "./f.txt", "-Encoding:$x"],
        &["StringConstant", "Parameter", "StringConstant", "Parameter"],
    );
    match ae(&[zstmt(vec![PsElement::Command(c)])]) {
        PsAcceptEditsResult::Passthrough(r) => assert!(r.contains("Colon-bound parameter"), "{r}"),
        other => panic!("expected passthrough, got {other:?}"),
    }
}

#[test]
fn zls_passthrough_on_h3_array_literal_child() {
    // Set-Content -Value:2,3 -Path ./f.txt — the inline `-Value:2,3` parses to an
    // array literal (a non-StringConstant `children[0]` → "Other"), which `h3`
    // rejects. WITHOUT the `children` thread this would be an UNDER-ASK
    // (auto-allow) because the raw colon-value "2,3" has no expr metacharacter.
    let c = PsCommand {
        name: "Set-Content".to_string(),
        name_type: "cmdlet".to_string(),
        args: vec![
            "-Value:2,3".to_string(),
            "-Path".to_string(),
            "./f.txt".to_string(),
        ],
        element_types: vec![
            "StringConstant".to_string(),
            "Parameter".to_string(),
            "Parameter".to_string(),
            "StringConstant".to_string(),
        ],
        children: vec![Some(vec!["Other".to_string()]), None, None],
        redirections: Vec::new(),
    };
    match ae(&[zstmt(vec![PsElement::Command(c)])]) {
        PsAcceptEditsResult::Passthrough(r) => {
            assert!(r.contains("cannot be statically validated"), "{r}");
        }
        other => panic!("expected passthrough, got {other:?}"),
    }
}

#[test]
fn zls_allows_safe_outvariable_binding() {
    let c = zc(
        "Set-Content",
        "cmdlet",
        &["-Path", "./f.txt", "-Value", "x", "-OutVariable", "foo"],
        &[
            "StringConstant",
            "Parameter",
            "StringConstant",
            "Parameter",
            "StringConstant",
            "Parameter",
            "StringConstant",
        ],
    );
    assert_eq!(
        ae(&[zstmt(vec![PsElement::Command(c)])]),
        PsAcceptEditsResult::Allow
    );
}

#[test]
fn zls_passthrough_on_protected_variable_write_arg_exact_message() {
    let c = zc(
        "Set-Content",
        "cmdlet",
        &[
            "-Path",
            "./f.txt",
            "-Value",
            "x",
            "-OutVariable",
            "PSDefaultParameterValues",
        ],
        &[
            "StringConstant",
            "Parameter",
            "StringConstant",
            "Parameter",
            "StringConstant",
            "Parameter",
            "StringConstant",
        ],
    );
    assert_eq!(
        ae(&[zstmt(vec![PsElement::Command(c)])]),
        PsAcceptEditsResult::Passthrough(
            "Variable-writing or ActionPreference argument in 'Set-Content' requires approval"
                .to_string(),
        )
    );
}

#[test]
fn zls_passthrough_on_unsafe_action_preference_value_exact_message() {
    let c = zc(
        "Set-Content",
        "cmdlet",
        &["-Path", "./f.txt", "-Value", "x", "-ErrorAction", "Suspend"],
        &[
            "StringConstant",
            "Parameter",
            "StringConstant",
            "Parameter",
            "StringConstant",
            "Parameter",
            "StringConstant",
        ],
    );
    assert_eq!(
        ae(&[zstmt(vec![PsElement::Command(c)])]),
        PsAcceptEditsResult::Passthrough(
            "Variable-writing or ActionPreference argument in 'Set-Content' requires approval"
                .to_string(),
        )
    );
}

#[test]
fn zls_passthrough_on_obfuscated_empty_inline_action_preference_value() {
    let c = zc(
        "Set-Content",
        "cmdlet",
        &[
            "-Path",
            "./f.txt",
            "-Value",
            "x",
            "-Err`orAction:",
            "Suspend",
        ],
        &[
            "StringConstant",
            "Parameter",
            "StringConstant",
            "Parameter",
            "StringConstant",
            "Parameter",
            "StringConstant",
        ],
    );
    assert_eq!(
        ae(&[zstmt(vec![PsElement::Command(c)])]),
        PsAcceptEditsResult::Passthrough(
            "Variable-writing or ActionPreference argument in 'Set-Content' requires approval"
                .to_string(),
        )
    );
}

#[test]
fn pls_guards_ignore_slash_prefixed_arguments_like_the_oracle() {
    let args = vec!["/ErrorAction".to_string(), "Suspend".to_string()];
    let element_types = vec![
        "StringConstant".to_string(),
        "Parameter".to_string(),
        "StringConstant".to_string(),
    ];
    assert!(!pls_has_action_preference_arg(&args, &element_types, false));

    let args = vec![
        "/OutVariable".to_string(),
        "PSDefaultParameterValues".to_string(),
    ];
    assert!(!pls_has_variable_write_arg(&args, &element_types, false));
}

#[test]
fn zls_passthrough_on_nested_variable_write_arg_exact_message() {
    let stmt = PsStatement {
        commands: vec![PsElement::Command(zc(
            "Set-Content",
            "cmdlet",
            &["-Path", "./outer.txt", "-Value", "ok"],
            &[
                "StringConstant",
                "Parameter",
                "StringConstant",
                "Parameter",
                "StringConstant",
            ],
        ))],
        nested_commands: vec![zc(
            "Set-Content",
            "cmdlet",
            &[
                "-Path",
                "./f.txt",
                "-Value",
                "x",
                "-OutVariable",
                "PSDefaultParameterValues",
            ],
            &[
                "StringConstant",
                "Parameter",
                "StringConstant",
                "Parameter",
                "StringConstant",
                "Parameter",
                "StringConstant",
            ],
        )],
        ..PsStatement::default()
    };
    assert_eq!(
        ae(&[stmt]),
        PsAcceptEditsResult::Passthrough(
            "Variable-writing or ActionPreference argument in nested 'Set-Content' requires approval"
                .to_string(),
        )
    );
}

#[test]
fn zls_passthrough_on_empty_statements() {
    assert!(matches!(
        ps_accept_edits_validate(&[], &[], false),
        PsAcceptEditsResult::Passthrough(_)
    ));
}

// --- PS-CALLER-06-5: PS5.1 cwd-first shadowing -----------------------------
//
// The battery entry point is OS-gated (PowerShell Core resolves PATH-first off
// Windows, so the shadowing cannot happen there). The PREDICATE is pure, so it
// is exercised directly on every platform — otherwise the whole guard would be
// untested on the machines it is developed on.

#[test]
fn shadow_predicate_flags_a_write_that_precedes_the_command() {
    // `Set-Content ./git.bat …; git status` — PS 5.1 would run ./git.bat.
    let stmts = vec![
        bstmt(vec![cmd("Set-Content", &["./git.bat", "evil"])]),
        bstmt(vec![cmd("git", &["status"])]),
    ];
    let u = battery_u_list(&stmts);
    assert_eq!(battery_shadowed_command(&stmts, &u).as_deref(), Some("git"));
}

/// ORDER is the whole point: a file written AFTER a command runs cannot shadow
/// it, so the same two commands the other way round must NOT ask.
#[test]
fn shadow_predicate_ignores_a_write_that_follows_the_command() {
    let stmts = vec![
        bstmt(vec![cmd("git", &["status"])]),
        bstmt(vec![cmd("Set-Content", &["./git.bat", "evil"])]),
    ];
    let u = battery_u_list(&stmts);
    assert_eq!(battery_shadowed_command(&stmts, &u), None);
}

/// A line-level redirection is not attached to any sub-command but still lands
/// before the next one runs, so it seeds the written set.
#[test]
fn shadow_predicate_seeds_from_line_level_redirections() {
    let mut writer = bstmt(vec![cmd("Write-Output", &["evil"])]);
    writer.redirections = vec![bredir("./git.cmd")];
    let stmts = vec![writer, bstmt(vec![cmd("git", &["status"])])];
    let u = battery_u_list(&stmts);
    assert_eq!(battery_shadowed_command(&stmts, &u).as_deref(), Some("git"));
}

/// Only a PATHEXT extension makes a file executable-by-shadowing. `git.notes`
/// is just a file; flagging it would be a false positive on every compound
/// command that happens to write a same-named document.
#[test]
fn shadow_predicate_ignores_a_non_executable_extension() {
    let stmts = vec![
        bstmt(vec![cmd("Set-Content", &["./git.notes", "x"])]),
        bstmt(vec![cmd("git", &["status"])]),
    ];
    let u = battery_u_list(&stmts);
    assert_eq!(battery_shadowed_command(&stmts, &u), None);
}

/// A single sub-command cannot be preceded by anything.
#[test]
fn shadow_predicate_needs_more_than_one_sub_command() {
    let stmts = vec![bstmt(vec![cmd("git", &["status"])])];
    let u = battery_u_list(&stmts);
    assert_eq!(battery_shadowed_command(&stmts, &u), None);
}

/// The write target is matched on its BASENAME, so a path elsewhere still
/// shadows — PS 5.1 resolves against the cwd the command runs in.
#[test]
fn shadow_predicate_matches_on_the_basename() {
    let stmts = vec![
        bstmt(vec![cmd("Copy-Item", &["src", "C:\\other\\dir\\git.exe"])]),
        bstmt(vec![cmd("git", &["status"])]),
    ];
    let u = battery_u_list(&stmts);
    assert_eq!(battery_shadowed_command(&stmts, &u).as_deref(), Some("git"));
}

/// A `-Flag:` prefix is stripped before the argument is read as a path, so
/// `-Path:./git.bat` is still a write target.
#[test]
fn shadow_predicate_strips_a_leading_parameter_flag() {
    let stmts = vec![
        bstmt(vec![cmd("Out-File", &["-FilePath:./git.bat"])]),
        bstmt(vec![cmd("git", &["status"])]),
    ];
    let u = battery_u_list(&stmts);
    assert_eq!(battery_shadowed_command(&stmts, &u).as_deref(), Some("git"));
}

/// An invocation that carries its OWN extension is shadowed by a write whose
/// stem equals that full name (`git.exe` ← `git.exe.bat`).
#[test]
fn shadow_predicate_matches_an_extension_bearing_invocation() {
    let stmts = vec![
        bstmt(vec![cmd("Set-Content", &["./git.exe.bat", "x"])]),
        bstmt(vec![cmd("git.exe", &["status"])]),
    ];
    let u = battery_u_list(&stmts);
    assert_eq!(
        battery_shadowed_command(&stmts, &u).as_deref(),
        Some("git.exe")
    );
}

/// A command that only READS cannot create a shadowing file, so its arguments
/// are not write targets.
#[test]
fn shadow_predicate_ignores_arguments_of_a_non_writing_command() {
    let stmts = vec![
        bstmt(vec![cmd("Get-Content", &["./git.bat"])]),
        bstmt(vec![cmd("git", &["status"])]),
    ];
    let u = battery_u_list(&stmts);
    assert_eq!(battery_shadowed_command(&stmts, &u), None);
}

#[test]
fn shadow_message_is_byte_exact() {
    assert_eq!(
        battery_shadow_message("git"),
        "An earlier sub-command writes a file (./git.*) that would shadow the later \
`git` command under Windows PowerShell 5.1 cwd-first resolution."
    );
}

/// `LDo`: the basename is lowercased and a PATHEXT suffix — and only a PATHEXT
/// suffix — is stripped for the stem.
#[test]
fn ldo_splits_basename_and_pathext_stem() {
    assert_eq!(
        battery_ldo("C:\\tools\\GIT.EXE"),
        ("git.exe".into(), "git".into())
    );
    assert_eq!(battery_ldo("./git.bat"), ("git.bat".into(), "git".into()));
    assert_eq!(
        battery_ldo("notes.md"),
        ("notes.md".into(), "notes.md".into())
    );
    // Drive-relative `C:foo` drops the prefix; `..` resolves.
    assert_eq!(battery_ldo("C:git.cmd").1, "git");
    assert_eq!(battery_ldo("a/../git.com").1, "git");
    // An alternate-data-stream suffix names a stream, not the file.
    assert_eq!(battery_ldo("git.bat:hidden").1, "git");
}

/// The WIRING, not just the predicate: on a Windows host the battery returns
/// the shadowing ask, and on a non-Windows host it returns nothing for the same
/// input. Without the injectable gate this could only be checked on Windows.
#[test]
fn battery_shadow_ask_is_windows_only() {
    let stmts = vec![
        bstmt(vec![cmd("Set-Content", &["./git.bat", "evil"])]),
        bstmt(vec![cmd("git", &["status"])]),
    ];
    let on_windows = with_windows_host(true, || battery(&stmts, false));
    assert_eq!(
        battery_ask_msg(&on_windows),
        Some(battery_shadow_message("git").as_str())
    );

    // PowerShell Core off Windows resolves PATH-first, so the same command is
    // not shadowed and asking would be a false positive.
    let elsewhere = with_windows_host(false, || battery(&stmts, false));
    assert!(
        elsewhere.is_none(),
        "no cwd-first shadowing off Windows: {elsewhere:?}"
    );
}
