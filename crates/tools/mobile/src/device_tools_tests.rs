//! Regression tests for the native tool boundary, without device permissions.

use super::*;
use async_trait::async_trait;
use lingxi_core::host::{
    calendar::{CalendarError, CalendarEvent, CalendarProvider, CalendarQuery},
    contacts::{Contact, ContactsError, ContactsProvider, ContactsQuery},
    deep_link::{DeepLinkError, DeepLinkOpener},
    device_status::{DeviceStatus, DeviceStatusError, DeviceStatusProvider},
    haptics::{HapticError, HapticService, HapticStyle},
    location::{LocationError, LocationFix, LocationProvider},
};
use permission::PermissionResult;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use tool_api::{
    test_support::{fresh_ctx, fresh_tx},
    tool_trait::{Tool, ToolCallResult, ToolError},
    BuiltinToolContext, ToolRegistry,
};

#[derive(Default)]
struct FakeDevices {
    failure: Option<&'static str>,
    large_records: bool,
    pending_location: bool,
    location_dropped: AtomicBool,
    calls: AtomicUsize,
    styles: Mutex<Vec<HapticStyle>>,
    urls: Mutex<Vec<String>>,
    calendar_queries: Mutex<Vec<CalendarQuery>>,
    contacts_queries: Mutex<Vec<ContactsQuery>>,
}

struct DropFlag<'a>(&'a AtomicBool);
impl Drop for DropFlag<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl LocationProvider for FakeDevices {
    async fn current_location(&self) -> Result<LocationFix, LocationError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.pending_location {
            let _flag = DropFlag(&self.location_dropped);
            return std::future::pending().await;
        }
        match self.failure {
            Some("permission_denied") => Err(LocationError::PermissionDenied),
            Some("timeout") => Err(LocationError::Timeout),
            Some("unavailable") => Err(LocationError::Unavailable),
            Some(_) => Err(LocationError::Other("native failure".into())),
            None => Ok(LocationFix {
                latitude: 31.2,
                longitude: 121.4,
                accuracy_m: Some(4.0),
                timestamp_ms: 1000,
            }),
        }
    }
}

#[async_trait]
impl DeviceStatusProvider for FakeDevices {
    async fn status(&self) -> Result<DeviceStatus, DeviceStatusError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.failure {
            Some("unavailable") => Err(DeviceStatusError::Unavailable),
            Some(_) => Err(DeviceStatusError::Other("native failure".into())),
            None => Ok(DeviceStatus {
                battery_percent: Some(75.0),
                charging: Some(true),
                network: "wifi".into(),
                low_power_mode: None,
            }),
        }
    }
}

#[async_trait]
impl HapticService for FakeDevices {
    async fn trigger(&self, style: HapticStyle) -> Result<(), HapticError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.styles.lock().unwrap().push(style);
        match self.failure {
            Some("unavailable") => Err(HapticError::Unavailable),
            Some(_) => Err(HapticError::Other("native failure".into())),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl DeepLinkOpener for FakeDevices {
    async fn open(&self, url: String) -> Result<(), DeepLinkError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.urls.lock().unwrap().push(url);
        match self.failure {
            Some("unavailable") => Err(DeepLinkError::Unavailable),
            Some("rejected") => Err(DeepLinkError::Rejected("native URL policy".into())),
            Some(_) => Err(DeepLinkError::Other("native failure".into())),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl CalendarProvider for FakeDevices {
    async fn list_events(&self, query: CalendarQuery) -> Result<Vec<CalendarEvent>, CalendarError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.calendar_queries.lock().unwrap().push(query);
        match self.failure {
            Some("permission_denied") => Err(CalendarError::PermissionDenied),
            Some("unavailable") => Err(CalendarError::Unavailable),
            Some("invalid_request") => Err(CalendarError::Invalid("native query policy".into())),
            Some(_) => Err(CalendarError::Other("native failure".into())),
            None => Ok((0..if self.large_records { 100 } else { 3 })
                .map(|id| CalendarEvent {
                    id: id.to_string(),
                    title: "Meeting".into(),
                    start_ms: 1000,
                    end_ms: 2000,
                    all_day: false,
                    location: None,
                    notes: self.large_records.then(|| "长\n".repeat(5000)),
                    calendar: Some("Work".into()),
                })
                .collect()),
        }
    }
}

#[async_trait]
impl ContactsProvider for FakeDevices {
    async fn search(&self, query: ContactsQuery) -> Result<Vec<Contact>, ContactsError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.contacts_queries.lock().unwrap().push(query);
        match self.failure {
            Some("permission_denied") => Err(ContactsError::PermissionDenied),
            Some("unavailable") => Err(ContactsError::Unavailable),
            Some("invalid_request") => Err(ContactsError::Invalid("native query policy".into())),
            Some(_) => Err(ContactsError::Other("native failure".into())),
            None => Ok((0..if self.large_records { 50 } else { 3 })
                .map(|id| Contact {
                    id: id.to_string(),
                    display_name: if self.large_records {
                        "王".repeat(600)
                    } else {
                        "Alice".into()
                    },
                    phones: vec!["+12345".into()],
                    emails: if self.large_records {
                        vec!["a".repeat(1000); 100]
                    } else {
                        vec!["alice@example.com".into()]
                    },
                })
                .collect()),
        }
    }
}

fn bare_ctx() -> BuiltinToolContext {
    tool_api::test_support::shell_test_ctx(mobile_linux_api::ProcessOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    })
}

fn with_devices(fake: Arc<FakeDevices>) -> BuiltinToolContext {
    BuiltinToolContext {
        location: Some(fake.clone()),
        device_status: Some(fake.clone()),
        haptics: Some(fake.clone()),
        deep_link: Some(fake.clone()),
        calendar: Some(fake.clone()),
        contacts: Some(fake),
        ..bare_ctx()
    }
}

fn tools(ctx: &BuiltinToolContext) -> Vec<(Box<dyn Tool>, Value)> {
    vec![
        (Box::new(LocationTool::new(ctx.clone())), json!({})),
        (Box::new(DeviceStatusTool::new(ctx.clone())), json!({})),
        (
            Box::new(HapticsTool::new(ctx.clone())),
            json!({ "style": "success" }),
        ),
        (
            Box::new(OpenUrlTool::new(ctx.clone())),
            json!({ "url": "https://example.com" }),
        ),
        (
            Box::new(CalendarTool::new(ctx.clone())),
            json!({ "start_ms": 1000, "end_ms": 2000 }),
        ),
        (
            Box::new(ContactsTool::new(ctx.clone())),
            json!({ "query": "Alice" }),
        ),
    ]
}

async fn call(tool: &dyn Tool, input: Value) -> ToolCallResult {
    tool.call(input, fresh_ctx(), fresh_tx())
        .await
        .expect("native call")
}

#[tokio::test]
async fn typed_tools_dispatch_and_return_structured_results() {
    let fake = Arc::new(FakeDevices::default());
    let ctx = with_devices(fake.clone());
    let location = call(&LocationTool::new(ctx.clone()), json!({})).await;
    assert_eq!(
        location.data,
        json!({ "latitude": 31.2, "longitude": 121.4, "accuracy_m": 4.0, "timestamp_ms": 1000 })
    );
    let status = call(&DeviceStatusTool::new(ctx.clone()), json!({})).await;
    assert_eq!(
        status.data,
        json!({ "battery_percent": 75.0, "charging": true, "network": "wifi", "low_power_mode": null })
    );
    let haptic = call(
        &HapticsTool::new(ctx.clone()),
        json!({ "style": "success" }),
    )
    .await;
    assert_eq!(
        haptic.data,
        json!({ "triggered": true, "style": "success" })
    );
    assert_eq!(*fake.styles.lock().unwrap(), vec![HapticStyle::Success]);
    let url = call(
        &OpenUrlTool::new(ctx.clone()),
        json!({ "url": "https://EXAMPLE.com" }),
    )
    .await;
    assert_eq!(
        url.data,
        json!({ "requested": true, "url": "https://example.com/" })
    );
    assert_eq!(*fake.urls.lock().unwrap(), vec!["https://example.com/"]);
    let calendar = call(
        &CalendarTool::new(ctx.clone()),
        json!({ "start_ms": 1000, "end_ms": 2000, "limit": 1 }),
    )
    .await;
    assert_eq!(calendar.data["events"].as_array().unwrap().len(), 1);
    assert_eq!(calendar.data["truncated"], true);
    assert_eq!(calendar.data["events"][0]["start_ms"], 1000);
    assert_eq!(
        *fake.calendar_queries.lock().unwrap(),
        vec![CalendarQuery {
            start_ms: 1000,
            end_ms: 2000,
            limit: 1
        }]
    );
    let contacts = call(
        &ContactsTool::new(ctx),
        json!({ "query": "  Alice  ", "limit": 2 }),
    )
    .await;
    assert_eq!(contacts.data["contacts"].as_array().unwrap().len(), 2);
    assert_eq!(contacts.data["truncated"], true);
    assert_eq!(
        contacts.data["contacts"][0]["emails"],
        json!(["alice@example.com"])
    );
    assert_eq!(
        *fake.contacts_queries.lock().unwrap(),
        vec![ContactsQuery {
            query: "Alice".into(),
            limit: 2
        }]
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), 6);
}

#[tokio::test]
async fn oversized_native_records_are_bounded_without_breaking_utf8_or_json() {
    let ctx = with_devices(Arc::new(FakeDevices {
        large_records: true,
        ..Default::default()
    }));
    let calendar = CalendarTool::new(ctx.clone());
    let events = call(
        &calendar,
        json!({ "start_ms": 0, "end_ms": 1, "limit": 100 }),
    )
    .await;
    assert_eq!(events.data["truncated"], true);
    assert!(events.data.to_string().len() <= calendar.max_result_size_chars());
    let events = events.data["events"].as_array().unwrap();
    assert!(!events.is_empty() && events.len() < 100);
    assert_eq!(events[0]["notes"].as_str().unwrap().chars().count(), 4000);
    let contacts = ContactsTool::new(ctx);
    let result = call(&contacts, json!({ "query": "王", "limit": 50 })).await;
    assert_eq!(result.data["truncated"], true);
    assert!(result.data.to_string().len() <= contacts.max_result_size_chars());
    let rows = result.data["contacts"].as_array().unwrap();
    assert!(!rows.is_empty() && rows.len() < 50);
    assert_eq!(
        rows[0]["display_name"].as_str().unwrap().chars().count(),
        500
    );
    assert_eq!(rows[0]["emails"].as_array().unwrap().len(), 10);
    assert_eq!(rows[0]["emails"][0].as_str().unwrap().chars().count(), 256);
}

#[tokio::test]
async fn defaults_and_every_haptic_style_reach_the_native_contract() {
    let fake = Arc::new(FakeDevices::default());
    let ctx = with_devices(fake.clone());
    call(
        &CalendarTool::new(ctx.clone()),
        json!({ "start_ms": 0, "end_ms": 1 }),
    )
    .await;
    call(&ContactsTool::new(ctx.clone()), json!({ "query": "张三" })).await;
    assert_eq!(fake.calendar_queries.lock().unwrap()[0].limit, 50);
    assert_eq!(fake.contacts_queries.lock().unwrap()[0].limit, 20);
    for style in ["light", "medium", "heavy", "success", "warning", "error"] {
        assert!(
            !call(&HapticsTool::new(ctx.clone()), json!({ "style": style }))
                .await
                .is_error
        );
    }
    assert_eq!(
        *fake.styles.lock().unwrap(),
        vec![
            HapticStyle::Light,
            HapticStyle::Medium,
            HapticStyle::Heavy,
            HapticStyle::Success,
            HapticStyle::Warning,
            HapticStyle::Error
        ]
    );
    for url in [
        "https://example.com/path?q=hello%20world",
        "http://example.com",
        "mailto:alice@example.com?subject=Hi",
        "tel:+1-234-567",
    ] {
        assert!(
            !call(&OpenUrlTool::new(ctx.clone()), json!({ "url": url }))
                .await
                .is_error
        );
    }
}

#[tokio::test]
async fn malformed_inputs_never_reach_a_native_service_even_on_direct_calls() {
    let fake = Arc::new(FakeDevices::default());
    let ctx = with_devices(fake.clone());
    let cases: Vec<(Box<dyn Tool>, Vec<Value>)> = vec![
        (
            Box::new(LocationTool::new(ctx.clone())),
            vec![json!([]), json!({ "continuous": true })],
        ),
        (
            Box::new(DeviceStatusTool::new(ctx.clone())),
            vec![Value::Null, json!({ "identifiers": true })],
        ),
        (
            Box::new(HapticsTool::new(ctx.clone())),
            vec![
                json!({}),
                json!({ "style": "continuous" }),
                json!({ "style": "light", "duration": 10 }),
                json!({ "style": 1 }),
            ],
        ),
        (
            Box::new(OpenUrlTool::new(ctx.clone())),
            vec![
                json!({ "url": 123 }),
                json!({ "url": "https://example.com", "bypass": true }),
            ],
        ),
        (
            Box::new(CalendarTool::new(ctx.clone())),
            vec![
                json!({ "start_ms": 100, "end_ms": 100 }),
                json!({ "start_ms": 100, "end_ms": 99 }),
                json!({ "start_ms": -1, "end_ms": 100 }),
                json!({ "start_ms": 0.5, "end_ms": 100 }),
                json!({ "start_ms": 0, "end_ms": 366_u64 * 86400000 + 1 }),
                json!({ "start_ms": u64::MAX - 1, "end_ms": u64::MAX }),
                json!({ "start_ms": 0, "end_ms": 100, "limit": 0 }),
                json!({ "start_ms": 0, "end_ms": 100, "limit": 101 }),
                json!({ "start_ms": 0, "end_ms": 100, "limit": "5" }),
                json!({ "start_ms": 0, "end_ms": 100, "limit": null }),
                json!({ "start_ms": 0, "end_ms": 100, "calendar": "all" }),
            ],
        ),
        (
            Box::new(ContactsTool::new(ctx)),
            vec![
                json!({ "query": "  " }),
                json!({ "query": "x".repeat(201) }),
                json!({ "query": 1 }),
                json!({ "query": "Alice", "limit": 0 }),
                json!({ "query": "Alice", "limit": 51 }),
                json!({ "query": "Alice", "limit": 1.5 }),
                json!({ "query": "Alice", "limit": null }),
                json!({ "query": "Alice", "include_all": true }),
            ],
        ),
    ];
    for (tool, inputs) in cases {
        for input in inputs {
            assert!(
                tool.validate_input(&input, &fresh_ctx()).await.is_err(),
                "{} accepted {input}",
                tool.name()
            );
            assert!(
                matches!(
                    tool.call(input.clone(), fresh_ctx(), fresh_tx()).await,
                    Err(ToolError::InvalidInput(_))
                ),
                "{} dispatched {input}",
                tool.name()
            );
        }
    }
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unsafe_or_malformed_urls_are_rejected_before_native_open() {
    let fake = Arc::new(FakeDevices::default());
    let tool = OpenUrlTool::new(with_devices(fake.clone()));
    for url in [
        "javascript:alert(1)",
        "data:text/plain,hello",
        "file:///etc/passwd",
        "intent://scan",
        "lingxi://settings",
        "telprompt:123",
        "https://user:password@example.com/",
        "https://user@example.com",
        "",
        "relative/path",
        "https:",
        "https:example.com",
        "https:///example.com",
        " https://example.com",
        "https://example.com/hello world",
        "https://example.com/\n",
        "https://example.com/%0aheader",
        "https://example.com/%zz",
        "https://example.com/%",
        "https://example.com\\@evil.com",
        "mailto:",
        "mailto://user@example.com",
        "tel:",
        "tel://1234",
        "tel:hello",
        "tel:123?body=bad",
    ] {
        assert!(
            matches!(
                tool.call(json!({ "url": url }), fresh_ctx(), fresh_tx())
                    .await,
                Err(ToolError::InvalidInput(_))
            ),
            "accepted {url:?}"
        );
    }
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn precancelled_calls_do_not_enter_any_native_provider() {
    let fake = Arc::new(FakeDevices::default());
    for (tool, input) in tools(&with_devices(fake.clone())) {
        let mut ctx = fresh_ctx();
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        ctx.cancel = Some(cancel);
        assert!(
            matches!(
                tool.call(input, ctx, fresh_tx()).await,
                Err(ToolError::Aborted)
            ),
            "{} ignored cancellation",
            tool.name()
        );
    }
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancellation_drops_an_inflight_native_wait() {
    let fake = Arc::new(FakeDevices {
        pending_location: true,
        ..Default::default()
    });
    let tool = LocationTool::new(with_devices(fake.clone()));
    let mut ctx = fresh_ctx();
    let cancel = tokio_util::sync::CancellationToken::new();
    ctx.cancel = Some(cancel.clone());
    let task = tokio::spawn(async move { tool.call(json!({}), ctx, fresh_tx()).await });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while fake.calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("native operation started");
    cancel.cancel();
    assert!(matches!(task.await.unwrap(), Err(ToolError::Aborted)));
    assert!(fake.location_dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn native_failures_preserve_codes_and_error_status() {
    for code in ["unavailable", "native_error"] {
        let ctx = with_devices(Arc::new(FakeDevices {
            failure: Some(code),
            ..Default::default()
        }));
        for (tool, input) in tools(&ctx) {
            let result = call(tool.as_ref(), input).await;
            assert!(result.is_error, "{} must flag {code}", tool.name());
            assert_eq!(result.data["error"]["code"], code);
        }
    }
    let denied = with_devices(Arc::new(FakeDevices {
        failure: Some("permission_denied"),
        ..Default::default()
    }));
    for (tool, input) in tools(&denied)
        .into_iter()
        .filter(|(tool, _)| matches!(tool.name(), "location" | "calendar" | "contacts"))
    {
        let result = call(tool.as_ref(), input).await;
        assert!(result.is_error);
        assert_eq!(result.data["error"]["code"], "permission_denied");
    }
    let timeout = with_devices(Arc::new(FakeDevices {
        failure: Some("timeout"),
        ..Default::default()
    }));
    assert_eq!(
        call(&LocationTool::new(timeout), json!({})).await.data["error"]["code"],
        "timeout"
    );
    let rejected = with_devices(Arc::new(FakeDevices {
        failure: Some("rejected"),
        ..Default::default()
    }));
    assert_eq!(
        call(
            &OpenUrlTool::new(rejected),
            json!({ "url": "https://example.com" })
        )
        .await
        .data["error"]["code"],
        "rejected"
    );
    let invalid = with_devices(Arc::new(FakeDevices {
        failure: Some("invalid_request"),
        ..Default::default()
    }));
    assert_eq!(
        call(
            &CalendarTool::new(invalid.clone()),
            json!({ "start_ms": 0, "end_ms": 1 })
        )
        .await
        .data["error"]["code"],
        "invalid_request"
    );
    assert_eq!(
        call(&ContactsTool::new(invalid), json!({ "query": "Alice" }))
            .await
            .data["error"]["code"],
        "invalid_request"
    );
}

#[tokio::test]
async fn missing_services_return_unavailable_and_sensitive_tools_ask() {
    for (tool, input) in tools(&bare_ctx()) {
        let permissions = tool.check_permissions(&input, &fresh_ctx()).await;
        if matches!(tool.name(), "device_status" | "haptics") {
            assert!(matches!(permissions, PermissionResult::Allow { .. }));
        } else {
            assert!(matches!(permissions, PermissionResult::Ask { .. }));
        }
        assert_eq!(tool.input_schema()["additionalProperties"], false);
        let result = call(tool.as_ref(), input).await;
        assert!(result.is_error);
        assert_eq!(result.data["error"]["code"], "unavailable");
    }
}

#[test]
fn registration_exposes_only_wired_native_tools() {
    let fake = Arc::new(FakeDevices::default());
    let mut reg = ToolRegistry::new();
    register_all(&mut reg, with_devices(fake.clone()));
    assert_eq!(
        reg.all_names(),
        [
            "location",
            "device_status",
            "haptics",
            "open_url",
            "calendar",
            "contacts"
        ]
    );
    let mut reg = ToolRegistry::new();
    register_all(
        &mut reg,
        BuiltinToolContext {
            contacts: Some(fake),
            ..bare_ctx()
        },
    );
    assert_eq!(reg.all_names(), ["contacts"]);
}
