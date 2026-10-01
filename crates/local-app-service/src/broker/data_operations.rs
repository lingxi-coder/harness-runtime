use super::authoring;
use super::required_string;
use super::LocalAppsHostBroker;
use local_app_contracts::approvals::CapabilityKind;
use local_apps::load_manifest;
use local_apps::AppCapability;
use local_apps::AppDataStore;
use local_apps::DataMutation;
use local_apps::DataQuery;
use local_apps::DataSortDirection;
use local_apps::DataSortKey;
use serde_json::json;
use serde_json::Map;
use serde_json::Value;

pub(super) fn normalize_query(input: &Value) -> Result<DataQuery, String> {
    if input.get("cursor").is_some() {
        return Err("query cursor is unsupported; use numeric offset".into());
    }
    let collection = required_string(input, "collection")?.to_string();
    let filters = input
        .get("filters")
        .cloned()
        .or_else(|| input.get("filter").map(|value| json!([value])))
        .unwrap_or_else(|| json!([]));
    let sort_key = input
        .get("sort_key")
        .or_else(|| input.get("sortKey"))
        .cloned()
        .unwrap_or(Value::Null);
    let sort_direction = input
        .get("sort_direction")
        .or_else(|| input.get("sortDirection"))
        .cloned()
        .unwrap_or_else(|| Value::String("ascending".into()));
    let limit = match input.get("limit") {
        None => 50,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| "query limit must be an integer".to_string())?,
    };
    if !(1..=100).contains(&limit) {
        return Err("query limit must be between 1 and 100".into());
    }
    let offset = match input.get("offset") {
        None => 0,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| "query offset must be a non-negative integer".to_string())?,
    };
    let filters = serde_json::from_value(filters)
        .map_err(|error| format!("invalid structured data query: {error}"))?;
    let (sort_key, sort_direction) = normalize_sort(input, sort_key, sort_direction)?;
    Ok(DataQuery {
        collection,
        filters,
        sort_key,
        sort_direction,
        limit: limit as u32,
        offset,
    })
}

pub(super) fn normalize_sort(
    input: &Value,
    legacy_key: Value,
    legacy_direction: Value,
) -> Result<(Option<DataSortKey>, DataSortDirection), String> {
    let mut sort_key = normalize_sort_key_value(&legacy_key)?;
    let mut sort_direction = normalize_sort_direction_value(&legacy_direction)?;
    if let Some(sort) = input.get("sort") {
        let (public_key, public_direction) = normalize_public_sort(sort)?;
        if let Some(public_key) = public_key {
            sort_key = Some(public_key);
        }
        if let Some(public_direction) = public_direction {
            sort_direction = public_direction;
        }
    }
    Ok((sort_key, sort_direction))
}

pub(super) fn normalize_public_sort(
    sort: &Value,
) -> Result<(Option<DataSortKey>, Option<DataSortDirection>), String> {
    match sort {
        Value::Null => Ok((None, None)),
        Value::String(_) => Ok((normalize_sort_key_value(sort)?, None)),
        Value::Object(object) => {
            let direction = object
                .get("direction")
                .or_else(|| object.get("sort_direction"))
                .or_else(|| object.get("sortDirection"))
                .map(normalize_sort_direction_value)
                .transpose()?;
            let key = if let Some(key) = object
                .get("key")
                .or_else(|| object.get("sort_key"))
                .or_else(|| object.get("sortKey"))
            {
                normalize_sort_key_value(key)?
            } else if object.contains_key("kind")
                || object.contains_key("field_id")
                || object.contains_key("fieldId")
            {
                Some(normalize_sort_key_object(object)?)
            } else {
                None
            };
            Ok((key, direction))
        }
        _ => Err("query sort must be a string, object, or null".into()),
    }
}

pub(super) fn normalize_sort_key_value(value: &Value) -> Result<Option<DataSortKey>, String> {
    match value {
        Value::Null => Ok(None),
        Value::String(value) => Ok(Some(normalize_sort_key_string(value)?)),
        Value::Object(object) => Ok(Some(normalize_sort_key_object(object)?)),
        _ => Err("query sort key must be a string, object, or null".into()),
    }
}

pub(super) fn normalize_sort_key_object(
    object: &Map<String, Value>,
) -> Result<DataSortKey, String> {
    if let Some(field_id) = object
        .get("field_id")
        .or_else(|| object.get("fieldId"))
        .and_then(Value::as_str)
    {
        return Ok(DataSortKey::Field(field_id.to_string()));
    }
    let Some(kind) = object.get("kind").and_then(Value::as_str) else {
        return Err("query sort object must include key/kind or field_id".into());
    };
    match normalize_sort_alias(kind).as_str() {
        "field" => {
            let field_id = object
                .get("field_id")
                .or_else(|| object.get("fieldId"))
                .and_then(Value::as_str)
                .ok_or_else(|| "field sort requires field_id".to_string())?;
            Ok(DataSortKey::Field(field_id.to_string()))
        }
        "record_id" => Ok(DataSortKey::RecordId),
        "created_at" => Ok(DataSortKey::CreatedAt),
        "updated_at" => Ok(DataSortKey::UpdatedAt),
        "revision" => Ok(DataSortKey::Revision),
        other => Err(format!("unsupported sort kind {other:?}")),
    }
}

pub(super) fn normalize_sort_key_string(value: &str) -> Result<DataSortKey, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("query sort key must not be empty".into());
    }
    Ok(match normalize_sort_alias(value).as_str() {
        "record_id" => DataSortKey::RecordId,
        "created_at" => DataSortKey::CreatedAt,
        "updated_at" => DataSortKey::UpdatedAt,
        "revision" => DataSortKey::Revision,
        _ => DataSortKey::Field(value.to_string()),
    })
}

pub(super) fn normalize_sort_alias(value: &str) -> String {
    value
        .trim()
        .replace('-', "_")
        .chars()
        .fold(String::new(), |mut normalized, ch| {
            if ch.is_uppercase() && !normalized.is_empty() {
                normalized.push('_');
            }
            normalized.push(ch.to_ascii_lowercase());
            normalized
        })
}

pub(super) fn normalize_sort_direction_value(value: &Value) -> Result<DataSortDirection, String> {
    let Some(value) = value.as_str() else {
        return Err("query sort direction must be a string".into());
    };
    match normalize_sort_alias(value).as_str() {
        "ascending" | "asc" => Ok(DataSortDirection::Ascending),
        "descending" | "desc" => Ok(DataSortDirection::Descending),
        other => Err(format!("unsupported sort direction {other:?}")),
    }
}

pub(super) fn normalize_mutations(input: &Value) -> Result<Vec<DataMutation>, String> {
    let collection = required_string(input, "collection")?;
    let operations = input
        .get("operations")
        .and_then(Value::as_array)
        .ok_or_else(|| "mutations require an operations array".to_string())?;
    let mut normalized = Vec::with_capacity(operations.len());
    for operation in operations {
        let mut operation = operation
            .as_object()
            .cloned()
            .ok_or_else(|| "each mutation must be an object".to_string())?;
        operation.insert("collection".into(), Value::String(collection.to_string()));
        normalized.push(
            serde_json::from_value(Value::Object(operation))
                .map_err(|error| format!("invalid structured mutation: {error}"))?,
        );
    }
    Ok(normalized)
}

impl LocalAppsHostBroker {
    pub(super) async fn query_data_value(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?.to_string();
        self.validate_qa_request(&input).await?;
        let qa_event_id = input
            .get("qa_handle")
            .and_then(Value::as_str)
            .map(|_| self.request_id("qa-query"));
        self.service()?
            .record(&app_id)
            .await
            .map_err(|e| e.to_string())?;
        let layout = self.layout(&app_id)?;
        let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
        let query = normalize_query(&input)?;
        let result = tokio::task::spawn_blocking(move || {
            AppDataStore::with_cached(layout, |store| store.query(&manifest, &query))
        })
        .await
        .map_err(|error| format!("data query worker failed: {error}"))?
        .map(|page| json!(page))
        .map_err(|error| error.to_string())?;
        self.validate_qa_request(&input).await?;
        if let Some(qa_event_id) = qa_event_id {
            // The durable artifact is the actual native query page. Core
            // validates its collection/record/revision against the causally
            // linked bridge mutation; wrapping it in request metadata would
            // make the real `records` array invisible to that validation.
            let evidence = self
                .record_qa_observation(&input, "query_data", result.clone(), qa_event_id, None)
                .await?;
            if let Some(handle) = input.get("qa_handle").and_then(Value::as_str) {
                return Ok(authoring::qa_result_with_evidence_ids(
                    result,
                    handle,
                    authoring::qa_observation_id(&evidence),
                ));
            }
        }
        Ok(result)
    }
    pub(super) async fn mutate_data_value(
        &self,
        input: Value,
        require_approval: bool,
        qa_bridge_event_id: Option<&str>,
    ) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?.to_string();
        self.validate_qa_request(&input).await?;
        self.service()?
            .record(&app_id)
            .await
            .map_err(|e| e.to_string())?;
        if require_approval {
            self.authorize_capability(
                &app_id,
                AppCapability::DataMutation,
                CapabilityKind::DataMutation,
                "The agent requested permission to modify this app's persisted data.",
            )
            .await?;
        }
        let layout = self.layout(&app_id)?;
        let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
        let mutations = normalize_mutations(&input)?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            });
        let result = tokio::task::spawn_blocking(move || {
            AppDataStore::with_cached(layout, |store| store.mutate(&manifest, &mutations, now_ms))
        })
        .await
        .map_err(|error| format!("data mutation worker failed: {error}"))?
        .map(|results| json!({ "results": results }))
        .map_err(|error| error.to_string())?;
        if let Some(event_id) = qa_bridge_event_id {
            // The page receives the actual datastore result immediately. Only
            // Host evidence attribution waits for the native action result;
            // failed/cancelled native actions must never erase this business
            // write or expose a fabricated empty response.
            if let Err(error) = self.validate_qa_request(&input).await {
                tracing::warn!(app_id = %app_id, %error, "QA identity changed after page write; omitting evidence attribution");
                return Ok(result);
            }
            if let Err(error) = self
                .buffer_qa_bridge_result(&app_id, event_id, result.clone())
                .await
            {
                tracing::warn!(app_id = %app_id, %error, "QA bridge evidence attribution unavailable after page write");
            }
            return Ok(result);
        }
        self.validate_qa_request(&input).await?;
        Ok(result)
    }
}
