//! Source-level guards for boundaries no longer enforced by separate crates.
use std::path::{Path, PathBuf};
use syn::visit::Visit;

struct Boundary {
    module: Vec<String>,
    layer: String,
    violations: Vec<String>,
}

impl Boundary {
    fn check(&mut self, path: Vec<String>) {
        let mut resolved = self.module.clone();
        let mut segments = path.iter().peekable();
        match segments.peek().map(|s| s.as_str()) {
            Some("crate") => {
                resolved.clear();
                segments.next();
            }
            Some("self") => {
                segments.next();
            }
            Some("super") => {
                while segments.peek().is_some_and(|s| s.as_str() == "super") {
                    resolved.pop();
                    segments.next();
                }
            }
            _ => {
                resolved.clear();
            }
        }
        resolved.extend(segments.cloned());
        let root = resolved.first().map(String::as_str).unwrap_or("");
        let forbidden = match self.layer.as_str() {
            "protocol" => matches!(
                root,
                "adapter"
                    | "presentation"
                    | "orchestrator"
                    | "tool_api"
                    | "tool_shell"
                    | "session"
                    | "permission"
                    | "platform_api"
            ),
            "presentation" => {
                matches!(root, "adapter" | "protocol" | "orchestrator" | "tool_shell")
            }
            _ => false,
        };
        if forbidden {
            self.violations.push(path.join("::"));
        }
    }

    fn check_use(&mut self, prefix: Vec<String>, tree: &syn::UseTree) {
        match tree {
            syn::UseTree::Path(p) => {
                let mut path = prefix;
                path.push(p.ident.to_string());
                self.check_use(path, &p.tree);
            }
            syn::UseTree::Group(g) => {
                for item in &g.items {
                    self.check_use(prefix.clone(), item);
                }
            }
            syn::UseTree::Name(n) => {
                let mut path = prefix;
                path.push(n.ident.to_string());
                self.check(path);
            }
            syn::UseTree::Rename(n) => {
                let mut path = prefix;
                path.push(n.ident.to_string());
                self.check(path);
            }
            syn::UseTree::Glob(_) => self.check(prefix),
        }
    }
}

impl<'ast> Visit<'ast> for Boundary {
    fn visit_path(&mut self, path: &'ast syn::Path) {
        self.check(path.segments.iter().map(|s| s.ident.to_string()).collect());
        syn::visit::visit_path(self, path);
    }
    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        self.check_use(Vec::new(), &item.tree);
    }
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        self.module.push(item.ident.to_string());
        syn::visit::visit_item_mod(self, item);
        self.module.pop();
    }
}

fn rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, files);
        } else if path.extension().is_some_and(|e| e == "rs") {
            files.push(path);
        }
    }
}

#[test]
fn client_layers_keep_their_dependency_direction() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for layer in ["protocol", "presentation"] {
        let mut files = Vec::new();
        rust_files(&src.join(layer), &mut files);
        for file in files {
            let relative = file.strip_prefix(&src).unwrap().with_extension("");
            let mut module: Vec<_> = relative
                .iter()
                .map(|s| s.to_string_lossy().into_owned())
                .collect();
            if module.last().is_some_and(|s| s == "mod") {
                module.pop();
            }
            let mut guard = Boundary {
                module,
                layer: layer.into(),
                violations: Vec::new(),
            };
            guard.visit_file(&syn::parse_file(&std::fs::read_to_string(&file).unwrap()).unwrap());
            assert!(
                guard.violations.is_empty(),
                "{}: {:?}",
                file.display(),
                guard.violations
            );
        }
    }
}

#[test]
fn guard_detects_grouped_aliases_and_relative_paths() {
    let mut guard = Boundary {
        module: vec!["protocol".into()],
        layer: "protocol".into(),
        violations: Vec::new(),
    };
    guard.visit_file(&syn::parse_file("use crate::{adapter as runtime_adapter}; mod nested { use super::super::presentation::*; } fn f() { crate::adapter::run(); }").unwrap());
    assert_eq!(guard.violations.len(), 3);
}
