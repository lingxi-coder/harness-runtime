//! Shared headless tool boundary for native file-memory extraction and dreaming.

use async_trait::async_trait;
use lingxi_core::host::tool_invoker::{
    SubagentInvocationContext, ToolInvocationContextState, ToolInvocationResult, ToolInvoker,
    ToolInvokerError,
};
use lingxi_core::types::{utf16_json::Utf16JsonProjection, AgentId, SessionId};
use std::any::Any;
use std::path::{Component, Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};

fn poisoned<T>(error: std::sync::PoisonError<T>) -> T {
    error.into_inner()
}

/// The shell capability actually advertised by the parent host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryAgentShell {
    Bash,
    PowerShell,
    Mobile,
}

impl Default for MemoryAgentShell {
    fn default() -> Self {
        if cfg!(target_os = "windows") {
            Self::PowerShell
        } else {
            Self::Bash
        }
    }
}

impl MemoryAgentShell {
    /// Prefer Bash on Windows when the parent advertises it, as Native does.
    #[must_use]
    pub fn from_tools(tools: &[lingxi_core::host::SourceToolWireDefinition]) -> Self {
        [Self::Bash, Self::PowerShell, Self::Mobile]
            .into_iter()
            .find(|shell| tools.iter().any(|tool| tool.name == shell.name()))
            .unwrap_or_default()
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Bash => "Bash",
            Self::PowerShell => "PowerShell",
            Self::Mobile => "Shell",
        }
    }

    #[must_use]
    pub fn is_posix(self) -> bool {
        self != Self::PowerShell
    }
}

use lingxi_core::host::memory_tool_scope::{
    memory_protected_component as protected_component, MemoryMutationRoot,
};

fn absolute_normalized(path: &Path, cwd: Option<&Path>) -> Option<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd?.join(path)
    };
    let mut output = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if !output.pop() {
                    return None;
                }
            }
            part => output.push(part.as_os_str()),
        }
    }
    output.is_absolute().then_some(output)
}

fn canonicalize_missing_leaf(path: &Path) -> Option<PathBuf> {
    let mut ancestor = path;
    let mut missing = Vec::new();
    loop {
        match std::fs::symlink_metadata(ancestor) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(ancestor.file_name()?.to_owned());
                ancestor = ancestor.parent()?;
            }
            Err(_) => return None,
        }
    }
    let mut resolved = std::fs::canonicalize(ancestor).ok()?;
    for part in missing.into_iter().rev() {
        resolved.push(part);
    }
    Some(resolved)
}

/// Resolve one `.md` target beneath the active memory directory, rejecting
/// protected components, alternate streams, and symlinks escaping that root.
#[must_use]
pub fn resolve_memory_markdown_path(root: &Path, raw: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    if !raw.ends_with(".md") || raw.contains('\0') {
        return None;
    }
    let root = std::fs::canonicalize(root).ok()?;
    let candidate = absolute_normalized(Path::new(raw), cwd)?;
    // File tools reject a leaf symlink before writing. Replacing it with its
    // canonical target here must not discard that independent safety check.
    if std::fs::symlink_metadata(&candidate).is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return None;
    }
    let resolved = canonicalize_missing_leaf(&candidate)?;
    // The executed target must satisfy the same restriction as the spelling
    // supplied by the model. Never rewrite authority through a lossy path.
    if !resolved.to_str()?.ends_with(".md") {
        return None;
    }
    let tail = resolved.strip_prefix(&root).ok()?;
    if tail.as_os_str().is_empty()
        || tail.components().any(|part| {
            protected_component(part.as_os_str())
                || part.as_os_str().to_string_lossy().contains(':')
        })
    {
        return None;
    }
    // Do not let an in-root symlink name bypass the protected-path check.
    if let Ok(raw_tail) = candidate.strip_prefix(&root) {
        if raw_tail
            .components()
            .any(|part| protected_component(part.as_os_str()))
        {
            return None;
        }
    }
    #[cfg(unix)]
    if let Ok(metadata) = std::fs::metadata(&resolved) {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 || !metadata.is_file() {
            return None;
        }
    }
    Some(resolved)
}

/// A deliberately literal single-command tokenizer. It admits quoted paths
/// but rejects expansions, redirections, command lists and substitutions.
fn literal_argv(command: &str, powershell: bool) -> Option<Vec<String>> {
    let mut args = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut started = false;
    let mut characters = command.chars().peekable();
    while let Some(ch) = characters.next() {
        if !powershell && quote != Some('\'') && ch == '\\' {
            let escaped = characters.next()?;
            // Bash removes a backslash-newline pair in either unquoted or
            // double-quoted text. Inside double quotes, other backslashes are
            // special only before $, `, ", and \; before an ordinary letter
            // the backslash is part of the argument that actually executes.
            if escaped == '\n' {
                continue;
            }
            if quote == Some('"') && !matches!(escaped, '$' | '`' | '"' | '\\') {
                word.push('\\');
            }
            word.push(escaped);
            started = true;
            continue;
        }
        if let Some(open) = quote {
            if ch == open {
                if powershell && characters.peek() == Some(&open) {
                    characters.next();
                    word.push(open);
                } else {
                    quote = None;
                }
            } else {
                if open == '"' && matches!(ch, '$' | '`') {
                    return None;
                }
                word.push(ch);
            }
            started = true;
            continue;
        }
        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                started = true;
            }
            '$' | '`' | ';' | '|' | '&' | '<' | '>' | '(' | ')' | '{' | '}' | '\n' | '\r'
            | '\0' => return None,
            ' ' | '\t' => {
                if started {
                    args.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            ch if powershell && ch.is_whitespace() => return None,
            ch => {
                word.push(ch);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if started {
        args.push(word);
    }
    (!args.is_empty()).then_some(args)
}

/// Admit a small, literal PowerShell reader grammar. Reuse the permission
/// crate's cmdlet identities and per-reader parameter tables, but do not use
/// its parser-unavailable passthrough or its wider accept-edits classifier.
fn powershell_command_is_read_only(command: &str) -> bool {
    use permission::powershell_containment::{is_parameter, normalize_cmdlet, PsOperation, FKN};

    // These constructs can evaluate code, feed another command, capture output
    // into state, or change the token grammar. A quoted literal containing one
    // is conservatively rejected as well; no general PowerShell evaluation is
    // needed to permit an ordinary memory/transcript reader.
    if command.contains([
        '$', '`', '|', ';', '&', '<', '>', '(', ')', '{', '}', '@', '#', ',', '\n', '\r', '\0',
    ]) {
        return false;
    }
    let Some(args) = literal_argv(command, true) else {
        return false;
    };
    let name = &args[0];
    // normalize_cmdlet also strips executable extensions. Do not let that
    // convenience mistake an arbitrary Get-Content.exe for the built-in cmdlet.
    if !name.chars().all(|ch| ch.is_ascii_alphabetic() || ch == '-') {
        return false;
    }
    let canonical = normalize_cmdlet(name);
    if canonical == "get-location" {
        return args.len() == 1;
    }
    if matches!(canonical.as_str(), "select-object" | "measure-object") {
        let mut arguments = args[1..].iter();
        while let Some(argument) = arguments.next() {
            let parameter = argument.to_ascii_lowercase();
            match (canonical.as_str(), parameter.as_str()) {
                ("select-object", "-unique")
                | ("measure-object", "-line" | "-word" | "-character") => {}
                ("select-object", "-first" | "-last" | "-skip" | "-skiplast") => {
                    if !arguments
                        .next()
                        .is_some_and(|value| value.parse::<u64>().is_ok())
                    {
                        return false;
                    }
                }
                _ => return false,
            }
        }
        return true;
    }
    if !matches!(
        canonical.as_str(),
        "get-content"
            | "get-childitem"
            | "get-item"
            | "get-itemproperty"
            | "get-itempropertyvalue"
            | "get-filehash"
            | "get-acl"
            | "test-path"
            | "resolve-path"
            | "convert-path"
            | "format-hex"
            | "select-string"
    ) {
        return false;
    }
    let Some(configuration) = FKN.get(canonical.as_str()) else {
        return false;
    };
    if configuration.operation_type != PsOperation::Read {
        return false;
    }
    let mut arguments = args[1..].iter();
    while let Some(argument) = arguments.next() {
        if !is_parameter(argument, None) {
            continue;
        }
        if !argument.starts_with('-') {
            return false;
        }
        let parameter = argument.to_ascii_lowercase();
        if matches!(
            parameter.as_str(),
            "-wait" | "-usetransaction" | "-credential" | "-inputobject" | "-inputstream"
        ) {
            return false;
        }
        if configuration.known_switches.contains(&parameter.as_str()) {
            continue;
        }
        if !configuration.path_params.contains(&parameter.as_str())
            && !configuration
                .known_value_params
                .contains(&parameter.as_str())
        {
            return false;
        }
        let Some(value) = arguments.next() else {
            return false;
        };
        if is_parameter(value, None) {
            return false;
        }
        if matches!(
            parameter.as_str(),
            "-readcount"
                | "-totalcount"
                | "-tail"
                | "-first"
                | "-head"
                | "-last"
                | "-depth"
                | "-count"
                | "-offset"
        ) && value.parse::<u64>().is_err()
        {
            return false;
        }
    }
    true
}

fn removal_paths(
    command: &str,
    root: &Path,
    cwd: Option<&Path>,
    powershell: bool,
) -> Option<Vec<PathBuf>> {
    let args = literal_argv(command, powershell)?;
    if powershell {
        if !matches!(
            args[0].to_ascii_lowercase().as_str(),
            "rm" | "remove-item" | "ri" | "del" | "erase"
        ) {
            return None;
        }
    } else if args[0] != "rm" {
        return None;
    }
    let mut output = Vec::new();
    let mut end_options = false;
    for arg in &args[1..] {
        if !end_options && !powershell && arg == "--" {
            end_options = true;
            continue;
        }
        if !end_options && !powershell && matches!(arg.as_str(), "-f" | "--force") {
            continue;
        }
        if powershell && matches!(arg.to_ascii_lowercase().as_str(), "-path" | "-literalpath") {
            continue;
        }
        if !end_options && arg.starts_with('-') {
            return None;
        }
        if arg.contains(['*', '?', '[', ']'])
            || powershell
                && arg.contains([
                    '$', '`', '(', ')', '{', '}', '|', ';', '&', '<', '>', '"', '\'', ',',
                ])
            || !Path::new(arg).is_absolute()
            // The original shell spelling executes unchanged. Lexical `..`
            // normalization before following a directory symlink can name a
            // different file, so deletion targets must already be unambiguous.
            || Path::new(arg).components().any(|component| component == Component::ParentDir)
        {
            return None;
        }
        output.push(resolve_memory_markdown_path(root, arg, cwd)?);
    }
    (!output.is_empty()).then_some(output)
}

/// Enforces the memory fork's tool boundary before the ordinary enforcing
/// invoker. Every accepted call still evaluates live deny/ask rules headlessly.
pub struct MemoryAgentToolInvoker {
    inner: Arc<dyn ToolInvoker>,
    memory_dir: PathBuf,
    mutation_root: Option<MemoryMutationRoot>,
    read_directories: Vec<PathBuf>,
    shell: MemoryAgentShell,
    active: Arc<AtomicBool>,
    next_call: AtomicU64,
    writes: Mutex<Vec<(u64, PathBuf)>>,
    touched: Mutex<Vec<(u64, PathBuf)>>,
}

impl MemoryAgentToolInvoker {
    /// Build a revocable local-memory tool boundary.
    #[must_use]
    pub fn new(inner: Arc<dyn ToolInvoker>, memory_dir: PathBuf) -> Self {
        let inner = inner.fork_for_memory(false).unwrap_or(inner);
        let memory_dir = std::fs::canonicalize(&memory_dir).unwrap_or(memory_dir);
        let mutation_root = MemoryMutationRoot::capture(&memory_dir).ok();
        Self {
            inner,
            memory_dir,
            mutation_root,
            read_directories: Vec::new(),
            shell: MemoryAgentShell::default(),
            active: Arc::new(AtomicBool::new(true)),
            next_call: AtomicU64::new(0),
            writes: Mutex::new(Vec::new()),
            touched: Mutex::new(Vec::new()),
        }
    }

    /// Select the parent's admitted tool identity; this never registers an
    /// alias or bypasses the ordinary registry and permission gate.
    #[must_use]
    pub fn with_shell(mut self, shell: MemoryAgentShell) -> Self {
        self.shell = shell;
        self
    }

    /// Permit a host-selected directory for reads without admitting mutations
    /// there. Dream uses this for the actual transcript directory.
    #[must_use]
    pub fn with_read_directory(mut self, directory: PathBuf) -> Self {
        let directory = std::fs::canonicalize(&directory).unwrap_or(directory);
        if !self.read_directories.contains(&directory) {
            self.read_directories.push(directory);
        }
        self
    }

    /// Revoke future calls when a worker completes, is killed, or is superseded.
    pub fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Release);
    }

    /// Successful Write/Edit targets only, in first-write order. Failed calls,
    /// reads, and unrelated concurrent filesystem changes never enter this list.
    #[must_use]
    pub fn completed_write_paths(&self) -> Vec<PathBuf> {
        ordered_paths(&self.writes)
    }

    /// Successful Write/Edit and validated shell-removal targets. Dream uses
    /// this wider set so a prune-only pass produces its actual change notice.
    #[must_use]
    pub fn completed_touched_paths(&self) -> Vec<PathBuf> {
        ordered_paths(&self.touched)
    }

    fn validate(
        &self,
        name: &str,
        mut input: Utf16JsonProjection,
        cwd: Option<&Path>,
    ) -> Result<(Utf16JsonProjection, Option<PathBuf>, Vec<PathBuf>), ToolInvokerError> {
        let denied = |message: String| ToolInvokerError::Internal(message.into());
        let allowed_message = || {
            let message = crate::auto_memory_extract::memory_tool_denial(
                &self.memory_dir.to_string_lossy(),
                self.shell.is_posix(),
            );
            if self.shell == MemoryAgentShell::Mobile {
                message.replacen("read-only Bash", "read-only Shell", 1)
            } else {
                message
            }
        };
        if !self.active.load(Ordering::Acquire) {
            return Err(denied("the memory pass is no longer active".into()));
        }
        match name {
            "Read" | "Grep" | "Glob" => Ok((input, None, Vec::new())),
            "Write" | "Edit" => {
                let path = input
                    .string_at("/file_path")
                    .and_then(|path| resolve_memory_markdown_path(&self.memory_dir, path, cwd))
                    .ok_or_else(|| denied(allowed_message()))?;
                input
                    .set_field("file_path", path.to_string_lossy().to_string().into())
                    .map_err(|error| denied(error.to_string()))?;
                Ok((input, Some(path), Vec::new()))
            }
            name if name == self.shell.name() => {
                if input
                    .at_pointer("/run_in_background")
                    .and_then(|value| value.as_bool())
                    == Some(true)
                {
                    return Err(denied("run_in_background is not available to the memory agent — run the command in the foreground.".into()));
                }
                let command = input.string_at("/command").ok_or_else(|| {
                    denied("the memory agent requires a literal shell command".into())
                })?;
                let powershell = !self.shell.is_posix();
                let read_only = if powershell {
                    powershell_command_is_read_only(command)
                } else {
                    permission::command_is_read_only(command)
                };
                let removals = if read_only {
                    Vec::new()
                } else {
                    removal_paths(command, &self.memory_dir, cwd, powershell).ok_or_else(|| {
                        denied(crate::auto_memory_extract::memory_shell_denial(
                            &self.memory_dir.to_string_lossy(),
                            !powershell,
                        ))
                    })?
                };
                if self.shell != MemoryAgentShell::Mobile {
                    input
                        .set_field(
                            "dangerouslyDisableSandbox",
                            Utf16JsonProjection::parse("false").expect("false is JSON"),
                        )
                        .map_err(|error| denied(error.to_string()))?;
                    input
                        .set_field(
                            "run_in_background",
                            Utf16JsonProjection::parse("false").expect("false is JSON"),
                        )
                        .map_err(|error| denied(error.to_string()))?;
                }
                Ok((input, None, removals))
            }
            _ if name.starts_with("mcp__") => Err(denied(
                "MCP tools are not available to the memory agent".into(),
            )),
            _ => Err(denied(allowed_message())),
        }
    }
}

#[async_trait]
impl ToolInvoker for MemoryAgentToolInvoker {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn permission_mode(&self) -> Option<String> {
        self.inner.permission_mode()
    }
    fn trusted_session_profile(
        &self,
    ) -> Option<lingxi_core::host::session_context::SessionModeSnapshot> {
        self.inner.trusted_session_profile()
    }
    fn durable_origin_state(&self) -> Option<ToolInvocationContextState> {
        self.inner.durable_origin_state()
    }
    async fn cleanup_computer_inputs(
        &self,
        agent_id: AgentId,
        session_id: Option<SessionId>,
    ) -> Result<(), ToolInvokerError> {
        self.inner
            .cleanup_computer_inputs(agent_id, session_id)
            .await
    }
    fn map_result_text(
        &self,
        name: &str,
        result: &Utf16JsonProjection,
    ) -> Option<Utf16JsonProjection> {
        self.inner.map_result_text(name, result)
    }
    fn map_result_is_error(&self, name: &str, result: &Utf16JsonProjection) -> Option<bool> {
        self.inner.map_result_is_error(name, result)
    }
    async fn parse_output(
        &self,
        name: &str,
        output: &Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ToolInvokerError> {
        self.inner.parse_output(name, output).await
    }
    fn tool_is_concurrency_safe(&self, name: &str, input: &Utf16JsonProjection) -> Option<bool> {
        self.inner.tool_is_concurrency_safe(name, input)
    }

    async fn invoke(
        &self,
        name: &str,
        input: Utf16JsonProjection,
        ctx: SubagentInvocationContext,
    ) -> Result<Utf16JsonProjection, ToolInvokerError> {
        self.invoke_detailed(name, input, ctx)
            .await
            .map(|result| result.data)
    }

    async fn invoke_detailed(
        &self,
        name: &str,
        input: Utf16JsonProjection,
        mut ctx: SubagentInvocationContext,
    ) -> Result<ToolInvocationResult, ToolInvokerError> {
        let (input, written_path, removed_paths) =
            self.validate(name, input, ctx.cwd.as_deref())?;
        let ignore_missing = input
            .string_at("/command")
            .and_then(|command| literal_argv(command, !self.shell.is_posix()))
            .is_some_and(|args| {
                args.iter()
                    .any(|arg| matches!(arg.as_str(), "-f" | "--force"))
            });
        let mutation_root = if written_path.is_some() || !removed_paths.is_empty() {
            let mut root = self.mutation_root.clone().ok_or_else(|| {
                ToolInvokerError::InvalidInput("the admitted memory root is unavailable".into())
            })?;
            let targets = written_path
                .iter()
                .cloned()
                .chain(removed_paths.iter().cloned())
                .collect::<Vec<_>>();
            root.admit_targets(&targets)
                .map_err(|error| ToolInvokerError::InvalidInput(error.to_string().into()))?;
            Some(root)
        } else {
            None
        };
        let sequence = self.next_call.fetch_add(1, Ordering::Relaxed);
        ctx.is_non_interactive_session = true;
        ctx.can_show_permission_prompts = false;
        let result = lingxi_core::host::memory_tool_scope::with_memory_tool_scope_and_removals(
            name.to_owned(),
            input.clone(),
            self.memory_dir.clone(),
            mutation_root,
            self.read_directories.clone(),
            removed_paths.clone(),
            ignore_missing,
            self.active.clone(),
            self.inner.invoke_detailed(name, input, ctx),
        )
        .await?;
        if !result.is_error {
            if let Some(path) = written_path {
                self.writes
                    .lock()
                    .unwrap_or_else(poisoned)
                    .push((sequence, path.clone()));
                self.touched
                    .lock()
                    .unwrap_or_else(poisoned)
                    .push((sequence, path));
            }
            self.touched
                .lock()
                .unwrap_or_else(poisoned)
                .extend(removed_paths.into_iter().map(|path| (sequence, path)));
        }
        Ok(result)
    }
}

fn ordered_paths(paths: &Mutex<Vec<(u64, PathBuf)>>) -> Vec<PathBuf> {
    let mut paths = paths.lock().unwrap_or_else(poisoned).clone();
    paths.sort_by_key(|(sequence, _)| *sequence);
    let mut ordered = Vec::new();
    for (_, path) in paths {
        if !ordered.contains(&path) {
            ordered.push(path);
        }
    }
    ordered
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoTools;
    #[async_trait]
    impl ToolInvoker for NoTools {
        fn as_any(&self) -> &dyn Any {
            self
        }
        async fn invoke(
            &self,
            _: &str,
            _: Utf16JsonProjection,
            _: SubagentInvocationContext,
        ) -> Result<Utf16JsonProjection, ToolInvokerError> {
            unreachable!()
        }
    }

    #[test]
    fn mobile_memory_shell_admits_readers_and_literal_prunes_without_bash_aliases() {
        let directory = tempfile::tempdir().unwrap();
        let wrapper = MemoryAgentToolInvoker::new(Arc::new(NoTools), directory.path().to_owned())
            .with_shell(MemoryAgentShell::Mobile);
        let input = |command: &str| {
            Utf16JsonProjection::from_serde_value(&serde_json::json!({"command":command})).unwrap()
        };
        assert!(wrapper.validate("Shell", input("ls"), None).is_ok());
        assert!(wrapper.validate("Bash", input("ls"), None).is_err());
        assert!(wrapper
            .validate("PowerShell", input("Get-Content a.md"), None)
            .is_err());
        let target = directory.path().canonicalize().unwrap().join("obsolete.md");
        let raw = input(&format!("rm -f '{}'", target.display()));
        assert!(wrapper.validate("Shell", raw.clone(), None).is_ok());
        assert_eq!(wrapper.validate("Shell", raw.clone(), None).unwrap().0, raw);
        for command in [
            "rm -rf /tmp/data.md",
            "rm /tmp/outside.md",
            "cat /tmp/x > /tmp/y",
            "find /tmp -delete",
            "rm $(pwd)/old.md",
        ] {
            assert!(
                wrapper.validate("Shell", input(command), None).is_err(),
                "{command}"
            );
        }
    }

    #[test]
    fn write_boundary_rejects_traversal_protected_names_and_symlink_escapes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("memory");
        std::fs::create_dir_all(&root).unwrap();
        let good = root.canonicalize().unwrap().join("feedback.md");
        assert_eq!(
            resolve_memory_markdown_path(&root, good.to_str().unwrap(), None),
            Some(good)
        );
        for path in [
            root.join("../outside.md"),
            root.join(".git/config.md"),
            root.join("AGENTS./a.md"),
            root.join(".g\u{200c}it/a.md"),
            root.join("notes.txt"),
            root.join("a:stream.md"),
        ] {
            assert!(
                resolve_memory_markdown_path(&root, path.to_str().unwrap(), None).is_none(),
                "{}",
                path.display()
            );
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(temp.path(), root.join("escape")).unwrap();
            assert!(resolve_memory_markdown_path(
                &root,
                root.join("escape/outside.md").to_str().unwrap(),
                None
            )
            .is_none());
        }
    }

    #[test]
    fn memory_shell_is_foreground_sandboxed_and_removal_is_literal() {
        let temp = tempfile::tempdir().unwrap();
        let invoker = MemoryAgentToolInvoker::new(Arc::new(NoTools), temp.path().to_path_buf());
        let raw =
            Utf16JsonProjection::parse(r#"{"command":"ls","dangerouslyDisableSandbox":true}"#)
                .unwrap();
        let (sanitized, _, _) = invoker.validate("Bash", raw, None).unwrap();
        assert_eq!(
            sanitized
                .at_pointer("/dangerouslyDisableSandbox")
                .unwrap()
                .as_bool(),
            Some(false)
        );
        assert_eq!(
            sanitized
                .at_pointer("/run_in_background")
                .unwrap()
                .as_bool(),
            Some(false)
        );
        let path = temp.path().canonicalize().unwrap().join("old note.md");
        assert_eq!(
            removal_paths(
                &format!("rm -f '{}'", path.display()),
                temp.path(),
                None,
                false
            ),
            Some(vec![path])
        );
        for command in [
            "rm -rf /tmp/old.md",
            "rm /tmp/*.md",
            "rm /tmp/old.md; touch /tmp/x",
            "rm $(pwd)/old.md",
            "cat /tmp/x > /tmp/y",
            "find /tmp -delete",
        ] {
            assert!(
                invoker
                    .validate(
                        "Bash",
                        Utf16JsonProjection::from_serde_value(
                            &serde_json::json!({"command":command})
                        )
                        .unwrap(),
                        None
                    )
                    .is_err(),
                "{command}"
            );
        }
        assert!(invoker
            .validate("Agent", Utf16JsonProjection::parse("{}").unwrap(), None)
            .is_err());
        invoker.set_active(false);
        assert!(invoker
            .validate("Read", Utf16JsonProjection::parse("{}").unwrap(), None)
            .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn leaf_symlinks_cannot_rewrite_markdown_authority_to_control_or_topic_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let lock = root.join(".consolidate-lock");
        let topic = root.join("topic.md");
        std::fs::write(&lock, "host-owned lock").unwrap();
        std::fs::write(&topic, "original topic").unwrap();
        let lock_alias = root.join("lock-alias.md");
        let topic_alias = root.join("topic-alias.md");
        std::os::unix::fs::symlink(&lock, &lock_alias).unwrap();
        std::os::unix::fs::symlink(&topic, &topic_alias).unwrap();
        let invoker = MemoryAgentToolInvoker::new(Arc::new(NoTools), root.clone());
        for alias in [&lock_alias, &topic_alias] {
            assert!(resolve_memory_markdown_path(&root, alias.to_str().unwrap(), None).is_none());
            for tool in ["Write", "Edit"] {
                let input = Utf16JsonProjection::from_serde_value(&serde_json::json!({
                    "file_path":alias,"content":"must not execute"
                }))
                .unwrap();
                assert!(
                    invoker.validate(tool, input, None).is_err(),
                    "{tool}: {}",
                    alias.display()
                );
            }
            assert!(std::fs::symlink_metadata(alias)
                .unwrap()
                .file_type()
                .is_symlink());
        }
        assert_eq!(
            resolve_memory_markdown_path(&root, topic.to_str().unwrap(), None),
            Some(topic.clone())
        );
        assert!(protected_component(std::ffi::OsStr::new(
            ".consolidate-lock"
        )));
        assert_eq!(std::fs::read_to_string(lock).unwrap(), "host-owned lock");
        assert_eq!(std::fs::read_to_string(topic).unwrap(), "original topic");
    }

    // Linux permits non-UTF-8 filenames; macOS filesystems/sandboxing reject
    // this fixture before the path boundary can inspect it.
    #[cfg(target_os = "linux")]
    #[test]
    fn canonical_paths_that_cannot_be_forwarded_losslessly_are_rejected() {
        use std::os::unix::ffi::OsStringExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let non_utf8 = root.join(std::ffi::OsString::from_vec(b"private-\xff".to_vec()));
        std::fs::create_dir(&non_utf8).unwrap();
        std::os::unix::fs::symlink(&non_utf8, root.join("alias-dir")).unwrap();
        let alias_path = root.join("alias-dir/topic.md");
        assert!(resolve_memory_markdown_path(&root, alias_path.to_str().unwrap(), None).is_none());
    }

    #[cfg(unix)]
    fn actual_bash_argument_bytes(command: &str) -> Vec<Vec<u8>> {
        // The controlled command becomes arguments to a capture function;
        // none of these argv-parity cases invokes rm or another target tool.
        let script = format!("capture_args() {{ printf '%s\\0' \"$@\"; }}\ncapture_args {command}");
        let output = std::process::Command::new("/bin/bash")
            .args(["--noprofile", "--norc", "-c", &script])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.ends_with(&[0]));
        output.stdout[..output.stdout.len() - 1]
            .split(|byte| *byte == 0)
            .map(<[u8]>::to_vec)
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn literal_bash_arguments_match_the_shells_double_quote_and_escape_bytes() {
        for command in [
            r#"rm "/memory/back\slash.md""#,
            r#"rm "/memory/back\\slash.md""#,
            r#"rm "/memory/dollar\$literal.md""#,
            r#"rm "/memory/escaped\"quote.md""#,
            r#"rm "/memory/escaped\`tick.md""#,
            r"rm /memory/a\ b.md",
            "rm /memory/line\\\nbreak.md",
            "rm \\\n /memory/line.md",
            "rm \"/memory/line\\\nbreak.md\"",
            r"rm '/memory/literal\backslash.md'",
            "rm /memory/non\u{00a0}breaking.md",
        ] {
            let parsed = literal_argv(command, false).expect("supported literal Bash command");
            let expected = parsed
                .into_iter()
                .map(String::into_bytes)
                .collect::<Vec<_>>();
            assert_eq!(actual_bash_argument_bytes(command), expected, "{command:?}");
        }
        for command in [
            r#"rm "/memory/$HOME.md""#,
            r#"rm "/memory/`pwd`.md""#,
            "rm /memory/a.md\\",
            "rm '/memory/a.md",
        ] {
            assert!(literal_argv(command, false).is_none(), "{command:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn double_quoted_backslash_cannot_remove_a_sibling_and_literal_in_root_removal_still_works() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().canonicalize().unwrap();
        let root = parent.join("memory");
        let sibling = parent.join("mem\\ory");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let victim = root.join("victim.md");
        let sibling_victim = sibling.join("victim.md");
        std::fs::write(&victim, "memory note").unwrap();
        std::fs::write(&sibling_victim, "sibling must survive").unwrap();
        let invoker = MemoryAgentToolInvoker::new(Arc::new(NoTools), root.clone());
        let ambiguous = format!(r#"rm "{}/mem\ory/victim.md""#, parent.display());
        assert_eq!(
            actual_bash_argument_bytes(&ambiguous)[1],
            sibling_victim.to_str().unwrap().as_bytes()
        );
        assert!(removal_paths(&ambiguous, &root, None, false).is_none());
        let input =
            Utf16JsonProjection::from_serde_value(&serde_json::json!({"command":ambiguous}))
                .unwrap();
        assert!(invoker.validate("Bash", input, None).is_err());
        assert!(victim.exists());
        assert!(sibling_victim.exists());

        // Exercise the actual shell only after the production validator grants
        // the command, on disposable fixtures wholly owned by this test.
        let positive = format!("rm -f '{}'", victim.display());
        let input = Utf16JsonProjection::from_serde_value(&serde_json::json!({"command":positive}))
            .unwrap();
        let (approved, _, _) = invoker.validate("Bash", input, None).unwrap();
        assert!(std::process::Command::new("/bin/bash")
            .args([
                "--noprofile",
                "--norc",
                "-c",
                approved.string_at("/command").unwrap()
            ])
            .status()
            .unwrap()
            .success());
        assert!(!victim.exists());
        assert_eq!(
            std::fs::read_to_string(&sibling_victim).unwrap(),
            "sibling must survive"
        );

        let backslash_note = root.join("literal\\name.md");
        std::fs::write(&backslash_note, "literal backslash name").unwrap();
        let command = format!(r#"rm "{}/literal\name.md""#, root.display());
        let input =
            Utf16JsonProjection::from_serde_value(&serde_json::json!({"command":command})).unwrap();
        let (approved, _, _) = invoker.validate("Bash", input, None).unwrap();
        assert_eq!(
            removal_paths(approved.string_at("/command").unwrap(), &root, None, false),
            Some(vec![backslash_note.clone()])
        );
        assert!(std::process::Command::new("/bin/bash")
            .args([
                "--noprofile",
                "--norc",
                "-c",
                approved.string_at("/command").unwrap()
            ])
            .status()
            .unwrap()
            .success());
        assert!(!backslash_note.exists());
        assert!(sibling_victim.exists());
    }

    #[cfg(unix)]
    #[test]
    fn removal_rejects_parent_traversal_across_directory_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().canonicalize().unwrap();
        let root = parent.join("memory");
        let outside_child = parent.join("outside/child");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside_child).unwrap();
        std::fs::write(root.join("victim.md"), "memory").unwrap();
        std::fs::write(parent.join("outside/victim.md"), "outside").unwrap();
        std::os::unix::fs::symlink(&outside_child, root.join("link")).unwrap();
        let command = format!("rm '{}/link/../victim.md'", root.display());
        assert!(removal_paths(&command, &root, None, false).is_none());
        assert!(root.join("victim.md").exists());
        assert!(parent.join("outside/victim.md").exists());
    }

    #[test]
    fn powershell_reader_subset_is_available_without_admitting_writes_or_evaluation() {
        let temp = tempfile::tempdir().unwrap();
        let invoker = MemoryAgentToolInvoker::new(Arc::new(NoTools), temp.path().to_path_buf())
            .with_shell(MemoryAgentShell::PowerShell);
        for command in [
            r"Get-Content -LiteralPath 'C:\memory\topic.md' -Raw",
            r"Get-Content 'C:\memory\topic.md' -TotalCount 20",
            r"GET-CONTENT 'C:\memory\topic.md' -Tail 5",
            r"Get-ChildItem -Path 'C:\memory' -File -Force",
            r"gci 'C:\memory' -Recurse",
            r"gc 'C:\memory\topic.md'",
            r"Get-Item -LiteralPath 'C:\memory\topic.md'",
            r"Get-ItemProperty -Path 'C:\memory\topic.md' -Name Length",
            r"Get-FileHash -Path 'C:\memory\topic.md' -Algorithm SHA256",
            r"Test-Path 'C:\memory\topic.md' -PathType Leaf",
            r"Resolve-Path 'C:\memory\topic.md'",
            r"Select-String -Path 'C:\memory\topic.md' -Pattern feedback -SimpleMatch",
            "Get-Location",
            "Select-Object -First 10",
            "Select-Object -Last 10 -Skip 2",
            "Measure-Object -Line -Word",
        ] {
            assert!(powershell_command_is_read_only(command), "{command}");
            let input = Utf16JsonProjection::from_serde_value(
                &serde_json::json!({"command":command,"dangerouslyDisableSandbox":true}),
            )
            .unwrap();
            let (approved, _, _) = invoker.validate("PowerShell", input, None).unwrap();
            assert_eq!(approved.string_at("/command"), Some(command));
            assert_eq!(
                approved
                    .at_pointer("/dangerouslyDisableSandbox")
                    .unwrap()
                    .as_bool(),
                Some(false)
            );
            assert_eq!(
                approved.at_pointer("/run_in_background").unwrap().as_bool(),
                Some(false)
            );
        }
        for command in [
            r"Set-Content -Path 'C:\memory\topic.md' -Value changed",
            r"Remove-Item 'C:\outside\topic.md'",
            r"Get-Content p | Set-Content q",
            r"Get-Content p | Select-Object -First 5",
            "Get-Content p > q",
            "Get-Content p 2>&1",
            "Get-Content $(Remove-Item p)",
            "Get-Content $path",
            "Get-Content @paths",
            "Get-Content p; Remove-Item q",
            "Get-Content p`nSet-Content q changed",
            "& Get-Content p",
            "Get-Content.exe p",
            r"C:\tools\Get-Content p",
            "Get-Content p -OutVariable captured",
            "Get-Content p \u{2013}OutVariable captured",
            "Get-Content p -OutFile q",
            "Get-ChildItem p -UnknownFlag",
            "Get-Content -Path",
            "Get-Content p -Wait",
            "Select-Object -First invalid",
            "Get-Content p -Raw:$true",
        ] {
            assert!(!powershell_command_is_read_only(command), "{command}");
            let input =
                Utf16JsonProjection::from_serde_value(&serde_json::json!({"command":command}))
                    .unwrap();
            assert!(
                invoker.validate("PowerShell", input, None).is_err(),
                "{command}"
            );
        }
    }

    #[test]
    fn powershell_doubled_quotes_do_not_change_the_validated_removal_target() {
        assert_eq!(
            literal_argv("Get-Content 'a''b'", true).unwrap(),
            ["Get-Content", "a'b"]
        );
        assert_eq!(
            literal_argv("Get-Content \"a\"\"b\"", true).unwrap(),
            ["Get-Content", "a\"b"]
        );
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("quoted'name.md");
        let command = format!(
            "Remove-Item '{}'",
            path.to_string_lossy().replace('\'', "''")
        );
        assert!(removal_paths(&command, temp.path(), None, true).is_none());
    }

    #[test]
    fn a_transcript_read_directory_does_not_expand_the_mutation_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let memory = temp.path().join("memory");
        let transcripts = temp.path().join("transcripts");
        std::fs::create_dir_all(&memory).unwrap();
        std::fs::create_dir_all(&transcripts).unwrap();
        let invoker = MemoryAgentToolInvoker::new(Arc::new(NoTools), memory)
            .with_read_directory(transcripts.clone())
            .with_read_directory(transcripts.clone());
        assert_eq!(
            invoker.read_directories,
            [transcripts.canonicalize().unwrap()]
        );
        let input = Utf16JsonProjection::from_serde_value(
            &serde_json::json!({"file_path":transcripts.join("notes.md"),"content":"not writable"}),
        )
        .unwrap();
        assert!(invoker.validate("Write", input, None).is_err());
    }
}
