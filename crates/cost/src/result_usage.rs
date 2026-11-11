//! Independent SDK-result accumulator fed at accepted accounting mutations.

use crate::{CostCalculator, ModelRef, ModelUsage, PricingCatalog, Usage};
use indexmap::IndexMap;
use lingxi_core::host::result_usage::{
    ResultCost, ResultModelCost, ResultModelUsage, ResultUsageTotals,
};

#[derive(Default)]
pub(crate) struct ResultUsageLedger {
    rows: IndexMap<ModelRef, ResultModelUsage>,
    query_rows: IndexMap<ModelRef, ResultModelUsage>,
    model_costs: IndexMap<ModelRef, ResultModelCost>,
    total: Option<ResultCost>,
}

impl ResultUsageLedger {
    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub(crate) fn initialize_models(&mut self, models: &IndexMap<ModelRef, ModelUsage>) {
        for (key, row) in models {
            self.model_costs
                .entry(key.clone())
                .or_insert_with(|| ResultModelCost {
                    model: key.model.clone(),
                    provider: key.provider.usage_wire_name(),
                    cost: ResultCost {
                        nano_usd: row.cost_nano_usd,
                        cost_usd: row.cost_nano_usd as f64 / 1_000_000_000.0,
                    },
                    restored: true,
                });
        }
    }

    pub(crate) fn record_total(&mut self, before: u64, after: u64) {
        self.record_total_usd(
            before,
            after,
            (i128::from(after) - i128::from(before)) as f64 / 1_000_000_000.0,
        );
    }

    fn record_total_usd(&mut self, before: u64, after: u64, cost_usd: f64) {
        let total = self.total.get_or_insert_with(|| ResultCost {
            nano_usd: before,
            cost_usd: before as f64 / 1_000_000_000.0,
        });
        // A late restored baseline or an aggregate charge is not interval
        // model usage, but still belongs in the cumulative cost projection.
        if total.nano_usd != before {
            total.cost_usd +=
                (i128::from(before) - i128::from(total.nano_usd)) as f64 / 1_000_000_000.0;
        }
        total.cost_usd += cost_usd;
        total.nano_usd = after;
    }

    pub(crate) fn record_transition(
        &mut self,
        before_total: u64,
        after_total: u64,
        before_models: &IndexMap<ModelRef, ModelUsage>,
        after_models: &IndexMap<ModelRef, ModelUsage>,
        catalog: &PricingCatalog,
    ) {
        self.initialize_models(before_models);
        let mut model_nano = 0_i128;
        let mut model_usd = 0.0;
        for (model, after) in after_models {
            let before = before_models.get(model);
            if before == Some(after) {
                continue;
            }
            let nano = i128::from(after.cost_nano_usd)
                - i128::from(before.map_or(0, |row| row.cost_nano_usd));
            let cost_usd =
                Self::priced_delta(before, after, catalog).unwrap_or(nano as f64 / 1_000_000_000.0);
            self.record_usd(before, after, cost_usd);
            model_nano += nano;
            model_usd += cost_usd;
        }
        let other_nano = i128::from(after_total) - i128::from(before_total) - model_nano;
        self.record_total_usd(
            before_total,
            after_total,
            model_usd + other_nano as f64 / 1_000_000_000.0,
        );
    }

    fn priced_delta(
        before: Option<&ModelUsage>,
        after: &ModelUsage,
        catalog: &PricingCatalog,
    ) -> Option<f64> {
        let empty = Usage::default();
        let old = before.map_or(&empty, |row| &row.usage);
        let old_nano = before.map_or(0, |row| row.cost_nano_usd);
        let (mut usage, subtract, accepted, sign) = if after.cost_nano_usd >= old_nano {
            (after.usage, old, after.cost_nano_usd - old_nano, 1.0)
        } else {
            (*old, &after.usage, old_nano - after.cost_nano_usd, -1.0)
        };
        macro_rules! subtract { ($($field:ident),+) => { $(
            usage.tokens.$field = usage.tokens.$field.checked_sub(subtract.tokens.$field)?;
        )+ }; }
        subtract!(
            input,
            output,
            cache_read,
            cache_write,
            cache_write_1h,
            reasoning_output
        );
        let web = usage
            .server_tool_use
            .map_or(0, |tools| tools.web_search_requests)
            .checked_sub(
                subtract
                    .server_tool_use
                    .map_or(0, |tools| tools.web_search_requests),
            )?;
        usage.server_tool_use = Some(crate::ServerToolUsage {
            web_search_requests: web,
        });
        let pricing = catalog
            .resolve(&after.model_ref)
            .map(|(pricing, _)| pricing)
            .unwrap_or_else(|_| PricingCatalog::default_unknown_pricing(&after.model_ref));
        Some(sign * CostCalculator::result_cost_usd(&usage, &pricing, accepted))
    }

    pub(crate) fn reset(&mut self) {
        self.rows.clear();
        self.query_rows.clear();
        self.model_costs.clear();
        self.total = None;
    }
    /// Record an individual accepted model-row transition, never a difference
    /// between result-time cumulative snapshots. Unchanged/replayed mutations
    /// have no effect. The first occurrence determines object insertion order.
    pub(crate) fn record(&mut self, before: Option<&ModelUsage>, after: &ModelUsage) {
        let cost_usd = (i128::from(after.cost_nano_usd)
            - i128::from(before.map_or(0, |row| row.cost_nano_usd))) as f64
            / 1_000_000_000.0;
        self.record_usd(before, after, cost_usd);
    }

    fn record_usd(&mut self, before: Option<&ModelUsage>, after: &ModelUsage, cost_usd: f64) {
        if before == Some(after) {
            return;
        }
        let empty = ModelUsage {
            model_ref: after.model_ref.clone(),
            usage: Default::default(),
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            cost_nano_usd: 0,
        };
        let before = before.unwrap_or(&empty);
        let cumulative = self
            .model_costs
            .entry(after.model_ref.clone())
            .or_insert_with(|| ResultModelCost {
                model: after.model_ref.model.clone(),
                provider: after.model_ref.provider.usage_wire_name(),
                cost: ResultCost {
                    nano_usd: before.cost_nano_usd,
                    cost_usd: before.cost_nano_usd as f64 / 1_000_000_000.0,
                },
                restored: before.cost_nano_usd != 0 || before.usage != crate::Usage::default(),
            });
        if cumulative.cost.nano_usd != before.cost_nano_usd {
            cumulative.cost.cost_usd +=
                (i128::from(before.cost_nano_usd) - i128::from(cumulative.cost.nano_usd)) as f64
                    / 1_000_000_000.0;
        }
        cumulative.cost.cost_usd += cost_usd;
        cumulative.cost.nano_usd = after.cost_nano_usd;
        Self::record_into(&mut self.rows, before, after, cost_usd);
        Self::record_into(&mut self.query_rows, before, after, cost_usd);
    }

    fn record_into(
        rows: &mut IndexMap<ModelRef, ResultModelUsage>,
        before: &ModelUsage,
        after: &ModelUsage,
        cost_usd: f64,
    ) {
        let row = rows
            .entry(after.model_ref.clone())
            .or_insert_with(|| ResultModelUsage {
                model: after.model_ref.model.clone(),
                provider: after.model_ref.provider.usage_wire_name(),
                ..Default::default()
            });
        macro_rules! add {
            ($field:ident, $old:expr, $new:expr) => {
                row.$field += i128::from($new) - i128::from($old);
            };
        }
        add!(nano_usd, before.cost_nano_usd, after.cost_nano_usd);
        row.cost_usd += cost_usd;
        add!(
            input_tokens,
            before.usage.tokens.input,
            after.usage.tokens.input
        );
        add!(
            output_tokens,
            before.usage.tokens.output,
            after.usage.tokens.output
        );
        add!(
            reasoning_tokens,
            before.usage.tokens.reasoning_output,
            after.usage.tokens.reasoning_output
        );
        add!(
            cache_read_input_tokens,
            before.cache_read_input_tokens,
            after.cache_read_input_tokens
        );
        add!(
            cache_creation_input_tokens,
            before.cache_creation_input_tokens,
            after.cache_creation_input_tokens
        );
        add!(
            cache_creation_5m_input_tokens,
            before.usage.tokens.cache_write,
            after.usage.tokens.cache_write
        );
        add!(
            cache_creation_1h_input_tokens,
            before.usage.tokens.cache_write_1h,
            after.usage.tokens.cache_write_1h
        );
        add!(
            web_search_requests,
            before
                .usage
                .server_tool_use
                .map_or(0, |x| x.web_search_requests),
            after
                .usage
                .server_tool_use
                .map_or(0, |x| x.web_search_requests)
        );
    }

    pub(crate) fn begin_query(&mut self) {
        self.query_rows.clear();
    }

    pub(crate) fn query_usage(&self, read: &mut dyn FnMut(&[ResultModelUsage])) {
        read(&self.query_rows.values().cloned().collect::<Vec<_>>());
    }

    pub(crate) fn take(&mut self) -> Vec<ResultModelUsage> {
        std::mem::take(&mut self.rows).into_values().collect()
    }

    pub(crate) fn publish(
        &mut self,
        publish: &mut dyn FnMut(&[ResultModelUsage], &ResultUsageTotals) -> bool,
    ) {
        let rows = self.rows.values().cloned().collect::<Vec<_>>();
        let totals = ResultUsageTotals {
            total: self.total.clone(),
            models: self.model_costs.values().cloned().collect(),
        };
        if publish(&rows, &totals) {
            self.rows.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProviderId, TokenUsage, Usage};

    #[test]
    fn accepted_categories_keep_native_float_bytes_without_repricing_quotes() {
        let catalog = PricingCatalog::builtin_reference();
        let mut row = usage(ProviderId::Anthropic, 1000);
        row.model_ref.model = "claude-sonnet-5-5".into();
        row.usage.tokens.output = 1000;
        row.usage.tokens.cache_read = 3;
        row.cost_nano_usd = 12_061_800;
        let after = IndexMap::from([(row.model_ref.clone(), row.clone())]);
        let mut ledger = ResultUsageLedger::default();
        ledger.record_transition(0, row.cost_nano_usd, &IndexMap::new(), &after, &catalog);
        ledger.publish(&mut |rows, totals| {
            assert_eq!(rows[0].cost_usd, 0.012061800000000001);
            assert_eq!(
                totals.total.as_ref().unwrap().cost_usd,
                0.012061800000000001
            );
            assert_eq!(totals.models[0].cost.cost_usd, 0.012061800000000001);
            assert_eq!(rows[0].nano_usd, 12_061_800);
            true
        });
        let mut zero = row.clone();
        zero.usage = Usage::default();
        zero.cache_read_input_tokens = 0;
        zero.cache_creation_input_tokens = 0;
        zero.cost_nano_usd = 0;
        ledger.record_transition(
            row.cost_nano_usd,
            0,
            &after,
            &IndexMap::from([(zero.model_ref.clone(), zero)]),
            &catalog,
        );
        ledger.publish(&mut |rows, totals| {
            assert_eq!(rows[0].cost_usd, -0.012061800000000001);
            assert_eq!(totals.total.as_ref().unwrap().cost_usd, 0.0);
            assert_eq!(totals.models[0].cost.cost_usd, 0.0);
            true
        });
        // A frozen provider/account quote has a different rate. Its accepted
        // integer amount must survive the public-format projection unchanged.
        row.cost_nano_usd = 5;
        let after = IndexMap::from([(row.model_ref.clone(), row.clone())]);
        ledger.reset();
        ledger.record_transition(0, 5, &IndexMap::new(), &after, &catalog);
        ledger.publish(&mut |rows, totals| {
            assert_eq!(rows[0].cost_usd, 0.000000005);
            assert_eq!(totals.total.as_ref().unwrap().nano_usd, 5);
            true
        });
    }

    fn usage(provider: ProviderId, input: u64) -> ModelUsage {
        ModelUsage {
            model_ref: ModelRef {
                provider,
                model: "shared".into(),
            },
            usage: Usage {
                tokens: TokenUsage {
                    input,
                    cache_write: 7,
                    cache_write_1h: 11,
                    ..Default::default()
                },
                ..Default::default()
            },
            cache_read_input_tokens: 3,
            cache_creation_input_tokens: 18,
            cost_nano_usd: 83800,
        }
    }

    #[test]
    fn query_and_result_publication_boundaries_are_independent() {
        let mut ledger = ResultUsageLedger::default();
        let first = usage(ProviderId::Anthropic, 3);
        ledger.record(None, &first);
        ledger.begin_query();
        let second = usage(ProviderId::Anthropic, 8);
        ledger.record(Some(&first), &second);
        ledger.query_usage(&mut |rows| assert_eq!(rows[0].input_tokens, 5));
        ledger.publish(&mut |rows, _| {
            assert_eq!(rows[0].input_tokens, 8);
            true
        });
        ledger.query_usage(&mut |rows| assert_eq!(rows[0].input_tokens, 5));
        ledger.publish(&mut |rows, _| {
            assert!(rows.is_empty());
            true
        });
    }

    #[test]
    fn cumulative_usd_keeps_observation_order_across_result_resets() {
        let mut ledger = ResultUsageLedger::default();
        let mut before = None;
        for call in 1..=3 {
            let mut after = usage(ProviderId::Anthropic, call);
            after.cost_nano_usd *= call;
            ledger.record_total(83800 * (call - 1), after.cost_nano_usd);
            ledger.record(before.as_ref(), &after);
            before = Some(after);
            if call == 2 {
                ledger.publish(&mut |_, _| true);
            }
        }
        ledger.publish(&mut |rows, totals| {
            assert_eq!(rows[0].cost_usd, 0.0000838);
            assert_eq!(totals.models[0].cost.cost_usd, 0.00025140000000000004);
            assert!(!totals.models[0].restored);
            assert_eq!(
                totals.total.as_ref().unwrap().cost_usd,
                0.00025140000000000004
            );
            true
        });
        ledger.reset();
        ledger.publish(&mut |rows, totals| {
            assert!(rows.is_empty());
            assert_eq!(*totals, ResultUsageTotals::default());
            true
        });
    }

    #[test]
    fn resets_independently_and_preserves_provider_identity_and_corrections() {
        let mut ledger = ResultUsageLedger::default();
        let first = usage(ProviderId::Anthropic, 10);
        ledger.record(None, &first);
        let a = ledger.take();
        assert_eq!(a[0].input_tokens, 10);
        assert_eq!(a[0].cache_creation_5m_input_tokens, 7);
        assert_eq!(a[0].cache_creation_1h_input_tokens, 11);
        assert!(ledger.take().is_empty());
        let second = usage(ProviderId::Anthropic, 20);
        ledger.record(Some(&first), &second);
        ledger.record(None, &usage(ProviderId::OpenAI, 5));
        ledger.record(Some(&second), &second); // replay
        let b = ledger.take();
        assert_eq!(b.len(), 2);
        assert_eq!(
            (b[0].provider.as_str(), b[0].input_tokens),
            ("firstParty", 10)
        );
        assert_eq!((b[1].provider.as_str(), b[1].input_tokens), ("openai", 5));
        ledger.record(Some(&second), &first); // late correction after publication
        assert_eq!(ledger.take()[0].input_tokens, -10);
    }
}
