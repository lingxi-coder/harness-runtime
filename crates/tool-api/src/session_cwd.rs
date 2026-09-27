//! `SessionCwd` — a shared, lock-free-read, switchable cell holding the
//! session's current working directory and its trusted-directory set.
//!
//! Backed by `arc-swap` so reads (`cwd()`, `trusted_dirs()`) never block on a
//! writer; `swap` atomically publishes a new `(cwd, trusted)` pair and fires
//! an optional on-swap callback (used by later tasks to invalidate
//! cwd-dependent caches). Until `set_on_swap` is called the callback is a
//! no-op, so constructing a `SessionCwd` and never calling `swap` is fully
//! inert.
//!
//! The cwd and its trusted-directory set are published together as a single
//! `Arc<CwdState>` so a concurrent reader can never observe a new cwd paired
//! with a stale trusted-directory set (or vice-versa) — see [`CwdState`].

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;

/// Callback invoked with the new cwd at the end of every [`SessionCwd::swap`].
type OnSwapCallback = Arc<dyn Fn(&Path) + Send + Sync>;

/// The cwd + trusted-directory set as they are published together, so a
/// single `ArcSwap` load always returns a matched pair — never cwd from one
/// generation paired with trusted_dirs from another.
struct CwdState {
    cwd: PathBuf,
    trusted_dirs: Vec<PathBuf>,
}

/// Shared, switchable session cwd + trusted-directory set.
///
/// Always held behind an `Arc` (see [`SessionCwd::new`]) so every tool
/// invocation can clone a handle and observe the latest swap.
pub struct SessionCwd {
    project_root: PathBuf,
    state: ArcSwap<CwdState>,
    on_swap: Mutex<Option<OnSwapCallback>>,
    /// Shared live-cwd cells (the file/shell/LSP tools' `LiveCwdCell`) kept in
    /// sync with every [`SessionCwd::swap`]. Empty until [`link_live_cwd`] is
    /// called, so an unlinked cell is fully inert.
    ///
    /// [`link_live_cwd`]: SessionCwd::link_live_cwd
    mirror_cells: Mutex<Vec<Arc<Mutex<PathBuf>>>>,
}

impl SessionCwd {
    /// Construct a new cell initialized to the process boot cwd and its
    /// initial trusted-directory set.
    #[must_use]
    pub fn new(boot_cwd: PathBuf, trusted: Vec<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            project_root: boot_cwd.clone(),
            state: ArcSwap::from_pointee(CwdState {
                cwd: boot_cwd,
                trusted_dirs: trusted,
            }),
            on_swap: Mutex::new(None),
            mirror_cells: Mutex::new(Vec::new()),
        })
    }

    /// Session project root captured at composition, distinct from a later shell cwd.
    #[must_use]
    pub fn project_root(&self) -> PathBuf {
        self.project_root.clone()
    }

    /// Current cwd (cloned out of the cell).
    #[must_use]
    pub fn cwd(&self) -> PathBuf {
        self.state.load().cwd.clone()
    }

    /// Current trusted-directory set (cloned out of the cell).
    #[must_use]
    pub fn trusted_dirs(&self) -> Vec<PathBuf> {
        self.state.load().trusted_dirs.clone()
    }

    /// Current `(cwd, trusted_dirs)` pair from a SINGLE `load()` of the shared
    /// state cell — the same generation for both fields.
    ///
    /// Prefer this over calling [`Self::cwd`] and [`Self::trusted_dirs`] back
    /// to back at any call site that needs BOTH: two separate accessor calls
    /// each do their own `load()`, so a concurrent `swap()` in between them
    /// could hand back a cwd from one generation paired with trusted_dirs from
    /// the next (or previous) generation. `snapshot()` loads the `CwdState`
    /// once and reads both fields off that single `Arc`, so it can never
    /// straddle a swap.
    #[must_use]
    pub fn snapshot(&self) -> (PathBuf, Vec<PathBuf>) {
        let state = self.state.load();
        (state.cwd.clone(), state.trusted_dirs.clone())
    }

    /// Atomically publish a new cwd + trusted-directory set as a single
    /// pair, then invoke the on-swap callback (if one has been registered)
    /// with the new cwd.
    ///
    /// The callback is invoked *after* the internal lock guarding it has
    /// been released: we only hold the lock long enough to clone out the
    /// `Arc<dyn Fn>`. This means a panicking callback unwinds without
    /// poisoning the mutex (so subsequent `swap()` calls keep working), and
    /// a reentrant callback that itself calls `swap()` or `set_on_swap()`
    /// cannot self-deadlock.
    pub fn swap(&self, cwd: PathBuf, trusted: Vec<PathBuf>) {
        self.state.store(Arc::new(CwdState {
            cwd: cwd.clone(),
            trusted_dirs: trusted,
        }));

        // Mirror the new cwd into any linked live-cwd cells so a file/Glob/Grep
        // read *between* this swap (e.g. a worktree enter/exit) and the next
        // Bash call observes the post-swap cwd instead of the stale pre-swap
        // one. We clone the handle list out under the lock (same discipline as
        // the on-swap callback below) so we never hold `mirror_cells` while
        // taking an individual cell's lock.
        let cells = self
            .mirror_cells
            .lock()
            .expect("mirror_cells mutex poisoned")
            .clone();
        for cell in &cells {
            *cell.lock().expect("live-cwd cell mutex poisoned") = cwd.clone();
        }

        let cb = self.on_swap.lock().expect("on_swap mutex poisoned").clone();
        if let Some(cb) = cb {
            cb(&cwd);
        }
    }

    /// Add `dir` to the current trusted-directory set — keeping the cwd and
    /// every existing trusted dir — and publish the widened pair via the same
    /// atomic `ArcSwap` store as [`Self::swap`]. Returns `true` when the set
    /// actually changed (the dir was not already trusted), `false` when it was
    /// already present (a strict no-op: nothing is published).
    ///
    /// Backs the runtime `/add-dir` live effect (parity 2.1.207 P1-08): a
    /// directory added mid-session becomes immediately accessible to the file
    /// tools (`Read`/`Edit`/`Write`/`Glob`/`Grep`/`NotebookEdit`), which gate
    /// on the allowed set read off [`Self::trusted_dirs`], WITHOUT a reboot —
    /// matching claude-code's live `toolPermissionContext.additionalWorkingDirectories`
    /// update. Unlike [`Self::swap`] this leaves the cwd untouched, so it does
    /// NOT fire the on-swap callback or mirror the live-cwd cells (both key off
    /// a cwd change, and none happened here).
    pub fn add_trusted_dir(&self, dir: PathBuf) -> bool {
        let cur = self.state.load();
        if cur.trusted_dirs.iter().any(|d| d == &dir) {
            return false;
        }
        let mut trusted = cur.trusted_dirs.clone();
        trusted.push(dir);
        self.state.store(Arc::new(CwdState {
            cwd: cur.cwd.clone(),
            trusted_dirs: trusted,
        }));
        true
    }

    /// Move the session cwd to `target` while PRESERVING the additional
    /// working directories (settings `additionalDirectories` + runtime
    /// `/add-dir` grants). The new trusted set is `[target, ...additional]`,
    /// where `additional` is the prior trusted set with the OLD cwd removed
    /// (and `target` de-duplicated).
    ///
    /// This backs the `/cd` command (parity 2.1.208): claude-code's `/cd` move
    /// fn (`sMs` → `process.chdir(e); kN(Ct())`) updates only the cwd and never
    /// touches `toolPermissionContext.additionalWorkingDirectories`, and its
    /// file-tool allow-set is `EJ = new Set([ln()/*cwd*/, ...additionalWorkingDirectories])`.
    /// So after a `/cd` the trusted set is `{new cwd} ∪ additional` — the old
    /// cwd is no longer trusted (it was `ln()`, now replaced), but every
    /// `/add-dir`/settings directory stays readable/writable. A plain
    /// [`Self::swap`] with `vec![target]` would instead drop those dirs.
    ///
    /// Like [`Self::swap`] (and unlike [`Self::add_trusted_dir`]) this DOES
    /// fire the on-swap callback and mirror the live-cwd cells, because the cwd
    /// changed.
    pub fn change_cwd(&self, target: PathBuf) {
        let cur = self.state.load();
        let old_cwd = cur.cwd.clone();
        // Additional working dirs = current trusted set minus the OLD cwd
        // (which `/cd` moves away from), with `target` de-duplicated so it
        // appears exactly once at the front.
        let mut trusted = Vec::with_capacity(cur.trusted_dirs.len() + 1);
        trusted.push(target.clone());
        for d in &cur.trusted_dirs {
            if *d != old_cwd && *d != target {
                trusted.push(d.clone());
            }
        }
        drop(cur);
        self.swap(target, trusted);
    }

    /// Register a callback invoked (with the new cwd) at the end of every
    /// [`SessionCwd::swap`]. No-op until called; replaces any prior callback.
    pub fn set_on_swap(&self, cb: Box<dyn Fn(&Path) + Send + Sync>) {
        *self.on_swap.lock().expect("on_swap mutex poisoned") = Some(Arc::from(cb));
    }

    /// Link a shared live-cwd cell (the file/shell/LSP tools' [`LiveCwdCell`])
    /// so every [`swap`](Self::swap) mirrors the new cwd into it immediately.
    ///
    /// Bash re-points its own copy of this cell at the start of each call, so
    /// without this link a worktree enter/exit leaves the cell holding the
    /// pre-swap cwd until the next Bash invocation — a file/Glob/Grep read in
    /// that window would resolve relative paths against the wrong directory.
    /// Idempotent-friendly: multiple cells can be linked; each is updated on
    /// every swap. No-op for swaps that occur before any link.
    ///
    /// [`LiveCwdCell`]: crate::LiveCwdCell
    pub fn link_live_cwd(&self, cell: Arc<Mutex<PathBuf>>) {
        self.mirror_cells
            .lock()
            .expect("mirror_cells mutex poisoned")
            .push(cell);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    fn boot() -> PathBuf {
        PathBuf::from("/boot/cwd")
    }

    #[test]
    fn new_cwd_equals_boot() {
        let sc = SessionCwd::new(boot(), vec![boot()]);
        assert_eq!(sc.cwd(), boot());
    }

    #[test]
    fn swap_mirrors_new_cwd_into_linked_cell() {
        let sc = SessionCwd::new(boot(), vec![boot()]);
        let cell: Arc<Mutex<PathBuf>> = Arc::new(Mutex::new(boot()));
        sc.link_live_cwd(cell.clone());
        // Linking alone must not perturb the cell.
        assert_eq!(*cell.lock().unwrap(), boot());

        let wt = PathBuf::from("/wt/root");
        sc.swap(wt.clone(), vec![wt.clone()]);
        assert_eq!(
            *cell.lock().unwrap(),
            wt,
            "swap must mirror the new cwd into the linked live-cwd cell"
        );
    }

    #[test]
    fn swap_updates_every_linked_cell() {
        let sc = SessionCwd::new(boot(), vec![boot()]);
        let a: Arc<Mutex<PathBuf>> = Arc::new(Mutex::new(boot()));
        let b: Arc<Mutex<PathBuf>> = Arc::new(Mutex::new(boot()));
        sc.link_live_cwd(a.clone());
        sc.link_live_cwd(b.clone());
        let wt = PathBuf::from("/wt/two");
        sc.swap(wt.clone(), vec![wt.clone()]);
        assert_eq!(*a.lock().unwrap(), wt);
        assert_eq!(*b.lock().unwrap(), wt);
    }

    #[test]
    fn swap_without_linked_cell_is_inert_and_still_fires_on_swap() {
        // No linked cell: swap must not panic, and the on-swap callback still
        // runs (mirroring is orthogonal to the callback).
        let sc = SessionCwd::new(boot(), vec![boot()]);
        let fired = Arc::new(AtomicUsize::new(0));
        let f = fired.clone();
        sc.set_on_swap(Box::new(move |_p: &Path| {
            f.fetch_add(1, Ordering::SeqCst);
        }));
        sc.swap(PathBuf::from("/x"), vec![PathBuf::from("/x")]);
        assert_eq!(fired.load(Ordering::SeqCst), 1);
        assert_eq!(sc.cwd(), PathBuf::from("/x"));
    }

    #[test]
    fn new_trusted_dirs_equals_initial() {
        let trusted = vec![boot(), PathBuf::from("/other")];
        let sc = SessionCwd::new(boot(), trusted.clone());
        assert_eq!(sc.trusted_dirs(), trusted);
    }

    #[test]
    fn swap_then_cwd_equals_new() {
        let sc = SessionCwd::new(boot(), vec![boot()]);
        let new_cwd = PathBuf::from("/worktree/foo");
        sc.swap(new_cwd.clone(), vec![new_cwd.clone()]);
        assert_eq!(sc.cwd(), new_cwd);
    }

    #[test]
    fn trusted_dirs_tracks_swap() {
        let sc = SessionCwd::new(boot(), vec![boot()]);
        let new_cwd = PathBuf::from("/worktree/foo");
        let new_trusted = vec![new_cwd.clone(), PathBuf::from("/worktree/bar")];
        sc.swap(new_cwd, new_trusted.clone());
        assert_eq!(sc.trusted_dirs(), new_trusted);
    }

    #[test]
    fn no_swap_is_inert() {
        // Constructing and never swapping must leave cwd/trusted untouched —
        // the INERT INVARIANT this whole plan depends on.
        let sc = SessionCwd::new(boot(), vec![boot()]);
        assert_eq!(sc.cwd(), boot());
        assert_eq!(sc.trusted_dirs(), vec![boot()]);
    }

    #[test]
    fn add_trusted_dir_publishes_live_and_returns_true() {
        // A runtime `/add-dir` add widens the trusted set immediately and
        // reports the change, keeping the cwd + existing dirs.
        let sc = SessionCwd::new(boot(), vec![boot()]);
        let extra = PathBuf::from("/extra");
        assert!(
            sc.add_trusted_dir(extra.clone()),
            "adding a NEW dir must report a change"
        );
        assert_eq!(
            sc.trusted_dirs(),
            vec![boot(), extra],
            "the new dir is appended after cwd/existing dirs"
        );
        assert_eq!(sc.cwd(), boot(), "cwd is untouched by add_trusted_dir");
    }

    #[test]
    fn add_trusted_dir_dedupes_and_returns_false() {
        // Re-adding a dir already in the set (including the cwd itself) is a
        // strict no-op — nothing is published and no change is reported (the
        // jzn-style change-compare the runtime effect keys the MCP
        // roots/list_changed notification off).
        let extra = PathBuf::from("/extra");
        let sc = SessionCwd::new(boot(), vec![boot(), extra.clone()]);
        assert!(
            !sc.add_trusted_dir(extra.clone()),
            "an already-trusted dir must report NO change"
        );
        assert!(
            !sc.add_trusted_dir(boot()),
            "the cwd is already trusted — re-adding it must report NO change"
        );
        assert_eq!(sc.trusted_dirs(), vec![boot(), extra]);
    }

    #[test]
    fn add_trusted_dir_does_not_fire_on_swap_callback() {
        // The cwd never changes on an add, so cwd-keyed cache invalidation
        // (the on-swap callback) must NOT fire.
        let sc = SessionCwd::new(boot(), vec![boot()]);
        let fired = Arc::new(AtomicUsize::new(0));
        let f = fired.clone();
        sc.set_on_swap(Box::new(move |_p: &Path| {
            f.fetch_add(1, Ordering::SeqCst);
        }));
        assert!(sc.add_trusted_dir(PathBuf::from("/extra")));
        assert_eq!(
            fired.load(Ordering::SeqCst),
            0,
            "add_trusted_dir must not fire the cwd on-swap callback"
        );
    }

    #[test]
    fn add_trusted_dir_does_not_mirror_into_live_cell() {
        // The live-cwd mirror cells track the cwd; an add leaves the cwd alone
        // so a linked cell must be untouched.
        let sc = SessionCwd::new(boot(), vec![boot()]);
        let cell: Arc<Mutex<PathBuf>> = Arc::new(Mutex::new(boot()));
        sc.link_live_cwd(cell.clone());
        assert!(sc.add_trusted_dir(PathBuf::from("/extra")));
        assert_eq!(
            *cell.lock().unwrap(),
            boot(),
            "add_trusted_dir must not perturb the linked live-cwd cell"
        );
    }

    #[test]
    fn change_cwd_preserves_additional_working_dirs() {
        // REGRESSION (review RV4): `/cd` must move the cwd while KEEPING the
        // additional working directories (settings dirs + runtime `/add-dir`
        // grants). Boot in `proj`, `/add-dir shared`, then `/cd other`: the
        // new trusted set is `[other, shared]` — `shared` survives, the old
        // cwd `proj` is dropped. A plain `swap(other, vec![other])` (the
        // pre-fix behavior) would drop `shared`, breaking a later Read/Write
        // under it.
        let proj = PathBuf::from("/home/me/project");
        let shared = PathBuf::from("/data/shared");
        let other = PathBuf::from("/home/me/other");
        let sc = SessionCwd::new(proj.clone(), vec![proj.clone()]);
        assert!(sc.add_trusted_dir(shared.clone()));
        assert_eq!(sc.trusted_dirs(), vec![proj.clone(), shared.clone()]);

        sc.change_cwd(other.clone());
        assert_eq!(sc.cwd(), other, "cwd moved to the /cd target");
        assert_eq!(
            sc.trusted_dirs(),
            vec![other.clone(), shared.clone()],
            "additional working dir survives /cd; old cwd is dropped"
        );
    }

    #[test]
    fn change_cwd_dedupes_target_already_trusted() {
        // If the /cd target was already an additional working dir, it must
        // appear exactly once (at the front), and the old cwd is still dropped.
        let proj = PathBuf::from("/home/me/project");
        let other = PathBuf::from("/home/me/other");
        let sc = SessionCwd::new(proj.clone(), vec![proj.clone()]);
        assert!(sc.add_trusted_dir(other.clone()));
        assert_eq!(sc.trusted_dirs(), vec![proj.clone(), other.clone()]);

        sc.change_cwd(other.clone());
        assert_eq!(sc.cwd(), other);
        assert_eq!(
            sc.trusted_dirs(),
            vec![other.clone()],
            "target de-duplicated; old cwd dropped"
        );
    }

    #[test]
    fn change_cwd_fires_on_swap_and_mirrors_live_cell() {
        // The cwd changed, so (like swap) the on-swap callback fires with the
        // new cwd and any linked live-cwd cell is updated.
        let sc = SessionCwd::new(boot(), vec![boot()]);
        let cell: Arc<Mutex<PathBuf>> = Arc::new(Mutex::new(boot()));
        sc.link_live_cwd(cell.clone());
        let seen: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
        let seen_clone = Arc::clone(&seen);
        sc.set_on_swap(Box::new(move |p: &Path| {
            *seen_clone.lock().unwrap() = Some(p.to_path_buf());
        }));

        let target = PathBuf::from("/home/me/other");
        sc.change_cwd(target.clone());
        assert_eq!(*seen.lock().unwrap(), Some(target.clone()));
        assert_eq!(*cell.lock().unwrap(), target);
    }

    #[test]
    fn on_swap_callback_fires_with_new_cwd() {
        let sc = SessionCwd::new(boot(), vec![boot()]);
        let seen: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
        let seen_clone = Arc::clone(&seen);
        sc.set_on_swap(Box::new(move |p: &Path| {
            *seen_clone.lock().unwrap() = Some(p.to_path_buf());
        }));
        let new_cwd = PathBuf::from("/worktree/foo");
        sc.swap(new_cwd.clone(), vec![new_cwd.clone()]);
        assert_eq!(*seen.lock().unwrap(), Some(new_cwd));
    }

    #[test]
    fn on_swap_callback_is_noop_until_set() {
        // No callback registered — swap must not panic and must still take
        // effect.
        let sc = SessionCwd::new(boot(), vec![boot()]);
        let new_cwd = PathBuf::from("/worktree/foo");
        sc.swap(new_cwd.clone(), vec![new_cwd.clone()]);
        assert_eq!(sc.cwd(), new_cwd);
    }

    #[test]
    fn snapshot_returns_boot_pair_before_any_swap() {
        let trusted = vec![boot(), PathBuf::from("/other")];
        let sc = SessionCwd::new(boot(), trusted.clone());
        assert_eq!(sc.snapshot(), (boot(), trusted));
    }

    #[test]
    fn snapshot_reflects_swap_as_one_matched_pair() {
        let sc = SessionCwd::new(boot(), vec![boot()]);
        let new_cwd = PathBuf::from("/worktree/foo");
        let new_trusted = vec![new_cwd.clone(), PathBuf::from("/worktree/bar")];
        sc.swap(new_cwd.clone(), new_trusted.clone());
        assert_eq!(sc.snapshot(), (new_cwd, new_trusted));
    }

    #[test]
    fn swap_publishes_atomic_pair() {
        // After a swap, cwd() and trusted_dirs() must both reflect the new
        // values together — the pair is published as one unit, never with
        // one field lagging the other.
        let sc = SessionCwd::new(boot(), vec![boot()]);
        let new_cwd = PathBuf::from("/worktree/foo");
        let new_trusted = vec![new_cwd.clone(), PathBuf::from("/worktree/bar")];
        sc.swap(new_cwd.clone(), new_trusted.clone());
        assert_eq!(sc.cwd(), new_cwd);
        assert_eq!(sc.trusted_dirs(), new_trusted);
    }

    #[test]
    fn concurrent_swaps_never_expose_torn_pair() {
        // Each swap in this test publishes trusted_dirs == vec![cwd], so any
        // single load of the shared `CwdState` must show that exact
        // invariant — proving cwd and trusted_dirs are always published
        // together as one atomic pair, never observed from two different
        // generations. This directly exercises the fix that replaced two
        // independent ArcSwap cells with a single ArcSwap<CwdState>.
        let sc = SessionCwd::new(boot(), vec![boot()]);
        let n_swaps = 500;

        let writer = {
            let sc = Arc::clone(&sc);
            thread::spawn(move || {
                for i in 0..n_swaps {
                    let p = PathBuf::from(format!("/worktree/{i}"));
                    sc.swap(p.clone(), vec![p]);
                }
            })
        };

        let readers: Vec<_> = (0..4)
            .map(|_| {
                let sc = Arc::clone(&sc);
                thread::spawn(move || {
                    for _ in 0..2000 {
                        // Load the single state cell once so cwd and
                        // trusted_dirs are guaranteed to come from the same
                        // published generation.
                        let state = sc.state.load();
                        assert_eq!(state.trusted_dirs, vec![state.cwd.clone()]);
                    }
                })
            })
            .collect();

        writer.join().unwrap();
        for r in readers {
            r.join().unwrap();
        }
    }

    #[test]
    fn panicking_on_swap_callback_does_not_poison_mutex() {
        // A panicking callback must unwind without poisoning the internal
        // on_swap mutex, and without leaving the callback lock held (which
        // would also self-deadlock a reentrant callback). A later swap —
        // even with a fresh, non-panicking callback — must still succeed.
        let sc = SessionCwd::new(boot(), vec![boot()]);
        sc.set_on_swap(Box::new(|_p: &Path| {
            panic!("boom");
        }));

        let new_cwd = PathBuf::from("/worktree/foo");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sc.swap(new_cwd.clone(), vec![new_cwd.clone()]);
        }));
        assert!(result.is_err(), "expected the panicking callback to unwind");

        // The swap's new state must have been published even though the
        // callback afterwards panicked.
        assert_eq!(sc.cwd(), new_cwd);

        let saw_later: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
        let saw_later_clone = Arc::clone(&saw_later);
        sc.set_on_swap(Box::new(move |p: &Path| {
            *saw_later_clone.lock().unwrap() = Some(p.to_path_buf());
        }));
        let later_cwd = PathBuf::from("/worktree/bar");
        sc.swap(later_cwd.clone(), vec![later_cwd.clone()]);
        assert_eq!(sc.cwd(), later_cwd);
        assert_eq!(*saw_later.lock().unwrap(), Some(later_cwd));
    }

    #[test]
    fn concurrent_reads_see_last_swap() {
        let sc = SessionCwd::new(boot(), vec![boot()]);
        let n_swaps = 50;
        let swap_count = Arc::new(AtomicUsize::new(0));

        let writer = {
            let sc = Arc::clone(&sc);
            let swap_count = Arc::clone(&swap_count);
            thread::spawn(move || {
                for i in 0..n_swaps {
                    let p = PathBuf::from(format!("/worktree/{i}"));
                    sc.swap(p.clone(), vec![p]);
                    swap_count.fetch_add(1, Ordering::SeqCst);
                }
            })
        };

        let readers: Vec<_> = (0..4)
            .map(|_| {
                let sc = Arc::clone(&sc);
                thread::spawn(move || {
                    // `cwd()` and `trusted_dirs()` each do their own
                    // `load()` of the single shared state cell, so back-to-
                    // back calls may still straddle two different swaps if
                    // a writer publishes in between them — that race is
                    // inherent to exposing two separate accessor methods
                    // and is not what this test checks. This test only
                    // asserts each individual read is well-formed
                    // (never torn/garbage) and that trusted_dirs always
                    // carries exactly one entry, as every swap in this test
                    // publishes.
                    for _ in 0..200 {
                        let cwd = sc.cwd();
                        let trusted = sc.trusted_dirs();
                        assert!(cwd == boot() || cwd.starts_with("/worktree/"));
                        assert_eq!(trusted.len(), 1);
                    }
                })
            })
            .collect();

        writer.join().unwrap();
        for r in readers {
            r.join().unwrap();
        }

        assert_eq!(swap_count.load(Ordering::SeqCst), n_swaps);
        // Final state must reflect the very last swap.
        assert_eq!(
            sc.cwd(),
            PathBuf::from(format!("/worktree/{}", n_swaps - 1))
        );
    }
}
