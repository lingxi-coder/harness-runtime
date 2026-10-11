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
        // `ClientTextDto` is the protocol string carrier; its adapter-only
        // projection conversions use this one foundation type and constructor.
        // Keep the exception exact and module-local so the protocol layer still
        // rejects every other dependency on core.
        let path_parts: Vec<_> = path.iter().map(String::as_str).collect();
        let is_protocol_text_module = (self.module.len() == 2
            && self.module[0].as_str() == "protocol"
            && self.module[1].as_str() == "text")
            || (self.module.len() == 3
                && self.module[0].as_str() == "protocol"
                && self.module[1].as_str() == "text"
                && self.module[2].as_str() == "tests");
        let is_protocol_text_projection = self.layer == "protocol"
            && is_protocol_text_module
            && matches!(
                path_parts.as_slice(),
                ["lingxi_core", "types", "utf16_json", "Utf16JsonProjection"]
                    | [
                        "lingxi_core",
                        "types",
                        "utf16_json",
                        "Utf16JsonProjection",
                        "root_string"
                    ]
            );
        if is_protocol_text_projection {
            return;
        }

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
                    | "lingxi_core"
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
        // A one-segment path is also how Rust represents a local value use, as
        // in `Self::OpenAiOAuthUpdated { session }`. Import trees are checked
        // separately by `visit_item_use`; only qualified paths prove a module
        // dependency here.
        if path.segments.len() > 1 {
            self.check(path.segments.iter().map(|s| s.ident.to_string()).collect());
        }
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
fn protocol_text_allows_only_its_utf16_projection_foundation_paths() {
    let mut text = Boundary {
        module: vec!["protocol".into(), "text".into()],
        layer: "protocol".into(),
        violations: Vec::new(),
    };
    text.visit_file(
        &syn::parse_file(
            "use lingxi_core::types::utf16_json::Utf16JsonProjection; \
             fn rebuild(units: Vec<u16>) { \
                 lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(\
                     String::new(), units); \
             }",
        )
        .unwrap(),
    );
    assert!(text.violations.is_empty());

    text.visit_file(&syn::parse_file("use lingxi_core::host::PermissionGate;").unwrap());
    assert_eq!(text.violations, vec!["lingxi_core::host::PermissionGate"]);

    let mut text_tests = Boundary {
        module: vec!["protocol".into(), "text".into(), "tests".into()],
        layer: "protocol".into(),
        violations: Vec::new(),
    };
    text_tests.visit_file(
        &syn::parse_file(
            "use lingxi_core::types::utf16_json::Utf16JsonProjection; \
             fn rebuild(units: Vec<u16>) { \
                 lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(\
                     String::new(), units); \
             }",
        )
        .unwrap(),
    );
    assert!(text_tests.violations.is_empty());
    text_tests.visit_file(&syn::parse_file("use lingxi_core::host::PermissionGate;").unwrap());
    assert_eq!(
        text_tests.violations,
        vec!["lingxi_core::host::PermissionGate"]
    );

    let mut commands = Boundary {
        module: vec!["protocol".into(), "commands".into()],
        layer: "protocol".into(),
        violations: Vec::new(),
    };
    commands.visit_file(
        &syn::parse_file("use lingxi_core::types::utf16_json::Utf16JsonProjection;").unwrap(),
    );
    assert_eq!(
        commands.violations,
        vec!["lingxi_core::types::utf16_json::Utf16JsonProjection"]
    );
}

#[test]
fn guard_ignores_bare_values_but_detects_qualified_module_paths() {
    let mut guard = Boundary {
        module: vec!["protocol".into()],
        layer: "protocol".into(),
        violations: Vec::new(),
    };
    guard.visit_file(&syn::parse_file(
        "fn f(session: &str) { let _ = session; let _ = Self::OpenAiOAuthUpdated { session }; }",
    ).unwrap());
    assert!(
        guard.violations.is_empty(),
        "local values are not module dependencies"
    );

    guard.visit_file(&syn::parse_file("fn f() { session::load(); }").unwrap());
    assert_eq!(guard.violations, vec!["session::load"]);
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
