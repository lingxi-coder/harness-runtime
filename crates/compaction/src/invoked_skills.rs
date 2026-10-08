//! Process-global invoked-skill registry — the Rust mirror of the binary's
//! module-level `Pt.invokedSkills` Map.
//!
//! A skill's expanded content is registered here when the model invokes it
//! (`zSr`, from the Skill tool) and consumed after a compaction by
//! [`crate::restore_post_compact_skills`] (`rRg`) so the model re-reads the
//! recently invoked skill instructions across the compact boundary — the skill
//! content survives compaction even though the boundary summary drops the
//! verbatim skill body.
//!
//! Legacy rows keep the binary's `"{agentId}:{skillName}"` key. Production rows
//! additionally carry the owning session because `LingXi` can host multiple
//! orchestrators in one process. The registry remains a `Lazy<Mutex<..>>` and
//! is deliberately NOT cleared by post-compact cleanup — skill content must
//! outlive compaction — but the host removes a session's rows when that session
//! is cleared or replaced (see [`clear_session`]).
//!
//! 1:1 with the binary quartet (`bin/claude.exe`, v2.1.207):
//! - `zSr(e,t,r,n=null)` → [`register`] (stamps `invokedAt = Date.now()`).
//! - `kGo(e)` → [`filter_for_agent`] (rows whose `agentId` matches).
//! - `n_n(e,t)` → [`write_back`] (overwrite an existing row's content; `""`
//!   clears it, a no-op when the key is absent).
//! - `N$t()` → the whole map, exposed here only via the test inspectors.

use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use lingxi_core::types::utf16_json::Utf16JsonProjection;

use crate::post_compact::SkillRestoreCandidate;

/// One registry row — `{skillName, skillPath, content, invokedAt, agentId}`.
#[derive(Debug, Clone)]
struct InvokedSkillEntry {
    skill_name: Utf16JsonProjection,
    skill_path: PathBuf,
    content: Utf16JsonProjection,
    invoked_at_ms: i64,
    insertion_order: u64,
    session_id: Option<String>,
    agent_id: Option<String>,
}

/// Scope that owns an invoked-skill registry row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvokedSkillScopeRef<'a> {
    /// Owning session/conversation identity, if known.
    pub session_id: Option<&'a str>,
    /// Owning subagent identity, if known.
    pub agent_id: Option<&'a str>,
}

impl<'a> InvokedSkillScopeRef<'a> {
    /// Construct a borrowed registry scope.
    #[must_use]
    pub const fn new(session_id: Option<&'a str>, agent_id: Option<&'a str>) -> Self {
        Self {
            session_id,
            agent_id,
        }
    }
}

/// The process-global registry (`Pt.invokedSkills`), keyed `"{agentId}:{name}"`.
static REGISTRY: Lazy<Mutex<HashMap<Vec<u16>, InvokedSkillEntry>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

// Native v2.1.261 `le.record` uses Map.set (byte 156638452), retaining the
// original position when an existing key is overwritten. `JLn` preserves that
// order before `bXo` stably sorts by invokedAt, so same-millisecond invocations
// must not inherit HashMap's randomized iteration order.
static NEXT_INSERTION_ORDER: AtomicU64 = AtomicU64::new(0);

fn lock() -> std::sync::MutexGuard<'static, HashMap<Vec<u16>, InvokedSkillEntry>> {
    REGISTRY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Registry key for a scoped invoked skill.
///
/// Legacy agent-only rows keep the historical `${agentId ?? ""}:${skillName}`
/// shape so existing tests/helpers still see `":skill"` for main-thread rows
/// when no session identity is available. Session-scoped rows add an explicit
/// `session:...|agent:...|skill:...` prefix so two orchestrators or a leader
/// and subagent can invoke the same skill name without colliding.
#[must_use]
pub fn registry_key(scope: InvokedSkillScopeRef<'_>, skill_name: &Utf16JsonProjection) -> Vec<u16> {
    let prefix = match scope.session_id {
        Some(session_id) => format!(
            "session:{session_id}|agent:{}|skill:",
            scope.agent_id.unwrap_or("")
        ),
        None => format!("{}:", scope.agent_id.unwrap_or("")),
    };
    let mut key = prefix.encode_utf16().collect::<Vec<_>>();
    key.extend(
        skill_name
            .string_units("")
            .expect("invoked skill name must be a root JSON string"),
    );
    key
}

/// `Date.now()` in ms since the Unix epoch (saturating for the pre-1970 edge).
/// Stamps `invokedAt`; restoration sorts descending on it.
fn now_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

/// `zSr(name, path, content, agentId)` — register (or overwrite) an invoked
/// skill, stamping `invokedAt = Date.now()`.
///
/// The Skill tool calls this at invocation time with the expanded skill content
/// the model receives; `agent_id` is `None` for the main thread (every `LingXi`
/// orchestrator runs as the main thread → key `":{name}"`).
pub fn register(
    skill_name: &Utf16JsonProjection,
    skill_path: &Path,
    content: &Utf16JsonProjection,
    agent_id: Option<&str>,
) {
    register_scoped(
        skill_name,
        skill_path,
        content,
        InvokedSkillScopeRef::new(None, agent_id),
    );
}

/// Register a skill under the provided conversation/session + agent scope.
pub fn register_scoped(
    skill_name: &Utf16JsonProjection,
    skill_path: &Path,
    content: &Utf16JsonProjection,
    scope: InvokedSkillScopeRef<'_>,
) {
    let key = registry_key(scope, skill_name);
    let mut registry = lock();
    let insertion_order = registry.get(&key).map_or_else(
        || NEXT_INSERTION_ORDER.fetch_add(1, Ordering::Relaxed),
        |entry| entry.insertion_order,
    );
    let entry = InvokedSkillEntry {
        skill_name: skill_name.clone(),
        skill_path: skill_path.to_path_buf(),
        content: content.clone(),
        invoked_at_ms: now_ms(),
        insertion_order,
        session_id: scope.session_id.map(str::to_string),
        agent_id: scope.agent_id.map(str::to_string),
    };
    registry.insert(key, entry);
}

/// `kGo(agentId)` — the registry rows whose `agentId` matches, projected into
/// [`SkillRestoreCandidate`]s (each carrying its registry key) for
/// [`crate::restore_post_compact_skills`]. `agent_id = None` selects the
/// main-thread rows.
#[must_use]
pub fn filter_for_agent(agent_id: Option<&str>) -> Vec<SkillRestoreCandidate> {
    filter_for_scope(InvokedSkillScopeRef::new(None, agent_id))
}

/// The registry rows whose session + agent scope matches exactly.
#[must_use]
pub fn filter_for_scope(scope: InvokedSkillScopeRef<'_>) -> Vec<SkillRestoreCandidate> {
    let registry = lock();
    let mut rows = registry
        .iter()
        .filter(|(_, e)| {
            e.session_id.as_deref() == scope.session_id && e.agent_id.as_deref() == scope.agent_id
        })
        .collect::<Vec<_>>();
    rows.sort_unstable_by_key(|(_, entry)| entry.insertion_order);
    rows.into_iter()
        .map(|(key, e)| SkillRestoreCandidate {
            key: key.clone(),
            name: e.skill_name.clone(),
            path: e.skill_path.clone(),
            content: e.content.clone(),
            invoked_at_ms: e.invoked_at_ms,
        })
        .collect()
}

/// Remove every invoked-skill row owned by a completed/replaced session.
pub fn clear_session(session_id: &str) {
    lock().retain(|_, entry| entry.session_id.as_deref() != Some(session_id));
}

/// RAII owner for one orchestrator's session-scoped registry rows.
///
/// Hosts that expose an explicit session-end seam still clear eagerly, but
/// desktop/mobile runtimes can also be dropped or rebuilt without firing that
/// seam. Keeping this guard inside the orchestrator makes teardown synchronous
/// and unconditional; replacing the live session clears the previous scope.
pub struct InvokedSkillSessionGuard {
    session_id: Mutex<String>,
}

impl InvokedSkillSessionGuard {
    /// Bind a guard to the orchestrator's initial session id.
    #[must_use]
    pub fn new(session_id: String) -> Self {
        Self {
            session_id: Mutex::new(session_id),
        }
    }

    /// Move ownership to `session_id`, clearing the previously-owned rows.
    pub fn replace(&self, session_id: String) {
        let mut current = self
            .session_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *current == session_id {
            return;
        }
        clear_session(&current);
        *current = session_id;
    }
}

impl Drop for InvokedSkillSessionGuard {
    fn drop(&mut self) {
        let session_id = self
            .session_id
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_session(session_id);
    }
}

/// `n_n(key, content)` — overwrite an existing entry's content. A no-op when the
/// key is absent (`if(r)…`, exactly like the binary). An empty root string clears
/// the stored body so a later compaction won't re-attach it.
pub fn write_back(key: &[u16], content: &Utf16JsonProjection) {
    if let Some(e) = lock().get_mut(key) {
        e.content = content.clone();
    }
}

/// Test-only: clear the whole registry. Exposed (not `cfg(test)`) so downstream
/// crates' tests can isolate their view of this process-global. Not part of the
/// binary surface.
#[doc(hidden)]
pub fn reset_for_test() {
    let mut registry = lock();
    registry.clear();
    NEXT_INSERTION_ORDER.store(0, Ordering::Relaxed);
}

/// Test-only: the stored source content for `key`, or `None` if absent. Lets tests
/// assert the `n_n` write-back / clear behaviour of [`write_back`] and
/// [`crate::restore_post_compact_skills`].
#[doc(hidden)]
#[must_use]
pub fn content_for_test(key: &[u16]) -> Option<Utf16JsonProjection> {
    lock().get(key).map(|e| e.content.clone())
}

/// Test-only serial lock guarding the process-global registry so tests across
/// this crate (here and `post_compact`) that register/write-back real rows don't
/// race. Not part of the binary surface.
#[doc(hidden)]
pub static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use crate::post_compact::{
        restore_post_compact_skills, AttachedSkillContent, SKILL_TRUNCATION_MARKER,
    };

    // The registry is a process-global; serialize the tests that mutate it so
    // they don't race, and `reset_for_test()` at the top of each for a clean
    // view.
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        let g = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_for_test();
        g
    }

    fn source(text: &str) -> Utf16JsonProjection {
        text.into()
    }

    fn units(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    fn text(value: &Utf16JsonProjection) -> &str {
        value.string_at("").expect("root string")
    }

    #[test]
    fn register_sets_key_and_filter_by_agent() {
        let _g = guard();
        register(
            &source("deploy"),
            Path::new("/skills/deploy"),
            &source("deploy body"),
            None,
        );
        register(
            &source("build"),
            Path::new("/skills/build"),
            &source("build body"),
            Some("agent:x"),
        );

        // Main-thread filter (None) sees only the `:deploy` row.
        let main = filter_for_agent(None);
        assert_eq!(main.len(), 1);
        assert_eq!(main[0].key, units(":deploy"));
        assert_eq!(text(&main[0].name), "deploy");
        assert_eq!(text(&main[0].content), "deploy body");
        assert_eq!(main[0].path, PathBuf::from("/skills/deploy"));

        // Agent filter sees only its own row.
        let agent = filter_for_agent(Some("agent:x"));
        assert_eq!(agent.len(), 1);
        assert_eq!(agent[0].key, units("agent:x:build"));
    }

    #[test]
    fn equal_timestamps_restore_in_registry_insertion_order() {
        let _g = guard();
        let expected = (0..12).map(|i| format!("skill-{i}")).collect::<Vec<_>>();
        for name in &expected {
            register(&source(name), Path::new("/s"), &source("body"), None);
        }
        // Force the millisecond collision that the native Map + stable sort
        // handles without relying on wall-clock scheduling in this test.
        for entry in lock().values_mut() {
            entry.invoked_at_ms = 42;
        }
        let restored = restore_post_compact_skills(filter_for_agent(None), &[]);
        assert_eq!(
            restored
                .iter()
                .map(|skill| text(&skill.name))
                .collect::<Vec<_>>(),
            expected.iter().map(String::as_str).collect::<Vec<_>>()
        );
    }

    #[test]
    fn overwriting_a_skill_preserves_its_map_position_on_timestamp_ties() {
        let _g = guard();
        for name in ["first", "second", "third"] {
            register(&source(name), Path::new("/s"), &source("body"), None);
        }
        register(
            &source("first"),
            Path::new("/updated"),
            &source("updated"),
            None,
        );
        write_back(&units(":second"), &source("persisted"));
        for entry in lock().values_mut() {
            entry.invoked_at_ms = 42;
        }
        let restored = restore_post_compact_skills(filter_for_agent(None), &[]);
        assert_eq!(
            restored
                .iter()
                .map(|skill| text(&skill.name))
                .collect::<Vec<_>>(),
            ["first", "second", "third"]
        );
        assert_eq!(text(&restored[0].content), "updated");
        assert_eq!(text(&restored[1].content), "persisted");
    }

    #[test]
    fn register_overwrites_same_key() {
        let _g = guard();
        register(&source("s"), Path::new("/s"), &source("first"), None);
        register(&source("s"), Path::new("/s"), &source("second"), None);
        let rows = filter_for_agent(None);
        assert_eq!(rows.len(), 1, "same key overwrites, not appends");
        assert_eq!(text(&rows[0].content), "second");
    }

    #[test]
    fn write_back_overwrites_and_clears() {
        let _g = guard();
        register(&source("s"), Path::new("/s"), &source("body"), None);
        let exact =
            Utf16JsonProjection::root_string("t�".to_string(), vec![0x0074, 0xD83D]).unwrap();
        write_back(&units(":s"), &exact);
        assert_eq!(content_for_test(&units(":s")), Some(exact));
        write_back(&units(":s"), &source(""));
        assert_eq!(text(&content_for_test(&units(":s")).unwrap()), "");
    }

    #[test]
    fn write_back_absent_key_is_noop() {
        let _g = guard();
        write_back(&units(":missing"), &source("x"));
        assert!(content_for_test(&units(":missing")).is_none());
    }

    #[test]
    fn registry_key_uses_empty_prefix_for_main_thread() {
        assert_eq!(
            registry_key(InvokedSkillScopeRef::new(None, None), &source("foo")),
            units(":foo")
        );
        assert_eq!(
            registry_key(
                InvokedSkillScopeRef::new(None, Some("agent:a")),
                &source("foo")
            ),
            units("agent:a:foo")
        );
    }

    #[test]
    fn scoped_rows_do_not_collide_across_session_or_agent() {
        let _g = guard();
        let session_a_main = InvokedSkillScopeRef::new(Some("sess:a"), None);
        let other_session_main = InvokedSkillScopeRef::new(Some("sess:b"), None);
        let session_a_agent = InvokedSkillScopeRef::new(Some("sess:a"), Some("agent:x"));

        register_scoped(
            &source("build"),
            Path::new("/sa"),
            &source("main a"),
            session_a_main,
        );
        register_scoped(
            &source("build"),
            Path::new("/sb"),
            &source("main b"),
            other_session_main,
        );
        register_scoped(
            &source("build"),
            Path::new("/sx"),
            &source("agent a"),
            session_a_agent,
        );

        let rows = filter_for_scope(session_a_main);
        assert_eq!(rows.len(), 1);
        assert_eq!(text(&rows[0].content), "main a");
        assert_eq!(
            rows[0].key,
            registry_key(session_a_main, &source("build")),
            "scoped key must be stable for write-back"
        );

        let rows = filter_for_scope(other_session_main);
        assert_eq!(rows.len(), 1);
        assert_eq!(text(&rows[0].content), "main b");

        let rows = filter_for_scope(session_a_agent);
        assert_eq!(rows.len(), 1);
        assert_eq!(text(&rows[0].content), "agent a");
    }

    #[test]
    fn clearing_one_session_preserves_other_scopes() {
        let _g = guard();
        let session_a = InvokedSkillScopeRef::new(Some("sess:a"), None);
        let session_b = InvokedSkillScopeRef::new(Some("sess:b"), None);
        register_scoped(&source("build"), Path::new("/a"), &source("a"), session_a);
        register_scoped(&source("build"), Path::new("/b"), &source("b"), session_b);

        clear_session("sess:a");

        assert!(filter_for_scope(session_a).is_empty());
        assert_eq!(filter_for_scope(session_b).len(), 1);
    }

    #[test]
    fn session_guard_clears_on_replace_and_drop() {
        let _g = guard();
        let session_a = InvokedSkillScopeRef::new(Some("sess:a"), None);
        let session_b = InvokedSkillScopeRef::new(Some("sess:b"), None);
        register_scoped(&source("build"), Path::new("/a"), &source("a"), session_a);
        register_scoped(&source("build"), Path::new("/b"), &source("b"), session_b);

        let owner = InvokedSkillSessionGuard::new("sess:a".to_string());
        owner.replace("sess:b".to_string());
        assert!(filter_for_scope(session_a).is_empty());
        assert_eq!(filter_for_scope(session_b).len(), 1);

        drop(owner);
        assert!(filter_for_scope(session_b).is_empty());
    }

    // --- rRg write-back semantics (n_n) via restore_post_compact_skills ----- //

    #[test]
    fn restore_writes_back_truncated_content() {
        // A kept-and-truncated candidate persists the truncated content back to
        // the registry (`if(!c&&u!==a.content)n_n(s,u)`).
        let _g = guard();
        let big = "x".repeat(40_000); // caps to ~5_000 tokens
        register(&source("trunc"), Path::new("/t"), &source(&big), None);
        let restored = restore_post_compact_skills(filter_for_agent(None), &[]);
        assert_eq!(restored.len(), 1);
        let stored = content_for_test(&units(":trunc")).expect("row present");
        assert!(
            text(&stored).ends_with(SKILL_TRUNCATION_MARKER),
            "truncated content persisted to the registry"
        );
        assert_eq!(stored, restored[0].content);
        assert_ne!(
            text(&stored),
            big,
            "the full body no longer occupies the row"
        );
    }

    #[test]
    fn restore_does_not_write_back_short_content() {
        // Short content is returned verbatim (`u===a.content`) → no write-back.
        let _g = guard();
        register(
            &source("short"),
            Path::new("/s"),
            &source("tiny body"),
            None,
        );
        let restored = restore_post_compact_skills(filter_for_agent(None), &[]);
        assert_eq!(restored.len(), 1);
        assert_eq!(text(&restored[0].content), "tiny body");
        assert_eq!(
            text(&content_for_test(&units(":short")).unwrap()),
            "tiny body"
        );
    }

    #[test]
    fn restore_body_match_counts_but_does_not_write_back() {
        // A body match (`c=true`) restores + counts toward the budget but never
        // writes back — the registry keeps the FULL (untruncated) body.
        let _g = guard();
        let big = "x".repeat(40_000);
        register(&source("bm"), Path::new("/b"), &source(&big), None);
        let already = [AttachedSkillContent::Body(source(&big))];
        let restored = restore_post_compact_skills(filter_for_agent(None), &already);
        assert_eq!(restored.len(), 1);
        assert!(text(&restored[0].content).ends_with(SKILL_TRUNCATION_MARKER));
        assert_eq!(
            text(&content_for_test(&units(":bm")).unwrap()),
            big.as_str(),
            "body match → no write-back; registry unchanged"
        );
    }

    #[test]
    fn restore_clears_registry_on_budget_overflow() {
        // Six equal ~5_000-token skills: exactly 5 fit (25_000) and 1 overflows;
        // the overflowed row's content is CLEARED (`n_n(s,"")`).
        let _g = guard();
        for i in 0..6 {
            register(
                &source(&format!("s{i}")),
                Path::new("/s"),
                &source(&"x".repeat(40_000)),
                None,
            );
        }
        let restored = restore_post_compact_skills(filter_for_agent(None), &[]);
        assert_eq!(restored.len(), 5, "5 fit, 6th overflows");
        let cleared: Vec<String> = (0..6)
            .map(|i| format!(":s{i}"))
            .filter(|k| content_for_test(&units(k)).is_some_and(|value| text(&value).is_empty()))
            .collect();
        assert_eq!(cleared.len(), 1, "exactly the overflowed row is cleared");
    }

    #[test]
    fn restore_sorts_by_invoked_at_desc() {
        // DESC by invokedAt: the most recently invoked skill leads the result.
        let _g = guard();
        register(&source("old"), Path::new("/o"), &source("old body"), None);
        std::thread::sleep(std::time::Duration::from_millis(3));
        register(&source("new"), Path::new("/n"), &source("new body"), None);
        let restored = restore_post_compact_skills(filter_for_agent(None), &[]);
        assert_eq!(restored.len(), 2);
        assert_eq!(text(&restored[0].name), "new", "most recent first");
        assert_eq!(text(&restored[1].name), "old");
    }

    #[test]
    fn restore_preserves_exact_utf16_across_repeated_compactions() {
        let _g = guard();
        let big = format!("{}😀{}", "x".repeat(19_899), "y".repeat(200));
        register(&source("repeat"), Path::new("/r"), &source(&big), None);

        let first = restore_post_compact_skills(filter_for_agent(None), &[]);
        assert_eq!(first.len(), 1);
        let first_exact = first[0].content.string_units("").unwrap();
        assert!(
            first_exact
                .windows(2)
                .any(|w| w == [0xD83D, 0x000A] || w == [0xD83D, 0x005B]),
            "wire image must retain the split surrogate at the truncation boundary"
        );

        let second = restore_post_compact_skills(filter_for_agent(None), &[]);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].content.string_units("").unwrap(), first_exact);
    }

    #[test]
    fn skill_name_and_body_keep_unpaired_units_and_distinct_keys() {
        let _g = guard();
        let scope = InvokedSkillScopeRef::new(Some("sess"), None);
        let replacement =
            Utf16JsonProjection::root_string("�a".into(), vec![0xD800, 0x0061]).unwrap();
        let other = Utf16JsonProjection::root_string("�a".into(), vec![0xD801, 0x0061]).unwrap();
        let body =
            Utf16JsonProjection::root_string("x�y".into(), vec![0x0078, 0xDFFF, 0x0079]).unwrap();
        register_scoped(&replacement, Path::new("/one"), &body, scope);
        register_scoped(&other, Path::new("/two"), &source("other"), scope);
        let restored = restore_post_compact_skills(filter_for_scope(scope), &[]);
        assert_eq!(restored.len(), 2);
        let first = restored
            .iter()
            .find(|skill| skill.name.string_units("").unwrap() == vec![0xD800, 0x0061])
            .expect("first exact key survives");
        let second = restored
            .iter()
            .find(|skill| skill.name.string_units("").unwrap() == vec![0xD801, 0x0061])
            .expect("second exact key survives");
        assert_eq!(first.name.string_units("").unwrap(), vec![0xD800, 0x0061]);
        assert_eq!(
            first.content.string_units("").unwrap(),
            vec![0x0078, 0xDFFF, 0x0079]
        );
        assert_eq!(second.name.string_units("").unwrap(), vec![0xD801, 0x0061]);
    }
}
