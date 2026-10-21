//! Credential-free acceptance of the four public startup catalog queries.
use provider_config::startup_catalog::{CatalogPlatform, StartupCatalog};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let home = std::path::PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("provide an isolated probe directory")?,
    );
    let mut rows = Vec::new();
    for (platform, version) in [
        (CatalogPlatform::Desktop, "0.1.0"),
        (CatalogPlatform::Cli, "0.9.0"),
        (CatalogPlatform::Android, "0.1.0"),
        (CatalogPlatform::Ios, "2.4.1"),
    ] {
        let source = StartupCatalog::new(platform, version, &home);
        let start = std::time::Instant::now();
        let catalog = source.load().await;
        let offered = catalog
            .providers
            .iter()
            .flat_map(|p| &p.models)
            .filter(|m| m.metadata.catalog_offered)
            .count();
        rows.push(serde_json::json!({"platform":platform,"appVersion":version,"elapsedMs":start.elapsed().as_millis(),"officialModels":offered,"providers":catalog.providers.len()}));
    }
    println!("{}", serde_json::to_string_pretty(&rows)?);
    if rows.iter().any(|r| r["officialModels"].as_u64() == Some(0)) {
        return Err(
            "one or more platforms used embedded fallback; inspect catalog/cache evidence".into(),
        );
    }
    Ok(())
}
