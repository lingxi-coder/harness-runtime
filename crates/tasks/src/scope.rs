//! Typed Local App workflow authority (design §18 Phase -1 step 8).
//!
//! # The problem this type replaces
//!
//! Three things used to derive Local App authority from a workflow's NAME:
//! `registry.rs`'s `find_nonterminal_local_app_workflows` (delete guard) and
//! `handlers/local_workflow.rs`'s `requires_workspace_lease` both asked "is
//! `workflow_id` a member of this crate's (now-deleted) `LOCAL_APP_BUILD_WORKFLOWS`?".
//! A `workflow_id`/`meta.name` is a string a *caller* supplies when launching
//! a workflow -- so any custom workflow that happens to reuse one of those
//! names got the same answer as the real one. Both now read a scope instead.
//!
//! Design §18 Phase -1 step 9 additionally called for the app's stored
//! `workflowModel` (`local_apps::types`'s `workflow_model` field, accepted on
//! the `scaffold` MCP tool and persisted by `LocalAppsService::commit_scaffold`)
//! to key a `workflowModel` default the same way. As of this correction that
//! step is unimplemented: `workflow_model` is written and preserved across
//! scaffold/update, but `git grep -n 'workflow_model\|workflowModel'` across
//! the tree turns up no production reader of the persisted value, and
//! `tool_workflow::BuiltinWorkflowDescriptor` (`tools/workflow/src/builtins.rs`)
//! has no `is_local_app_build` field or method -- a name a stale copy of this
//! very comment once claimed existed. Do not restate either "keyed on the
//! script's identity" or "keys on the name" here until a real reader exists;
//! today there is none.
//!
//! The three sibling comments in `apps/engine-mobile` that used to repeat the
//! same non-existent symbol (`local_apps_build.rs`'s test-module tail,
//! `tests/component_literal_scan.rs`'s module doc and
//! `tests/component_literal_allowlist.txt`'s `tasks/src` section) have all
//! been corrected; the scan one additionally named
//! `BUILTIN_WORKFLOWS.local_app_build_workflow_names()`, which does not exist
//! either. What replaced `tool_workflow`'s array is nothing: Phase 9 moved the
//! Local App workflows into the plugin bundle, so `tool_workflow` no longer
//! answers "which workflows build a Local App" at all, and the only surviving
//! enumeration of those basenames is `component_literal_scan.rs`'s
//! `local_app_workflow_basenames()`.
//!
//! [`LocalAppWorkflowTaskScope`] is the replacement authority token: the Host
//! (the composition binding that just resolved a real `LocalAppPluginBinding`
//! handle) mints one by calling the constructor for the capability slot it
//! actually resolved, and the guards read the scope instead of a name.
//!
//! # What this type guarantees
//!
//! 1. **Every construction path is a purpose constructor.** The only ways to
//!    obtain a value are [`LocalAppWorkflowTaskScope::for_build`],
//!    [`LocalAppWorkflowTaskScope::for_use_test`] and
//!    [`LocalAppWorkflowTaskScope::for_mcp_authoring`]. There is no
//!    `Default`, no `From`/`FromStr`/`TryFrom`, no `Deserialize` (see below),
//!    no public field and no `&mut` accessor, and both fields are private --
//!    so callers must choose a purpose explicitly at a constructor call and
//!    cannot edit it after the fact. This type does not prove that the Host
//!    made that choice; provenance remains the Host integration's obligation.
//! 2. **`app_id` is well formed.** Every constructor is fallible and rejects
//!    an `app_id` that does not match `^[a-z0-9][a-z0-9-]{0,53}$` -- the same
//!    grammar `local_apps::ids::is_valid_app_id` enforces before an id is
//!    ever used in a path. `tasks` deliberately does NOT depend on
//!    `local-apps` for this (see `MIRRORED GRAMMAR` below), so the check is a
//!    local copy of a 6-line predicate, not a shared type.
//!
//! # What this type does NOT guarantee
//!
//! **Ownership.** Well-formedness is not provenance. `for_build("victim")`
//! succeeds for any well-formed id, including one belonging to somebody
//! else's app, and the type has no way to know which app the caller is
//! entitled to. Design §8.1 requires that a custom workflow get nothing
//! "即使伪造 `meta.name` 或 `args.app_id`" -- the `meta.name` half is closed
//! by construction (guarantee 1); the `args.app_id` half is the **Host's**
//! obligation, and this type cannot discharge it. Passing `args.app_id`
//! straight into a constructor hands a hostile app the victim's lease and
//! delete guard, and nothing in this module will notice.
//!
//! What the Host owes is not "never let the string originate in `args`" --
//! the app id has to be named somewhere, and a
//! `LocalAppPluginBinding` handle names a WORKFLOW, not an app. What it owes
//! is that the id be one it RESOLVED rather than one it was told: before
//! minting, the Host must have established from its own state that the id
//! names a real, fully scaffolded app whose materialized manifest and binding
//! authorize exactly the workflow that is about to run, and that the script
//! is that workflow rather than something wearing its name. `engine-mobile`'s
//! `apply_materialized_local_app_collections_with_identity` is one seam that
//! does this; its constructor call documents the checks that stand between
//! `args` and this type. Live launch enrichment and
//! `resolve_adopted_local_app_scope` mint Build/UseTest scopes only after
//! verifying the plugin script and Host-owned app state. MCP authoring follows
//! the same "resolved, not told" duty at its launch mint.
//!
//! It also does not guarantee the id names an app that exists, or that the
//! app is in a state where the purpose makes sense. Those are lookups, and
//! lookups belong to whoever holds the store.
//!
//! # serde surface
//!
//! [`LocalAppWorkflowTaskScope`] implements `Serialize` and **not**
//! `Deserialize`, and that asymmetry is deliberate:
//!
//! - `Serialize` is legitimate: persisting a task's scope alongside its state
//!   is what step 8 asks for, and writing a scope out cannot create authority
//!   that did not already exist.
//! - `Deserialize` would be a public, name-accepting constructor. A
//!   `#[derive(Deserialize)]` ignores field privacy: it builds the struct
//!   from any `{"app_id": …, "purpose": …}` object, which is exactly the
//!   "authority from a caller-supplied string" this type exists to remove,
//!   and it does so invisibly -- the derive is one word and has no call site
//!   to review. Routing it through a validating `#[serde(try_from = …)]`
//!   does not rescue it either: the only thing such a conversion can check is
//!   the grammar above, and a forger supplies a perfectly well-formed victim
//!   id. Validation would buy nothing and would advertise a safety it does
//!   not have.
//!
//! So the read direction is left as a compile error on purpose. When a
//! persistence seam that reads scopes back actually exists, restore
//! explicitly AT that seam -- `#[serde(skip)]` the field and have the Host
//! re-mint the scope from the binding it resolves on load, or give the store
//! module its own named wire struct whose conversion says out loud whose
//! bytes it trusts. Adding `Deserialize` back here instead would silently
//! undo guarantee 1 for every present and future holder.
//! The test `scope_type_does_not_implement_deserialize` below pins this.
//!
//! [`LocalAppWorkflowPurpose`] keeps `Deserialize`: a purpose on its own
//! carries no authority (it names no app), and the store side needs to read
//! the discriminant back. It is the *pair* that is authority.
//!
//! # MIRRORED GRAMMAR
//!
//! `local_apps::ids::is_valid_app_id` is the original. `tasks` does not take
//! a PRODUCTION dependency on `local-apps`: both crates classify as `engine`
//! so `scripts/checks/check_deps.py` would permit the edge, but `local-apps` pulls
//! bundled SQLite, vendored libgit2 and two tree-sitter grammars into
//! `tasks`, and `tasks` has five dependents (including `cron` and
//! `coordinator`, which build none of that today) for what is a six-line
//! predicate. The copy is pinned by the test
//! `app_id_grammar_matches_the_local_apps_corpus` below, whose corpus is
//! the one from `local_apps::ids`'s own tests.
//!
//! A DEV-dependency is a different tradeoff: `check_deps.py` excludes
//! dev-dependencies from its edge check, so it costs the five dependents
//! nothing, and it lets `app_id_grammar_agrees_with_local_apps_ids` call the
//! real `local_apps::ids::is_valid_app_id` directly instead of trusting that
//! the copied corpus above was transcribed correctly -- so `tasks/Cargo.toml`
//! carries that dev-dependency and both tests run.
//!
//! # Scope of this module
//!
//! This module only introduces the type. The two `tasks` call sites above now
//! read a scope, which reaches a task row through
//! [`crate::task_trait::TaskSpawnInput::LocalWorkflow`]'s `scope` field. The
//! design §18 Phase -1 step 9 `workflowModel` default has no implementation
//! to migrate onto a scope yet -- see the module-level note above.

use serde::{Deserialize, Serialize};

/// Why a Local App workflow is running. The three purposes named by design
/// §18 Phase -1 step 8 / §8.1's `LocalAppWorkflowTaskScope` sketch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LocalAppWorkflowPurpose {
    /// Builds/updates the app's workspace. The only purpose that takes the
    /// exclusive workspace permission lease.
    Build,
    /// Runs the app's use-test workflow against an already-built workspace.
    UseTest,
    /// Runs the MCP-authoring workflow for the app.
    McpAuthoring,
}

/// Longest accepted app id -- the `{0,53}` tail plus the leading character,
/// matching `local_apps::ids::APP_ID_MAX_LEN`.
const APP_ID_MAX_LEN: usize = 54;

/// True iff `id` matches `^[a-z0-9][a-z0-9-]{0,53}$`.
///
/// A local mirror of `local_apps::ids::is_valid_app_id`; see the module docs'
/// `MIRRORED GRAMMAR` section for why it is a copy and what pins it.
fn is_well_formed_app_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    if bytes.is_empty() || bytes.len() > APP_ID_MAX_LEN {
        return false;
    }
    let first_ok = bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit();
    first_ok
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// A scope constructor was handed an `app_id` that is not a well-formed app
/// id. Says nothing about whether the caller *owns* a well-formed id -- see
/// the module docs' "What this type does NOT guarantee".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid app id {app_id:?}: must match ^[a-z0-9][a-z0-9-]{{0,53}}$")]
pub struct MalformedAppId {
    app_id: String,
}

impl MalformedAppId {
    /// The rejected string, for logging. Kept verbatim so a log line shows
    /// what was actually attempted.
    pub fn app_id(&self) -> &str {
        &self.app_id
    }
}

/// Caller-unsettable Local App workflow authority: which app, and why this
/// workflow run is allowed to touch it.
///
/// Read the module docs before using this: it guarantees that the *purpose*
/// was chosen through an explicit constructor and that the *app id* is well
/// formed. It deliberately guarantees neither that the Host minted it nor
/// that the caller was entitled to that app id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct LocalAppWorkflowTaskScope {
    app_id: String,
    purpose: LocalAppWorkflowPurpose,
}

impl LocalAppWorkflowTaskScope {
    /// The single private mint. Every public constructor differs only in the
    /// purpose it hard-codes, so there is exactly one place where the
    /// grammar check can be forgotten.
    fn checked(
        app_id: impl Into<String>,
        purpose: LocalAppWorkflowPurpose,
    ) -> Result<Self, MalformedAppId> {
        let app_id = app_id.into();
        if !is_well_formed_app_id(&app_id) {
            return Err(MalformedAppId { app_id });
        }
        Ok(Self { app_id, purpose })
    }

    /// Scope for the workflow that builds/updates `app_id`'s workspace.
    /// Design: "只有 Build 取得 exclusive workspace permission lease" -- see
    /// [`Self::requires_workspace_lease`].
    ///
    /// # Errors
    /// [`MalformedAppId`] if `app_id` does not match the app-id grammar.
    pub fn for_build(app_id: impl Into<String>) -> Result<Self, MalformedAppId> {
        Self::checked(app_id, LocalAppWorkflowPurpose::Build)
    }

    /// Scope for `app_id`'s use-test workflow run.
    ///
    /// # Errors
    /// [`MalformedAppId`] if `app_id` does not match the app-id grammar.
    pub fn for_use_test(app_id: impl Into<String>) -> Result<Self, MalformedAppId> {
        Self::checked(app_id, LocalAppWorkflowPurpose::UseTest)
    }

    /// Scope for `app_id`'s MCP-authoring workflow run.
    ///
    /// # Errors
    /// [`MalformedAppId`] if `app_id` does not match the app-id grammar.
    pub fn for_mcp_authoring(app_id: impl Into<String>) -> Result<Self, MalformedAppId> {
        Self::checked(app_id, LocalAppWorkflowPurpose::McpAuthoring)
    }

    /// The app this scope grants authority over.
    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    /// Why this workflow run is allowed to touch `app_id`.
    pub fn purpose(&self) -> LocalAppWorkflowPurpose {
        self.purpose
    }

    /// Design: "只有 Build 取得 exclusive workspace permission lease" -- only
    /// a `Build`-purpose scope should let its holder take the app's
    /// workspace permission lease.
    pub fn requires_workspace_lease(&self) -> bool {
        matches!(self.purpose, LocalAppWorkflowPurpose::Build)
    }

    /// Design: "`tasks` 对所有三种 purpose 都让 App delete guard 按 app ID
    /// 阻塞" -- every purpose blocks deleting the app while the workflow is
    /// non-terminal, not just `Build`.
    ///
    /// Reachability, so nobody has to grep for it — re-derived at HEAD, because
    /// the previous version of this note was stale in BOTH directions:
    ///
    /// * [`Self::for_build`] — Host-verified launch and restart-adoption
    ///   mints in `engine-mobile/src/workflow_support.rs`.
    /// * [`Self::for_mcp_authoring`] — ONE production mint, `launch`
    ///   (`workflow_support.rs`).
    /// * [`Self::for_use_test`] — Host-verified plugin launches and
    ///   restart-adoption both mint this purpose, never custom workflows.
    ///
    /// So "every purpose blocks delete" is enforced and exercised for all
    /// three purposes; `registry.rs`'s
    /// `find_nonterminal_local_app_workflows` carries the same note, and
    /// `registry_test.rs` pins each purpose at that guard.
    pub fn blocks_delete(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Autoref specialization: `implements_deserialize!(T)` is `true` iff
    /// `T: DeserializeOwned`, WITHOUT requiring the bound at the call site.
    ///
    /// The specialized impl sits on `&Probe<T>` behind the bound and the
    /// fallback on `Probe<T>`. The call site passes `&&Probe<T>`: method
    /// resolution walks the deref chain `&&Probe<T>` -> `&Probe<T>` ->
    /// `Probe<T>` and stops at the first step that has a candidate, so the
    /// bounded impl wins when it applies and is simply not a candidate when
    /// it does not. (One `&` is not enough -- the method takes `&self`, so
    /// the fallback would already match at the first step.)
    /// This is the only way to assert the ABSENCE of a trait impl in a
    /// regular test -- a plain `serde_json::from_str::<T>` assertion cannot
    /// be written at all once the impl is gone, so it could not be a
    /// permanent regression test.
    mod de_probe {
        use serde::de::DeserializeOwned;
        use std::marker::PhantomData;

        pub struct Probe<T>(pub PhantomData<T>);

        pub trait ProbeFallback {
            fn implements_deserialize(&self) -> bool {
                false
            }
        }
        impl<T> ProbeFallback for Probe<T> {}

        pub trait ProbeSpecialized {
            fn implements_deserialize(&self) -> bool {
                true
            }
        }
        impl<'a, T: DeserializeOwned> ProbeSpecialized for &'a Probe<T> {}
    }

    macro_rules! implements_deserialize {
        ($t:ty) => {{
            #[allow(unused_imports)]
            use de_probe::{ProbeFallback as _, ProbeSpecialized as _};
            (&&de_probe::Probe::<$t>(::std::marker::PhantomData)).implements_deserialize()
        }};
    }

    /// R1. `#[derive(Deserialize)]` is a public constructor that accepts an
    /// arbitrary `{"app_id": …, "purpose": …}` object regardless of field
    /// privacy, so it hands out the exact authority the type exists to
    /// withhold. It must stay absent.
    ///
    /// The two controls are the point: `String` and
    /// [`LocalAppWorkflowPurpose`] prove the probe can answer `true`, so a
    /// `false` for the scope is a measurement and not a probe that never
    /// fires. Re-add `#[derive(Deserialize)]` to
    /// [`LocalAppWorkflowTaskScope`] and this test goes red.
    #[test]
    fn scope_type_does_not_implement_deserialize() {
        assert!(
            implements_deserialize!(String),
            "probe control: String does implement DeserializeOwned"
        );
        assert!(
            implements_deserialize!(LocalAppWorkflowPurpose),
            "probe control: purpose keeps Deserialize on purpose (see module docs)"
        );

        assert!(
            !implements_deserialize!(LocalAppWorkflowTaskScope),
            "LocalAppWorkflowTaskScope must NOT implement Deserialize: a derive \
             would let `serde_json::from_str(r#\"{{\"app_id\":\"victim\",\
             \"purpose\":\"Build\"}}\"#)` mint a Build scope for any app. See the \
             module docs' `serde surface` section before changing this."
        );
    }

    /// The write direction stays available -- persistence genuinely needs it,
    /// and serialising a scope cannot create authority.
    #[test]
    fn scope_still_serializes_for_persistence() {
        let scope = LocalAppWorkflowTaskScope::for_build("app-1").expect("well-formed id");
        let json = serde_json::to_string(&scope).expect("serializes");
        assert_eq!(json, r#"{"app_id":"app-1","purpose":"Build"}"#);
    }

    /// A purpose comes only from the explicit typed constructor, never from a
    /// string that could collide with a real workflow's `meta.name`.
    ///
    /// A hostile caller can still supply a string that *looks* like it could
    /// be a workflow basename. Feed such a string into the only public string
    /// input this type accepts (`app_id`) via a *non*-Build constructor, and
    /// purpose must stay whatever the caller selected explicitly -- the
    /// string never gets reinterpreted as workflow authority. Well-formedness
    /// is not the defence here; the absence of a name-taking constructor is.
    #[test]
    fn scope_purpose_is_explicit_not_derived_from_meta_name() {
        let workflow_looking_id = "plugin-build-name";

        let scope = LocalAppWorkflowTaskScope::for_use_test(workflow_looking_id)
            .expect("workflow-looking strings that satisfy app-id grammar stay ordinary app ids");

        assert_eq!(scope.app_id(), workflow_looking_id);
        assert_eq!(scope.purpose(), LocalAppWorkflowPurpose::UseTest);
        assert!(!scope.requires_workspace_lease());
    }

    /// R2, half one. Every constructor rejects an id that could not be an
    /// app id -- path traversal, separators, uppercase, empty, over-long.
    /// All three constructors are checked because the grammar check lives in
    /// one private mint and a future refactor could bypass it for one of
    /// them.
    #[test]
    fn every_constructor_rejects_a_malformed_app_id() {
        let too_long = "a".repeat(APP_ID_MAX_LEN + 1);
        for bad in [
            "",
            "../evil",
            "..",
            "a/b",
            "a\\b",
            "a.b",
            "-leading-dash",
            "Upper",
            "under_score",
            "spa ce",
            "über",
            too_long.as_str(),
        ] {
            for made in [
                LocalAppWorkflowTaskScope::for_build(bad),
                LocalAppWorkflowTaskScope::for_use_test(bad),
                LocalAppWorkflowTaskScope::for_mcp_authoring(bad),
            ] {
                let err = made.expect_err(&format!("expected {bad:?} to be rejected"));
                assert_eq!(err.app_id(), bad);
            }
        }
    }

    /// R2, half two -- stated so the limit is not mistaken for a guarantee.
    /// A well-formed id belonging to somebody else is accepted, because this
    /// type cannot know who owns what. The Host must source `app_id` from a
    /// resolved binding, never from caller-supplied args.
    #[test]
    fn a_well_formed_victim_app_id_is_still_accepted() {
        let victim = LocalAppWorkflowTaskScope::for_build("victim-app")
            .expect("`victim-app` is well formed; ownership is the Host's to check");
        assert_eq!(victim.app_id(), "victim-app");
        assert!(victim.requires_workspace_lease());
    }

    /// Pins the mirrored grammar against the corpus `local_apps::ids`'s own
    /// tests use. `tasks` cannot reach that crate (module docs,
    /// `MIRRORED GRAMMAR`), so drift shows up as a difference between these
    /// two lists rather than as a link error.
    #[test]
    fn app_id_grammar_matches_the_local_apps_corpus() {
        let max_len = "a".repeat(APP_ID_MAX_LEN);
        for id in ["a", "0", "abc-123", "9-", max_len.as_str()] {
            assert!(is_well_formed_app_id(id), "expected valid: {id}");
        }

        let too_long = "a".repeat(APP_ID_MAX_LEN + 1);
        for id in [
            "",
            "-leading-dash",
            "Upper",
            "under_score",
            "spa ce",
            "..",
            "../evil",
            "a/b",
            "a\\b",
            "a.b",
            "über",
            too_long.as_str(),
        ] {
            assert!(!is_well_formed_app_id(id), "expected invalid: {id}");
        }
    }

    /// Real cross-crate agreement, as a DEV-dependency (see module docs'
    /// `MIRRORED GRAMMAR`): calls the ACTUAL `local_apps::ids::is_valid_app_id`
    /// side by side with this module's mirrored `is_well_formed_app_id`
    /// across one shared corpus, so a future edit to either grammar that
    /// silently drifts from the other fails HERE, not by two independently
    /// "passing" tests that quietly stopped agreeing.
    #[test]
    fn app_id_grammar_agrees_with_local_apps_ids() {
        let max_len = "a".repeat(APP_ID_MAX_LEN);
        let valid = ["a", "0", "abc-123", "9-", max_len.as_str()];
        let too_long = "a".repeat(APP_ID_MAX_LEN + 1);
        let invalid = [
            "",
            "-leading-dash",
            "Upper",
            "under_score",
            "spa ce",
            "..",
            "../evil",
            "a/b",
            "a\\b",
            "a.b",
            "über",
            too_long.as_str(),
        ];
        for id in valid {
            assert_eq!(
                is_well_formed_app_id(id),
                local_apps::ids::is_valid_app_id(id),
                "grammars disagree on {id:?} (expected both to accept)"
            );
            assert!(is_well_formed_app_id(id), "expected valid: {id}");
        }
        for id in invalid {
            assert_eq!(
                is_well_formed_app_id(id),
                local_apps::ids::is_valid_app_id(id),
                "grammars disagree on {id:?} (expected both to reject)"
            );
            assert!(!is_well_formed_app_id(id), "expected invalid: {id}");
        }
    }

    #[test]
    fn only_build_purpose_requires_a_workspace_lease() {
        assert!(LocalAppWorkflowTaskScope::for_build("app-1")
            .expect("valid")
            .requires_workspace_lease());
        assert!(!LocalAppWorkflowTaskScope::for_use_test("app-1")
            .expect("valid")
            .requires_workspace_lease());
        assert!(!LocalAppWorkflowTaskScope::for_mcp_authoring("app-1")
            .expect("valid")
            .requires_workspace_lease());
    }

    #[test]
    fn every_purpose_blocks_delete() {
        assert!(LocalAppWorkflowTaskScope::for_build("app-1")
            .expect("valid")
            .blocks_delete());
        assert!(LocalAppWorkflowTaskScope::for_use_test("app-1")
            .expect("valid")
            .blocks_delete());
        assert!(LocalAppWorkflowTaskScope::for_mcp_authoring("app-1")
            .expect("valid")
            .blocks_delete());
    }

    #[test]
    fn purpose_and_app_id_round_trip_distinct_apps() {
        let build = LocalAppWorkflowTaskScope::for_build("app-a").expect("valid");
        let use_test = LocalAppWorkflowTaskScope::for_use_test("app-b").expect("valid");
        let mcp = LocalAppWorkflowTaskScope::for_mcp_authoring("app-c").expect("valid");

        assert_eq!(build.app_id(), "app-a");
        assert_eq!(use_test.app_id(), "app-b");
        assert_eq!(mcp.app_id(), "app-c");
        assert_ne!(build.purpose(), use_test.purpose());
        assert_ne!(use_test.purpose(), mcp.purpose());
    }
}
