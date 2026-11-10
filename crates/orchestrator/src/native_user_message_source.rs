//! Explicit Native provenance for user-message rows.
//!
//! This source is separate from [`crate::mod_prompt_origin`], which describes
//! the hook ingress and must never be used to infer transcript user origin.

use std::future::Future;

use lingxi_core::types::utf16_json::Utf16JsonProjection;

/// Optional Native fields stamped only by an ingress that knows their source.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NativeUserMessageSource {
    /// Exact Native `origin` object for this user row.
    pub message_origin: Option<Utf16JsonProjection>,
    /// Exact Native `promptSource` value for this user row.
    pub prompt_source: Option<Utf16JsonProjection>,
}

tokio::task_local! {
    static NATIVE_USER_MESSAGE_SOURCE: NativeUserMessageSource;
    static QUEUED_TURN_AUTHORITY: bool;
}

/// Scope the host's human-input authority independently of transcript
/// visibility. Model-generated visible kickoffs must not acquire human-only
/// lifecycle effects merely because their user row is not meta.
pub async fn with_queued_turn_authority<F: Future>(in_human_turn: bool, future: F) -> F::Output {
    QUEUED_TURN_AUTHORITY.scope(in_human_turn, future).await
}

/// Read a trusted queued-turn authority supplied by the owning ingress. This
/// value is never inferred from model text or persisted row provenance.
#[must_use]
pub fn queued_turn_authority() -> Option<bool> {
    QUEUED_TURN_AUTHORITY.try_with(|human| *human).ok()
}

/// Scope Native row provenance to one explicit user-input dispatch.
pub async fn with_source<F: Future>(source: NativeUserMessageSource, future: F) -> F::Output {
    NATIVE_USER_MESSAGE_SOURCE.scope(source, future).await
}

/// Scope the known Native `promptSource:"sdk"` value without supplying a
/// user origin. Used only by noninteractive SDK/headless ingress adapters.
pub async fn with_sdk_source<F: Future>(future: F) -> F::Output {
    with_source(sdk(), future).await
}

/// Read the current explicit Native source, if the ingress supplied one.
#[must_use]
pub fn current() -> Option<NativeUserMessageSource> {
    NATIVE_USER_MESSAGE_SOURCE.try_with(Clone::clone).ok()
}

/// Construct the exact source object used for a real human input.
#[must_use]
pub fn human() -> NativeUserMessageSource {
    NativeUserMessageSource {
        message_origin: Some(
            Utf16JsonProjection::parse(r#"{"kind":"human"}"#)
                .expect("static Native human origin is valid JSON"),
        ),
        prompt_source: None,
    }
}

/// Construct the known noninteractive SDK prompt source without inventing an
/// origin; Native's headless input constructor omits `origin`.
#[must_use]
pub fn sdk() -> NativeUserMessageSource {
    NativeUserMessageSource {
        message_origin: None,
        prompt_source: Some(Utf16JsonProjection::from("sdk")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn source_scopes_keep_human_origin_and_sdk_prompt_source_distinct() {
        let human = with_source(human(), async { current().unwrap() }).await;
        assert_eq!(
            human
                .message_origin
                .as_ref()
                .and_then(|origin| origin.string_at("/kind")),
            Some("human")
        );
        assert!(human.prompt_source.is_none());

        let sdk = with_sdk_source(async { current().unwrap() }).await;
        assert!(sdk.message_origin.is_none());
        assert_eq!(
            sdk.prompt_source
                .as_ref()
                .and_then(|source| source.string_at("")),
            Some("sdk")
        );
        assert!(
            current().is_none(),
            "scoped provenance never leaks between turns"
        );
    }

    #[tokio::test]
    async fn queued_turn_authority_is_explicit_nested_and_task_local() {
        assert_eq!(queued_turn_authority(), None);
        with_queued_turn_authority(false, async {
            assert_eq!(queued_turn_authority(), Some(false));
            with_queued_turn_authority(true, async {
                assert_eq!(queued_turn_authority(), Some(true));
            })
            .await;
            assert_eq!(queued_turn_authority(), Some(false));
            assert_eq!(
                tokio::spawn(async { queued_turn_authority() })
                    .await
                    .unwrap(),
                None
            );
        })
        .await;
        assert_eq!(queued_turn_authority(), None);
    }
}
