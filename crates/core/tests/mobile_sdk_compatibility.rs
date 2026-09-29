//! Compile-time and wire compatibility for shared host SDK types.
use lingxi_core::host::mobile_linux::{LinuxCommandRequest, LinuxCommandResult};
use lingxi_core::host::process::{ProcessError, ProcessOutput, ProcessStreamSink};
use lingxi_core::host::sandbox::{NetworkPolicy, ResourceLimits, SandboxBackend};
use std::sync::Arc;

fn sdk_request(value: LinuxCommandRequest) -> mobile_linux_api::LinuxCommandRequest {
    value
}
fn sdk_stream(value: Arc<dyn ProcessStreamSink>) -> Arc<dyn mobile_linux_api::ProcessStreamSink> {
    value
}

#[test]
fn legacy_types_are_sdk_types_and_preserve_wire_names() {
    let request = sdk_request(LinuxCommandRequest {
        command: "/bin/true".into(),
        args: vec![],
        cwd: None,
        env: Default::default(),
        stdin: None,
        timeout_ms: None,
        network: NetworkPolicy::Disabled,
        resource_limits: ResourceLimits::default(),
        mounts: vec![],
    });
    let encoded = serde_json::to_value(request).unwrap();
    assert_eq!(encoded["network"], "Disabled");
    assert_eq!(
        serde_json::to_string(&SandboxBackend::AndroidProot).unwrap(),
        r#""AndroidProot""#
    );
    let _: mobile_linux_api::ProcessError = ProcessError::Timeout;
    let result: LinuxCommandResult = ProcessOutput {
        stdout: "ok".into(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    }
    .into();
    let _: mobile_linux_api::LinuxCommandResult = result;
    let _same_trait = sdk_stream;
}

#[test]
fn harness_product_paths_remain_outside_the_sdk_defaults() {
    use lingxi_core::host::mobile_linux::guest_paths;
    assert_eq!(
        guest_paths::local_app_build_project("app", "dev"),
        "/var/lingxi/local-app-build/app/dev/project"
    );
    assert_eq!(
        guest_paths::workspace("id"),
        mobile_linux_api::guest_paths::workspace("id")
    );
}
