//! Locale-aware ordering for filesystem skill directory names.
//!
//! Native sorts names with JavaScript `String.prototype.localeCompare`. This
//! module uses the same ICU4X profile for valid Unicode text and accepts raw
//! UTF-16 input, but ICU4X currently replaces unpaired surrogates differently
//! from the pinned JavaScript runtime for some comparisons.

use icu_collator::options::{AlternateHandling, CollatorOptions, Strength};
use icu_collator::preferences::{CollationCaseFirst, CollationNumericOrdering};
use icu_collator::{Collator, CollatorBorrowed, CollatorPreferences};
use icu_locale::Locale;

/// Construct the default-locale collator used for local filesystem skill names.
pub(crate) fn native_skill_name_collator() -> CollatorBorrowed<'static> {
    collator_for_locale(&native_default_locale())
}

/// Compare UTF-16 names using the same default-locale ICU profile as Native
/// `localeCompare`. Scalar Unicode ordering is pinned to the JavaScript oracle;
/// some lone-surrogate pairs remain a known collation gap.
#[must_use]
pub fn native_locale_compare_utf16(left: &[u16], right: &[u16]) -> std::cmp::Ordering {
    native_skill_name_collator().compare_utf16(left, right)
}

/// Construct the Native `localeCompare` sort profile for a BCP-47 locale.
///
/// Kept crate-visible so the same source-shaped profile can be tested against
/// the pinned Native component oracle with explicit locales.
pub(crate) fn collator_for_locale(locale: &str) -> CollatorBorrowed<'static> {
    let locale = Locale::try_from_str(locale).unwrap_or_else(|_| {
        Locale::try_from_str("en-US").expect("ICU4X compiled data includes en-US")
    });
    let mut preferences = CollatorPreferences::from(&locale);
    preferences.case_first = Some(CollationCaseFirst::False);
    preferences.numeric_ordering = Some(CollationNumericOrdering::False);

    let mut options = CollatorOptions::default();
    options.strength = Some(Strength::Tertiary);
    options.alternate_handling = Some(AlternateHandling::NonIgnorable);

    Collator::try_new(preferences, options)
        .expect("ICU4X compiled collation data must provide the requested locale")
}

fn native_default_locale() -> String {
    #[cfg(any(target_vendor = "apple", target_os = "windows", target_os = "android"))]
    {
        if let Some(locale) = sys_locale::get_locale() {
            return normalize_locale_identifier(&locale);
        }
        "en-US".to_owned()
    }

    #[cfg(all(unix, not(any(target_vendor = "apple", target_os = "android"))))]
    {
        lingxi_core::native_locale::current_lc_ctype()
            .as_deref()
            .map(normalize_locale_identifier)
            .unwrap_or_else(|| "en-US".to_owned())
    }

    #[cfg(not(any(unix, target_os = "windows", target_os = "android")))]
    {
        "en-US".to_owned()
    }
}

fn normalize_locale_identifier(locale: &str) -> String {
    let without_encoding = locale.split('.').next().unwrap_or(locale);
    let without_modifier = without_encoding
        .split('@')
        .next()
        .unwrap_or(without_encoding);
    if without_modifier.is_empty()
        || without_modifier.eq_ignore_ascii_case("C")
        || without_modifier.eq_ignore_ascii_case("POSIX")
    {
        return "en-US".to_owned();
    }
    without_modifier.replace('_', "-")
}

#[cfg(test)]
mod tests {
    use super::{collator_for_locale, normalize_locale_identifier};
    use std::cmp::Ordering;

    #[test]
    fn en_us_matches_pinned_native_valid_scalar_sort_oracle() {
        // Captured with the pinned Bun 1.4.3 oracle. Names include the Unicode
        // scalar/punctuation directory-name captures and distinguish locale
        // collation from code-point, numeric, and punctuation sorting.
        let input = [
            "file2",
            "file10",
            "foo-2",
            "foo_2",
            "ä",
            "ae",
            "a",
            "A",
            "é",
            "e\u{0301}",
            "i",
            "İ",
            "ı",
            "😀",
            "skill-🧪-probe",
            "skill.alpha+beta",
        ];
        let collator = collator_for_locale("en-US");
        let mut actual = input;
        actual.sort_by(|left, right| collator.compare(left, right));

        assert_eq!(
            actual,
            [
                "😀",
                "a",
                "A",
                "ä",
                "ae",
                "é",
                "e\u{0301}",
                "file10",
                "file2",
                "foo_2",
                "foo-2",
                "i",
                "İ",
                "ı",
                "skill-🧪-probe",
                "skill.alpha+beta",
            ]
        );
        assert_eq!(collator.compare("é", "e\u{0301}"), Ordering::Equal);
        assert_eq!(collator.compare("file10", "file2"), Ordering::Less);
        assert_eq!(collator.compare("foo_2", "foo-2"), Ordering::Less);
    }

    #[test]
    fn explicit_locale_changes_native_default_collation_pair() {
        assert_eq!(
            collator_for_locale("en-US").compare("z", "ä"),
            Ordering::Greater
        );
        assert_eq!(
            collator_for_locale("sv-SE").compare("z", "ä"),
            Ordering::Less
        );
    }

    #[test]
    fn posix_locale_names_map_to_native_english_fallback() {
        for input in ["C", "POSIX", "C.UTF-8", "POSIX.UTF-8", ""] {
            assert_eq!(normalize_locale_identifier(input), "en-US", "{input:?}");
        }
        assert_eq!(normalize_locale_identifier("sv_SE.UTF-8"), "sv-SE");
    }
}
