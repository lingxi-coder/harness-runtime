//! Per-account usage beside the per-model ledger.
//!
//! A response is added to both. The session total is still incremented once
//! by the caller. Rows with `attribution: None` are requests that had no
//! selected account.

use crate::Usage;
use lingxi_core::host::provider_accounts::AccountUsageAttribution;
use serde::{Deserialize, Serialize};

/// One model under one account, or under the unattributed bucket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountModelUsage {
    /// `None` is the unattributed bucket.
    pub attribution: Option<AccountUsageAttribution>,
    pub model: String,
    pub usage: Usage,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cost_nano_usd: u64,
    pub request_count: u64,
}

/// Add one settled response. Same account, generation, and model merge.
pub(crate) fn add(
    rows: &mut Vec<AccountModelUsage>,
    attribution: Option<AccountUsageAttribution>,
    model: &str,
    usage: &Usage,
    cost_nano_usd: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
) {
    if let Some(row) = rows
        .iter_mut()
        .find(|row| row.attribution == attribution && row.model == model)
    {
        row.usage.add(usage);
        row.cache_read_input_tokens = row
            .cache_read_input_tokens
            .saturating_add(cache_read_input_tokens);
        row.cache_creation_input_tokens = row
            .cache_creation_input_tokens
            .saturating_add(cache_creation_input_tokens);
        row.cost_nano_usd = row.cost_nano_usd.saturating_add(cost_nano_usd);
        row.request_count = row.request_count.saturating_add(1);
        return;
    }
    rows.push(AccountModelUsage {
        attribution,
        model: model.to_owned(),
        usage: *usage,
        cache_read_input_tokens,
        cache_creation_input_tokens,
        cost_nano_usd,
        request_count: 1,
    });
}

pub(crate) fn identity_key(row: &AccountModelUsage) -> String {
    match &row.attribution {
        Some(account) => format!(
            "a\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}",
            account.provider_id,
            account.account_id,
            account.identity_generation,
            auth_kind_name(account.auth_kind),
            account.credential_slot,
            row.model
        ),
        None => format!("z\u{1}{}", row.model),
    }
}

fn auth_kind_name(kind: lingxi_core::host::provider_accounts::AccountAuthKind) -> &'static str {
    use lingxi_core::host::provider_accounts::AccountAuthKind;
    match kind {
        AccountAuthKind::ApiKey => "api_key",
        AccountAuthKind::OAuth => "oauth",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::provider_accounts::{
        AccountAuthKind, AccountMetadata, AccountUsageAttribution, CredentialRef,
    };

    fn account(
        id: &str,
        provider: &str,
        kind: AccountAuthKind,
        generation: u64,
    ) -> AccountMetadata {
        AccountMetadata {
            account_id: id.into(),
            provider_id: provider.into(),
            display_name: format!("name-{id}"),
            auth_kind: kind,
            execution_binding_id: provider.into(),
            credential_ref: CredentialRef {
                store_scope: "host".into(),
                secret_ref: format!("slot-{id}-{generation}"),
            },
            enabled: true,
            user_priority: 0,
            config_revision: 1,
            identity_generation: generation,
        }
    }

    fn usage(input: u64) -> Usage {
        Usage {
            tokens: crate::TokenUsage {
                input,
                output: 2,
                ..crate::TokenUsage::default()
            },
            ..Usage::default()
        }
    }

    #[test]
    fn separate_keys_and_generations_do_not_merge() {
        let key_a = AccountUsageAttribution::from_account(&account(
            "a",
            "openai",
            AccountAuthKind::ApiKey,
            1,
        ));
        let key_b = AccountUsageAttribution::from_account(&account(
            "b",
            "openai",
            AccountAuthKind::ApiKey,
            1,
        ));
        let oauth = AccountUsageAttribution::from_account(&account(
            "sub",
            "openai-chatgpt",
            AccountAuthKind::OAuth,
            3,
        ));
        let rotated = AccountUsageAttribution::from_account(&account(
            "a",
            "openai",
            AccountAuthKind::ApiKey,
            2,
        ));
        assert_eq!(oauth.provider_id, "openai");
        assert_eq!(oauth.auth_kind, AccountAuthKind::OAuth);
        let mut rows = Vec::new();
        add(
            &mut rows,
            Some(key_a.clone()),
            "gpt-4.1-mini",
            &usage(10),
            100,
            1,
            0,
        );
        add(
            &mut rows,
            Some(key_a.clone()),
            "gpt-4.1-mini",
            &usage(5),
            50,
            0,
            1,
        );
        add(
            &mut rows,
            Some(key_b.clone()),
            "gpt-4.1-mini",
            &usage(7),
            70,
            0,
            0,
        );
        add(
            &mut rows,
            Some(oauth.clone()),
            "gpt-4.1-mini",
            &usage(3),
            30,
            0,
            0,
        );
        add(
            &mut rows,
            Some(rotated.clone()),
            "gpt-4.1-mini",
            &usage(9),
            90,
            0,
            0,
        );
        add(&mut rows, None, "gpt-4.1-mini", &usage(1), 10, 0, 0);
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].usage.tokens.input, 15);
        assert_eq!(rows[0].cost_nano_usd, 150);
        assert_eq!(rows[0].request_count, 2);
        assert_eq!(rows[0].cache_read_input_tokens, 1);
        assert_eq!(rows[0].cache_creation_input_tokens, 1);
        let model_total: u64 = rows.iter().map(|row| row.cost_nano_usd).sum();
        assert_eq!(model_total, 100 + 50 + 70 + 30 + 90 + 10);
        let secret = "sk-live-secret";
        let dumped = format!("{rows:?}{}", serde_json::to_string(&rows).unwrap());
        assert!(!dumped.contains(secret));
        assert!(!dumped.contains("name-a"));
        assert!(dumped.contains("slot-a-1"));
        assert!(!rows
            .iter()
            .any(|row| row.attribution.as_ref().is_some_and(|item| {
                item.account_id == "a"
                    && item.identity_generation == 1
                    && row.usage.tokens.input != 15
            })));
    }

    #[test]
    fn empty_account_rows_are_stored() {
        let state = crate::CostState::default();
        let value = serde_json::to_value(&state).unwrap();
        assert_eq!(value["per_account_usage"].as_array().map(Vec::len), Some(0));
        let restored: crate::CostState = serde_json::from_value(value).unwrap();
        assert!(restored.per_account_usage.is_empty());

        let vector = crate::CostStateVector::from(&crate::CostState::default());
        let value = serde_json::to_value(&vector).unwrap();
        assert_eq!(value["per_account_usage"].as_array().map(Vec::len), Some(0));
        let restored: crate::CostStateVector = serde_json::from_value(value.clone()).unwrap();
        assert!(restored.per_account_usage.is_empty());
        assert_eq!(
            restored.try_into_state().unwrap().per_account_usage.len(),
            0
        );
    }

    #[test]
    fn legacy_cost_states_preserve_totals_and_usage_without_account_rows() {
        let mut expected = crate::CostState {
            cost_revision: 7,
            total_nano_usd: 12_345_678_901,
            total_api_duration_ms: 1_500,
            total_api_duration_without_retries_ms: 1_200,
            total_tool_duration_ms: 300,
            total_web_search_requests: 2,
            total_lines_added: 17,
            total_lines_removed: 5,
            last_usage: Some(usage(53)),
            last_usage_revision: Some(6),
            last_cache_read_input_tokens: 11,
            last_cache_creation_input_tokens: 13,
            external_nano_usd: 200_000_000,
            unverified_nano_usd: 500_000_000,
            legacy_opening_balance_nano_usd: 145_678_901,
            legacy_import_evaluated: true,
            ..Default::default()
        };
        let mut legacy = serde_json::to_value(&expected).unwrap();
        legacy.as_object_mut().unwrap().remove("per_account_usage");
        let restored: crate::CostState = serde_json::from_value(legacy).unwrap();
        assert_eq!(restored, expected);

        // The durable vector also carries structured per-model identities.
        let model = crate::ModelRef {
            provider: crate::ProviderId::OpenAI,
            model: "gpt-4.1-mini".into(),
        };
        expected.per_model_usage.insert(
            model.clone(),
            crate::ModelUsage {
                model_ref: model,
                usage: usage(53),
                cache_read_input_tokens: 11,
                cache_creation_input_tokens: 13,
                cost_nano_usd: 12_000_000_000,
            },
        );
        let mut legacy = serde_json::to_value(crate::CostStateVector::from(&expected)).unwrap();
        legacy.as_object_mut().unwrap().remove("per_account_usage");
        let restored: crate::CostStateVector = serde_json::from_value(legacy).unwrap();
        assert_eq!(restored.try_into_state().unwrap(), expected);
    }

    #[test]
    fn malformed_account_rows_are_still_rejected() {
        for invalid in [
            serde_json::Value::Null,
            serde_json::json!({}),
            serde_json::json!("invalid"),
            serde_json::json!([{}]),
        ] {
            let mut value = serde_json::to_value(crate::CostState::default()).unwrap();
            value["per_account_usage"] = invalid.clone();
            assert!(serde_json::from_value::<crate::CostState>(value).is_err());

            let mut value =
                serde_json::to_value(crate::CostStateVector::from(&crate::CostState::default()))
                    .unwrap();
            value["per_account_usage"] = invalid;
            assert!(serde_json::from_value::<crate::CostStateVector>(value).is_err());
        }
    }

    #[test]
    fn duplicate_account_rows_are_rejected() {
        let mut state = crate::CostState::default();
        add(&mut state.per_account_usage, None, "m", &usage(1), 1, 0, 0);
        let mut vector = crate::CostStateVector::from(&state);
        let restored: crate::CostStateVector =
            serde_json::from_value(serde_json::to_value(&vector).unwrap()).unwrap();
        assert_eq!(restored.try_into_state().unwrap(), state);
        vector
            .per_account_usage
            .push(vector.per_account_usage[0].clone());
        assert!(matches!(
            vector.try_into_state(),
            Err(crate::CostPersistError::Storage(message)) if message.contains("duplicate account")
        ));
    }
}
