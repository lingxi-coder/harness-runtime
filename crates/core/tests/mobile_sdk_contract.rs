//! Shared SDK type identity and persisted wire names.
use mobile_linux_api::{LinuxCommandRequest, LinuxCommandResult};
use mobile_linux_api::{NetworkPolicy, ResourceLimits, SandboxBackend};
use mobile_linux_api::{ProcessError, ProcessOutput, ProcessStreamSink};
use std::sync::Arc;

fn sdk_request(value: LinuxCommandRequest) -> mobile_linux_api::LinuxCommandRequest {
    value
}
fn sdk_stream(value: Arc<dyn ProcessStreamSink>) -> Arc<dyn mobile_linux_api::ProcessStreamSink> {
    value
}

#[test]
fn sdk_types_preserve_wire_names() {
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
    let _: fn(Arc<dyn ProcessStreamSink>) -> Arc<dyn mobile_linux_api::ProcessStreamSink> =
        sdk_stream;
}
