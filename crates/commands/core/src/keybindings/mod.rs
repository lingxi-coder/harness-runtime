//! `/keybindings` — write the keybindings template (exclusive-create) and open
//! it in `$EDITOR`.
//!
//! Port of the claude-code `type: 'local'` command
//! `src/commands/keybindings/keybindings.ts` (the async `call()`):
//!
//! 1. If keybinding customization is **not** enabled, return the locked preview
//!    string and do nothing else.
//! 2. Resolve the keybindings path (`$LINGXI_CONFIG_DIR ?? ~/.claude` joined
//!    with `keybindings.json`, mirroring `getKeybindingsPath()` →
//!    `getClaudeConfigHomeDir()`).
//! 3. `mkdir -p` the parent, then write the template with the `wx` flag
//!    (exclusive create). An `EEXIST` (file already present) is swallowed and
//!    flips `file_exists` to `true`; any other write error propagates.
//! 4. Open the file in the editor (`editFileInEditor`). The four return strings
//!    are byte-faithful with the TS:
//!    - editor error, pre-existing file:
//!      `"Opened {path}. Could not open in editor: {error}"`
//!    - editor error, freshly created:
//!      `"Created {path}. Could not open in editor: {error}"`
//!    - editor ok, pre-existing file: `"Opened {path} in your editor."`
//!    - editor ok, freshly created:
//!      `"Created {path} with template. Opened in your editor."`
//!
//! ## Faithfulness notes / accepted divergences
//!
//! - **Template content** is byte-identical to the TS
//!   `generateKeybindingsTemplate()` for the canonical build: all five
//!   feature flags (`KAIROS`/`KAIROS_BRIEF`, `QUICK_SEARCH`, `TERMINAL_PANEL`,
//!   `MESSAGE_ACTIONS`, `VOICE_MODE`) **off** and the non-Windows platform
//!   branch (`ctrl+v` image-paste, `shift+tab` mode-cycle). The reserved
//!   shortcuts that cannot be rebound (`ctrl+c`, `ctrl+d`, `ctrl+m`) are
//!   pre-filtered out exactly as `filterReservedShortcuts` does, so the
//!   embedded [`KEYBINDINGS_TEMPLATE`] is the verbatim
//!   `JSON.stringify(config, null, 2) + '\n'` output. It is embedded as a data
//!   file rather than re-serialized so key insertion order is preserved without
//!   depending on a `serde_json` ordered-map feature.
//! - **Enablement gate.** claude-code gates this behind the
//!   `tengu_keybinding_customization_release` `GrowthBook` flag (off for external
//!   users → the preview string). There is no `GrowthBook` in this crate, so the
//!   gate is an injected boolean defaulting to `false` (the external-user
//!   default — the preview branch). Tests inject `true` to exercise the
//!   create/open branches.
//! - **Editor spawn** is environment-dependent (the TS `editFileInEditor`), so
//!   it is an injected closure. The default resolves `$EDITOR` → `$VISUAL` →
//!   platform default and spawns it on the path; tests stub it to a no-op
//!   success (or a forced error to exercise the "Could not open in editor"
//!   branches).

use async_trait::async_trait;
use command_api::model::{BuiltinCommandHandler, CommandResult};
use command_api::parser::ParsedSlashCommand;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ── Runtime keybindings machinery (GAP D) ────────────────────────────────────
//
// `keybindings.rs` historically held ONLY the `/keybindings` command (write the
// template, open `$EDITOR`). GAP D adds the loader/parser/resolver/runtime-keymap
// that actually consults `~/.lingxi/keybindings.json`, a 1:1 port of
// `claude-code/src/keybindings/{schema,parser,match,resolver,reservedShortcuts,
// validate,defaultBindings,loadUserBindings}.ts`. The `/keybindings` handler
// below is unchanged.
pub mod default_bindings;
pub mod keymap;
pub mod loader;
pub mod matcher;
pub mod parser;
pub mod reserved;
pub mod resolver;
pub mod schema;
pub mod types;
pub mod validate;

pub use keymap::{InputKey, Keymap, Resolution};
pub use loader::{load_keybindings, KeybindingsLoadResult};
pub use resolver::get_binding_display_text;
pub use types::{Chord, KeybindingBlock, ParsedBinding, ParsedKeystroke};
pub use validate::{KeybindingWarning, KeybindingWarningType};

/// The byte-faithful keybindings template, identical to the TS
/// `generateKeybindingsTemplate()` output (features off, non-Windows platform,
/// reserved shortcuts filtered). Embedded verbatim to preserve key order.
pub const KEYBINDINGS_TEMPLATE: &str = include_str!("keybindings_template.json");

/// The locked preview string returned when keybinding customization is disabled
/// (1:1 with the TS `keybindings.ts` early-return branch).
pub const KEYBINDINGS_PREVIEW_DISABLED: &str =
    "Keybinding customization is not enabled. This feature is currently in preview.";

/// Injected editor-spawn seam. Returns `Ok(())` on success, or `Err(message)`
/// where `message` is surfaced verbatim in the "Could not open in editor:
/// {error}" branches (mirrors the TS `EditorResult.error`).
pub type EditorSpawner = Arc<dyn Fn(&Path) -> Result<(), String> + Send + Sync>;

/// Resolve the keybindings file path, mirroring the TS `getKeybindingsPath()` →
/// `join(getClaudeConfigHomeDir(), 'keybindings.json')` where
/// `getClaudeConfigHomeDir()` is `$LINGXI_CONFIG_DIR ?? join(homedir(),
/// '.claude')`.
#[must_use]
pub fn keybindings_path() -> PathBuf {
    resolve_keybindings_path(
        std::env::var_os(branding::CONFIG_DIR_ENV),
        std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")),
    )
}

/// Pure path resolver (extracted for testing without mutating process env):
/// `$LINGXI_CONFIG_DIR ?? join(home, '.claude')` then join `keybindings.json`.
/// Matches claude-code `tr()` `??`: a SET `$LINGXI_CONFIG_DIR` is honored
/// verbatim — including an empty value (which then resolves cwd-relative) —
/// and only an UNSET var falls back to `<home>/.claude`. A missing home falls
/// back to the current directory so the path is always well-formed.
fn resolve_keybindings_path(
    config_dir_env: Option<std::ffi::OsString>,
    home_env: Option<std::ffi::OsString>,
) -> PathBuf {
    let base = match config_dir_env {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = home_env.map_or_else(|| PathBuf::from("."), PathBuf::from);
            home.join(branding::DOT_DIR)
        }
    };
    base.join("keybindings.json")
}

/// Default real-editor spawner: resolve `$EDITOR` → `$VISUAL` → platform
/// default and run it on `path`, blocking until it exits. A non-zero exit or a
/// spawn failure becomes the `Err(message)` surfaced in the editor-error
/// branches.
fn spawn_real_editor(path: &Path) -> Result<(), String> {
    let editor = resolve_editor();
    let status = std::process::Command::new(&editor)
        .arg(path)
        .status()
        .map_err(|e| format!("{editor}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        match status.code() {
            Some(code) => Err(format!("{editor} exited with code {code}")),
            None => Err(format!("{editor} terminated by signal")),
        }
    }
}

#[cfg(unix)]
fn platform_default_editor() -> String {
    "vi".to_string()
}
#[cfg(windows)]
fn platform_default_editor() -> String {
    "notepad.exe".to_string()
}
#[cfg(not(any(unix, windows)))]
fn platform_default_editor() -> String {
    "vi".to_string()
}

/// `$EDITOR` → `$VISUAL` → platform default. Empty env values are treated as
/// missing (matches the orchestrator's `resolve_editor`).
fn resolve_editor() -> String {
    if let Some(v) = std::env::var_os("EDITOR") {
        if !v.is_empty() {
            return v.to_string_lossy().into_owned();
        }
    }
    if let Some(v) = std::env::var_os("VISUAL") {
        if !v.is_empty() {
            return v.to_string_lossy().into_owned();
        }
    }
    platform_default_editor()
}

/// `/keybindings` handler — writes the template (exclusive create) and opens it
/// in the editor.
///
/// Handle-free (the TS command goes through no orchestrator handle): it does the
/// filesystem work and editor spawn directly, like [`crate::ExportHandler`].
/// The enablement gate and editor spawn are injected so the create / open /
/// editor-error / preview branches are all unit-testable.
#[derive(Clone)]
pub struct KeybindingsHandler {
    /// Whether keybinding customization is enabled. `false` → preview branch.
    enabled: bool,
    /// Editor-spawn seam (default = real `$EDITOR` spawn).
    spawn_editor: EditorSpawner,
}

impl std::fmt::Debug for KeybindingsHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeybindingsHandler")
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}

impl Default for KeybindingsHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl KeybindingsHandler {
    /// Construct a `KeybindingsHandler` with the real-editor spawner.
    ///
    /// Enabled by default: claude-code 2.1.205 gates the command on
    /// `isEnabled: () => ufe()` (a GrowthBook flag) and, when disabled, HIDES
    /// the command entirely — the older "…is not enabled. This feature is
    /// currently in preview." string is gone from the 2.1.205 binary. LingXi
    /// advertises `/keybindings` (it's a purely-local editor-open with no
    /// backend), so the parity-correct behavior is for it to actually work
    /// (open/create the file) rather than render the phantom preview string.
    #[must_use]
    pub fn new() -> Self {
        Self {
            enabled: true,
            spawn_editor: Arc::new(spawn_real_editor),
        }
    }

    /// Construct with an explicit enablement gate, keeping the real-editor
    /// spawner. Used by composition roots that resolve the customization flag at
    /// boot.
    #[must_use]
    pub fn with_enabled(enabled: bool) -> Self {
        Self {
            enabled,
            spawn_editor: Arc::new(spawn_real_editor),
        }
    }

    /// Construct with an explicit gate **and** a custom editor spawner. Used by
    /// tests to stub the editor to a no-op success (or a forced error).
    #[must_use]
    pub fn with_config(enabled: bool, spawn_editor: EditorSpawner) -> Self {
        Self {
            enabled,
            spawn_editor,
        }
    }

    /// Core logic against an explicit target path. Returns the byte-faithful
    /// display string. Separated from [`Self::handle`] so tests can drive it
    /// against a temp path without mutating process-global env.
    fn run_at(&self, path: &Path) -> String {
        if !self.enabled {
            return KEYBINDINGS_PREVIEW_DISABLED.to_string();
        }

        // mkdir -p parent, then exclusive-create write (the TS `wx` flag).
        // An already-present file flips `file_exists` and is otherwise a no-op;
        // any other write error propagates (mirrors the TS `throw e`).
        let mut file_exists = false;
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                // The TS `mkdir` rejection would propagate out of `call()`; we
                // surface it through the same editor-less failure shape so the
                // user still sees the cause rather than a panic.
                return format!("Could not create {}: {e}", path.display());
            }
        }
        match write_exclusive(path, KEYBINDINGS_TEMPLATE) {
            Ok(()) => {}
            Err(WriteError::AlreadyExists) => file_exists = true,
            Err(WriteError::Other(e)) => {
                return format!("Could not write {}: {e}", path.display());
            }
        }

        let verb = if file_exists { "Opened" } else { "Created" };
        match (self.spawn_editor)(path) {
            Err(error) => format!(
                "{verb} {}. Could not open in editor: {error}",
                path.display()
            ),
            Ok(()) => {
                if file_exists {
                    format!("Opened {} in your editor.", path.display())
                } else {
                    format!(
                        "Created {} with template. Opened in your editor.",
                        path.display()
                    )
                }
            }
        }
    }
}

/// Result of the exclusive-create write: distinguishes the swallowed
/// already-exists case (TS `EEXIST`) from any other I/O error (TS `throw e`).
enum WriteError {
    /// The target already existed (`O_EXCL` / `wx` rejection).
    AlreadyExists,
    /// Any other write failure.
    Other(std::io::Error),
}

/// Write `content` to `path` with exclusive-create semantics (the TS `wx`
/// flag): fail with [`WriteError::AlreadyExists`] if the file already exists,
/// avoiding a TOCTOU stat pre-check.
fn write_exclusive(path: &Path, content: &str) -> Result<(), WriteError> {
    use std::io::Write;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut f) => f.write_all(content.as_bytes()).map_err(WriteError::Other),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(WriteError::AlreadyExists),
        Err(e) => Err(WriteError::Other(e)),
    }
}

#[async_trait]
impl BuiltinCommandHandler for KeybindingsHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        let path = keybindings_path();
        CommandResult::Done {
            display: Some(self.run_at(&path)),
        }
    }

    fn name(&self) -> &str {
        "keybindings"
    }

    fn description(&self) -> &str {
        // 1:1 with the oracle command object
        // (`c_T={name:"keybindings",description:"Open your keyboard shortcuts
        // file",…}` @ cc2.1.238 295148735 — byte-identical in 2.1.220). Kept as
        // a literal rather than routed through `core_description` so the
        // handler stays self-describing for this non-core name.
        "Open your keyboard shortcuts file"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn args() -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "keybindings".to_string(),
            raw_args: String::new(),
            positional_args: vec![],
        }
    }

    fn unique_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        std::env::temp_dir().join(format!(
            "lingxi-keybindings-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    /// no-op success editor spawner that records how many times it ran.
    fn counting_ok_spawner(counter: Arc<AtomicUsize>) -> EditorSpawner {
        Arc::new(move |_path: &Path| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    #[test]
    fn template_is_byte_faithful_to_ts_output() {
        // Anchors: object wrapper, schema + docs keys, and the filtered
        // reserved shortcuts must be absent. The whole 5684-byte body is the
        // verbatim TS `JSON.stringify(config, null, 2) + '\n'`.
        assert!(KEYBINDINGS_TEMPLATE.starts_with("{\n"));
        assert!(KEYBINDINGS_TEMPLATE.ends_with("}\n"));
        assert!(KEYBINDINGS_TEMPLATE
            .contains("\"$schema\": \"https://www.schemastore.org/claude-code-keybindings.json\""));
        assert!(KEYBINDINGS_TEMPLATE
            .contains("\"$docs\": \"https://code.claude.com/docs/en/keybindings\""));
        assert!(KEYBINDINGS_TEMPLATE.contains("\"context\": \"Global\""));
        // Reserved (non-rebindable) shortcuts are filtered out of the template.
        assert!(
            !KEYBINDINGS_TEMPLATE.contains("app:interrupt"),
            "ctrl+c not filtered"
        );
        assert!(
            !KEYBINDINGS_TEMPLATE.contains("app:exit"),
            "ctrl+d not filtered"
        );
        assert!(
            !KEYBINDINGS_TEMPLATE.contains("permission:toggleDebug"),
            "ctrl+d not filtered"
        );
        // Non-Windows / features-off canonical keys present.
        assert!(KEYBINDINGS_TEMPLATE.contains("\"ctrl+v\": \"chat:imagePaste\""));
        assert!(KEYBINDINGS_TEMPLATE.contains("\"shift+tab\": \"chat:cycleMode\""));
        // The template must be valid JSON.
        let parsed: serde_json::Value = serde_json::from_str(KEYBINDINGS_TEMPLATE).unwrap();
        assert!(parsed.get("bindings").and_then(|b| b.as_array()).is_some());
    }

    #[test]
    fn disabled_gate_returns_preview_string_and_writes_nothing() {
        let dir = unique_dir("preview");
        let path = dir.join("keybindings.json");
        let counter = Arc::new(AtomicUsize::new(0));
        let h = KeybindingsHandler::with_config(false, counting_ok_spawner(counter.clone()));
        let out = h.run_at(&path);
        assert_eq!(out, KEYBINDINGS_PREVIEW_DISABLED);
        // No file written, editor never spawned.
        assert!(!path.exists(), "preview branch must not create the file");
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn first_call_creates_file_with_template_and_opens_editor() {
        let dir = unique_dir("create");
        let path = dir.join("keybindings.json");
        let counter = Arc::new(AtomicUsize::new(0));
        let h = KeybindingsHandler::with_config(true, counting_ok_spawner(counter.clone()));

        let out = h.run_at(&path);
        assert_eq!(
            out,
            format!(
                "Created {} with template. Opened in your editor.",
                path.display()
            )
        );
        // The file was created with the exact template bytes.
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written, KEYBINDINGS_TEMPLATE);
        // The editor-open effect fired exactly once.
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        // Second call hits EEXIST → "Opened ... in your editor.".
        let out2 = h.run_at(&path);
        assert_eq!(out2, format!("Opened {} in your editor.", path.display()));
        // File content unchanged by the exclusive-create no-op.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            KEYBINDINGS_TEMPLATE
        );
        assert_eq!(counter.load(Ordering::SeqCst), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn editor_error_on_created_file_uses_created_prefix() {
        let dir = unique_dir("err-created");
        let path = dir.join("keybindings.json");
        let spawn: EditorSpawner = Arc::new(|_p: &Path| Err("vi exited with code 1".to_string()));
        let h = KeybindingsHandler::with_config(true, spawn);

        let out = h.run_at(&path);
        assert_eq!(
            out,
            format!(
                "Created {}. Could not open in editor: vi exited with code 1",
                path.display()
            )
        );
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn editor_error_on_existing_file_uses_opened_prefix() {
        let dir = unique_dir("err-opened");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("keybindings.json");
        // Pre-create the file so the write hits EEXIST.
        std::fs::write(&path, "preexisting").unwrap();
        let spawn: EditorSpawner = Arc::new(|_p: &Path| Err("no editor".to_string()));
        let h = KeybindingsHandler::with_config(true, spawn);

        let out = h.run_at(&path);
        assert_eq!(
            out,
            format!(
                "Opened {}. Could not open in editor: no editor",
                path.display()
            )
        );
        // Exclusive-create did not clobber the pre-existing content.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "preexisting");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn disabled_gate_still_returns_the_preview_branch() {
        // The gate itself is unchanged: an explicitly-disabled handler still
        // takes the preview branch (kept for callers that resolve a flag).
        // The DEFAULT, however, is now enabled (2.1.205 parity) — asserted in
        // `handle_default_is_enabled_and_opens_the_file`.
        let h = KeybindingsHandler::with_enabled(false);
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s, KEYBINDINGS_PREVIEW_DISABLED);
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    /// The `new()` default is enabled (claude-code 2.1.205 removed the
    /// customization gate's preview string): running `/keybindings` opens or
    /// creates the file rather than rendering the phantom preview.
    #[tokio::test]
    async fn handle_default_is_enabled_and_opens_the_file() {
        let opened = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = opened.clone();
        let h = KeybindingsHandler::with_config(
            true,
            std::sync::Arc::new(move |_p: &std::path::Path| {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }),
        );
        // The real `new()` default is enabled…
        assert!(KeybindingsHandler::new().enabled);
        // …and an enabled handler opens the file, never the preview string.
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => {
                assert_ne!(s, KEYBINDINGS_PREVIEW_DISABLED);
                assert!(opened.load(std::sync::atomic::Ordering::SeqCst));
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn path_honors_config_dir_env() {
        // LINGXI_CONFIG_DIR set → used verbatim as the base.
        let p = resolve_keybindings_path(
            Some(std::ffi::OsString::from("/custom/cfg")),
            Some(std::ffi::OsString::from("/home/u")),
        );
        assert_eq!(p, PathBuf::from("/custom/cfg/keybindings.json"));
    }

    #[test]
    fn path_falls_back_to_home_dot_claude() {
        // No LINGXI_CONFIG_DIR → join(home, '.claude', 'keybindings.json').
        let p = resolve_keybindings_path(None, Some(std::ffi::OsString::from("/home/u")));
        assert_eq!(p, PathBuf::from("/home/u/.lingxi/keybindings.json"));
    }

    #[test]
    fn path_empty_config_dir_is_honored_verbatim() {
        // A set-but-EMPTY LINGXI_CONFIG_DIR is honored verbatim (claude-code
        // `??`), resolving cwd-relative — NOT treated as unset.
        let p = resolve_keybindings_path(
            Some(std::ffi::OsString::new()),
            Some(std::ffi::OsString::from("/home/u")),
        );
        assert_eq!(p, PathBuf::from("keybindings.json"));
    }

    #[test]
    fn name_and_description() {
        let h = KeybindingsHandler::new();
        assert_eq!(h.name(), "keybindings");
        assert_eq!(h.description(), "Open your keyboard shortcuts file");
    }
}
