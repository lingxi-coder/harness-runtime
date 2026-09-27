//! Merging [`FileStateCache`]s — used when a `SideQuery` or `ForkedAgent`
//! resolves and its cached Reads need to be folded back into the parent
//! (spec §23.4).

use super::cache::FileStateCache;

/// Fold every entry from `from` into `into`, keeping whichever side has the
/// newer `timestamp`.
pub fn merge_caches(into: &FileStateCache, from: &FileStateCache) {
    for (path, state) in from.dump() {
        let p = path.to_str().unwrap_or("");
        if let Some(existing) = into.get(p) {
            if state.timestamp > existing.timestamp {
                into.set(p, state);
            }
        } else {
            into.set(p, state);
        }
    }
}
