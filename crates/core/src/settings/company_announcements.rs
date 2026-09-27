//! `companyAnnouncements` startup-message selection.
//!
//! 1:1 port of claude-code 2.1.207's announcement helpers `jxo` (select) and
//! `oip` (gate):
//!
//! ```js
//! function oip(){let e=Wn().companyAnnouncements;return!!e&&e.some((t)=>t)}
//! function jxo(e){
//!   if(Wxo!==null)return Wxo;                       // process memo
//!   let t=(Wn().companyAnnouncements??[]).filter((n)=>n);   // non-empty only
//!   if(t.length===0)return null;
//!   let r=St().numStartups===1 ? t[0] : t[Math.floor(Math.random()*t.length)];
//!   if(!r)return null;
//!   if(e)Wxo=r;                                      // memoize when asked
//!   return r
//! }
//! ```
//!
//! The `companyAnnouncements` settings array is read from the merged
//! [`crate::settings::schema::SettingsJson`]. Non-empty strings are kept; the
//! first is chosen deterministically on the very first launch
//! (`numStartups === 1`), otherwise one is chosen uniformly at random. A
//! process-level memo ([`AnnouncementMemo`]) keeps the SAME announcement stable
//! across re-renders within one process (the `Wxo` cache; the `e` flag on `jxo`).
//!
//! The composition root ([`startup_announcement`]) ties the pieces together the
//! way CC's `LVs` component does: it selects the announcement (memoized on the
//! process-global [`PROCESS_MEMO`] = the `Wxo` cache) and pairs it with the
//! optional `Message from <organizationName>:` prefix (CC's `em_`, from
//! `Nc()?.organizationName`). The CLI supplies `num_startups` (the global-config
//! `numStartups` counter) and `organization_name` (the global-config
//! `oauthAccount.organizationName`) and renders the returned block in the TUI
//! startup banner.

use std::sync::Mutex;

/// `oip()` — is there at least one non-empty announcement to show?
///
/// `!!e && e.some((t)=>t)`: `Some` list with at least one non-empty string.
#[must_use]
pub fn has_company_announcement(announcements: Option<&[String]>) -> bool {
    announcements.is_some_and(|a| a.iter().any(|s| !s.is_empty()))
}

/// Pure selection with an injected random index (the `Math.floor(random*len)`
/// value). Filters to non-empty strings first (`.filter((n)=>n)`), returns the
/// first on `num_startups == 1`, else the entry at `random_index % len`.
///
/// Returns `None` when there are no non-empty announcements.
#[must_use]
pub fn select_company_announcement_at(
    announcements: Option<&[String]>,
    num_startups: u64,
    random_index: usize,
) -> Option<String> {
    let non_empty: Vec<&String> = announcements?.iter().filter(|s| !s.is_empty()).collect();
    if non_empty.is_empty() {
        return None;
    }
    let chosen = if num_startups == 1 {
        non_empty[0]
    } else {
        non_empty[random_index % non_empty.len()]
    };
    Some(chosen.clone())
}

/// `jxo`-equivalent selection: picks the first announcement on the first launch
/// (`num_startups == 1`) else a uniformly-random one. Dependency-free entropy
/// (process/thread nanos) drives the random branch — the exact index is not a
/// parity-observable value (any configured announcement is valid to show), and
/// the deterministic first-launch branch is fully specified.
#[must_use]
pub fn select_company_announcement(
    announcements: Option<&[String]>,
    num_startups: u64,
) -> Option<String> {
    select_company_announcement_at(announcements, num_startups, random_seed_index())
}

/// Process-level memo mirroring binary `Wxo`: the first memoized selection is
/// returned verbatim on every subsequent call, keeping the announcement stable
/// across re-renders within one process.
#[derive(Debug, Default)]
pub struct AnnouncementMemo {
    /// The `Wxo` cache: `None` = not yet memoized (CC `Wxo===null`), `Some(s)` =
    /// the memoized announcement. Only ever holds a truthy selection — a null
    /// result is never cached (CC's `if(e)Wxo=r`, where `r` is always a string).
    cell: Mutex<Option<String>>,
}

impl AnnouncementMemo {
    /// Construct an empty memo. `const` so the process-global [`PROCESS_MEMO`]
    /// (the port of the module-level `Wxo` `var`) can be a plain `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cell: Mutex::new(None),
        }
    }

    /// `jxo(memoize)`: return the memoized selection if present, otherwise
    /// compute a fresh selection and — when `memoize` is `true` — cache it.
    ///
    /// Mirrors `if(Wxo!==null)return Wxo; …; if(e)Wxo=r; return r`.
    pub fn select(
        &self,
        announcements: Option<&[String]>,
        num_startups: u64,
        memoize: bool,
    ) -> Option<String> {
        let mut guard = self
            .cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // `if(Wxo!==null)return Wxo` — a cached (always truthy) selection wins.
        if let Some(cached) = guard.as_ref() {
            return Some(cached.clone());
        }
        let selected = select_company_announcement(announcements, num_startups);
        // `if(e)Wxo=r` — memoize ONLY a truthy result; a null selection is not
        // cached, so a later call re-evaluates (CC never stores `null` in `Wxo`).
        if memoize {
            if let Some(s) = selected.as_ref() {
                *guard = Some(s.clone());
            }
        }
        selected
    }
}

/// The startup company-announcement block, mirroring CC's `LVs` render (an Ink
/// column of an optional dim `Message from <org>:` line above the announcement):
///
/// ```js
/// em_ = !IS_DEMO && Nc()?.organizationName && `Message from ${organizationName}:`;  // dim
/// PVs = jxo(true);                                                                    // announcement
/// // <column>{em_}{PVs}</column>
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupAnnouncement {
    /// `Message from <organizationName>:` prefix (CC `em_`) — present only when
    /// an org name is available; rendered dim above the body. `None` = omit.
    pub org_prefix: Option<String>,
    /// The selected announcement text (CC `PVs`).
    pub body: String,
}

/// Process-global announcement memo — the port of CC's module-level `Wxo` `var`
/// (`jxo`/`rm_` operate on it). Keeps the SAME announcement chosen once per
/// process, stable across any re-render. Production reads it through
/// [`startup_announcement`]; tests inject a fresh [`AnnouncementMemo`] via
/// [`startup_announcement_with`] so they never poison this shared cache.
static PROCESS_MEMO: AnnouncementMemo = AnnouncementMemo::new();

/// Composition-root selection + formatting against an injected memo (testable).
///
/// 1:1 with CC's `LVs`: `PVs = jxo(true)` (memoized selection over the merged
/// `companyAnnouncements`), and `em_ = organizationName && "Message from <org>:"`.
/// Returns `None` when there is no non-empty announcement to show — the `oip()`
/// gate false case (`select` yields `None`).
#[must_use]
pub fn startup_announcement_with(
    memo: &AnnouncementMemo,
    announcements: Option<&[String]>,
    num_startups: u64,
    organization_name: Option<&str>,
) -> Option<StartupAnnouncement> {
    let body = memo.select(announcements, num_startups, true)?;
    let org_prefix = organization_name
        .map(str::trim)
        .filter(|o| !o.is_empty())
        .map(|o| format!("Message from {o}:"));
    Some(StartupAnnouncement { org_prefix, body })
}

/// Composition-root entry using the process-global [`PROCESS_MEMO`] (the `Wxo`
/// cache) — the production call the CLI makes once at startup. See
/// [`startup_announcement_with`] for the selection/formatting contract.
#[must_use]
pub fn startup_announcement(
    announcements: Option<&[String]>,
    num_startups: u64,
    organization_name: Option<&str>,
) -> Option<StartupAnnouncement> {
    startup_announcement_with(
        &PROCESS_MEMO,
        announcements,
        num_startups,
        organization_name,
    )
}

/// Dependency-free entropy for the uniform-random branch. Not cryptographic;
/// used only to pick which of several equally-valid announcements to show.
fn random_seed_index() -> usize {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    nanos as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn gate_true_only_when_a_non_empty_entry_exists() {
        assert!(!has_company_announcement(None));
        assert!(!has_company_announcement(Some(&[])));
        assert!(!has_company_announcement(Some(&v(&["", ""]))));
        assert!(has_company_announcement(Some(&v(&["", "hi"]))));
    }

    #[test]
    fn first_launch_is_deterministic_first_non_empty() {
        // num_startups == 1 → always the FIRST non-empty entry, regardless of the
        // random index.
        let a = v(&["", "first", "second"]);
        assert_eq!(
            select_company_announcement_at(Some(&a), 1, 999),
            Some("first".to_string())
        );
    }

    #[test]
    fn empty_and_all_empty_yield_none() {
        assert_eq!(select_company_announcement_at(None, 1, 0), None);
        assert_eq!(select_company_announcement_at(Some(&v(&[])), 5, 0), None);
        assert_eq!(
            select_company_announcement_at(Some(&v(&["", ""])), 5, 0),
            None
        );
    }

    #[test]
    fn random_branch_picks_a_non_empty_member() {
        // num_startups != 1 → index into the FILTERED (non-empty) list.
        let a = v(&["", "a", "b", "c"]);
        // Filtered list is [a,b,c]; index 4 % 3 == 1 → "b".
        assert_eq!(
            select_company_announcement_at(Some(&a), 7, 4),
            Some("b".to_string())
        );
        // Whatever the index, the result is always one of the non-empty members.
        for idx in 0..20usize {
            let got = select_company_announcement_at(Some(&a), 7, idx).unwrap();
            assert!(["a", "b", "c"].contains(&got.as_str()), "got {got}");
        }
    }

    #[test]
    fn memo_keeps_the_selection_stable_across_calls() {
        let memo = AnnouncementMemo::new();
        let a = v(&["only"]);
        let first = memo.select(Some(&a), 3, true);
        assert_eq!(first, Some("only".to_string()));
        // Even if a later call passes different inputs, the memoized value wins.
        let second = memo.select(Some(&v(&["different"])), 3, true);
        assert_eq!(second, Some("only".to_string()));
    }

    #[test]
    fn memo_does_not_cache_when_memoize_false() {
        let memo = AnnouncementMemo::new();
        // memoize=false must not populate the cache (binary: `if(e)Wxo=r`).
        let _ = memo.select(Some(&v(&["a"])), 1, false);
        // A subsequent memoize=true call then decides the cached value.
        let cached = memo.select(Some(&v(&["b"])), 1, true);
        assert_eq!(cached, Some("b".to_string()));
    }

    #[test]
    fn memo_does_not_cache_a_null_result() {
        // CC `jxo` only caches a truthy string (`if(e)Wxo=r`); a `null` result
        // is NOT stored in `Wxo`, so a later call with a real announcement
        // re-evaluates rather than returning a stale `None`.
        let memo = AnnouncementMemo::new();
        assert_eq!(memo.select(None, 1, true), None);
        assert_eq!(memo.select(Some(&v(&["", ""])), 5, true), None);
        // Now that an announcement exists, selection proceeds (not stuck at None).
        assert_eq!(
            memo.select(Some(&v(&["fresh"])), 1, true),
            Some("fresh".to_string())
        );
    }

    // ---- composition root (`LVs`): startup_announcement ----

    #[test]
    fn startup_first_launch_yields_first_entry_no_org() {
        // A configured non-empty array on the very first launch renders the
        // FIRST non-empty entry, with no `Message from …` prefix when there is
        // no org name (CC `em_` falsy).
        let memo = AnnouncementMemo::new();
        let a = v(&["", "hello", "world"]);
        let got = startup_announcement_with(&memo, Some(&a), 1, None);
        assert_eq!(
            got,
            Some(StartupAnnouncement {
                org_prefix: None,
                body: "hello".to_string(),
            })
        );
    }

    #[test]
    fn startup_includes_org_prefix_when_org_name_present() {
        // `em_ = organizationName && "Message from <org>:"` — the dim prefix.
        let memo = AnnouncementMemo::new();
        let a = v(&["heads up"]);
        let got = startup_announcement_with(&memo, Some(&a), 1, Some("Acme")).unwrap();
        assert_eq!(got.org_prefix.as_deref(), Some("Message from Acme:"));
        assert_eq!(got.body, "heads up");
    }

    #[test]
    fn startup_blank_org_name_omits_prefix() {
        // A whitespace-only / empty org name is treated as absent (no prefix).
        let memo = AnnouncementMemo::new();
        let a = v(&["hi"]);
        assert_eq!(
            startup_announcement_with(&memo, Some(&a), 1, Some("   "))
                .unwrap()
                .org_prefix,
            None
        );
        let memo2 = AnnouncementMemo::new();
        assert_eq!(
            startup_announcement_with(&memo2, Some(&a), 1, Some(""))
                .unwrap()
                .org_prefix,
            None
        );
    }

    #[test]
    fn startup_absent_or_all_empty_renders_nothing() {
        // `oip()` false → the whole block is omitted (CC `if(!PVs)return null`).
        assert_eq!(
            startup_announcement_with(&AnnouncementMemo::new(), None, 1, Some("Acme")),
            None
        );
        assert_eq!(
            startup_announcement_with(&AnnouncementMemo::new(), Some(&v(&[])), 5, Some("Acme")),
            None
        );
        assert_eq!(
            startup_announcement_with(
                &AnnouncementMemo::new(),
                Some(&v(&["", ""])),
                5,
                Some("Acme")
            ),
            None
        );
    }

    #[test]
    fn startup_selection_is_memoized_per_process() {
        // Once selected+memoized, later calls on the SAME memo return the same
        // announcement even under different inputs (the `Wxo` cache).
        let memo = AnnouncementMemo::new();
        let first = startup_announcement_with(&memo, Some(&v(&["one"])), 3, None);
        assert_eq!(first.as_ref().map(|s| s.body.as_str()), Some("one"));
        let second = startup_announcement_with(&memo, Some(&v(&["two"])), 3, Some("Acme"));
        // Body stays "one" (memoized); the org prefix is recomputed per call.
        assert_eq!(second.as_ref().map(|s| s.body.as_str()), Some("one"));
        assert_eq!(
            second.and_then(|s| s.org_prefix).as_deref(),
            Some("Message from Acme:")
        );
    }
}
