//! Bootstrap discovery of installed plugins from disk (GAP E).
//!
//! Mirrors claude-code's cache-only loader (`pluginLoader.ts:1348`
//! `createPluginFromPath` + `1618` standard `hooks/hooks.json`): walk the
//! plugins directory, read each `<plugin>/.lingxi-plugin/plugin.json`, and
//! auto-detect the `commands/`, `agents/`, `skills/<name>/SKILL.md`,
//! `output-styles/` component directories, the standard `hooks/hooks.json`,
//! and the `.mcp.json` / `.lsp.json` server configs.

use std::fs;
use std::path::Path;

/// Write a self-contained fixture plugin under `root/<name>`.
fn write_fixture_plugin(root: &Path, name: &str) {
    let plugin_dir = root.join(name);
    fs::create_dir_all(plugin_dir.join(".lingxi-plugin")).unwrap();
    fs::write(
        plugin_dir.join(".lingxi-plugin").join("plugin.json"),
        r#"{"name":"myplugin","version":"1.2.3","description":"a fixture plugin","author":{"name":"Ada"},"homepage":"https://example.com"}"#,
    )
    .unwrap();

    fs::create_dir_all(plugin_dir.join("commands")).unwrap();
    fs::write(
        plugin_dir.join("commands").join("hello.md"),
        "---\ndescription: say hi\n---\nHello from the plugin.\n",
    )
    .unwrap();

    fs::create_dir_all(plugin_dir.join("agents")).unwrap();
    fs::write(
        plugin_dir.join("agents").join("helper.md"),
        "---\nname: helper\ndescription: helps\n---\nYou help.\n",
    )
    .unwrap();

    fs::create_dir_all(plugin_dir.join("hooks")).unwrap();
    fs::write(
        plugin_dir.join("hooks").join("hooks.json"),
        r#"{"description":"fixture hooks","hooks":{"PreToolUse":[{"matcher":"Write","hooks":[{"type":"command","command":"echo hi"}]}]}}"#,
    )
    .unwrap();
}

#[tokio::test]
async fn discovers_a_single_installed_plugin_with_all_components() {
    let tmp = tempfile::tempdir().unwrap();
    write_fixture_plugin(tmp.path(), "myplugin");

    let discovered = plugin::discover_installed_plugins(tmp.path()).await;

    assert_eq!(discovered.len(), 1, "exactly one plugin discovered");
    let (id, manifest, dir) = &discovered[0];

    // Manifest metadata parsed from plugin.json.
    assert_eq!(manifest.name, "myplugin");
    assert_eq!(manifest.version, "1.2.3");
    assert_eq!(manifest.description, "a fixture plugin");
    assert_eq!(manifest.author.as_deref(), Some("Ada"));
    assert_eq!(manifest.homepage.as_deref(), Some("https://example.com"));

    // The freshly-minted id is stamped onto the manifest too.
    assert_eq!(manifest.id, *id);

    // Auto-detected component directories (createPluginFromPath Step 3).
    assert_eq!(
        manifest.components.commands.len(),
        1,
        "commands/hello.md auto-detected"
    );
    assert_eq!(
        manifest.components.agents.len(),
        1,
        "agents/helper.md auto-detected"
    );

    // Standard hooks/hooks.json parsed into HookDefinition(s).
    assert_eq!(
        manifest.components.hooks.len(),
        1,
        "hooks/hooks.json PreToolUse entry parsed"
    );

    // The install dir points at the plugin root.
    assert_eq!(dir, &tmp.path().join("myplugin"));
}

#[tokio::test]
async fn detects_skill_subdirs_mcp_and_lsp_configs() {
    // Skills use `skills/<name>/SKILL.md` (one level deep, SKILL.md only —
    // `validatePlugin.ts:735-739`); MCP servers from `.mcp.json`; LSP servers
    // from `.lsp.json` (`mcpPluginIntegration.ts` / `lspPluginIntegration.ts`).
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("p");
    fs::create_dir_all(dir.join(".lingxi-plugin")).unwrap();
    fs::write(
        dir.join(".lingxi-plugin").join("plugin.json"),
        r#"{"name":"p","version":"1.0.0"}"#,
    )
    .unwrap();

    // skills/greeter/SKILL.md is detected; a flat skills/stray.md is NOT.
    fs::create_dir_all(dir.join("skills").join("greeter")).unwrap();
    fs::write(
        dir.join("skills").join("greeter").join("SKILL.md"),
        "---\nname: greeter\ndescription: hi\n---\nbody\n",
    )
    .unwrap();
    fs::write(dir.join("skills").join("stray.md"), "not a skill").unwrap();

    fs::write(
        dir.join(".mcp.json"),
        r#"{"mcpServers":{"echo":{"command":"echo"}}}"#,
    )
    .unwrap();
    fs::write(
        dir.join(".lsp.json"),
        r#"{"pyls":{"command":"pylsp","extensionToLanguage":{".py":"python"}}}"#,
    )
    .unwrap();

    let discovered = plugin::discover_installed_plugins(tmp.path()).await;
    assert_eq!(discovered.len(), 1);
    let comps = &discovered[0].1.components;

    assert_eq!(
        comps.skills.len(),
        1,
        "only skills/greeter/SKILL.md is a skill"
    );
    assert!(comps.skills[0].path.ends_with("greeter/SKILL.md"));
    assert!(
        comps.mcp_servers.contains_key("echo"),
        "echo MCP server from .mcp.json"
    );
    assert!(
        comps.lsp_servers.contains_key("pyls"),
        "pyls LSP server from .lsp.json"
    );
}

/// §14: `keywords`, `license`, `repository`, `metadata`, `author.email`,
/// `author.url`, and `channels[].displayName` are all parsed at
/// `discovery.rs` but were previously dropped building `PluginManifest`.
/// This pins that they now survive onto the manifest, byte-shaped to the
/// oracle's `plugin.json` schema (`repository` a plain string, `metadata`
/// preserved unread).
#[tokio::test]
async fn preserves_metadata_fields_author_contact_and_channel_display_name() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("richplugin");
    fs::create_dir_all(dir.join(".lingxi-plugin")).unwrap();
    fs::write(
        dir.join(".lingxi-plugin").join("plugin.json"),
        r#"{
            "name": "richplugin",
            "version": "2.0.0",
            "description": "a plugin with metadata fields",
            "author": {"name": "Ada", "email": "ada@example.com", "url": "https://ada.example.com"},
            "homepage": "https://example.com",
            "keywords": ["weather", "utility"],
            "license": "MIT",
            "repository": "https://github.com/example/richplugin",
            "metadata": {"catalogId": "abc123", "tier": "gold"},
            "mcpServers": {"echo": {"command": "echo"}},
            "channels": [{"server": "echo", "displayName": "Echo Channel"}]
        }"#,
    )
    .unwrap();

    let discovered = plugin::discover_installed_plugins(tmp.path()).await;
    assert_eq!(discovered.len(), 1);
    let manifest = &discovered[0].1;

    assert_eq!(
        manifest.keywords,
        vec!["weather".to_string(), "utility".to_string()],
        "keywords parsed and preserved"
    );
    assert_eq!(
        manifest.license.as_deref(),
        Some("MIT"),
        "license preserved"
    );
    assert_eq!(
        manifest.repository.as_deref(),
        Some("https://github.com/example/richplugin"),
        "repository preserved as a plain string"
    );
    assert_eq!(manifest.author.as_deref(), Some("Ada"));
    assert_eq!(
        manifest.author_email.as_deref(),
        Some("ada@example.com"),
        "author.email preserved, not dropped by RawAuthor's reduction to a name"
    );
    assert_eq!(
        manifest.author_url.as_deref(),
        Some("https://ada.example.com"),
        "author.url preserved"
    );
    assert_eq!(
        manifest
            .metadata
            .as_ref()
            .and_then(|v| v.get("catalogId"))
            .and_then(|v| v.as_str()),
        Some("abc123"),
        "free-form metadata object preserved unread"
    );

    assert_eq!(manifest.channels.len(), 1);
    assert_eq!(
        manifest.channels[0].display_name.as_deref(),
        Some("Echo Channel"),
        "channels[].displayName preserved"
    );
}

/// Oracle `metadata` preprocess: `z.preprocess((e)=>isPlainObject(e)?e:void 0,
/// ...)` silently maps a non-object value to `undefined` rather than failing
/// the whole manifest parse. A JSON array is not a plain object.
#[tokio::test]
async fn non_object_metadata_is_dropped_not_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("arrmeta");
    fs::create_dir_all(dir.join(".lingxi-plugin")).unwrap();
    fs::write(
        dir.join(".lingxi-plugin").join("plugin.json"),
        r#"{"name":"arrmeta","metadata":["not","an","object"]}"#,
    )
    .unwrap();

    let discovered = plugin::discover_installed_plugins(tmp.path()).await;
    assert_eq!(
        discovered.len(),
        1,
        "a non-object metadata value must not sink the whole plugin"
    );
    assert!(
        discovered[0].1.metadata.is_none(),
        "a non-object metadata value is dropped, not preserved"
    );
}

#[tokio::test]
async fn missing_plugins_dir_yields_no_plugins() {
    let tmp = tempfile::tempdir().unwrap();
    let absent = tmp.path().join("does-not-exist");
    let discovered = plugin::discover_installed_plugins(&absent).await;
    assert!(discovered.is_empty());
}

#[tokio::test]
async fn a_dir_without_a_manifest_is_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    // A stray directory with no `.lingxi-plugin/plugin.json`.
    fs::create_dir_all(tmp.path().join("not-a-plugin").join("commands")).unwrap();
    let discovered = plugin::discover_installed_plugins(tmp.path()).await;
    assert!(
        discovered.is_empty(),
        "directories without a manifest are not plugins"
    );
}

// ── (M4 cc2.1.198) `--plugin-dir` inline discovery (`EBm` path arm port) ────

/// A plugin DIRECTORY passed via `--plugin-dir` loads like an installed one.
#[tokio::test]
async fn cli_plugin_dir_loads_a_directory() {
    let tmp = tempfile::tempdir().unwrap();
    write_fixture_plugin(tmp.path(), "myplugin");
    let discovered = plugin::discover_cli_plugin_dirs(&[tmp.path().join("myplugin")]).await;
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].1.name, "myplugin");
}

/// A missing path is a warn + skip (binary `Plugin path does not exist: …,
/// skipping`), never an error; other entries still load.
#[tokio::test]
async fn cli_plugin_dir_skips_missing_paths() {
    let tmp = tempfile::tempdir().unwrap();
    write_fixture_plugin(tmp.path(), "myplugin");
    let discovered = plugin::discover_cli_plugin_dirs(&[
        tmp.path().join("no-such-dir"),
        tmp.path().join("myplugin"),
    ])
    .await;
    assert_eq!(discovered.len(), 1, "missing path skipped, real one loads");
    assert_eq!(discovered[0].1.name, "myplugin");
}

/// A `.zip` passed via `--plugin-dir` is extracted (guarded) and loaded; a
/// single wrapper directory is unwrapped (`Yor` port).
#[tokio::test]
async fn cli_plugin_dir_loads_a_zip_with_wrapper_dir() {
    use std::io::Write;
    let tmp = tempfile::tempdir().unwrap();
    // Build myplugin.zip containing wrapper/<plugin tree>.
    write_fixture_plugin(tmp.path(), "wrapper");
    let mut buf = Vec::new();
    {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts = zip::write::SimpleFileOptions::default();
        for rel in [
            "wrapper/.lingxi-plugin/plugin.json",
            "wrapper/commands/hello.md",
            "wrapper/agents/helper.md",
            "wrapper/hooks/hooks.json",
        ] {
            w.start_file(rel, opts).unwrap();
            let on_disk = tmp.path().join(rel);
            w.write_all(&fs::read(on_disk).unwrap()).unwrap();
        }
        w.finish().unwrap();
    }
    let zip_path = tmp.path().join("myplugin.zip");
    fs::write(&zip_path, &buf).unwrap();

    let discovered = plugin::discover_cli_plugin_dirs(&[zip_path]).await;
    assert_eq!(
        discovered.len(),
        1,
        "zip extracts + wrapper unwraps + loads"
    );
    assert_eq!(discovered[0].1.name, "myplugin");
    // The returned dir is the UNWRAPPED plugin root (holds the manifest dir).
    assert!(discovered[0].2.join(".lingxi-plugin").is_dir());
}

/// Write a minimal named plugin at `root/<dir_name>` whose manifest `name` is
/// `plugin_name` (oracle 2.1.265 `--plugin-dir` folder-of-plugins).
fn write_named_fixture_plugin(root: &Path, dir_name: &str, plugin_name: &str) {
    let plugin_dir = root.join(dir_name);
    fs::create_dir_all(plugin_dir.join(".lingxi-plugin")).unwrap();
    fs::write(
        plugin_dir.join(".lingxi-plugin").join("plugin.json"),
        format!(r#"{{"name":"{plugin_name}","version":"1.0.0"}}"#),
    )
    .unwrap();
}

/// 2.1.265: `--plugin-dir` pointing at a folder of plugins loads each child
/// that has a readable `.lingxi-plugin/plugin.json` (oracle `X1s`/`KMe`).
#[tokio::test]
async fn cli_plugin_dir_loads_each_child_of_a_folder_of_plugins() {
    let tmp = tempfile::tempdir().unwrap();
    let collection = tmp.path().join("plugins");
    write_named_fixture_plugin(&collection, "beta", "beta-plugin");
    write_named_fixture_plugin(&collection, "alpha", "alpha-plugin");
    fs::create_dir_all(collection.join("empty-child")).unwrap();
    fs::create_dir_all(collection.join(".hidden")).unwrap();
    write_named_fixture_plugin(&collection.join(".hidden"), "nope", "hidden-plugin");

    let discovered = plugin::discover_cli_plugin_dirs(&[collection]).await;
    let mut names: Vec<_> = discovered.iter().map(|d| d.1.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["alpha-plugin", "beta-plugin"],
        "each manifest child loads; skipped/hidden children do not"
    );
}

/// A directory that is itself a plugin (has `.lingxi-plugin/plugin.json`) is
/// NOT expanded as a collection, even when it also has component subdirs.
#[tokio::test]
async fn cli_plugin_dir_does_not_expand_a_plugin_into_component_children() {
    let tmp = tempfile::tempdir().unwrap();
    write_fixture_plugin(tmp.path(), "myplugin");
    let discovered = plugin::discover_cli_plugin_dirs(&[tmp.path().join("myplugin")]).await;
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].1.name, "myplugin");
}

/// 2.1.265 watch snapshot: re-classifying a folder of plugins picks up a
/// child added after the first load (oracle `watchCollections` / `wAr`).
#[tokio::test]
async fn cli_plugin_dir_collection_children_picks_up_a_new_child() {
    let tmp = tempfile::tempdir().unwrap();
    let collection = tmp.path().join("plugins");
    write_named_fixture_plugin(&collection, "alpha", "alpha-plugin");

    let first = plugin::cli_plugin_dir_collection_children(&collection)
        .await
        .expect("folder of plugins");
    assert_eq!(first, ["alpha"]);

    write_named_fixture_plugin(&collection, "beta", "beta-plugin");
    let second = plugin::cli_plugin_dir_collection_children(&collection)
        .await
        .expect("folder of plugins");
    assert_eq!(second, ["alpha", "beta"]);
}
