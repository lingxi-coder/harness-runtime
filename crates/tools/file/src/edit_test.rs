use super::*;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use telemetry::{AnalyticsBus, InMemorySink};
    use tempfile::TempDir;
    use tool_api::test_support::{fresh_ctx, fresh_tx, make_dummy_fs};

    fn make_ctx(tmp: &TempDir) -> (BuiltinToolContext, Arc<InMemorySink>) {
        let bus = Arc::new(AnalyticsBus::new());
        let sink = Arc::new(InMemorySink::default());
        (
            tool_api::test_support::ctx_for_file_tools(
                make_dummy_fs(),
                bus,
                vec![tmp.path().to_path_buf()],
            ),
            sink,
        )
    }

    /// Simulate a prior full `Read` of `target` so the read-before-write
    /// staleness guard (Batch F) is satisfied: records the file's current
    /// raw-UTF-8 content (the same form `Read` stores) and its current floored
    /// mtime under the canonicalized key, with `offset`/`limit` = `None` (a
    /// full read). Call AFTER writing the file's bytes so the seeded mtime
    /// matches the on-disk mtime (guard fires only when current mtime is
    /// strictly greater).
    fn seed_full_read(ctx: &BuiltinToolContext, target: &std::path::Path) {
        let canon = std::fs::canonicalize(target).unwrap();
        let bytes = std::fs::read(&canon).unwrap();
        // Decode the SAME way the guard compares (raw UTF-8, BOM-stripped),
        // falling back to lossy for non-UTF-8 fixtures (e.g. UTF-16LE), where
        // the mtime check alone governs.
        let content = crate::shared::decode_utf8_strict(&bytes)
            .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).into_owned());
        let mtime_ms = std::fs::metadata(&canon)
            .ok()
            .and_then(|m| m.modified().ok())
            .map_or(0, tool_api::read_file_state::mtime_ms_floor);
        tool_api::read_file_state::set(
            &ctx.read_file_state,
            canon,
            tool_api::read_file_state::ReadFileEntry {
                content,
                mtime_ms,
                offset: None,
                limit: None,
                // Simulates a prior full `Read`.
                from_read: true,
                seeded_from_context: false,
                is_partial_view: false,
            },
        );
    }

    #[test]
    fn tool_name_is_edit() {
        assert_eq!(TOOL_NAME, "Edit");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn leaf_symlink_is_refused_without_touching_target() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("target.txt");
        let link = tmp.path().join("link.txt");
        std::fs::write(&target, "original").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = FileEditTool::new(ctx);

        let error = tool
            .call(
                json!({
                    "file_path": link.to_string_lossy(),
                    "old_string": "original",
                    "new_string": "changed"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect_err("Edit must refuse a final symlink");
        let message = match error {
            tool_api::tool_trait::ToolError::InvalidInput(message) => message,
            other => panic!("unexpected error: {other:?}"),
        };
        assert!(message.starts_with("Refusing to write "));
        assert!(
            message.ends_with("it is a symbolic link. Write to the link's target path instead.")
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
    }

    #[test]
    fn patch_truncation_template_byte_locked() {
        assert_eq!(
            PATCH_TRUNCATION_SUFFIX_TEMPLATE,
            "\n\n... [{N} lines truncated] ..."
        );
    }

    #[test]
    fn patch_truncation_suffix_substitutes_n() {
        assert_eq!(
            patch_truncation_suffix(42),
            "\n\n... [42 lines truncated] ..."
        );
    }

    #[test]
    fn edit_result_message_single_is_byte_locked() {
        // FileEditTool.ts:589-593 (non-interactive, modifiedNote empty).
        assert_eq!(
            edit_result_message("/tmp/a.txt", false, false),
            "The file /tmp/a.txt has been updated successfully. (file state is current in your context — no need to Read it back)"
        );
    }

    #[test]
    fn edit_result_message_replace_all_is_byte_locked() {
        // FileEditTool.ts:581-586 (non-interactive, modifiedNote empty) + the
        // file-state-current suffix (binary const `Pyn`).
        assert_eq!(
            edit_result_message("/tmp/a.txt", true, false),
            "The file /tmp/a.txt has been updated. All occurrences were successfully replaced. (file state is current in your context — no need to Read it back)"
        );
    }

    #[test]
    fn edit_result_message_echoes_original_path_verbatim() {
        // claude-code echoes the input path, not a canonicalized form.
        assert_eq!(
            edit_result_message("./relative/../weird/path.txt", false, false),
            "The file ./relative/../weird/path.txt has been updated successfully. (file state is current in your context — no need to Read it back)"
        );
    }

    #[test]
    fn edit_result_message_stale_recovered_appends_the_note() {
        // The staleRecovered branch replaces the current-state suffix with the
        // byte-exact modified-on-disk note (binary Edit mapper `a = i ? … : Pyn`).
        assert_eq!(
            edit_result_message("/tmp/a.txt", false, true),
            "The file /tmp/a.txt has been updated successfully. (note: the file had been modified on disk since you last read it \u{2014} the edit applied cleanly, but the file contains other changes not in your context. Read it before edits that depend on surrounding content.)"
        );
        assert_eq!(
            edit_result_message("/tmp/a.txt", true, true),
            "The file /tmp/a.txt has been updated. All occurrences were successfully replaced. (note: the file had been modified on disk since you last read it \u{2014} the edit applied cleanly, but the file contains other changes not in your context. Read it before edits that depend on surrounding content.)"
        );
    }

    /// End-to-end `tengu_cedar_sundial` stale recovery: the file is modified on
    /// disk after the seeded Read (newer mtime + different content), but the
    /// edit still applies cleanly to the CURRENT content — with the flag on the
    /// call succeeds against the current content, appends the modified-on-disk
    /// note, and marks `staleRecovered: true` (conditional spread). With the
    /// flag off (default) the same setup yields the stale J2n error.

    /// The `kq` probe for these tests. The real global is a `OnceLock`, so it can be
    /// published only once per test binary; this one reads a switch the test flips,
    /// which is how a single end-to-end test can exercise BOTH answers.
    static KQ_ANSWER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    struct SwitchableReadAutoAllow;

    impl lingxi_core::host::read_auto_allow::ReadAutoAllow for SwitchableReadAutoAllow {
        fn read_auto_allowed(&self, _path: &str) -> bool {
            KQ_ANSWER.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    fn set_kq_answer(allowed: bool) {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            lingxi_core::host::read_auto_allow::set_read_auto_allow_probe(std::sync::Arc::new(
                SwitchableReadAutoAllow,
            ));
        });
        KQ_ANSWER.store(allowed, std::sync::atomic::Ordering::SeqCst);
    }

    #[tokio::test]
    async fn stale_recovery_end_to_end_depends_on_kq() {
        for read_auto_allowed in [false, true] {
            let tmp = TempDir::new().unwrap();
            let target = tmp.path().join("a.txt");
            std::fs::write(&target, "hello world").unwrap();
            let (ctx, _sink) = make_ctx(&tmp);
            seed_full_read(&ctx, &target);
            // Modify on disk AFTER the read: different content, mtime bumped
            // strictly forward so the guard sees staleness.
            std::fs::write(&target, "hello world plus formatter churn").unwrap();
            let future = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
            filetime::set_file_mtime(&target, filetime::FileTime::from_system_time(future))
                .unwrap();

            // The ONLY thing that differs between the two halves is `kq` —
            // whether the model could have read this file. `GKe` applies in
            // both.
            set_kq_answer(read_auto_allowed);
            let tool = FileEditTool::new(ctx);
            let outcome = tool
                .call(
                    json!({
                        "file_path": target.to_str().unwrap(),
                        "old_string": "world",
                        "new_string": "Rust"
                    }),
                    fresh_ctx(),
                    fresh_tx(),
                )
                .await;

            if read_auto_allowed {
                let result = outcome.expect("a readable path whose edit applies must recover");
                // Applied against the CURRENT content.
                assert_eq!(
                    std::fs::read_to_string(&target).unwrap(),
                    "hello Rust plus formatter churn"
                );
                assert_eq!(result.data["staleRecovered"], true);
                let mc = result.model_content.unwrap();
                assert!(
                    mc.ends_with("Read it before edits that depend on surrounding content.)"),
                    "stale note missing: {mc}"
                );
                assert!(!mc.contains("file state is current"));
            } else {
                let err = outcome.expect_err("default (flag off) must keep the stale error");
                // FT-07: the refusal a stale Edit actually hits is the oracle's
                // VALIDATE-phase message (6 hits in 2.1.238), not the rare
                // call-phase `File content has changed since it was last read`
                // (2 hits) the port used to emit here. That call-phase constant
                // is still kept, byte-locked, for its own branch.
                assert!(
                    err.to_string().contains(
                        "File has been modified since read, either by the user or by a linter."
                    ),
                    "expected the validate-phase stale error, got: {err}"
                );
                // File untouched on the error path.
                assert_eq!(
                    std::fs::read_to_string(&target).unwrap(),
                    "hello world plus formatter churn"
                );
            }
        }
    }

    #[test]
    fn stale_edit_applies_mirrors_gke() {
        // Now the PURE `GKe` predicate: no flag, and no permission question.
        // Whether the model was allowed to read the file is oracle `kq`,
        // applied separately at the call site.
        assert!(stale_edit_applies("alpha beta", "alpha", false)); // applies
        assert!(!stale_edit_applies("alpha beta", "", false)); // "" → no_match
        assert!(!stale_edit_applies("alpha beta", "gamma", false)); // no_match
        assert!(!stale_edit_applies("dup x dup", "dup", false)); // ambiguous
        assert!(stale_edit_applies("dup x dup", "dup", true)); // replace_all ⇒ applies
    }

    #[test]
    fn normalize_edit_aliases_fills_canonical_keys() {
        let mut v = json!({
            "path": "/p.txt", "old_str": "a", "new_str": "b", "replace_name": "true"
        });
        normalize_edit_aliases(&mut v);
        assert_eq!(v["file_path"], "/p.txt");
        assert_eq!(v["old_string"], "a");
        assert_eq!(v["new_string"], "b");
        assert_eq!(v["replace_all"], true);
    }

    #[test]
    fn normalize_edit_aliases_explicit_canonical_wins_and_replace_name_bool() {
        let mut v = json!({
            "file_path": "/canon.txt", "path": "/alias.txt",
            "old_string": "x", "old_str": "y", "new_string": "z",
            "replace_name": true,
        });
        normalize_edit_aliases(&mut v);
        assert_eq!(v["file_path"], "/canon.txt"); // explicit canonical wins
        assert_eq!(v["old_string"], "x");
        assert_eq!(v["replace_all"], true); // replace_name: true → replace_all
    }

    #[tokio::test]
    async fn single_replacement_succeeds() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("a.txt");
        std::fs::write(&target, "hello world").unwrap();
        let (ctx, sink) = make_ctx(&tmp);
        ctx.bus.attach_sink(sink.clone()).await;
        // Simulate the prior full Read that the staleness guard now requires.
        seed_full_read(&ctx, &target);
        let tool = FileEditTool::new(ctx);
        let result = tool
            .call(
                json!({
                    "file_path": target.to_str().unwrap(),
                    "old_string": "world",
                    "new_string": "Rust"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // Model-facing `content` is the byte-faithful single-edit message and
        // echoes the ORIGINAL input path verbatim (not canonicalized).
        let input_path = target.to_str().unwrap();
        assert_eq!(
            result.model_content.as_deref().unwrap(),
            edit_result_message(input_path, false, false)
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello Rust");
        // Result data is the binary FileEditTool record (1:1 field set); the
        // model render rides on model_content, NOT inside data.
        assert_eq!(result.data["filePath"], input_path);
        assert_eq!(result.data["oldString"], "world");
        assert_eq!(result.data["newString"], "Rust");
        assert!(result.data["originalFile"]
            .as_str()
            .unwrap()
            .starts_with("hello world"));
        assert_eq!(result.data["userModified"], false);
        assert_eq!(result.data["replaceAll"], false);
        assert!(result.data.get("content").is_none());
        assert!(result.data.get("replacements").is_none());
    }

    // ── Finding #16: deletion (new_string == "") consumes the trailing newline,
    // byte-faithful to claude-code `AUa` (binary offset 201328322). ──────────

    #[tokio::test]
    async fn deletion_consumes_trailing_newline_no_blank_line() {
        // Deleting a whole line (old_string has no trailing "\n") must also
        // remove the line's newline so no blank line is left behind — claude-code
        // `AUa` replaces `old_string + "\n"` when new_string is empty.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("a.txt");
        std::fs::write(&target, "alpha\nbravo\ncharlie\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = FileEditTool::new(ctx);
        tool.call(
            json!({
                "file_path": target.to_str().unwrap(),
                "old_string": "bravo",
                "new_string": ""
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        // Trailing "\n" of the deleted line is consumed: no leftover blank line.
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "alpha\ncharlie\n"
        );
    }

    #[tokio::test]
    async fn deletion_old_string_already_ending_in_newline_not_double_consumed() {
        // When old_string already ends in "\n", the `AUa` deletion branch's
        // `!t.endsWith("\n")` guard is false, so we replace exactly old_string
        // (no extra newline consumed → the NEXT line's newline survives).
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("b.txt");
        std::fs::write(&target, "alpha\nbravo\ncharlie\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = FileEditTool::new(ctx);
        tool.call(
            json!({
                "file_path": target.to_str().unwrap(),
                "old_string": "bravo\n",
                "new_string": ""
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        // Exactly "bravo\n" removed; charlie's own newline is untouched.
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "alpha\ncharlie\n"
        );
    }

    #[tokio::test]
    async fn deletion_without_following_newline_replaces_verbatim() {
        // old_string with no trailing "\n" AND the file does NOT contain
        // `old_string + "\n"` (e.g. the match is the last token, no newline
        // after it) → `AUa`'s `e.includes(t+"\n")` is false, so it replaces
        // old_string verbatim.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("c.txt");
        std::fs::write(&target, "keep this charlie").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = FileEditTool::new(ctx);
        tool.call(
            json!({
                "file_path": target.to_str().unwrap(),
                "old_string": " charlie",
                "new_string": ""
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep this");
    }

    #[tokio::test]
    async fn deletion_replace_all_consumes_each_trailing_newline() {
        // replace_all deletion mirrors `AUa`'s `replaceAll(t+"\n", "")` branch:
        // every occurrence (and its trailing newline) is removed.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("d.txt");
        std::fs::write(&target, "DROP\nkeep\nDROP\ntail\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = FileEditTool::new(ctx);
        tool.call(
            json!({
                "file_path": target.to_str().unwrap(),
                "old_string": "DROP",
                "new_string": "",
                "replace_all": true
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep\ntail\n");
    }

    // ── Finding #19: maximum-editable-file-size cap (1 GiB), claude-code
    // `validateInput` errorCode 10 / constant `DYa = 1073741824`. ────────────

    #[test]
    fn max_edit_file_size_constant_matches_binary() {
        // Binary `DYa` (offset 202620510) is 1073741824 (== 1 GiB), and the
        // cap renders as `1GB` through the `formatFileSize`/`Ma` formatter.
        assert_eq!(MAX_EDIT_FILE_SIZE, 1_073_741_824);
        assert_eq!(crate::read::format_file_size(MAX_EDIT_FILE_SIZE), "1GB");
    }

    #[test]
    fn too_large_message_is_byte_exact() {
        // Reconstruct the exact `File is too large to edit (...). Maximum
        // editable file size is 1GB.` message for a representative over-cap size.
        let over = MAX_EDIT_FILE_SIZE + 1; // 1073741825 bytes → still "1GB" via Ma
        let msg = format!(
            "File is too large to edit ({}). Maximum editable file size is {}.",
            crate::read::format_file_size(over),
            crate::read::format_file_size(MAX_EDIT_FILE_SIZE),
        );
        assert_eq!(
            msg,
            "File is too large to edit (1GB). Maximum editable file size is 1GB."
        );
    }

    #[tokio::test]
    async fn over_cap_file_is_rejected_and_untouched() {
        // A file whose reported size exceeds the cap is rejected before the body
        // is read and is left byte-for-byte unchanged. We force the size check
        // by stubbing `fs::metadata` is not feasible, so use a sparse-file
        // allocation to reach >1 GiB cheaply (no actual 1 GiB write).
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("huge.txt");
        let f = std::fs::File::create(&target).unwrap();
        // `set_len` creates a sparse file on macOS/Linux (no physical blocks
        // until written), so the on-disk size is >1 GiB at ~zero cost.
        f.set_len(MAX_EDIT_FILE_SIZE + 1).unwrap();
        drop(f);
        // Guard: only run if the filesystem honored the sparse length.
        let reported = std::fs::metadata(&target).unwrap().len();
        assert!(reported > MAX_EDIT_FILE_SIZE, "sparse file not over cap");

        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileEditTool::new(ctx);
        let err = tool
            .call(
                json!({
                    "file_path": target.to_str().unwrap(),
                    "old_string": "anything",
                    "new_string": "x"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        // `ToolError::InvalidInput`'s Display prefixes "invalid input: "; the
        // byte-exact model-facing message is the contained substring.
        assert!(
            err.to_string()
                .ends_with("File is too large to edit (1GB). Maximum editable file size is 1GB."),
            "unexpected error: {err}"
        );
        // File length unchanged (rejected before any write).
        assert_eq!(std::fs::metadata(&target).unwrap().len(), reported);
    }

    #[tokio::test]
    async fn at_cap_file_is_not_rejected() {
        // The cap is STRICT (`size > DYa`): a file exactly AT the cap is not
        // rejected by the size gate. (It then fails downstream for an absent
        // match / unread-file guard — i.e. NOT with the too-large message.)
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("atcap.txt");
        let f = std::fs::File::create(&target).unwrap();
        f.set_len(MAX_EDIT_FILE_SIZE).unwrap(); // exactly 1 GiB (sparse)
        drop(f);
        let reported = std::fs::metadata(&target).unwrap().len();
        assert_eq!(reported, MAX_EDIT_FILE_SIZE);

        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileEditTool::new(ctx);
        let err = tool
            .call(
                json!({
                    "file_path": target.to_str().unwrap(),
                    "old_string": "anything",
                    "new_string": "x"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        // NOT the too-large message — the size gate passed.
        assert!(
            !err.to_string().contains("File is too large to edit"),
            "at-cap file must not trip the strict size gate; got: {err}"
        );
    }

    // ── Finding #23: Edit `validateInput` deny-directory (errorCode 2) / UNC
    // early-allow / Perforce read-only guard (errorCode 11). Binary offset
    // 202622179; `H7e`/`cfr` at 193220147/193219993; `k7e` at 193229321. ──────

    #[test]
    fn perforce_read_only_message_is_byte_exact() {
        // `k7e` (binary offset 193229321): the em-dash is a real `U+2014`, and
        // the backtick-fenced `p4 edit <file>` / `chmod` guidance is verbatim.
        assert_eq!(
            PERFORCE_READ_ONLY_MESSAGE,
            "File is read-only \u{2014} it has not been opened for edit in Perforce. \
Run `p4 edit <file>` to check it out, then retry. Do not chmod the file writable; \
that bypasses Perforce tracking."
        );
        // Confirm a literal em-dash is present (catches an accidental ASCII `-`).
        assert!(PERFORCE_READ_ONLY_MESSAGE.contains('\u{2014}'));
    }

    #[test]
    fn owner_write_bit_matches_binary_128() {
        // `H7e`'s `(e & 128) === 0`: 128 == 0o200 == S_IWUSR.
        assert_eq!(OWNER_WRITE_BIT, 128);
        assert_eq!(OWNER_WRITE_BIT, 0o200);
    }

    #[tokio::test]
    async fn unc_path_skips_stat_gates_but_is_still_blocked_by_sandbox() {
        // The UNC/network early-allow (`i.startsWith("\\\\")||i.startsWith("//")
        // → {result:!0}`) makes `validateInput` PASS without running the
        // `stat`-based size/Perforce gates. In LingXi those gates are guarded by
        // `!is_unc`, so a `//`-prefixed path never trips them. (The sandbox
        // `canonicalize_and_validate` then blocks the out-of-tree network path —
        // that is LingXi's trusted-dir guard, not the binary's `validateInput`,
        // and is the expected terminal outcome for a `//host/share` target that
        // lives outside the tmp sandbox.) The point under test: the failure is
        // NOT one of the stat gates' messages.
        let tmp = TempDir::new().unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileEditTool::new(ctx);
        let err = tool
            .call(
                json!({
                    "file_path": "//server/share/file.txt",
                    "old_string": "a",
                    "new_string": "b"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains("File is too large to edit"),
            "UNC path must skip the size gate; got: {msg}"
        );
        assert!(
            !msg.contains("File is read-only"),
            "UNC path must skip the Perforce gate; got: {msg}"
        );
    }

    #[test]
    fn unc_prefix_detection_matches_binary() {
        // `i.startsWith("\\\\")||i.startsWith("//")` — the two prefixes the
        // binary treats as UNC/network paths, computed on the raw `file_path`.
        let is_unc = |p: &str| p.starts_with("\\\\") || p.starts_with("//");
        assert!(is_unc("//server/share"));
        assert!(is_unc(r"\\server\share"));
        // Single leading slash / single backslash are NOT UNC.
        assert!(!is_unc("/etc/hosts"));
        assert!(!is_unc(r"\etc\hosts"));
        assert!(!is_unc("relative/path"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn perforce_read_only_gate_fires_only_when_mode_enabled() {
        // ALL `LINGXI_PERFORCE_MODE`-dependent assertions live in this ONE
        // test (crate convention — no `serial_test` dep) so the process-global
        // env var is mutated within a single sequential unit. We restore the
        // prior value at the end.
        use std::os::unix::fs::PermissionsExt;
        let prior = std::env::var("LINGXI_PERFORCE_MODE").ok();

        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("ro.txt");
        std::fs::write(&target, "alpha\nbravo\n").unwrap();
        // Clear the owner-write bit (0o444 == r--r--r--): owner-write unset.
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o444)).unwrap();

        // (1) Perforce mode OFF (env unset) → guard does NOT fire even though
        // the file is read-only; the edit proceeds past the gate (and fails
        // later only on the unread-file staleness guard, NOT the Perforce msg).
        std::env::remove_var("LINGXI_PERFORCE_MODE");
        {
            let (ctx, _sink) = make_ctx(&tmp);
            let tool = FileEditTool::new(ctx);
            let err = tool
                .call(
                    json!({
                        "file_path": target.to_str().unwrap(),
                        "old_string": "bravo",
                        "new_string": "delta"
                    }),
                    fresh_ctx(),
                    fresh_tx(),
                )
                .await
                .unwrap_err();
            assert!(
                !err.to_string().contains("File is read-only"),
                "Perforce gate must stay dormant when mode is off; got: {err}"
            );
        }

        // (2) Perforce mode ON + owner-write unset → reject with the byte-exact
        // `k7e` message (errorCode 11).
        std::env::set_var("LINGXI_PERFORCE_MODE", "1");
        {
            let (ctx, _sink) = make_ctx(&tmp);
            let tool = FileEditTool::new(ctx);
            let err = tool
                .call(
                    json!({
                        "file_path": target.to_str().unwrap(),
                        "old_string": "bravo",
                        "new_string": "delta"
                    }),
                    fresh_ctx(),
                    fresh_tx(),
                )
                .await
                .unwrap_err();
            assert!(
                err.to_string().ends_with(PERFORCE_READ_ONLY_MESSAGE),
                "expected byte-exact Perforce message; got: {err}"
            );
        }

        // (3) Perforce mode ON but the file IS owner-writable (0o644) → guard
        // does NOT fire (mode bit set), edit proceeds past the gate.
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        {
            let (ctx, _sink) = make_ctx(&tmp);
            let tool = FileEditTool::new(ctx);
            let err = tool
                .call(
                    json!({
                        "file_path": target.to_str().unwrap(),
                        "old_string": "bravo",
                        "new_string": "delta"
                    }),
                    fresh_ctx(),
                    fresh_tx(),
                )
                .await
                .unwrap_err();
            assert!(
                !err.to_string().contains("File is read-only"),
                "owner-writable file must skip the Perforce gate; got: {err}"
            );
        }

        // (4) is_env_truthy semantics flow through `cfr`: "on"/"true"/"yes" are
        // truthy; "0"/"false"/"" are not. Spot-check via the helper directly so
        // the gate's enable condition is locked to the env-truthiness allowlist.
        std::env::set_var("LINGXI_PERFORCE_MODE", "on");
        assert!(is_perforce_mode_enabled());
        std::env::set_var("LINGXI_PERFORCE_MODE", "0");
        assert!(!is_perforce_mode_enabled());
        std::env::set_var("LINGXI_PERFORCE_MODE", "");
        assert!(!is_perforce_mode_enabled());

        // Restore prior env state.
        match prior {
            Some(v) => std::env::set_var("LINGXI_PERFORCE_MODE", v),
            None => std::env::remove_var("LINGXI_PERFORCE_MODE"),
        }
    }

    #[tokio::test]
    async fn empty_old_string_creates_new_file() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("created.txt");
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileEditTool::new(ctx);
        let _result = tool
            .call(
                json!({
                    "file_path": target.to_str().unwrap(),
                    "old_string": "",
                    "new_string": "brand new contents\n"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "brand new contents\n"
        );
    }

    #[tokio::test]
    async fn empty_old_string_on_existing_content_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("exists.txt");
        std::fs::write(&target, "already here").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = FileEditTool::new(ctx);
        let err = tool
            .call(
                json!({
                    "file_path": target.to_str().unwrap(),
                    "old_string": "",
                    "new_string": "x"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("Cannot create new file - file already exists."));
        // original content untouched
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "already here");
    }

    #[tokio::test]
    async fn empty_old_string_replaces_empty_file() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("blank.txt");
        std::fs::write(&target, "   \n").unwrap(); // whitespace-only ⇒ effectively empty
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = FileEditTool::new(ctx);
        tool.call(
            json!({
                "file_path": target.to_str().unwrap(),
                "old_string": "",
                "new_string": "seeded"
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "seeded");
    }

    #[tokio::test]
    async fn rejects_editing_notebook() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("nb.ipynb");
        std::fs::write(&target, "{\"cells\": []}").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = FileEditTool::new(ctx);
        let err = tool
            .call(
                json!({
                    "file_path": target.to_str().unwrap(),
                    "old_string": "cells",
                    "new_string": "x"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Jupyter Notebook"));
        assert!(err.to_string().contains("NotebookEdit"));
    }

    #[tokio::test]
    async fn rejects_identical_old_and_new() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("a.txt");
        std::fs::write(&target, "hello").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = FileEditTool::new(ctx);
        let err = tool
            .call(
                json!({
                    "file_path": target.to_str().unwrap(),
                    "old_string": "hello",
                    "new_string": "hello"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("No changes to make"));
    }

    #[tokio::test]
    async fn nonexistent_file_with_nonempty_old_string_errors() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("missing.txt");
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileEditTool::new(ctx);
        let err = tool
            .call(
                json!({
                    "file_path": target.to_str().unwrap(),
                    "old_string": "x",
                    "new_string": "y"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        // FT-06: byte-locked `FileEditTool.validateInput` errorCode-4 message —
        // the same sentence Read emits, including the cwd note. No sibling
        // shares `missing`'s stem and the path is under cwd, so neither
        // `" Did you mean ...?"` suffix fires.
        let cwd = std::fs::canonicalize(tmp.path()).unwrap();
        let msg = err.to_string();
        assert!(
            msg.ends_with(&format!(
                "File does not exist. Note: your current working directory is {}.",
                cwd.display()
            )),
            "expected the byte-locked cwd-note message, got: {msg}"
        );
    }

    #[tokio::test]
    async fn rejects_ambiguous_without_replace_all() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("a.txt");
        std::fs::write(&target, "foo foo foo").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = FileEditTool::new(ctx);
        let err = tool
            .call(
                json!({
                    "file_path": target.to_str().unwrap(),
                    "old_string": "foo",
                    "new_string": "bar"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        // Byte-locked multiple-match message (FileEditTool.ts:336): `count` ==
        // TS `matches`, trailing `String:` echoes the original `old_string`.
        match err {
            ToolError::InvalidInput(m) => assert_eq!(
                m,
                "Found 3 matches of the string to replace, but replace_all is false. To replace all occurrences, set replace_all to true. To replace only one occurrence, please provide more context to uniquely identify the instance.\nString: foo"
            ),
            other => panic!("expected InvalidInput, got {other:?}"),
        }
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "foo foo foo");
    }

    #[tokio::test]
    async fn replace_all_handles_multiple_matches() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("a.txt");
        std::fs::write(&target, "foo foo foo").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = FileEditTool::new(ctx);
        let result = tool
            .call(
                json!({
                    "file_path": target.to_str().unwrap(),
                    "old_string": "foo",
                    "new_string": "bar",
                    "replace_all": true
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // replace_all path emits the "All occurrences were successfully
        // replaced." message verbatim to the model.
        let input_path = target.to_str().unwrap();
        assert_eq!(
            result.model_content.as_deref().unwrap(),
            edit_result_message(input_path, true, false)
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "bar bar bar");
    }

    #[tokio::test]
    async fn rejects_no_match() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("a.txt");
        std::fs::write(&target, "hello").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = FileEditTool::new(ctx);
        let err = tool
            .call(
                json!({
                    "file_path": target.to_str().unwrap(),
                    "old_string": "absent",
                    "new_string": "x"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        // Byte-locked string-to-replace-not-found message (FileEditTool.ts:321),
        // echoing the original `old_string` ("absent") verbatim. Pure-ASCII
        // old_string ⇒ `pUa` false ⇒ NO escape-swap note appended.
        match err {
            ToolError::InvalidInput(m) => {
                assert_eq!(m, "String to replace not found in file.\nString: absent");
                assert!(!m.contains("tried swapping"));
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_match_appends_escape_swap_note_for_non_ascii() {
        // old_string has a non-ASCII char (`pUa` true) but neither its literal
        // nor its `\uXXXX` escaped form is in the file ⇒ not found, with the
        // byte-locked escape-swap note appended (binary offset 202624182).
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("a.txt");
        std::fs::write(&target, "plain ascii content").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = FileEditTool::new(ctx);
        let err = tool
            .call(
                json!({
                    "file_path": target.to_str().unwrap(),
                    "old_string": "café",
                    "new_string": "x"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidInput(m) => {
                assert_eq!(
                    m,
                    format!("String to replace not found in file.\nString: café{ESCAPE_SWAP_NOTE}")
                );
                assert!(m.contains("tried swapping"));
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_match_appends_note_for_unicode_escape_old_string() {
        // old_string contains a `\uXXXX` escape (`pUa` true via Tlo) absent from
        // the file ⇒ not found, with the escape-swap note appended.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("a.txt");
        std::fs::write(&target, "no match here").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = FileEditTool::new(ctx);
        let err = tool
            .call(
                json!({
                    "file_path": target.to_str().unwrap(),
                    "old_string": "x\\u00e9y",
                    "new_string": "z"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidInput(m) => {
                assert!(m.contains("tried swapping"));
                assert!(m.starts_with("String to replace not found in file.\nString: x\\u00e9y"));
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn edit_succeeds_via_escape_swap_decode() {
        // File has the literal `é`; model sent the `\uXXXX` escape. The `vIe`
        // escape-decode fallback locates and replaces the literal text.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("a.txt");
        std::fs::write(&target, "let v = café;").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = FileEditTool::new(ctx);
        tool.call(
            json!({
                "file_path": target.to_str().unwrap(),
                "old_string": "caf\\u00e9",
                "new_string": "latte"
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .expect("escape-swap decode should locate and replace literal `café`");
        let written = std::fs::read_to_string(&target).unwrap();
        assert_eq!(written, "let v = latte;");
    }

    #[tokio::test]
    async fn edit_succeeds_via_non_ascii_escaped_form() {
        // File stores the ESCAPED form `é`; model sent the literal `é`. The
        // `vIe` non-ASCII regex fallback (dUa) locates the escaped run.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("a.txt");
        std::fs::write(&target, "x = \\u00e9 ;").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = FileEditTool::new(ctx);
        tool.call(
            json!({
                "file_path": target.to_str().unwrap(),
                "old_string": "é",
                "new_string": "E"
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .expect("non-ASCII escaped-form fallback should locate `\\u00e9`");
        let written = std::fs::read_to_string(&target).unwrap();
        assert_eq!(written, "x = E ;");
    }

    #[tokio::test]
    async fn structured_patch_is_a_hunk_array() {
        // `data.structuredPatch` is the binary's jsdiff hunk ARRAY (not a string
        // preview): each hunk carries oldStart/oldLines/newStart/newLines + lines
        // with ` `/`+`/`-` prefixes. (No truncation — the hunk array is complete.)
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("big.txt");
        let big: String = (0..100).map(|i| format!("L{i}\n")).collect();
        std::fs::write(&target, &big).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = FileEditTool::new(ctx);
        let result = tool
            .call(
                json!({
                    "file_path": target.to_str().unwrap(),
                    "old_string": "L",
                    "new_string": "M",
                    "replace_all": true
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        let sp = result.data["structuredPatch"]
            .as_array()
            .expect("hunk array");
        assert!(!sp.is_empty(), "expected at least one hunk");
        let h0 = &sp[0];
        assert!(h0["oldStart"].is_number());
        assert!(h0["oldLines"].is_number());
        assert!(h0["newStart"].is_number());
        assert!(h0["newLines"].is_number());
        let lines = h0["lines"].as_array().expect("hunk lines");
        let first = lines[0].as_str().unwrap();
        assert!(
            matches!(first.chars().next(), Some(' ' | '+' | '-')),
            "hunk line must carry a diff prefix: {first:?}"
        );
    }

    #[tokio::test]
    async fn edits_crlf_file_preserving_endings() {
        // old_string spans a line break — only matchable against the
        // LF-normalized in-memory view; the rewrite must re-apply CRLF.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("crlf.txt");
        std::fs::write(&target, b"line one\r\nline two\r\nline three\r\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = FileEditTool::new(ctx);
        tool.call(
            json!({
                "file_path": target.to_str().unwrap(),
                // Matches across the CRLF that was normalized to LF.
                "old_string": "line one\nline two",
                "new_string": "first\nsecond"
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        // ASSERT BYTES: CRLF preserved on disk, including the rewritten region.
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"first\r\nsecond\r\nline three\r\n"
        );
    }

    #[tokio::test]
    async fn edits_utf16le_file_preserving_bom_and_encoding() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("u16.txt");
        // "hello world" in UTF-16LE with BOM.
        let mut bytes = vec![0xFF, 0xFE];
        for u in "hello world".encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        std::fs::write(&target, &bytes).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = FileEditTool::new(ctx);
        tool.call(
            json!({
                "file_path": target.to_str().unwrap(),
                "old_string": "world",
                "new_string": "Rust"
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        // ASSERT BYTES: BOM preserved, re-encoded as UTF-16LE.
        let mut expected = vec![0xFF, 0xFE];
        for u in "hello Rust".encode_utf16() {
            expected.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(std::fs::read(&target).unwrap(), expected);
    }

    #[tokio::test]
    async fn mixed_endings_pick_dominant_crlf() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("mixed.txt");
        // 3 CRLF vs 1 bare LF ⇒ CRLF dominates (TS crlf > lf).
        std::fs::write(&target, b"a\r\nb\r\nc\r\nd\nEDITME").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = FileEditTool::new(ctx);
        tool.call(
            json!({
                "file_path": target.to_str().unwrap(),
                "old_string": "EDITME",
                "new_string": "done"
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        // ASSERT BYTES: the lone LF after `c\r\n` is rewritten to the dominant
        // CRLF (matches TS writeTextContent collapsing then re-applying CRLF).
        assert_eq!(std::fs::read(&target).unwrap(), b"a\r\nb\r\nc\r\nd\r\ndone");
    }

    #[tokio::test]
    async fn curly_double_in_file_matches_straight_and_preserves_curly() {
        // File has curly "hello"; model sends straight "hello". Edit must match
        // and rewrite preserving the file's curly typography (Batch E).
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("curly.txt");
        std::fs::write(&target, "say \u{201C}hello\u{201D} now").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = FileEditTool::new(ctx);
        tool.call(
            json!({
                "file_path": target.to_str().unwrap(),
                "old_string": "\"hello\"",
                "new_string": "\"world\""
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        // new_string straight doubles ⇒ curly applied by open/close context.
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "say \u{201C}world\u{201D} now"
        );
    }

    #[tokio::test]
    async fn curly_single_contraction_keeps_right_single() {
        // File uses curly singles; new_string contains a contraction `don't`.
        // The apostrophe (letter on both sides) must become a right single
        // curly, NOT an opening quote.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("contraction.txt");
        std::fs::write(&target, "a \u{2018}b\u{2019} c").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = FileEditTool::new(ctx);
        tool.call(
            json!({
                "file_path": target.to_str().unwrap(),
                "old_string": "'b'",
                "new_string": "don't"
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "a don\u{2019}t c"
        );
    }

    #[tokio::test]
    async fn curly_single_open_vs_close_positions() {
        // Leading quote (start of replacement) ⇒ opening; trailing quote (after
        // a letter, end) ⇒ closing.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("quotes.txt");
        std::fs::write(&target, "x \u{2018}q\u{2019} y").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = FileEditTool::new(ctx);
        tool.call(
            json!({
                "file_path": target.to_str().unwrap(),
                "old_string": "'q'",
                "new_string": "'word'"
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "x \u{2018}word\u{2019} y"
        );
    }

    #[tokio::test]
    async fn no_curly_in_file_leaves_new_string_untouched() {
        // Plain ASCII file ⇒ exact match ⇒ no quote normalization ⇒ new_string
        // straight quotes stay straight.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("plain.txt");
        std::fs::write(&target, "say \"hello\" now").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = FileEditTool::new(ctx);
        tool.call(
            json!({
                "file_path": target.to_str().unwrap(),
                "old_string": "\"hello\"",
                "new_string": "\"world\""
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        // Straight quotes preserved verbatim (no curly applied).
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "say \"world\" now"
        );
    }

    // ───────────────────────── Batch F: staleness guard ─────────────────────

    #[tokio::test]
    async fn edit_without_prior_read_errors_not_read() {
        // Editing an existing file with NO recorded Read → FILE_NOT_READ_ERROR.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("unread.txt");
        std::fs::write(&target, "hello world").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        // Deliberately do NOT seed a prior read.
        let tool = FileEditTool::new(ctx);
        let err = tool
            .call(
                json!({
                    "file_path": target.to_str().unwrap(),
                    "old_string": "world",
                    "new_string": "Rust"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        // ToolError::Display prefixes "invalid input: "; assert the exact
        // byte-locked message on the InvalidInput payload.
        match err {
            ToolError::InvalidInput(m) => assert_eq!(m, crate::FILE_NOT_READ_ERROR),
            other => panic!("expected InvalidInput, got {other:?}"),
        }
        // File untouched.
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello world");
    }

    #[tokio::test]
    async fn partial_read_then_edit_unchanged_mtime_succeeds() {
        // 2.1.212: a partial (offset/limit) read is NOT rejected as "not read".
        // claude-code's guard (`FOg`) throws "File has not been read yet" only
        // when NO read-state entry exists; a ranged read still has an entry, so
        // with the mtime unchanged the edit proceeds. (Before 2.1.212 this
        // raised FILE_NOT_READ_ERROR before even checking the mtime.)
        use filetime::{set_file_mtime, FileTime};
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("partial.txt");
        std::fs::write(&target, "a\nb\nc\n").unwrap();
        set_file_mtime(&target, FileTime::from_unix_time(1_000_000_000, 0)).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let canon = std::fs::canonicalize(&target).unwrap();
        let mtime_ms = std::fs::metadata(&canon)
            .ok()
            .and_then(|m| m.modified().ok())
            .map_or(0, tool_api::read_file_state::mtime_ms_floor);
        // Seed a PARTIAL read (offset/limit set) — a ranged view, not a full one.
        tool_api::read_file_state::set(
            &ctx.read_file_state,
            canon,
            tool_api::read_file_state::ReadFileEntry {
                content: "a\nb\nc\n".into(),
                mtime_ms,
                offset: Some(1),
                limit: Some(2),
                // Simulates a prior PARTIAL `Read` (offset/limit set).
                from_read: true,
                seeded_from_context: false,
                is_partial_view: false,
            },
        );
        let tool = FileEditTool::new(ctx);
        let res = tool
            .call(
                json!({
                    "file_path": target.to_str().unwrap(),
                    "old_string": "b",
                    "new_string": "B"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ranged read + unchanged mtime must let the edit proceed");
        assert!(!res.is_error);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "a\nB\nc\n");
    }

    #[tokio::test]
    async fn external_modify_then_edit_errors_modified() {
        // Read → external modify (mtime bumps AND content changes) → Edit must
        // refuse with FILE_UNEXPECTEDLY_MODIFIED_ERROR.
        use filetime::{set_file_mtime, FileTime};
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("ext.txt");
        std::fs::write(&target, "original\n").unwrap();
        set_file_mtime(&target, FileTime::from_unix_time(1_000_000_000, 0)).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        // Seed the full read as of the OLD mtime/content.
        seed_full_read(&ctx, &target);
        // Now an external actor rewrites the file AND bumps the mtime forward.
        std::fs::write(&target, "tampered\n").unwrap();
        set_file_mtime(&target, FileTime::from_unix_time(2_000_000_000, 0)).unwrap();
        let tool = FileEditTool::new(ctx);
        let err = tool
            .call(
                json!({
                    "file_path": target.to_str().unwrap(),
                    "old_string": "tampered",
                    "new_string": "x"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        match err {
            // FT-07: stale-changed-content returns the oracle's
            // `validateInput` errorCode-7 literal (cc-238.js @226407883),
            // not the call-phase race sentence `WVo`.
            ToolError::InvalidInput(m) => assert_eq!(m, crate::FILE_UNEXPECTEDLY_MODIFIED_ERROR),
            other => panic!("expected InvalidInput, got {other:?}"),
        }
        // Edit refused → file left as the external content.
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "tampered\n");
    }

    #[tokio::test]
    async fn same_content_touch_then_edit_proceeds_via_fallback() {
        // Read(full) → mtime bumped but bytes UNCHANGED (cloud-sync/antivirus
        // touch) → Edit proceeds via the content-equality fallback.
        use filetime::{set_file_mtime, FileTime};
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("touched.txt");
        std::fs::write(&target, "keep me\n").unwrap();
        set_file_mtime(&target, FileTime::from_unix_time(1_000_000_000, 0)).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        // Touch: bump mtime forward WITHOUT changing content.
        set_file_mtime(&target, FileTime::from_unix_time(2_000_000_000, 0)).unwrap();
        let tool = FileEditTool::new(ctx);
        let _result = tool
            .call(
                json!({
                    "file_path": target.to_str().unwrap(),
                    "old_string": "keep me",
                    "new_string": "edited"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "edited\n");
    }

    #[tokio::test]
    async fn post_write_set_lets_immediate_second_edit_succeed() {
        // First Edit succeeds with a seeded read; its post-write `set` updates
        // the registry so a SECOND immediate Edit (no re-seed) also succeeds.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("twice.txt");
        std::fs::write(&target, "alpha beta\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = FileEditTool::new(ctx);
        tool.call(
            json!({
                "file_path": target.to_str().unwrap(),
                "old_string": "alpha",
                "new_string": "ALPHA"
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        // No re-seed: the post-write map.set from the first edit must satisfy
        // the guard for the second edit.
        let _result = tool
            .call(
                json!({
                    "file_path": target.to_str().unwrap(),
                    "old_string": "beta",
                    "new_string": "BETA"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "ALPHA BETA\n");
    }

    #[tokio::test]
    async fn lf_file_round_trips_as_lf() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("lf.txt");
        std::fs::write(&target, b"alpha\nbeta\ngamma\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = FileEditTool::new(ctx);
        tool.call(
            json!({
                "file_path": target.to_str().unwrap(),
                "old_string": "beta",
                "new_string": "BETA"
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        // ASSERT BYTES: still pure LF (no CR introduced).
        assert_eq!(std::fs::read(&target).unwrap(), b"alpha\nBETA\ngamma\n");
    }

    #[tokio::test]
    async fn description_is_short_label_and_long_prompt_is_verbatim_ts() {
        let tmp = TempDir::new().unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileEditTool::new(ctx);
        // R-EditDesc: description() is the SHORT display label (claude-code
        // `async description(){return "A tool for editing files"}`), NOT the long
        // body — the long body lives in prompt().
        let d = tool
            .description(
                &json!({}),
                &DescriptionOptions {
                    is_non_interactive_session: false,
                },
            )
            .await;
        assert_eq!(d, "A tool for editing files");
        // The long body is prompt(model:None) ⇒ Dh(undefined)=false ⇒ LONG.
        // Header + the `\n-` pre-read seam (Usage: immediately followed by the
        // first bullet). v2.1.183 dropped the trailing space the older builds
        // had after "reading the file." — the pre-read bullet now ends exactly
        // at the period with no trailing space (binary `wBp()`/`RBp()`).
        let long = tool
            .prompt(&PromptOptions {
                include_examples: false,
                model: None,
                model_profile: None,
            })
            .await;
        assert!(long.starts_with(
            "Performs exact string replacements in files.\n\nUsage:\n- You must use your `Read` tool"
        ));
        assert!(long.contains("before editing. This tool will error"));
        assert!(long.contains(
            "This tool will error if you attempt an edit without reading the file.\n- When editing text"
        ));
        // No trailing space after the first bullet (the v2.1.183 one-byte fix).
        assert!(!long.contains("reading the file. \n"));
        // Locks the compact-format decision (line number + tab, not padded-arrow).
        assert!(long.contains("The line number prefix format is: line number + tab."));
        // Final bullet, with NO trailing newline.
        assert!(long
            .ends_with("This parameter is useful if you want to rename a variable for instance."));
        // Locks `minimalUniquenessHint` empty (non-`ant` 3P build).
        assert!(!long.contains("smallest old_string"));
        // prompt(model:None) ⇒ Dh(undefined)=false ⇒ LONG.
        // description() (short label) and the long prompt() differ — confirm.
        assert_ne!(long, d);
    }

    #[tokio::test]
    async fn short_prompt_for_simple_system_model() {
        let tmp = TempDir::new().unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileEditTool::new(ctx);
        // model:claude-opus-4-8 ⇒ Dh=true ⇒ SHORT prompt (byte-anchor).
        let p = tool
            .prompt(&PromptOptions {
                include_examples: false,
                model: Some("claude-opus-4-8".to_string()),
                model_profile: None,
            })
            .await;
        assert_eq!(p, EDIT_PROMPT_SHORT);
        assert!(p.starts_with("Performs exact string replacement in a file.\n\n- You must Read the file in this conversation before editing, or the call will fail."));
        // Line-prefix slot resolves to "line number + tab" (N$e() default false).
        assert!(p.contains("Strip the Read line prefix (line number + tab) before matching."));
        assert!(p.ends_with("- `replace_all: true` replaces every occurrence instead."));
    }

    /// Worktree parity plan (Task 3): a RELATIVE `file_path` must resolve
    /// against the CURRENT `session_cwd` — so after `EnterWorktree` swaps the
    /// cwd into a worktree, a relative-path `Edit` lands under the worktree,
    /// NOT the boot cwd (which is what `std::fs::canonicalize` would resolve
    /// a relative path against if the tool passed it through unmodified).
    #[tokio::test]
    async fn relative_path_edit_follows_session_cwd_swap_into_worktree() {
        let tmp = TempDir::new().unwrap();
        let wt = tmp.path().join("wt");
        std::fs::create_dir(&wt).unwrap();
        let target = wt.join("rel.txt");
        std::fs::write(&target, "hello world").unwrap();

        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        // Swap the session cwd into the worktree subdir (trusting it too), the
        // way `EnterWorktree` does.
        ctx.session_cwd.swap(wt.clone(), vec![wt.clone()]);
        assert_eq!(ctx.cwd(), wt);

        let tool = FileEditTool::new(ctx);
        tool.call(
            json!({
                "file_path": "rel.txt",
                "old_string": "world",
                "new_string": "worktree"
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "hello worktree",
            "relative file_path must resolve under the SWAPPED worktree cwd"
        );
        assert!(
            !tmp.path().join("rel.txt").exists(),
            "must NOT have landed under the boot cwd"
        );
    }

    /// INERT INVARIANT companion: with no `session_cwd.swap(..)` call, a
    /// relative `file_path` resolves under the boot cwd exactly as it did
    /// before this task's fix.
    #[tokio::test]
    async fn relative_path_edit_resolves_under_boot_cwd_without_swap() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("rel.txt");
        std::fs::write(&target, "hello world").unwrap();

        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        assert_eq!(ctx.cwd(), tmp.path(), "no swap happened in this test");

        let tool = FileEditTool::new(ctx);
        tool.call(
            json!({
                "file_path": "rel.txt",
                "old_string": "world",
                "new_string": "boot"
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello boot");
    }

    /// S2 (PathAtlas): fenced guest space is refused before any file-state
    /// or old-string validation runs.
    #[tokio::test]
    async fn fenced_guest_path_is_refused_before_file_state() {
        let host = TempDir::new().unwrap();
        let ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_guest_alias_fs("/workspace/abc", host.path(), "/fenced"),
            Arc::new(AnalyticsBus::new()),
            vec![host.path().to_path_buf()],
        );
        let tool = FileEditTool::new(ctx);
        let err = tool
            .call(
                serde_json::json!({
                    "file_path": "/fenced/config.txt",
                    "old_string": "a",
                    "new_string": "b"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidInput(message) => {
                assert!(message.contains("not host-backed"), "{message}");
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }
}
