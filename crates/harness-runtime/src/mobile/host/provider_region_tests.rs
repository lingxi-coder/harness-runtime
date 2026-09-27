#[test]
fn empty_routable_catalog_respects_the_captured_provider_region() {
    for (region, enabled_profiles) in [
        (llm_runtime::Region::International, Vec::<&str>::new()),
        (llm_runtime::Region::ChinaMainland, Vec::<&str>::new()),
        (llm_runtime::Region::ChinaMainland, vec!["openai"]),
    ] {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join(branding::DOT_DIR);
        std::fs::create_dir_all(&home).expect("create mobile home");
        std::fs::write(
            home.join("settings.json"),
            serde_json::to_vec(&serde_json::json!({ "providerRegion": region })).unwrap(),
        )
        .expect("write provider region");
        let cfg = MobileConfig {
            cwd: tmp.path().to_path_buf(),
            lingxi_home: home,
            routing: Some(serde_json::json!({ "mobileEnabledProfiles": enabled_profiles })),
            ..MobileConfig::default()
        };
        let (handle, listener) = build_submit_handle_with_config(cfg, tmp.path());
        assert_eq!(handle.inner.provider_region, region);
        assert!(handle.inner.routable_listings.is_empty());

        handle.runtime().block_on(async {
            handle.submit(ClientCommand::ListModels).await.unwrap();
            let events = listener.received.lock().await;
            let (models, details) = events
                .iter()
                .find_map(|event| match event {
                    Ev::ModelList {
                        models, details, ..
                    } => Some((models, details)),
                    _ => None,
                })
                .expect("ModelList must be emitted");
            assert!(
                models.len() > 1,
                "the configurable catalog must stay visible"
            );
            assert!(!details.is_empty());
            let allowed_profiles: std::collections::HashSet<_> = llm_runtime::builtin_presets()
                .providers
                .into_iter()
                .filter(|provider| provider.regions.contains(&region))
                .map(|provider| provider.profile_name)
                .collect();
            assert!(details
                .iter()
                .all(|detail| allowed_profiles.contains(&detail.provider_id)));
            assert_eq!(
                details.iter().any(|detail| detail.provider_id == "openai"),
                region == llm_runtime::Region::International,
            );
            assert_eq!(
                details.iter().any(|detail| detail.provider_id == "glm"),
                region == llm_runtime::Region::ChinaMainland,
            );
        });
    }
}
