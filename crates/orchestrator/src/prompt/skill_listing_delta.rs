//! Membership changes append small catalog updates instead of rewriting history.
use std::collections::HashSet;

pub(crate) struct SkillMembershipDelta<T> {
    pub(crate) added: Vec<T>,
    pub(crate) withdrawn: Vec<Vec<u16>>,
    pub(crate) initial: bool,
}

pub(crate) fn collect<T>(
    entries: Vec<T>,
    sent: &mut HashSet<Vec<u16>>,
    name: impl Fn(&T) -> Vec<u16>,
) -> SkillMembershipDelta<T> {
    let initial = sent.is_empty();
    let current = entries.iter().map(&name).collect::<HashSet<_>>();
    let mut withdrawn = sent.difference(&current).cloned().collect::<Vec<_>>();
    withdrawn.sort();
    sent.retain(|name| current.contains(name));
    let added = entries
        .into_iter()
        .filter(|entry| !sent.contains(&name(entry)))
        .collect::<Vec<_>>();
    for entry in &added {
        sent.insert(name(entry));
    }
    SkillMembershipDelta {
        added,
        withdrawn,
        initial,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }
    fn key(value: &String) -> Vec<u16> {
        value.encode_utf16().collect()
    }

    #[test]
    fn unchanged_configuration_emits_nothing_and_toggle_updates_are_minimal() {
        let mut sent = HashSet::new();
        let initial = collect(names(&["stable"]), &mut sent, key);
        assert!(initial.initial);
        assert_eq!(initial.added, names(&["stable"]));
        let same = collect(names(&["stable"]), &mut sent, key);
        assert!(same.added.is_empty());
        assert!(same.withdrawn.is_empty());
        let enable = collect(names(&["stable", "decision"]), &mut sent, key);
        assert_eq!(enable.added, names(&["decision"]));
        assert!(enable.withdrawn.is_empty());
        assert!(!enable.initial);
        let backend_edit = collect(names(&["stable", "decision"]), &mut sent, key);
        assert!(backend_edit.added.is_empty());
        assert!(backend_edit.withdrawn.is_empty());
        let disable = collect(names(&["stable"]), &mut sent, key);
        assert!(disable.added.is_empty());
        assert_eq!(disable.withdrawn, vec![key(&"decision".into())]);
        let repeat = collect(names(&["stable"]), &mut sent, key);
        assert!(repeat.added.is_empty());
        assert!(repeat.withdrawn.is_empty());
        let restore = collect(names(&["stable", "decision"]), &mut sent, key);
        assert_eq!(restore.added, enable.added);
        assert!(restore.withdrawn.is_empty());
    }
    #[test]
    fn context_reset_rebuilds_one_listing_and_preserves_source_order() {
        let mut sent = HashSet::new();
        let first = collect(names(&["zeta", "decision", "alpha"]), &mut sent, key);
        sent.clear();
        let compacted = collect(names(&["zeta", "decision", "alpha"]), &mut sent, key);
        assert!(compacted.initial);
        assert_eq!(first.added, compacted.added);
    }
}
