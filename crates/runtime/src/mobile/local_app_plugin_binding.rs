//! Composition binding from a Local App build target to the one plugin
//! workflow authorized to build it.
//!
//! A build target maps to a workflow NAME string, and that string is then
//! used as authority: `workflow_support::apply_materialized_local_app_collections_with_identity`
//! resolves a required workflow id for the app's pinned runtime profile and
//! REFUSES a caller-selected workflow that does not match it. That refusal is
//! security-relevant — without it a caller could point any app at any
//! workflow's collection/persistence contract — and it is pinned by the
//! characterization tests in this module plus
//! `workflow_support::tests::local_app_workflow_routes_each_published_profile_to_its_matching_workflow`
//! and its two refusal siblings.
//!
//! P-1.1 wrapped the map's output in a typed [`LocalAppWorkflowHandle`] so the
//! Host (the `apply_materialized_*` seam) stopped doing its own string
//! comparison and error formatting: it asks this binding to `resolve` a build
//! target and `enforce` the result against the caller's launched workflow id.
//! P-1.1 left the map itself (`required_workflow_id_for`) inside
//! `workflow_support.rs`, delegated to from here. P-1.3 moves the map INTO
//! this module: `workflow_support.rs` no longer contains a Local App workflow
//! name of any kind, so this binding is now the one place a build target maps
//! to its required workflow id, both in the sense that the Host never
//! compares the strings itself (Phase -1's original point) and in the sense
//! that no name lives outside this file (§18 Phase -1 step 4 / §19.3's gate).

use local_app_service::app_build::LocalAppBuildTarget;

/// Canonical Plugin-qualified workflow identities.  Keeping these reserved
/// names in the composition binding lets the workflow launcher avoid a second
/// production-side identity table (and keeps project workflows that merely
/// shadow the names from gaining Host authority).
pub(crate) const PLUGIN_BUILD_WORKFLOW_ID: &str = "lingxi-local-app:local-app-build";
pub(crate) const PLUGIN_USE_TEST_WORKFLOW_ID: &str = "lingxi-local-app:local-app-use-test";
pub(crate) const PLUGIN_MCP_AUTHORING_WORKFLOW_ID: &str =
    "lingxi-local-app:local-app-mcp-authoring";

pub(crate) fn is_plugin_workflow_id(workflow_id: &str) -> bool {
    matches!(
        workflow_id,
        PLUGIN_BUILD_WORKFLOW_ID | PLUGIN_USE_TEST_WORKFLOW_ID | PLUGIN_MCP_AUTHORING_WORKFLOW_ID
    )
}

/// Every [`LocalAppBuildTarget`] variant currently requires the same
/// workflow, so this has no reason to disagree with any of them today — but
/// it is written as a wildcard-free `match`, not `let _ = build_target;
/// PLUGIN_BUILD_WORKFLOW_ID`, on purpose: the module doc and
/// `build_targets!`'s doc comment both claim that adding a new
/// `LocalAppBuildTarget` variant forces this function to gain an arm
/// (`error[E0004]` otherwise). A wildcard or an unconditional constant would
/// make that claim false — a new variant would compile silently and inherit
/// `PLUGIN_BUILD_WORKFLOW_ID` whether or not that is still correct.
fn required_workflow_id_for(build_target: LocalAppBuildTarget) -> &'static str {
    match build_target {
        LocalAppBuildTarget::ReactDomR4
        | LocalAppBuildTarget::Canvas2dR4
        | LocalAppBuildTarget::Three3dR4
        | LocalAppBuildTarget::Phaser2dR4 => PLUGIN_BUILD_WORKFLOW_ID,
    }
}

/// A resolved workflow identity for a Local App build target.
///
/// Opaque by construction, not merely by convention: the inner id has no
/// `pub(crate)` accessor, so nothing outside this module can compare it
/// against a bare string literal even if it wanted to. The point of routing
/// through [`LocalAppPluginBinding`] is that the Host no longer picks or
/// compares workflow names itself — it asks the binding to do it and consumes
/// the typed `Result`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LocalAppWorkflowHandle(&'static str);

impl LocalAppWorkflowHandle {
    /// The wire workflow id this handle currently resolves to.
    ///
    /// Reachable only from inside this module: it exists so `enforce` can
    /// compare a caller-selected id against it and name it in the refusal.
    /// There is deliberately NO accessor handing the handle (or its id) out
    /// to the Host — an earlier revision had one, used by nothing but this
    /// module's own tests, and it was both `dead_code` on the non-test
    /// `--lib` target and an invitation to go back to comparing raw strings
    /// at the seam. Callers get `enforce`'s `Result`, not the string.
    fn id(self) -> &'static str {
        self.0
    }
}

/// A composition binding from a Local App's pinned runtime profile (via its
/// [`LocalAppBuildTarget`]) to the one workflow authorized to build it.
pub(crate) struct LocalAppPluginBinding {
    handle: LocalAppWorkflowHandle,
}

impl LocalAppPluginBinding {
    /// Resolve the workflow handle a `build_target` is pinned to.
    ///
    /// Reads the build-target → workflow-id map owned by this module
    /// ([`required_workflow_id_for`]) and wraps its output in a typed handle
    /// so callers never touch the string themselves.
    pub(crate) fn resolve(build_target: LocalAppBuildTarget) -> Self {
        Self {
            handle: LocalAppWorkflowHandle(required_workflow_id_for(build_target)),
        }
    }

    /// Refuse a caller-selected workflow id that does not match this
    /// binding's resolved handle.
    ///
    /// Preserves the pre-Phase -1 inline refusal byte-for-byte: same app id,
    /// same pinned runtime-profile family, same required and caller-selected
    /// workflow ids named in the message.
    pub(crate) fn enforce(
        &self,
        app_id: &str,
        family: local_apps::AppRuntimeProfile,
        launched_workflow_id: &str,
    ) -> Result<(), tool_workflow::WorkflowLaunchError> {
        if launched_workflow_id != self.handle.id() {
            return Err(tool_workflow::WorkflowLaunchError(format!(
                "app {app_id:?} is pinned to runtime profile {family}, which must use {}; \
                 refusing caller-selected workflow {launched_workflow_id}",
                self.handle.id()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generates [`expected_binding`] and [`ALL_BUILD_TARGETS`] from ONE list
    /// so they cannot drift apart.
    ///
    /// Before this task each was maintained separately: `expected_binding`
    /// was its own wildcard-free `match`, and `ALL_BUILD_TARGETS` was a
    /// hand-typed `[LocalAppBuildTarget; 5]` array beside it. Adding a sixth
    /// `LocalAppBuildTarget` variant forces `expected_binding` (and
    /// `required_workflow_id_for`) to gain an arm — proven: it is
    /// `error[E0004]` otherwise — but filling in ONLY that arm compiled and
    /// passed, because nothing tied the array's length to the variant count:
    /// the new mapping would ship with the whole suite green and zero
    /// characterization coverage of it. This macro closes that gap: extending
    /// the invocation below to cover a new variant (still forced, since the
    /// generated `expected_binding` match is still wildcard-free) extends
    /// `ALL_BUILD_TARGETS` in the very same edit, so the loop in
    /// `resolve_pins_the_literal_workflow_id_of_every_build_target` is
    /// guaranteed to exercise it.
    macro_rules! build_targets {
        ($($variant:ident => ($family:expr, $required:expr)),+ $(,)?) => {
            /// Every build target the suite below exercises. Generated by
            /// `build_targets!` in lockstep with [`expected_binding`] — see
            /// that macro's doc comment for why this is not a hand-typed
            /// array.
            const ALL_BUILD_TARGETS: &[LocalAppBuildTarget] =
                &[$(LocalAppBuildTarget::$variant),+];

            /// The runtime-profile family and the plugin workflow id every
            /// build target must resolve to.
            fn expected_binding(
                target: LocalAppBuildTarget,
            ) -> (local_apps::AppRuntimeProfile, &'static str) {
                match target {
                    $(LocalAppBuildTarget::$variant => ($family, $required),)+
                }
            }
        };
    }

    build_targets! {
        ReactDomR4 => (local_apps::AppRuntimeProfile::ReactDom, PLUGIN_BUILD_WORKFLOW_ID),
        Canvas2dR4 => (local_apps::AppRuntimeProfile::Canvas2d, PLUGIN_BUILD_WORKFLOW_ID),
        Three3dR4 => (local_apps::AppRuntimeProfile::Three3d, PLUGIN_BUILD_WORKFLOW_ID),
        Phaser2dR4 => (local_apps::AppRuntimeProfile::Phaser2d, PLUGIN_BUILD_WORKFLOW_ID),
    }

    #[test]
    fn resolve_pins_the_literal_workflow_id_of_every_build_target() {
        for &target in ALL_BUILD_TARGETS {
            let (family, required) = expected_binding(target);

            // The `family` column, unlike `required`, was pinned by nothing.
            // It is only fed to `enforce` and then re-used to BUILD the
            // expected refusal message, so both assertions below predict the
            // value they were handed: giving `Three3dR4` the flatly wrong
            // family `Phaser2d` left the whole suite green. Nothing else in
            // the suite caught it either, because `Three3d`, `Phaser2d` and
            // `Babylon3d` all require the SAME workflow id — the `required`
            // column cannot disambiguate them.
            //
            // So cross the column against a source that is not itself.
            // `LocalAppBuildTarget::template_id` is the PRODUCTION target ->
            // scaffold-id map (`app_build`, the id reported back to the
            // model and serialized by `broker`), and
            // `AppRuntimeProfile::as_str` is the production family -> wire
            // spelling (`local-apps`). Neither is derived from this table, and
            // the expected value is DERIVED — the kebab-cased wire spelling —
            // rather than a second hand-written target -> family match, so
            // this stays honest for a sixth variant without a new arm here:
            // the only arm that variant needs is the production `template_id`
            // one the exhaustive match already forces.
            //
            // Not `LocalAppBuildTarget::runtime_profile()`, the obvious second
            // map: it is a module-private `#[cfg(test)]` method of
            // `app_build` and is unreachable from this module. Not the
            // `current_binding_for_family` + `from_runtime_binding` round trip
            // either: `available_contracts()` publishes four families, so that
            // path cannot reach `Babylon3dR1` at all — the one row this suite
            // is otherwise the sole cover for.
            let scaffold_id = target.template_id();
            let family_named_by_scaffold_id = scaffold_id.split('/').nth(1).unwrap_or_else(|| {
                panic!("{target:?} scaffold id {scaffold_id:?} has no family segment")
            });
            assert_eq!(
                family_named_by_scaffold_id,
                family.as_str().replace('_', "-"),
                "{target:?} is listed in `build_targets!` under runtime profile family {family}, \
                 but its production scaffold id {scaffold_id:?} names a different family"
            );

            let binding = LocalAppPluginBinding::resolve(target);

            binding
                .enforce("demo1234", family, required)
                .unwrap_or_else(|error| {
                    panic!("{target:?} must resolve to {required}, but it refused it: {error}")
                });

            let launched = PLUGIN_USE_TEST_WORKFLOW_ID;
            let error = binding
                .enforce("demo1234", family, launched)
                .expect_err("a workflow id other than the pinned one must be refused");
            let message = error.to_string();
            // Full-message equality, not a substring `contains`: this also
            // pins the two things a `contains` check on the tail fragment
            // alone would miss — the `family` clause (nothing else in this
            // test asserts it survives a refactor) and the `{app_id:?}` DEBUG
            // quoting (a `{app_id}` plain-display regression would still
            // satisfy `contains("demo1234")`).
            let expected_message = format!(
                "app {:?} is pinned to runtime profile {family}, which must use {required}; \
                 refusing caller-selected workflow {launched}",
                "demo1234"
            );
            assert_eq!(
                message, expected_message,
                "{target:?} refusal message must be byte-identical to the pinned form"
            );
        }
    }

    #[test]
    fn enforce_accepts_a_matching_launched_id() {
        let binding = LocalAppPluginBinding::resolve(LocalAppBuildTarget::ReactDomR4);
        binding
            .enforce(
                "demo1234",
                local_apps::AppRuntimeProfile::ReactDom,
                PLUGIN_BUILD_WORKFLOW_ID,
            )
            .expect("matching launched workflow id must be accepted");
    }

    #[test]
    fn enforce_refuses_a_mismatched_launched_id_and_names_the_specifics() {
        let binding = LocalAppPluginBinding::resolve(LocalAppBuildTarget::ReactDomR4);
        let error = binding
            .enforce(
                "demo1234",
                local_apps::AppRuntimeProfile::ReactDom,
                PLUGIN_USE_TEST_WORKFLOW_ID,
            )
            .expect_err("mismatched launched workflow id must be refused");
        let message = error.to_string();
        // The debug-quoted app id specifically — `{app_id:?}`, not
        // `{app_id}` — so a regression that swapped Debug for Display (both
        // satisfy a bare `contains("demo1234")`) is caught here.
        assert!(
            message.contains("\"demo1234\""),
            "must name the app id in DEBUG-quoted form: {message}"
        );
        // The runtime-profile family clause: nothing else in this suite
        // fails if it is dropped from the message.
        assert!(
            message.contains("is pinned to runtime profile react_dom, which"),
            "must name the pinned runtime-profile family: {message}"
        );
        // Ordered fragment, not two independent `contains`: a refactor that
        // swapped the required and the caller-selected id would satisfy the
        // latter while telling the operator the exact opposite of the truth.
        assert!(
            message.contains(
                "must use lingxi-local-app:local-app-build; refusing caller-selected workflow \
                 lingxi-local-app:local-app-use-test"
            ),
            "must name the required id first and the caller-selected id second: {message}"
        );
        // Byte-identical to HEAD's pre-Phase -1 inline refusal message, in
        // full — not merely fragment-by-fragment.
        assert_eq!(
            message,
            "app \"demo1234\" is pinned to runtime profile react_dom, which must use \
             lingxi-local-app:local-app-build; refusing caller-selected workflow \
             lingxi-local-app:local-app-use-test",
            "refusal message must be byte-identical to the pinned form: {message}"
        );
    }
    /// The service tests that its prose names none of the workflows the host
    /// reserves, against a list of its own
    /// (`local_app_plugin::WORKFLOW_IDS`, the plugin crate's list of its workflow
    /// ids). That list mirrors this module's registry, so a workflow added here
    /// and not there would be a name the service's prose could start using
    /// without any test noticing.
    #[test]
    fn the_service_test_list_of_reserved_workflow_ids_is_this_registry() {
        let mut registry = vec![
            PLUGIN_BUILD_WORKFLOW_ID,
            PLUGIN_USE_TEST_WORKFLOW_ID,
            PLUGIN_MCP_AUTHORING_WORKFLOW_ID,
        ];
        let mut listed = local_app_plugin::WORKFLOW_IDS.to_vec();
        registry.sort_unstable();
        listed.sort_unstable();
        assert_eq!(
            listed, registry,
            "the service's list must name exactly the workflows this registry reserves"
        );
    }
}
