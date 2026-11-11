//! Byte replay of bounded native 2.1.321 fork helpers. The fixed owner header is
//! LingXi's deliberate extension; the remaining native rows must match exactly.

use std::sync::Arc;

use lingxi_core::host::FileSystem;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use platform_posix::PosixFileSystem;
use serde_json::{json, Value};
use session::branch::{render_branch_from_captured, CapturedBranch};
use session::jsonl::{route_lines, session_path};
use uuid::Uuid;

fn fixed_header(id: Uuid) -> String {
    json!({"type":"fixed-session-mode", "sessionId":id, "sessionMode":"code", "instructionEpoch":0})
        .to_string()
}

#[tokio::test]
async fn fork_rows_match_native_321_bytes() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/session_fork_2_1_321.json")).unwrap();
    let source = Uuid::parse_str(fixture["sourceSessionId"].as_str().unwrap()).unwrap();
    let target = Uuid::parse_str(fixture["targetSessionId"].as_str().unwrap()).unwrap();
    let cwd = fixture["cwd"].as_str().unwrap();
    let prototype = route_lines(fixture["forkCases"][0]["sourceJsonl"].as_str().unwrap())
        .messages_in_order[0]
        .clone();
    for case in fixture["forkCases"].as_array().unwrap() {
        let home = tempfile::tempdir().unwrap();
        let catalog = Utf16JsonProjection::parse(case["catalogJson"].as_str().unwrap()).unwrap();
        for index in 0..catalog.at_pointer("").unwrap().array_len().unwrap() {
            let row = catalog.subprojection(&format!("/{index}")).unwrap();
            let id = Uuid::new_v4();
            let path = session_path(home.path(), cwd, &id.to_string());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut records = vec![
                fixed_header(id),
                json!({
                    "type": "user", "uuid": Uuid::new_v4(), "parentUuid": null,
                    "sessionId": id, "cwd": cwd, "version": "2.1.321",
                    "timestamp": "2026-11-09T12:34:56.000Z",
                    "message": { "role": "user", "content": "catalog prompt" },
                })
                .to_string(),
            ];
            for (field, kind) in [
                ("customTitle", "custom-title"),
                ("aiTitle", "ai-title"),
                ("summary", "summary"),
            ] {
                if let Ok(title) = row.subprojection(&format!("/{field}")) {
                    let record = Utf16JsonProjection::object_units(vec![
                        (
                            "type".encode_utf16().collect(),
                            Utf16JsonProjection::from(kind),
                        ),
                        (
                            "sessionId".encode_utf16().collect(),
                            Utf16JsonProjection::from(id.to_string()),
                        ),
                        (field.encode_utf16().collect(), title),
                    ])
                    .unwrap();
                    records.push(record.to_json_string().unwrap());
                }
            }
            std::fs::write(path, format!("{}\n", records.join("\n"))).unwrap();
        }
        // The adapter supplies LingXi's required owner header without changing
        // the independently generated native source or expected row bytes.
        let content = format!(
            "{}\n{}",
            fixed_header(source),
            case["sourceJsonl"].as_str().unwrap()
        );
        let loaded = route_lines(&content);
        let selected: Vec<_> = case["selectedUuids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|uuid| {
                loaded
                    .message(uuid.as_str().unwrap())
                    .cloned()
                    .unwrap_or_else(|| {
                        let mut selected = prototype.clone();
                        selected.uuid = uuid.as_str().unwrap().to_owned();
                        selected
                    })
            })
            .collect();
        let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(home.path().to_path_buf()));
        let rendered = render_branch_from_captured(
            CapturedBranch {
                lingxi_home: home.path(),
                source_cwd: cwd,
                target_cwd: cwd,
                source_session_id: source,
                target_session_id: target,
                source_content: &content,
                messages: &selected,
                custom_title: case["customTitle"].as_str(),
                mode: None,
            },
            fs,
        )
        .await
        .unwrap();
        let (header, actual) = rendered.content.split_once('\n').unwrap();
        let header = serde_json::from_str::<Value>(header).unwrap();
        assert_eq!(header["type"], "fixed-session-mode");
        if case["name"] == "newer-skill-epoch-with-null-sidechain" {
            assert_eq!(header["instructionEpoch"], 18);
        }
        if case["name"] == "newer-skill-epoch-with-zero-sidechain" {
            assert_eq!(header["instructionEpoch"], 20);
        }
        assert_eq!(
            actual,
            case["expectedJsonl"].as_str().unwrap(),
            "{}",
            case["name"]
        );
    }
}

#[tokio::test]
async fn automatic_fork_name_checks_real_linked_worktree_catalogs() {
    use std::process::Command;
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let repository = root.join("repository");
    let sibling = root.join("sibling");
    let home = root.join("home");
    std::fs::create_dir_all(&repository).unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .arg("-C")
            .arg(&repository)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "--quiet"]);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "--quiet",
        "--allow-empty",
        "-m",
        "fixture",
    ]);
    git(&[
        "worktree",
        "add",
        "--quiet",
        "--detach",
        sibling.to_str().unwrap(),
        "HEAD",
    ]);
    for number in [1, 2] {
        let id = Uuid::new_v4();
        let path = session_path(&home, sibling.to_str().unwrap(), &id.to_string());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let title = if number == 1 {
            "worktree collision (Branch)".to_owned()
        } else {
            format!("worktree collision (Branch {number})")
        };
        let row = json!({"type":"user", "uuid":Uuid::new_v4(), "sessionId":id, "parentUuid":null,
            "timestamp":"2026-11-09T12:34:56.000Z", "cwd":sibling.to_str().unwrap(),
            "version":"2.1.321", "message":{"role":"user", "content":"stored"}});
        let title = json!({"type":"custom-title", "customTitle":title, "sessionId":id});
        std::fs::write(path, format!("{}\n{row}\n{title}\n", fixed_header(id))).unwrap();
    }
    let source = Uuid::new_v4();
    let target = Uuid::new_v4();
    let row = json!({"type":"user", "uuid":Uuid::new_v4(), "sessionId":source, "parentUuid":null,
        "timestamp":"2026-11-09T12:34:56.000Z", "cwd":repository.to_str().unwrap(),
        "version":"2.1.321", "message":{"role":"user", "content":"worktree collision"}});
    let content = format!("{}\n{row}\n", fixed_header(source));
    let selected = route_lines(&content).messages_in_order;
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(repository.clone()));
    let fork = render_branch_from_captured(
        CapturedBranch {
            lingxi_home: &home,
            source_cwd: repository.to_str().unwrap(),
            target_cwd: repository.to_str().unwrap(),
            source_session_id: source,
            target_session_id: target,
            source_content: &content,
            messages: &selected,
            custom_title: None,
            mode: None,
        },
        fs,
    )
    .await
    .unwrap();
    assert_eq!(fork.result.title, "worktree collision (Branch 3)");
}
