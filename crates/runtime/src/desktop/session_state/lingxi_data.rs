//! Record references from the LingXi Data v1 contract are safe to carry in a
//! Chat transcript. Live source rows and task credentials stay in the host.
use serde::{Deserialize, Serialize};

pub const LINGXI_DATA_BASELINE: &str = "d18563624a7745587250fdc1047b3e3ef2a31f55";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DataRef {
    pub dataset: String,
    pub record: String,
    pub revision: u64,
    pub schema_version: u64,
}

impl DataRef {
    pub fn is_valid(&self) -> bool {
        data_id(&self.dataset)
            && data_id(&self.record)
            && (1..=MAX_SAFE_INTEGER).contains(&self.revision)
            && (1..=MAX_SAFE_INTEGER).contains(&self.schema_version)
    }
}

fn data_id(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_references_match_the_pinned_contract() {
        let wire = serde_json::json!({"dataset": "work-orders", "record": "order-7", "revision": 2, "schemaVersion": 1});
        let reference: DataRef = serde_json::from_value(wire.clone()).unwrap();
        assert!(reference.is_valid());
        assert_eq!(serde_json::to_value(&reference).unwrap(), wire);
        assert!(!DataRef {
            revision: 0,
            ..reference.clone()
        }
        .is_valid());
        assert!(!DataRef {
            record: "../record".into(),
            ..reference
        }
        .is_valid());
        assert!(serde_json::from_value::<DataRef>(serde_json::json!({
            "dataset": "data", "record": "row", "revision": 1, "schemaVersion": 1, "value": {"private": true}
        })).is_err());
    }
}
