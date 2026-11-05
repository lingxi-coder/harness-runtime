//! Private pages on this computer, shared by the `Page` tool and the serve host.
//!
//! Hosts opt in with their `private_pages` config flag once they can open the
//! page. CLI and headless leave it off, so the `Page` tool is absent there.
//! The `artifact-design` and `plan-artifact` skills stay registered either way.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use lingxi_core::host::FileSystem;
use private_page::PageStore;

/// The one store per config home in this process, so the tool and the serve
/// host share the store's in-process writer serialization.
pub fn shared_store(fs: Arc<dyn FileSystem>, config_home: &Path) -> Arc<PageStore> {
    static STORES: OnceLock<Mutex<HashMap<PathBuf, Arc<PageStore>>>> = OnceLock::new();
    let mut stores = STORES
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    stores
        .entry(config_home.to_path_buf())
        .or_insert_with(|| Arc::new(PageStore::new(fs, config_home.to_path_buf())))
        .clone()
}

/// Register the `Page` tool over `store`.
pub fn register_tool(
    registry: &mut tool_api::ToolRegistry,
    ctx: tool_api::BuiltinToolContext,
    store: Arc<PageStore>,
) {
    registry.register_builtin(Arc::new(tool_ui::PageTool::new(ctx, store)));
}
