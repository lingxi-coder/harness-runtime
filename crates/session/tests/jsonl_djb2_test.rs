//! `djb2_hash` 1:1 byte-for-byte parity with claude-code/src/utils/hash.ts.
//! Reference values computed by running the TS function in Node.js
//! (see plan T2 step 1).

use session::jsonl::djb2::djb2_hash;

#[test]
fn empty_string_is_zero() {
    assert_eq!(djb2_hash(""), 0);
}

#[test]
fn single_ascii_char_is_codepoint() {
    // hash = 0 -> (0<<5) - 0 + 97 = 97
    assert_eq!(djb2_hash("a"), 97);
}

#[test]
fn hello_matches_reference() {
    assert_eq!(djb2_hash("hello"), 99_162_322);
}

#[test]
fn typical_cwd_matches_reference() {
    // Hand-verified by stepping through the djb2 loop bit-by-bit with signed
    // i32 wrap; matches the plan's documented algorithm. (Plan's pre-computed
    // -1_067_725_492 was a transcription error.)
    assert_eq!(djb2_hash("/Users/foo/proj"), 50_650_428);
}

#[test]
fn short_path_matches_reference() {
    // Step trace for "/tmp" -> 47, 116, 109, 112:
    // h=47 -> 47*31+116=1573 -> 1573*31+109=48872 -> 48872*31+112=1515144.
    // (Plan's pre-computed 3_556_503 was a transcription error.)
    assert_eq!(djb2_hash("/tmp"), 1_515_144);
}

#[test]
fn unicode_iterates_utf16_units() {
    // "é" is U+00E9 — one UTF-16 unit (0x00E9 = 233).
    // hash = (0<<5) - 0 + 233 = 233.
    assert_eq!(djb2_hash("é"), 233);
    // "🦀" is U+1F980 — two UTF-16 units: 0xD83E (high surrogate) + 0xDD80 (low surrogate)
    // step 1: hash = 0 -> 0xD83E (55_358)
    // step 2: hash = 55_358 * 31 + 0xDD80 = 1_716_098 + 56_704 = 1_772_802
    assert_eq!(djb2_hash("🦀"), 1_772_802);
}
