//! Existing Local Apps bundle compiler, enabled only for the mobile profile.

#[cfg(feature = "mobile")]
mod mobile {

    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn required_string<'a>(value: &'a serde_json::Value, key: &str) -> &'a str {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("builtin Plugin manifest requires string field {key:?}"))
    }

    fn read_inventory(path: &Path) -> Vec<local_apps::InventoryEntry> {
        let raw = fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        let entries: Vec<_> = raw
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(local_apps::InventoryEntry::new)
            .collect();
        assert!(
            !entries.is_empty(),
            "{} must declare at least one builtin plugin file",
            path.display()
        );
        entries
    }

    pub fn build() {
        // Where the plugin tree is comes from the crate that ships it, not from a path out of this one.
        let inventory_path = local_app_builder_plugin::inventory_path().to_path_buf();
        let plugin_root = local_app_builder_plugin::root().to_path_buf();

        // Watching the directory as well as the explicit descriptor is load-bearing:
        // a newly-added, undeclared file must rerun this build script so pack() can
        // reject it as FileOutsideInventory instead of leaving a stale artifact.
        println!("cargo:rerun-if-changed={}", inventory_path.display());
        println!("cargo:rerun-if-changed={}", plugin_root.display());

        let inventory = read_inventory(&inventory_path);
        let manifest_path = plugin_root.join(".lingxi-plugin/plugin.json");
        let manifest_bytes = fs::read(&manifest_path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", manifest_path.display()));
        let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)
            .unwrap_or_else(|error| panic!("invalid builtin Plugin manifest: {error}"));
        let author = manifest
            .get("author")
            .and_then(|value| value.get("name"))
            .and_then(serde_json::Value::as_str)
            .expect("builtin Plugin manifest requires author.name");
        let default_enabled = manifest
            .get("defaultEnabled")
            .and_then(serde_json::Value::as_bool)
            .expect("builtin Plugin manifest requires boolean defaultEnabled");
        let lsp_servers = manifest
            .get("lspServers")
            .cloned()
            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
        let lsp_server_records = lsp_servers
            .as_object()
            .expect("builtin Plugin manifest lspServers must be an inline object");
        for (name, value) in lsp_server_records {
            let record = value
                .as_object()
                .unwrap_or_else(|| panic!("builtin Plugin LSP server {name:?} must be an object"));
            assert!(
                record
                    .get("command")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|command| !command.trim().is_empty()),
                "builtin Plugin LSP server {name:?} requires a non-empty command"
            );
            assert!(
                record
                    .get("extensionToLanguage")
                    .and_then(serde_json::Value::as_object)
                    .is_some_and(|extensions| !extensions.is_empty()),
                "builtin Plugin LSP server {name:?} requires extensionToLanguage"
            );
        }
        let lsp_servers_json = serde_json::to_string(&lsp_servers)
            .expect("serialize builtin Plugin LSP server declarations");
        for (field, expected) in [
            ("name", "lingxi-local-app"),
            ("skills", "./skills/"),
            ("agents", "./agents/"),
            ("workflows", "./workflows/"),
        ] {
            assert_eq!(
                required_string(&manifest, field),
                expected,
                "builtin Plugin manifest field {field:?} must keep its production value"
            );
        }
        let packed = local_apps::pack(&plugin_root, &inventory).unwrap_or_else(|error| {
            panic!(
                "builtin Local App plugin inventory does not match {}: {error}",
                plugin_root.display()
            )
        });
        let declared_paths: std::collections::BTreeSet<_> =
            inventory.iter().map(|entry| entry.path.as_str()).collect();
        let packed_paths: std::collections::BTreeSet<_> = packed
            .inventory
            .iter()
            .map(|entry| entry.path.as_str())
            .collect();
        assert_eq!(
            declared_paths, packed_paths,
            "builtin Plugin inventory contains a path pruned or omitted by the packer"
        );
        let thresholds = local_apps::load_baseline().expect("valid performance thresholds");
        assert!(
            packed.archive.len() as u64 <= thresholds.builtin_archive_max_bytes,
            "builtin Plugin archive exceeds the checked-in byte budget"
        );
        let extracted_bytes: u64 = packed.inventory.iter().map(|entry| entry.bytes).sum();
        assert!(
            extracted_bytes <= thresholds.builtin_extracted_max_bytes,
            "builtin Plugin extracted files exceed the checked-in byte budget"
        );

        let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
        fs::write(out_dir.join("lingxi-local-app.bundle"), &packed.archive)
            .expect("write compiled builtin plugin archive");
        let catalog_rel = "assets/templates/catalog.json";
        assert!(
            packed
                .inventory
                .iter()
                .any(|entry| entry.path == catalog_rel),
            "builtin Plugin inventory must contain {catalog_rel}"
        );
        let catalog_bytes = fs::read(plugin_root.join(catalog_rel))
            .expect("read compiled builtin Local App catalog");
        fs::write(
            out_dir.join("lingxi-local-app-catalog.json"),
            &catalog_bytes,
        )
        .expect("write compiled builtin Local App catalog");

        let mut descriptor = format!(
        "pub(crate) const COMPILED_PLUGIN_NAME: &str = {:?};\n\
         pub(crate) const COMPILED_PLUGIN_DISPLAY_NAME: &str = {:?};\n\
         pub(crate) const COMPILED_PLUGIN_VERSION: &str = {:?};\n\
         pub(crate) const COMPILED_PLUGIN_DESCRIPTION: &str = {:?};\n\
         pub(crate) const COMPILED_PLUGIN_AUTHOR: &str = {:?};\n\
         pub(crate) const COMPILED_PLUGIN_DEFAULT_ENABLED: bool = {:?};\n\
         pub(crate) const COMPILED_PLUGIN_LSP_SERVERS_JSON: &str = {:?};\n\
         pub(crate) const COMPILED_PLUGIN_ARCHIVE_DIGEST: &str = {:?};\n\
         pub(crate) const COMPILED_PLUGIN_CATALOG_BYTES: &[u8] = include_bytes!(concat!(env!(\"OUT_DIR\"), \"/lingxi-local-app-catalog.json\"));\n\
         pub(crate) const COMPILED_PLUGIN_INVENTORY: &[(&str, u64, &str)] = &[\n",
        required_string(&manifest, "name"),
        required_string(&manifest, "displayName"),
        required_string(&manifest, "version"),
        required_string(&manifest, "description"),
        author,
        default_enabled,
        lsp_servers_json,
        packed.archive_digest
    );
        for entry in &packed.inventory {
            descriptor.push_str(&format!(
                "    ({:?}, {}, {:?}),\n",
                entry.path, entry.bytes, entry.sha256
            ));
        }
        descriptor.push_str("];\n");
        fs::write(out_dir.join("lingxi-local-app-descriptor.rs"), descriptor)
            .expect("write compiled builtin plugin descriptor");
    }
}

fn main() {
    #[cfg(feature = "mobile")]
    mobile::build();
}
