//! Pure text projection for Native's 2.1.289 server-fallback refusal API row.
//!
//! The formatter consumes resolved host facts. In particular, callers must
//! supply the refusal-policy (`eYe`) result, display label (`$o`), catalog
//! family, Fable-copy suppression (`Dw`), model-specific first-time exception,
//! help URL (`b1r`), actual provider kind, and feedback eligibility (`QA`).
//! Organization `availableModels` membership is not a substitute for the
//! refusal-policy result.

/// Native provider kinds that enable the special no-model cyber notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalProviderKind {
    FirstParty,
    AnthropicAws,
    AnthropicGoogleCloud,
    Gateway,
    Bedrock,
    Foundry,
    Mantle,
    Vertex,
    Other,
}

impl RefusalProviderKind {
    fn has_cyber_verification_copy(self) -> bool {
        matches!(
            self,
            Self::FirstParty | Self::AnthropicAws | Self::AnthropicGoogleCloud | Self::Gateway
        )
    }
}

/// User-visible brand strings interpolated by the Native formatter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefusalBrandCopy<'a> {
    pub api_error_prefix: &'a str,
    pub product_name: &'a str,
    pub generic_model_label: &'a str,
}

/// Owned brand copy for snapshots that outlive one refusal event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusalBrandCopyOwned {
    pub api_error_prefix: String,
    pub product_name: String,
    pub generic_model_label: String,
}

impl RefusalBrandCopyOwned {
    fn as_borrowed(&self) -> RefusalBrandCopy<'_> {
        RefusalBrandCopy {
            api_error_prefix: &self.api_error_prefix,
            product_name: &self.product_name,
            generic_model_label: &self.generic_model_label,
        }
    }
}

/// Resolved facts required to render one refusal API-error body.
///
/// This is deliberately data-only: the formatter performs no provider,
/// catalog, settings, feedback-policy, or model-eligibility lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefusalApiTextInput<'a> {
    /// The refusal category supplied by the provider, if it was a string.
    ///
    /// This API accepts Rust UTF-8 text only. An ill-formed UTF-16 category
    /// cannot be represented here; the upstream decoder/transport must either
    /// reject it or choose a policy before calling this formatter.
    pub category: Option<&'a str>,
    /// The original request ID. The textual suffix has a stricter filter than
    /// the row metadata, which retains this original value.
    pub request_id: Option<&'a str>,
    /// The model serving the refused request; absent means Native had no model.
    pub serving_model: Option<&'a str>,
    /// Result of Native's `eYe(serving_model)` policy decision. This is not an
    /// organization allowlist-membership test.
    pub model_eligible: bool,
    /// The independently resolved `$o(serving_model)` display label.
    pub display_label: Option<&'a str>,
    /// The current catalog family (`La(Nm(model))?.family`), if known.
    pub model_family: Option<&'a str>,
    /// Resolved `Dw(model)` fact; true suppresses first-time family wording.
    pub fable_copy_suppressed: bool,
    /// Resolved Native `v7(model)` explicit first-time-model exception.
    pub opus_5_5_exception: bool,
    /// The current `b1r(serving_model)` result. Required only when the
    /// model-labelled branch is selected.
    pub help_url: Option<&'a str>,
    /// Actual resolved provider kind; custom endpoint URLs are not enough to
    /// infer first-party provider identity.
    pub provider: RefusalProviderKind,
    /// `!Ce()` from Native launch options.
    pub interactive: bool,
    /// `QA()` feedback-command and product-policy result.
    pub feedback_eligible: bool,
    /// Explicit output-brand strings rather than an inferred default brand.
    pub brand: RefusalBrandCopy<'a>,
}

/// Owned, already-resolved facts for formatting multiple refusal events.
///
/// Construct this from the current query's trusted model/provider snapshot.
/// Event-specific category and request ID stay arguments to [`Self::format`],
/// so a caller cannot accidentally reuse one event's metadata for another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusalApiTextSnapshot {
    pub serving_model: Option<String>,
    pub model_eligible: bool,
    pub display_label: Option<String>,
    pub model_family: Option<String>,
    pub fable_copy_suppressed: bool,
    pub opus_5_5_exception: bool,
    pub help_url: Option<String>,
    pub provider: RefusalProviderKind,
    pub interactive: bool,
    pub feedback_eligible: bool,
    pub brand: RefusalBrandCopyOwned,
}

impl RefusalApiTextSnapshot {
    /// Format an event using this snapshot's owned route/policy facts.
    pub fn format(
        &self,
        category: Option<&str>,
        request_id: Option<&str>,
    ) -> Result<String, RefusalApiTextError> {
        format_refusal_api_error_text(&RefusalApiTextInput {
            category,
            request_id,
            serving_model: self.serving_model.as_deref(),
            model_eligible: self.model_eligible,
            display_label: self.display_label.as_deref(),
            model_family: self.model_family.as_deref(),
            fable_copy_suppressed: self.fable_copy_suppressed,
            opus_5_5_exception: self.opus_5_5_exception,
            help_url: self.help_url.as_deref(),
            provider: self.provider,
            interactive: self.interactive,
            feedback_eligible: self.feedback_eligible,
            brand: self.brand.as_borrowed(),
        })
    }
}

/// A formatter input cannot be represented faithfully as a Rust UTF-8 string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RefusalApiTextError {
    /// Native `slice(0, 255)` left an unpaired UTF-16 surrogate in the
    /// sanitized Details category. Rust `String` cannot carry that code unit.
    #[error("refusal category truncation produced an unrepresentable lone UTF-16 surrogate")]
    UnrepresentableCategoryUtf16,
    /// Native always resolves a help link in the model-labelled branch; a
    /// missing host fact must not be replaced with a guessed URL.
    #[error("model-labelled refusal text needs its resolved Native help URL")]
    MissingResolvedHelpUrl,
}

/// Render Native's `$dt("refusal", ...)` text for the supplied resolved facts.
pub fn format_refusal_api_error_text(
    input: &RefusalApiTextInput<'_>,
) -> Result<String, RefusalApiTextError> {
    let body = match (
        input.serving_model,
        input.model_eligible,
        input.display_label,
    ) {
        (Some(_), true, Some(label)) => format_model_labelled_body(input, label)?,
        _ => format_no_displayable_model_body(input),
    };

    let details = details_suffix(input.category)?;
    let request_id = request_id_suffix(input.request_id);
    Ok(format!("{body}{details}{request_id}"))
}

fn format_model_labelled_body(
    input: &RefusalApiTextInput<'_>,
    label: &str,
) -> Result<String, RefusalApiTextError> {
    let help_url = input
        .help_url
        .ok_or(RefusalApiTextError::MissingResolvedHelpUrl)?;
    let first_time_copy = first_time_refusal_copy(input, label);

    let (lead, recovery) = match first_time_copy {
        Some(copy) => (
            format!(
                "{label}'s safeguards flagged this session (https://www.anthropic.com/legal/aup). {copy}"
            ),
            format!(
                "{} can't respond to your last message with {label}.",
                input.brand.product_name
            ),
        ),
        None => (
            format!(
                "{label}'s safeguards flagged this message (https://www.anthropic.com/legal/aup). {}",
                category_copy(input.category)
            ),
            format!(
                "{} can't respond to this message with {label}.",
                input.brand.product_name
            ),
        ),
    };

    let recovery_action = if input.interactive {
        "Double press esc to edit your last message, or try a different model with /model."
    } else {
        "Try rephrasing the request in a new session or change your model."
    };
    let help_action = if !input.interactive {
        format!("Learn more: {help_url}")
    } else if input.feedback_eligible {
        format!("Send feedback with /feedback or learn more: {help_url}")
    } else {
        format!("You can learn more: {help_url}")
    };

    Ok(format!(
        "{}: {lead} {recovery}\n\n{recovery_action}\n\n{help_action}",
        input.brand.api_error_prefix
    ))
}

fn format_no_displayable_model_body(input: &RefusalApiTextInput<'_>) -> String {
    let cyber_label = match input.serving_model {
        Some(_) => input.display_label.unwrap_or("undefined"),
        None => "This model",
    };
    if input.category == Some("cyber") && input.provider.has_cyber_verification_copy() {
        let link_action = if input.interactive && input.feedback_eligible {
            "Send feedback with /feedback or learn more: https://support.claude.com/en/articles/14604842-real-time-cyber-safeguards-on-claude"
        } else {
            "Learn more: https://support.claude.com/en/articles/14604842-real-time-cyber-safeguards-on-claude"
        };
        return format!(
            "{}: {}'s safeguards flagged this message. {} {} {link_action}",
            input.brand.api_error_prefix,
            cyber_label,
            "Our intentionally broad safeguards allow us to deliver more capabilities faster, but can sometimes flag legitimate cybersecurity work.",
            "Apply to the Cyber Verification Program to reduce these interruptions."
        );
    }

    let label = match input.serving_model {
        Some(_) => input.display_label.unwrap_or("undefined"),
        None => input.brand.generic_model_label,
    };
    let help_action = if input.interactive && input.feedback_eligible {
        "Send feedback with /feedback or learn more: https://www.anthropic.com/legal/aup"
    } else {
        "Learn more: https://www.anthropic.com/legal/aup"
    };
    format!(
        "{}: {label} can't help with this. Start a new session to continue.\n\n{help_action}",
        input.brand.api_error_prefix
    )
}

fn category_copy(category: Option<&str>) -> &'static str {
    match category {
        Some("cyber") => {
            "Our intentionally broad safeguards allow us to deliver more capabilities faster, but can sometimes flag legitimate coding and cybersecurity tasks."
        }
        Some("bio") => {
            "Our intentionally broad safeguards allow us to deliver more capabilities faster, but can sometimes flag legitimate biology tasks."
        }
        _ => "This sometimes happens with safe, normal conversations.",
    }
}

fn first_time_refusal_copy(input: &RefusalApiTextInput<'_>, label: &str) -> Option<String> {
    if input.fable_copy_suppressed {
        return None;
    }

    let is_sonnet_family = input.model_family == Some("sonnet");
    if !is_sonnet_family && !input.opus_5_5_exception {
        return None;
    }

    let first_time_line = if is_sonnet_family {
        "You may be seeing this for the first time on a Sonnet model"
    } else {
        "You may be seeing this for the first time"
    };
    let copy = match input.category? {
        "cyber" if is_sonnet_family || input.opus_5_5_exception => format!(
            "{label} is more capable and has stronger safeguards as a result, which can sometimes flag non-cybersecurity work. We're improving these safeguards to reduce the amount of incorrectly flagged messages."
        ),
        "bio" if !is_sonnet_family && input.opus_5_5_exception => format!(
            "{label} is more capable and has stronger safeguards as a result, which can sometimes flag biology-research-adjacent work. We're improving these safeguards to reduce the amount of incorrectly flagged messages."
        ),
        "frontier_llm" if !is_sonnet_family && input.opus_5_5_exception => format!(
            "{label} is more capable and has stronger safeguards as a result. We're improving these safeguards to reduce the amount of incorrectly flagged messages."
        ),
        _ => return None,
    };
    Some(format!("{first_time_line}: {copy}"))
}

fn details_suffix(category: Option<&str>) -> Result<String, RefusalApiTextError> {
    let Some(category) = category else {
        return Ok(String::new());
    };
    let cleaned = sanitize_details_category(category)?;
    if cleaned.is_empty() {
        Ok(String::new())
    } else {
        Ok(format!("\n\nDetails: `[{}]`", cleaned))
    }
}

fn sanitize_details_category(category: &str) -> Result<String, RefusalApiTextError> {
    // Native `zee` runs Bun.stripANSI, then replaces each run matching
    // `p = /[\p{Cc}\p{Cf}\p{Cs}\p{Co}\p{Cn}\u2028\u2029\p{Default_Ignorable_Code_Point}\u2800]+/gu`
    // with one space. A Rust `str` is already well-formed UTF-8, so the Native
    // OV lone-surrogate cleanup has no input-side work to do here.
    let without_ansi = crate::host::display::strip_ansi_text(category);
    let replaced = replace_native_category_controls(&without_ansi);

    // Native `fi` (`179180145..179180367`) removes backticks/brackets,
    // collapses ECMAScript whitespace, trims, slices 255 UTF-16 code units,
    // and trims once more.
    let mut without_delimiters = String::with_capacity(replaced.len());
    for ch in replaced.chars() {
        if !matches!(ch, '`' | '[' | ']') {
            without_delimiters.push(ch);
        }
    }
    let collapsed = collapse_ecmascript_whitespace(&without_delimiters);
    let units = collapsed.encode_utf16().take(255).collect::<Vec<_>>();
    let truncated = String::from_utf16(&units)
        .map_err(|_| RefusalApiTextError::UnrepresentableCategoryUtf16)?;
    Ok(trim_ecmascript_whitespace(&truncated).to_owned())
}

fn replace_native_category_controls(value: &str) -> String {
    use unicode_general_category::{get_general_category, GeneralCategory};

    let mut out = String::with_capacity(value.len());
    let mut in_replaced_run = false;
    for ch in value.chars() {
        let category = get_general_category(ch);
        let replace = matches!(
            category,
            GeneralCategory::Control
                | GeneralCategory::Format
                | GeneralCategory::Surrogate
                | GeneralCategory::PrivateUse
                | GeneralCategory::Unassigned
                | GeneralCategory::LineSeparator
                | GeneralCategory::ParagraphSeparator
        ) || is_default_ignorable(ch)
            || ch == '\u{2800}';
        if replace {
            if !in_replaced_run {
                out.push(' ');
                in_replaced_run = true;
            }
        } else {
            in_replaced_run = false;
            out.push(ch);
        }
    }
    out
}

/// Unicode Default_Ignorable_Code_Point ranges used by the pinned Native
/// sanitizer. General categories Cc/Cf/Co/Cn are covered independently above.
fn is_default_ignorable(ch: char) -> bool {
    let codepoint = u32::from(ch);
    matches!(
        codepoint,
        0x00AD
            | 0x034F
            | 0x061C
            | 0x115F..=0x1160
            | 0x17B4..=0x17B5
            | 0x180B..=0x180F
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x206F
            | 0x3164
            | 0xFE00..=0xFE0F
            | 0xFEFF
            | 0xFFA0
            | 0xFFF0..=0xFFFB
            | 0x16FE4
            | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A
            | 0xE0000..=0xE0FFF
    )
}

fn collapse_ecmascript_whitespace(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut pending_space = false;
    for ch in value.chars() {
        if is_ecmascript_whitespace(ch) {
            pending_space = true;
            continue;
        }
        if pending_space && !out.is_empty() {
            out.push(' ');
        }
        pending_space = false;
        out.push(ch);
    }
    out
}

fn trim_ecmascript_whitespace(value: &str) -> &str {
    let start = value
        .char_indices()
        .find_map(|(index, ch)| (!is_ecmascript_whitespace(ch)).then_some(index))
        .unwrap_or(value.len());
    let end = value
        .char_indices()
        .rev()
        .find_map(|(index, ch)| (!is_ecmascript_whitespace(ch)).then_some(index + ch.len_utf8()))
        .unwrap_or(start);
    &value[start..end]
}

fn is_ecmascript_whitespace(ch: char) -> bool {
    matches!(
        ch,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

fn request_id_suffix(request_id: Option<&str>) -> String {
    let Some(request_id) = request_id else {
        return String::new();
    };
    if request_id.is_empty()
        || request_id.len() > 255
        || !request_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return String::new();
    }
    format!("\n\nRequest ID: {request_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const CLAUDE: RefusalBrandCopy<'static> = RefusalBrandCopy {
        api_error_prefix: "API Error",
        product_name: "Claude Code",
        generic_model_label: "Claude",
    };

    fn base_input() -> RefusalApiTextInput<'static> {
        RefusalApiTextInput {
            category: None,
            request_id: None,
            serving_model: None,
            model_eligible: false,
            display_label: None,
            model_family: None,
            fable_copy_suppressed: false,
            opus_5_5_exception: false,
            help_url: None,
            provider: RefusalProviderKind::Other,
            interactive: false,
            feedback_eligible: false,
            brand: CLAUDE,
        }
    }

    fn first_native_snapshot() -> RefusalApiTextSnapshot {
        // Local Strings are moved into the snapshot; no borrow from the setup
        // scope is retained by the returned value.
        let serving_model = String::from("claude-sonnet-5");
        let display_label = String::from("Claude Sonnet 5");
        let model_family = String::from("sonnet");
        let help_url = String::from("https://support.claude.com/en/articles/8106465");
        RefusalApiTextSnapshot {
            serving_model: Some(serving_model),
            model_eligible: true,
            display_label: Some(display_label),
            model_family: Some(model_family),
            fable_copy_suppressed: false,
            opus_5_5_exception: false,
            help_url: Some(help_url),
            provider: RefusalProviderKind::FirstParty,
            interactive: true,
            feedback_eligible: true,
            brand: RefusalBrandCopyOwned {
                api_error_prefix: String::from("API Error"),
                product_name: String::from("Claude Code"),
                generic_model_label: String::from("Claude"),
            },
        }
    }

    #[test]
    fn six_native_formatter_vectors_match_the_pinned_fixture() {
        let fixtures: Value = serde_json::from_str(include_str!(
            "../../../../docs/parity/mods-refusal-fallback-text-2.1.289.json"
        ))
        .expect("refusal formatter fixture is valid JSON");
        let outputs = fixtures["outputs"]
            .as_array()
            .expect("fixture outputs are present");
        let vectors = [
            (
                "first-party interactive sonnet cyber refusal",
                RefusalApiTextInput {
                    category: Some("cyber"),
                    request_id: Some("req_289"),
                    serving_model: Some("claude-sonnet-5"),
                    model_eligible: true,
                    display_label: Some("Claude Sonnet 5"),
                    model_family: Some("sonnet"),
                    help_url: Some("https://support.claude.com/en/articles/8106465"),
                    provider: RefusalProviderKind::FirstParty,
                    interactive: true,
                    feedback_eligible: true,
                    ..base_input()
                },
            ),
            (
                "non-interactive first-party cyber with no displayable model",
                RefusalApiTextInput {
                    category: Some("cyber"),
                    request_id: Some("req_289"),
                    serving_model: Some("claude-sonnet-5"),
                    display_label: Some("Claude Sonnet 5"),
                    model_family: Some("sonnet"),
                    help_url: Some("https://support.claude.com/en/articles/8106465"),
                    provider: RefusalProviderKind::FirstParty,
                    feedback_eligible: true,
                    ..base_input()
                },
            ),
            (
                "non-interactive Bedrock bio with feedback disabled and no model",
                RefusalApiTextInput {
                    category: Some("bio"),
                    request_id: Some("bad id"),
                    provider: RefusalProviderKind::Bedrock,
                    ..base_input()
                },
            ),
            (
                "interactive mode with nonessential traffic disabled suppresses feedback CTA",
                RefusalApiTextInput {
                    interactive: true,
                    ..base_input()
                },
            ),
            (
                "eligible Fable model uses the Fable help page",
                RefusalApiTextInput {
                    category: Some("bio"),
                    serving_model: Some("claude-fable-5-1"),
                    model_eligible: true,
                    display_label: Some("Claude Fable 5.1"),
                    model_family: Some("fable"),
                    fable_copy_suppressed: true,
                    help_url: Some("https://support.claude.com/en/articles/15363606"),
                    provider: RefusalProviderKind::Foundry,
                    interactive: true,
                    ..base_input()
                },
            ),
            (
                "eligible Opus 5 effort alias uses the Opus help page",
                RefusalApiTextInput {
                    category: Some("bio"),
                    request_id: Some("req_opus"),
                    serving_model: Some("claude-opus-5[1m]"),
                    model_eligible: true,
                    display_label: Some("Claude Opus 5"),
                    opus_5_5_exception: false,
                    help_url: Some("https://support.claude.com/en/articles/16049681"),
                    provider: RefusalProviderKind::FirstParty,
                    feedback_eligible: true,
                    ..base_input()
                },
            ),
        ];

        assert_eq!(vectors.len(), outputs.len());
        for (name, facts) in vectors {
            let expected = outputs
                .iter()
                .find(|entry| entry["name"] == name)
                .unwrap_or_else(|| panic!("missing native fixture vector {name}"));
            assert_eq!(
                format_refusal_api_error_text(&facts).unwrap(),
                expected["content"].as_str().unwrap(),
                "{name}"
            );
        }
    }

    #[test]
    fn owned_snapshot_formats_event_metadata_without_mutating_route_facts() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RefusalApiTextSnapshot>();

        let snapshot = first_native_snapshot();
        let before = snapshot.clone();
        let cloned = snapshot.clone();
        let fixtures: Value = serde_json::from_str(include_str!(
            "../../../../docs/parity/mods-refusal-fallback-text-2.1.289.json"
        ))
        .expect("refusal formatter fixture is valid JSON");
        let expected = fixtures["outputs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == "first-party interactive sonnet cyber refusal")
            .unwrap()["content"]
            .as_str()
            .unwrap();

        assert_eq!(
            snapshot.format(Some("cyber"), Some("req_289")).unwrap(),
            expected
        );
        assert_eq!(
            cloned.format(Some("cyber"), Some("req_289")).unwrap(),
            expected
        );
        assert_eq!(
            snapshot.format(Some("bio"), Some("bad id")).unwrap(),
            snapshot.format(Some("bio"), None).unwrap(),
            "invalid request IDs affect only the text suffix"
        );
        assert!(snapshot
            .format(Some("bio"), Some("bad id"))
            .unwrap()
            .ends_with("\n\nDetails: `[bio]`"));
        assert!(!snapshot
            .format(Some("bio"), Some("bad id"))
            .unwrap()
            .contains("Request ID:"));
        assert_eq!(snapshot, before);
        assert_eq!(cloned, before);
    }

    #[test]
    fn category_uses_native_whitespace_delimiter_and_ansi_rules() {
        let facts = RefusalApiTextInput {
            category: Some("\u{feff}\x1b[31mcyber\x1b[0m\u{0085}"),
            ..base_input()
        };
        let text = format_refusal_api_error_text(&facts).unwrap();
        assert!(text.ends_with("\n\nDetails: `[cyber]`"));

        let facts = RefusalApiTextInput {
            category: Some("[bio]`"),
            ..facts
        };
        assert!(format_refusal_api_error_text(&facts)
            .unwrap()
            .ends_with("\n\nDetails: `[bio]`"));

        let facts = RefusalApiTextInput {
            category: Some("\u{00a0}bio\u{00a0}"),
            ..base_input()
        };
        assert!(format_refusal_api_error_text(&facts)
            .unwrap()
            .ends_with("\n\nDetails: `[bio]`"));
    }

    #[test]
    fn details_cap_is_utf16_and_split_surrogate_is_reported() {
        assert_eq!(
            sanitize_details_category(&"x".repeat(256)).unwrap(),
            "x".repeat(255)
        );

        let category = format!("{}😀", "x".repeat(253));
        let mut facts = RefusalApiTextInput {
            category: Some(&category),
            ..base_input()
        };
        let text = format_refusal_api_error_text(&facts).unwrap();
        assert!(text.ends_with(&format!("\n\nDetails: `[{category}]`")));

        let split = format!("{}😀", "x".repeat(254));
        facts.category = Some(&split);
        assert_eq!(
            format_refusal_api_error_text(&facts),
            Err(RefusalApiTextError::UnrepresentableCategoryUtf16)
        );
    }

    #[test]
    fn invalid_request_id_is_not_rendered_and_missing_help_fact_fails_closed() {
        let invalid_id = RefusalApiTextInput {
            request_id: Some("bad id"),
            ..base_input()
        };
        assert!(!format_refusal_api_error_text(&invalid_id)
            .unwrap()
            .contains("Request ID:"));

        let missing_url = RefusalApiTextInput {
            category: Some("bio"),
            serving_model: Some("model-id"),
            model_eligible: true,
            display_label: Some("Model"),
            interactive: true,
            ..base_input()
        };
        assert_eq!(
            format_refusal_api_error_text(&missing_url),
            Err(RefusalApiTextError::MissingResolvedHelpUrl)
        );
    }

    #[test]
    fn opus_5_5_exception_uses_first_time_copy_without_sonnet_label() {
        let facts = RefusalApiTextInput {
            category: Some("cyber"),
            serving_model: Some("claude-opus-5-5"),
            model_eligible: true,
            display_label: Some("Claude Opus 5.5"),
            opus_5_5_exception: true,
            help_url: Some("https://support.claude.com/en/articles/16049681"),
            ..base_input()
        };
        let text = format_refusal_api_error_text(&facts).unwrap();
        assert!(text.contains(
            "You may be seeing this for the first time: Claude Opus 5.5 is more capable"
        ));
        assert!(!text.contains("on a Sonnet model"));
    }

    #[test]
    fn no_model_cyber_copy_uses_the_native_provider_kind_gate() {
        let eligible = [
            RefusalProviderKind::FirstParty,
            RefusalProviderKind::AnthropicAws,
            RefusalProviderKind::AnthropicGoogleCloud,
            RefusalProviderKind::Gateway,
        ];
        for provider in eligible {
            let facts = RefusalApiTextInput {
                category: Some("cyber"),
                provider,
                ..base_input()
            };
            assert!(format_refusal_api_error_text(&facts)
                .unwrap()
                .contains("Cyber Verification Program"));
        }

        let ineligible = [
            RefusalProviderKind::Bedrock,
            RefusalProviderKind::Foundry,
            RefusalProviderKind::Mantle,
            RefusalProviderKind::Vertex,
            RefusalProviderKind::Other,
        ];
        for provider in ineligible {
            let facts = RefusalApiTextInput {
                category: Some("cyber"),
                provider,
                ..base_input()
            };
            assert!(!format_refusal_api_error_text(&facts)
                .unwrap()
                .contains("Cyber Verification Program"));
        }
    }
}
