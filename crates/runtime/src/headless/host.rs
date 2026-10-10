//! Product services used by the embedded headless runner.
//!
//! The runner owns protocol, operation and transcript state. Its embedding
//! host owns environment overlays, account metadata and background-job identity.

pub use llm_runtime::auth::anthropic::environment::OAuthDescriptorCredential;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Host capabilities that cannot be implemented by the protocol runner.
/// Defaults express an absent optional product capability.
#[async_trait::async_trait]
pub trait HeadlessHostServices: Send + Sync {
    /// Bind optional product services once the runtime has been assembled.
    async fn runtime_ready(&self, _runtime: &crate::desktop::DesktopRuntime) -> Result<(), String> {
        Ok(())
    }

    /// Read the host's environment overlay, including SDK updates received on
    /// this connection. Implementations decide whether to include process env.
    fn environment_variable(&self, name: &str) -> Option<String>;

    /// Model OAuth descriptor/file material acquired by the host. The runtime
    /// selects this before stored OAuth and never opens a process descriptor.
    fn oauth_descriptor_credential(&self) -> Option<OAuthDescriptorCredential> {
        None
    }

    /// Apply an SDK environment update to this host's execution environment.
    /// The runtime never mutates the process-global environment.
    fn update_environment_variables(
        &self,
        variables: HashMap<String, String>,
    ) -> Result<(), String>;

    /// Refresh a product's background launch record after transcript relocation.
    /// Hosts without background launch records have nothing to refresh.
    fn refresh_launch_identity(&self, _cwd: &Path, _transcript: &Path) -> Result<(), String> {
        Ok(())
    }

    /// Disable a product background job if both identity refresh and transcript
    /// rollback failed. Hosts without background jobs have nothing to disable.
    fn launch_identity_rollback_failed(&self, _detail: &str) {}

    /// Remember a successful permission-mode change in product preferences.
    fn remember_permission_mode(&self, _mode: &str) {}

    /// Optional product trust store. An absent store cannot record trust, so
    /// directory relocation still requires the native trust handshake.
    fn trust_config_path(&self) -> Option<PathBuf> {
        None
    }

    /// Optional host-owned install/account identity for Anthropic request
    /// metadata. Returning `None` must not create or load a global identity.
    fn request_identity(&self) -> Option<crate::desktop::DesktopRequestIdentity> {
        None
    }

    /// Account information advertised by initialize. An unauthenticated host
    /// has no account metadata.
    fn account_metadata(&self) -> serde_json::Value {
        serde_json::json!({})
    }

    /// Process identity advertised by initialize, overridable by virtual hosts.
    fn process_id(&self) -> u32 {
        std::process::id()
    }
}

/// Freeze request identity and UA from this embedding's environment overlay.
/// Native 2.1.293 fixes user type to `external` and normalizes print entrypoint
/// to `sdk-cli`. Explicit other entrypoints and SDK/app suffixes are retained.
pub fn configure_desktop_request_state(
    config: &mut crate::desktop::DesktopConfig,
    host: &dyn HeadlessHostServices,
    options: &super::config::HeadlessOptions,
) {
    config.composition = Some(crate::desktop::DesktopSessionComposition::HeadlessCli);
    config.native_thinking_display = Some(
        lingxi_llm_client::providers::anthropic::thinking_display::ThinkingDisplayPolicy {
            explicit: options.thinking_display.clone(),
            omit_default: matches!(options.output_format, super::config::OutputFormat::Text)
                || (matches!(options.output_format, super::config::OutputFormat::Json)
                    && !options.verbose),
        },
    );
    let mut ua = llm_runtime::model::user_agent::UserAgentEnv::from_lookup(|name| {
        if name == "USER_TYPE" {
            Some("external".into())
        } else {
            host.environment_variable(name)
        }
    });
    // The pinned production binary's UA template uses literal `external`.
    ua.user_type = Some("external".into());
    ua.entrypoint = Some(
        match ua.entrypoint.as_deref().filter(|value| !value.is_empty()) {
            Some("local_agent") => "local-agent".into(),
            Some("cli") => "sdk-cli".into(),
            Some(value) => value.into(),
            None if host
                .environment_variable("CLAUDE_CODE_ACTION")
                .is_some_and(|value| {
                    matches!(
                        value.to_ascii_lowercase().as_str(),
                        "1" | "true" | "yes" | "on"
                    )
                }) =>
            {
                "claude-code-github-action".into()
            }
            None => "sdk-cli".into(),
        },
    );
    ua.agent_sdk_version = ua.agent_sdk_version.filter(|value| !value.is_empty());
    ua.client_app = ua.client_app.filter(|value| !value.is_empty());
    config.user_agent_environment = Some(ua);
    config.anthropic_client_metadata = Some(lingxi_llm_client::providers::anthropic::request_policy::AnthropicClientMetadata::claude_code_2_1_293(std::env::consts::OS, std::env::consts::ARCH));
    config.anthropic_compatible_version = Some(super::CLAUDE_CODE_REFERENCE_VERSION.into());
    let mut identity = host.request_identity();
    if let Some(identity) = &mut identity {
        if let Some(extra) = host.environment_variable("CLAUDE_CODE_EXTRA_METADATA") {
            identity.extra_metadata = serde_json::from_str::<serde_json::Value>(&extra)
                .ok()
                .and_then(|value| value.as_object().cloned());
        }
    }
    config.request_identity = identity;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop::{DesktopConfig, DesktopRequestIdentity};
    struct Host {
        environment: HashMap<String, String>,
        identity: Option<DesktopRequestIdentity>,
    }
    impl HeadlessHostServices for Host {
        fn environment_variable(&self, name: &str) -> Option<String> {
            self.environment.get(name).cloned()
        }
        fn update_environment_variables(&self, _: HashMap<String, String>) -> Result<(), String> {
            panic!("snapshot must never mutate the environment")
        }
        fn request_identity(&self) -> Option<DesktopRequestIdentity> {
            self.identity.clone()
        }
    }
    #[test]
    fn request_state_uses_only_the_host_snapshot() {
        let host = Host {
            environment: [
                ("USER_TYPE", "external"),
                ("CLAUDE_CODE_ENTRYPOINT", "sdk-cli"),
                ("CLAUDE_AGENT_SDK_VERSION", "fixture"),
                ("CLAUDE_AGENT_SDK_CLIENT_APP", "embedding"),
                (
                    "CLAUDE_CODE_EXTRA_METADATA",
                    r#"{"team":"host","device_id":"stale"}"#,
                ),
            ]
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect(),
            identity: Some(DesktopRequestIdentity {
                device_id: "device-host".into(),
                account_uuid: "account-host".into(),
                extra_metadata: None,
            }),
        };
        let mut config = DesktopConfig {
            user_agent_environment: Some(llm_runtime::model::user_agent::UserAgentEnv {
                user_type: Some("ambient-sentinel".into()),
                entrypoint: Some("ambient-entrypoint".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        configure_desktop_request_state(&mut config, &host, &Default::default());
        let ua = config.user_agent_environment.as_ref().unwrap();
        assert_eq!(
            llm_runtime::model::user_agent::user_agent(
                ua,
                config.anthropic_compatible_version.as_deref().unwrap()
            ),
            "claude-cli/2.1.293 (external, sdk-cli, agent-sdk/fixture, client-app/embedding)"
        );
        let identity = config.request_identity.unwrap();
        assert_eq!(
            llm_runtime::ApiService::build_api_metadata_user_id_with_extra(
                &identity.device_id,
                &identity.account_uuid,
                "session-host",
                Some("parent-host"),
                identity.extra_metadata
            ),
            r#"{"team":"host","device_id":"device-host","account_uuid":"account-host","session_id":"session-host","parent_session_id":"parent-host"}"#
        );
    }
    #[test]
    fn native_print_entrypoint_rules_preserve_explicit_other_entries() {
        for (entry, expected) in [
            (None, "sdk-cli"),
            (Some(""), "sdk-cli"),
            (Some("cli"), "sdk-cli"),
            (Some("local_agent"), "local-agent"),
            (Some("remote"), "remote"),
        ] {
            let mut environment = HashMap::from([
                ("USER_TYPE".into(), "internal".into()),
                ("CLAUDE_AGENT_SDK_VERSION".into(), String::new()),
                ("CLAUDE_AGENT_SDK_CLIENT_APP".into(), String::new()),
            ]);
            if let Some(entry) = entry {
                environment.insert("CLAUDE_CODE_ENTRYPOINT".into(), entry.into());
            }
            let host = Host {
                environment,
                identity: None,
            };
            let mut config = DesktopConfig::default();
            configure_desktop_request_state(&mut config, &host, &Default::default());
            let ua = config.user_agent_environment.unwrap();
            assert_eq!(ua.user_type.as_deref(), Some("external"));
            assert_eq!(ua.entrypoint.as_deref(), Some(expected));
            assert!(ua.agent_sdk_version.is_none() && ua.client_app.is_none());
        }
    }
    #[test]
    fn absent_identity_clears_a_prior_snapshot_without_global_identity_creation() {
        let host = Host {
            environment: HashMap::new(),
            identity: None,
        };
        let mut config = DesktopConfig {
            request_identity: Some(DesktopRequestIdentity {
                device_id: "prior-install".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        configure_desktop_request_state(&mut config, &host, &Default::default());
        assert!(config.request_identity.is_none());
        assert_eq!(
            llm_runtime::model::user_agent::user_agent(
                config.user_agent_environment.as_ref().unwrap(),
                "2.1.293"
            ),
            "claude-cli/2.1.293 (external, sdk-cli)"
        );
    }
    #[test]
    fn native_main_display_selection_follows_output_protocol_and_explicit_precedence() {
        use super::super::config::{HeadlessOptions, OutputFormat};
        let host = Host {
            environment: HashMap::new(),
            identity: None,
        };
        for (format, verbose, expected) in [
            (OutputFormat::Text, false, Some("omitted")),
            (OutputFormat::Json, false, Some("omitted")),
            (OutputFormat::Json, true, None),
            (OutputFormat::StreamJson, true, None),
        ] {
            let mut config = DesktopConfig::default();
            let options = HeadlessOptions {
                output_format: format,
                verbose,
                ..Default::default()
            };
            configure_desktop_request_state(&mut config, &host, &options);
            assert_eq!(
                config.native_thinking_display.unwrap().selected_display(),
                expected
            );
        }
        let mut config = DesktopConfig::default();
        configure_desktop_request_state(
            &mut config,
            &host,
            &HeadlessOptions {
                thinking_display: Some("highlights".into()),
                ..Default::default()
            },
        );
        assert_eq!(
            config.native_thinking_display.unwrap().selected_display(),
            Some("highlights")
        );
    }
}
