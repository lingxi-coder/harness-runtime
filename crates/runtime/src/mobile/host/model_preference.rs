//! Last explicit interactive model selection, including its provider profile.
use std::path::Path;

pub(super) fn load(home: &Path) -> Option<String> {
    if home.as_os_str().is_empty() {
        return None;
    }
    let map = migrations::global_config::read_map(&home.join("last-model.json")).ok()?;
    let model = map.get("model")?.as_str()?;
    valid(model).then(|| model.to_string())
}

fn valid(model: &str) -> bool {
    !model.trim().is_empty() && model.len() <= 1024 && !model.chars().any(char::is_control)
}

pub(super) fn resolve(
    model: &str,
    listings: &[platform_api::ModelListing],
) -> Option<(String, Option<String>)> {
    let (id, profile) = platform_api::parse_model_ref(model, listings);
    listings
        .iter()
        .any(|entry| {
            entry.request_model == id
                && profile
                    .as_deref()
                    .is_none_or(|wanted| entry.provider_id == wanted)
        })
        .then_some((id, profile))
}

pub(super) fn save(home: &Path, model: &str) -> Result<(), String> {
    if home.as_os_str().is_empty() || !valid(model) {
        return Err("invalid model preference path or reference".into());
    }
    std::fs::create_dir_all(home).map_err(|error| error.to_string())?;
    let bytes = serde_json::to_vec(&serde_json::json!({ "model": model }))
        .map_err(|error| error.to_string())?;
    platform_api::rooted_fs::atomic_write(
        home,
        Path::new("last-model.json"),
        &bytes,
        platform_api::rooted_fs::AtomicWriteOptions::default(),
    )
    .map_err(|error| error.to_string())
}
