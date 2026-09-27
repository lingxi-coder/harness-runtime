//! Compile-time audit proc-macros for the lingxi-telemetry tengu event tree.
//!
//! See spec §7 line 790-792. Two macros:
//!
//! - [`tengu_event_audit`] — invoked from `crates/telemetry/src/tengu/mod.rs`
//!   at crate root; walks `../telemetry/src/tengu/*.rs` at compile time.
//! - [`_audit_source`] — doc-hidden test helper that takes a string-literal
//!   source as input; same audit logic but inline. Used by `trybuild` cases
//!   to exercise the negative paths.

#![forbid(unsafe_code)]

use proc_macro::TokenStream;
use quote::quote;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use syn::{parse_macro_input, LitStr};

/// Compile-time event-audit macro. Takes no arguments; expands to `()` on
/// success, `compile_error!(...)` on failure.
///
/// Invoke from `tengu/mod.rs` as `telemetry_macros::tengu_event_audit!();`.
#[proc_macro]
pub fn tengu_event_audit(input: TokenStream) -> TokenStream {
    let _ = input;
    // Read the INVOKING crate's manifest directory at macro-expansion time.
    // Embedding this proc-macro crate's `CARGO_MANIFEST_DIR` with `env!` makes a
    // cached dylib non-relocatable: a build from a temporary staged checkout
    // keeps pointing at that deleted checkout when Cargo later reuses it from
    // the real worktree.
    let invoking_crate_dir = std::env::var_os("CARGO_MANIFEST_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../telemetry"),
        PathBuf::from,
    );
    let tengu_dir = invoking_crate_dir.join("src").join("tengu");

    let mut errors: Vec<String> = Vec::new();
    let categories = [
        "api", "agent", "session", "tool", "cost", "oauth", "memory", "settings",
    ];
    for cat in &categories {
        let path = tengu_dir.join(format!("{cat}.rs"));
        match std::fs::read_to_string(&path) {
            Ok(src) => {
                if let Err(msg) = audit_source(&src) {
                    errors.push(format!("{cat}.rs: {msg}"));
                }
            }
            Err(e) => {
                errors.push(format!(
                    "{cat}.rs: failed to read {}: {}",
                    path.display(),
                    e
                ));
            }
        }
    }

    if errors.is_empty() {
        // Empty TokenStream: macro is invoked at item position
        // (module root), so we must not emit an expression like `()`.
        TokenStream::new()
    } else {
        let joined = errors.join("\n  ");
        let msg = format!("tengu_event_audit rejected schema:\n  {joined}");
        quote!(compile_error!(#msg);).into()
    }
}

/// Doc-hidden test helper. Takes a string-literal source as input, runs the
/// same audit logic, and emits `compile_error!` on rejection. Used by the
/// `trybuild` compile_fail / compile_pass cases.
///
/// Called from `fn main()` (expression position), so success expands to `()`.
#[proc_macro]
#[doc(hidden)]
pub fn _audit_source(input: TokenStream) -> TokenStream {
    let lit = parse_macro_input!(input as LitStr);
    match audit_source(&lit.value()) {
        Ok(()) => quote!(()).into(),
        Err(msg) => {
            let m = format!("audit rejected: {msg}");
            quote!(compile_error!(#m);).into()
        }
    }
}

/// Private — parses a single source string and audits its payload structs +
/// payload enums. Not exported (proc-macro crates cannot export non-macro fns).
///
/// Returns `Ok(())` if the source is acceptable, `Err(detail)` otherwise.
fn audit_source(src: &str) -> Result<(), String> {
    let file = syn::parse_file(src).map_err(|e| format!("parse error: {e}"))?;

    // First pass: enumerate every enum defined in the file. The second pass
    // verifies that any enum referenced as a payload field is `#[non_exhaustive]`.
    let mut enums_by_name: HashMap<String, bool> = HashMap::default();
    for item in &file.items {
        if let syn::Item::Enum(e) = item {
            let has_non_exhaustive = e.attrs.iter().any(|a| a.path().is_ident("non_exhaustive"));
            enums_by_name.insert(e.ident.to_string(), has_non_exhaustive);
        }
    }

    // The whitelist of acceptable primitive type segment names (last segment).
    let allowed_types: HashSet<&str> = [
        "bool",
        "i32",
        "i64",
        "u32",
        "u64",
        "f32",
        "f64",
        "Verified",
        "PiiTagged",
        "SessionId",
    ]
    .into_iter()
    .collect();

    for item in &file.items {
        let syn::Item::Struct(s) = item else { continue };
        let name = s.ident.to_string();
        if !name.ends_with("Payload") {
            continue;
        }

        // (1) #[serde(deny_unknown_fields)] is required.
        let has_deny = s.attrs.iter().any(|a| {
            if !a.path().is_ident("serde") {
                return false;
            }
            let mut found = false;
            // Best-effort: parse the meta list and look for the bareword.
            let _ = a.parse_nested_meta(|m| {
                if m.path.is_ident("deny_unknown_fields") {
                    found = true;
                }
                Ok(())
            });
            found
        });
        if !has_deny {
            return Err(format!(
                "{name}: missing #[serde(deny_unknown_fields)] (spec §7 line 768)"
            ));
        }

        // (2) Every field must be a whitelisted type.
        let syn::Fields::Named(fields) = &s.fields else {
            return Err(format!("{name}: payload structs must use named fields"));
        };
        for field in &fields.named {
            let field_name = field
                .ident
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default();
            let type_ok =
                field_type_is_allowed(&field.ty, &field_name, &allowed_types, &enums_by_name)?;
            if !type_ok {
                let ty = &field.ty;
                let ty_str = quote!(#ty).to_string();
                return Err(format!(
                    "{name}::{field_name}: disallowed field type `{}` — \
                    user-derived strings must use Verified or PiiTagged \
                    (spec §7 line 772-774)",
                    ty_str.trim()
                ));
            }
        }
    }
    Ok(())
}

fn field_type_is_allowed(
    ty: &syn::Type,
    field_name: &str,
    allowed_types: &HashSet<&str>,
    enums_by_name: &HashMap<String, bool>,
) -> Result<bool, String> {
    let syn::Type::Path(p) = ty else {
        return Ok(false);
    };
    let segs = &p.path.segments;
    let last = segs
        .last()
        .expect("type path must have at least one segment");
    let ident = last.ident.to_string();

    // Special-case 1: the LoopIterationPayload `extra: serde_json::Value` field.
    if field_name == "extra" {
        // Must be exactly `serde_json::Value` (full path) OR `Value` (alias).
        let path_str = segs
            .iter()
            .map(|s| s.ident.to_string())
            .collect::<Vec<_>>()
            .join("::");
        if path_str == "serde_json::Value" || ident == "Value" {
            return Ok(true);
        }
        return Ok(false);
    }
    // serde_json::Value is otherwise disallowed.
    let path_str_check = segs
        .iter()
        .map(|s| s.ident.to_string())
        .collect::<Vec<_>>()
        .join("::");
    if path_str_check == "serde_json::Value" || ident == "Value" {
        // Not in the `extra` slot -> reject.
        return Err(format!(
            "field `{field_name}`: serde_json::Value is only allowed for the \
            free-form `extra` field name"
        ));
    }

    // Whitelist primitives + Verified / PiiTagged / SessionId by leaf name.
    if allowed_types.contains(ident.as_str()) {
        return Ok(true);
    }

    // Option<T> / Vec<T> — unwrap one level and recurse.
    if ident == "Option" || ident == "Vec" {
        let syn::PathArguments::AngleBracketed(args) = &last.arguments else {
            return Ok(false);
        };
        let Some(syn::GenericArgument::Type(inner)) = args.args.first() else {
            return Ok(false);
        };
        return field_type_is_allowed(inner, field_name, allowed_types, enums_by_name);
    }

    // Enum used as field — must be locally defined AND `#[non_exhaustive]`.
    if let Some(non_exh) = enums_by_name.get(&ident) {
        if !non_exh {
            return Err(format!(
                "field `{field_name}` uses enum `{ident}` which lacks #[non_exhaustive] \
                (spec §7 line 785-786)"
            ));
        }
        return Ok(true);
    }

    Ok(false)
}
