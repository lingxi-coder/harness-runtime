// lingxi-code/crates/memory/tests/memdir_module_exists.rs
//! Smoke test: the new modules compile and the locked constants exist
//! with the exact values from spec §7.

#[test]
fn constants_match_spec_wire_identifiers() {
    assert_eq!(memory::MAX_MEMORY_FILE_SIZE, 10 * 1024 * 1024);
    assert_eq!(memory::MEMORY_AGE_PENALTY_DAYS, 30);
    assert_eq!(memory::MEMORY_AGE_HARD_DROP_DAYS, 365);
    assert_eq!(memory::MEMORY_MIN_AGE_WEIGHT_BPS, 1_000);
    assert_eq!(memory::DEFAULT_RELEVANT_MEMORIES, 5);
}

#[test]
fn memory_entry_re_exported_from_protocol() {
    fn _accepts(_: lingxi_core::types::MemoryEntry) {}
}
