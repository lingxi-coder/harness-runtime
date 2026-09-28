//! Port of claude-code `BashTool.isSearchOrReadCommand` /
//! `isSearchOrReadBashCommand` (`tools/BashTool/BashTool.tsx:95-172`, command
//! sets `:60-77`) plus the trivial `FileReadTool`/`GrepTool`/`GlobTool`
//! classifiers. Feeds the TUI collapsed read/search fold: a contiguous run of
//! read/search/list tool uses folds into a single summary line.
//!
//! Reuses the already-tested [`super::silent::split_command_with_operators`]
//! (`silent.rs:63`) for the quote-/escape-/operator-aware split so the bash
//! classification matches the security path's tokenization exactly.

use super::silent::split_command_with_operators;

/// `{isSearch,isRead,isList}` classification of one tool use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadSearchKind {
    /// Grep/Glob, or a bash `grep`/`rg`/`find`/… command.
    pub is_search: bool,
    /// Read, or a bash `cat`/`head`/`tail`/… command.
    pub is_read: bool,
    /// A bash `ls`/`tree`/`du` command.
    pub is_list: bool,
}

impl ReadSearchKind {
    /// True when this use folds into a collapsed group.
    #[must_use]
    pub fn is_collapsible(self) -> bool {
        self.is_search || self.is_read || self.is_list
    }
}

/// `BashTool.tsx:60` — search commands.
const BASH_SEARCH_COMMANDS: &[&str] = &[
    "find", "grep", "rg", "ag", "ack", "locate", "which", "whereis",
];
/// `BashTool.tsx:63` — read/view/analysis/data-processing commands.
const BASH_READ_COMMANDS: &[&str] = &[
    "cat", "head", "tail", "less", "more", "wc", "stat", "file", "strings", "jq", "awk", "cut",
    "sort", "uniq", "tr",
];
/// `BashTool.tsx:72` — directory-listing commands.
const BASH_LIST_COMMANDS: &[&str] = &["ls", "tree", "du"];
/// `BashTool.tsx:77` — semantic-neutral commands (don't change the pipeline's
/// read/search nature: `ls a && echo --- && ls b` is still a read).
const BASH_NEUTRAL_COMMANDS: &[&str] = &["echo", "printf", "true", "false", ":"];

/// Port of `isSearchOrReadBashCommand` (`BashTool.tsx:95`). Every base command
/// in the operator-split pipeline must be neutral or a search/read/list command;
/// any other base command (or a parse failure / all-neutral pipeline) is not
/// collapsible.
#[must_use]
pub fn is_search_or_read_bash_command(command: &str) -> ReadSearchKind {
    let parts = split_command_with_operators(command);
    if parts.is_empty() {
        return ReadSearchKind::default();
    }
    let mut kind = ReadSearchKind::default();
    let mut has_non_neutral = false;
    let mut skip_redirect_target = false;
    for part in &parts {
        if skip_redirect_target {
            skip_redirect_target = false;
            continue;
        }
        match part.as_str() {
            ">" | ">>" | ">&" => {
                skip_redirect_target = true;
                continue;
            }
            "||" | "&&" | "|" | ";" => continue,
            _ => {}
        }
        let base = match part.split_whitespace().next() {
            Some(b) => b,
            None => continue,
        };
        if BASH_NEUTRAL_COMMANDS.contains(&base) {
            continue;
        }
        has_non_neutral = true;
        let is_search = BASH_SEARCH_COMMANDS.contains(&base);
        let is_read = BASH_READ_COMMANDS.contains(&base);
        let is_list = BASH_LIST_COMMANDS.contains(&base);
        if !is_search && !is_read && !is_list {
            return ReadSearchKind::default();
        }
        kind.is_search |= is_search;
        kind.is_read |= is_read;
        kind.is_list |= is_list;
    }
    if !has_non_neutral {
        return ReadSearchKind::default();
    }
    kind
}

/// Per-tool classifier (`getToolSearchOrReadInfo` delegate). Read/Grep/Glob are
/// static; Bash parses its `command`. Unknown tools are not collapsible.
#[must_use]
pub fn is_search_or_read_command(tool: &str, command: Option<&str>) -> ReadSearchKind {
    match tool {
        "Read" => ReadSearchKind {
            is_read: true,
            ..Default::default()
        },
        "Grep" | "Glob" => ReadSearchKind {
            is_search: true,
            ..Default::default()
        },
        "Bash" => command
            .map(is_search_or_read_bash_command)
            .unwrap_or_default(),
        _ => ReadSearchKind::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_grep_glob_are_static() {
        assert!(is_search_or_read_command("Read", None).is_read);
        assert!(is_search_or_read_command("Grep", None).is_search);
        assert!(is_search_or_read_command("Glob", None).is_search);
        assert!(!is_search_or_read_command("Edit", None).is_collapsible());
    }

    #[test]
    fn bash_read_search_list() {
        assert!(is_search_or_read_bash_command("cat foo.rs").is_read);
        assert!(is_search_or_read_bash_command("rg needle src").is_search);
        assert!(is_search_or_read_bash_command("ls -la").is_list);
    }

    #[test]
    fn bash_neutral_only_is_not_collapsible() {
        assert!(!is_search_or_read_bash_command("echo hi").is_collapsible());
    }

    #[test]
    fn bash_mixed_neutral_and_read_is_read() {
        let k = is_search_or_read_bash_command("cat a && echo --- && cat b");
        assert!(k.is_read && !k.is_search);
    }

    #[test]
    fn bash_any_non_read_command_breaks_it() {
        assert!(!is_search_or_read_bash_command("cat a && rm b").is_collapsible());
    }

    #[test]
    fn bash_redirect_target_is_skipped() {
        // `> out` must not be treated as a base command.
        assert!(is_search_or_read_bash_command("cat a > out").is_read);
    }
}
