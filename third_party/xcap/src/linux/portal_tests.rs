//! Exercise the portal against a private, real D-Bus daemon. The fake portal
//! emits Response *before* returning the request handle to expose races.
use super::super::{utils::portal_unavailable, wayland_capture::org_freedesktop_portal_screenshot};
use super::{start_portal_worker, ScreenCast};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use zbus::{message::Header, Connection, OwnedFd, OwnedObjectPath, OwnedVariant, Variant};

const SESSION: &str = "/org/freedesktop/portal/desktop/session/server/actual";
const DESKTOP: &str = "/org/freedesktop/portal/desktop";

type Trace = Arc<Mutex<Vec<String>>>;

struct Bus(Child);
impl Bus {
    fn start() -> (Self, String) {
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("dbus-daemon must be installed for the portal tests");
        let mut address = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        (Self(child), address.trim().to_string())
    }
}
impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Success,
    CancelCreate,
    DenySelect,
    DenyStart,
    WrongHandle,
    InvalidSession,
    CancelScreenshot,
    DenyScreenshot,
}

struct MockPortal {
    trace: Trace,
    mode: Mode,
}

impl MockPortal {
    async fn respond(
        &self,
        stage: &str,
        options: &HashMap<String, OwnedVariant>,
        header: Header<'_>,
        connection: &Connection,
        results: HashMap<&str, Variant<'_>>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        let sender = header
            .sender()
            .unwrap()
            .trim_start_matches(':')
            .replace('.', "_");
        let token = <&str>::try_from(options.get("handle_token").unwrap()).unwrap();
        let handle = OwnedObjectPath::try_from(format!(
            "/org/freedesktop/portal/desktop/request/{sender}/{token}"
        ))
        .unwrap();
        let status = match (self.mode, stage) {
            (Mode::CancelCreate, "create") | (Mode::CancelScreenshot, "screenshot") => 1_u32,
            (Mode::DenySelect, "select")
            | (Mode::DenyStart, "start")
            | (Mode::DenyScreenshot, "screenshot") => 2_u32,
            _ => 0_u32,
        };
        self.trace.lock().unwrap().push(stage.to_owned());
        // Deliberately emit before the method reply: a client that subscribes
        // only after calling the portal will miss this signal.
        if status == 0 {
            connection
                .emit_signal(
                    None::<&str>,
                    &handle,
                    "org.freedesktop.portal.Request",
                    "Response",
                    &(status, results),
                )
                .await
                .unwrap();
        } else {
            let empty: HashMap<&str, Variant<'_>> = HashMap::new();
            connection
                .emit_signal(
                    None::<&str>,
                    &handle,
                    "org.freedesktop.portal.Request",
                    "Response",
                    &(status, empty),
                )
                .await
                .unwrap();
        }
        if self.mode == Mode::WrongHandle && stage == "create" {
            Ok(
                OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/request/wrong/path")
                    .unwrap(),
            )
        } else {
            Ok(handle)
        }
    }
}

#[zbus::interface(name = "org.freedesktop.portal.ScreenCast")]
impl MockPortal {
    async fn create_session(
        &self,
        options: HashMap<String, OwnedVariant>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        let path = if self.mode == Mode::InvalidSession {
            "not-an-object-path"
        } else {
            SESSION
        };
        let mut results = HashMap::new();
        results.insert("session_handle", Variant::from(path));
        self.respond("create", &options, header, connection, results)
            .await
    }

    async fn select_sources(
        &self,
        session: OwnedObjectPath,
        options: HashMap<String, OwnedVariant>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        assert_eq!(session.as_str(), SESSION);
        self.respond("select", &options, header, connection, HashMap::new())
            .await
    }

    async fn start(
        &self,
        session: OwnedObjectPath,
        _parent: &str,
        options: HashMap<String, OwnedVariant>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        assert_eq!(session.as_str(), SESSION);
        let stream_properties: HashMap<String, Variant<'_>> = HashMap::new();
        let streams = vec![(42_u32, stream_properties)];
        let mut results = HashMap::new();
        results.insert("streams", Variant::from(streams));
        self.respond("start", &options, header, connection, results)
            .await
    }

    fn open_pipe_wire_remote(
        &self,
        session: OwnedObjectPath,
        _options: HashMap<String, OwnedVariant>,
    ) -> zbus::fdo::Result<OwnedFd> {
        assert_eq!(session.as_str(), SESSION);
        self.trace.lock().unwrap().push("open".to_string());
        let fd: std::os::fd::OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        Ok(fd.into())
    }
}

struct MockSession(Trace);
#[zbus::interface(name = "org.freedesktop.portal.Session")]
impl MockSession {
    fn close(&self) {
        self.0.lock().unwrap().push("close".to_string());
    }
}

struct MockScreenshot {
    portal: MockPortal,
    uri: String,
}

#[zbus::interface(name = "org.freedesktop.portal.Screenshot")]
impl MockScreenshot {
    async fn screenshot(
        &self,
        _parent: &str,
        options: HashMap<String, OwnedVariant>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        let mut results = HashMap::new();
        results.insert("uri", Variant::from(self.uri.as_str()));
        self.portal
            .respond("screenshot", &options, header, connection, results)
            .await
    }
}

fn run(mode: Mode) -> (Trace, Result<(), crate::XCapError>) {
    let (_bus, address) = Bus::start();
    let trace: Trace = Arc::new(Mutex::new(Vec::new()));
    let result = zbus::block_on(async {
        let _server = zbus::connection::Builder::address(address.as_str())
            .name("org.freedesktop.portal.Desktop")
            .serve_at(
                DESKTOP,
                MockPortal {
                    trace: trace.clone(),
                    mode,
                },
            )
            .serve_at(SESSION, MockSession(trace.clone()))
            .build()
            .await
            .unwrap();
        let connection = zbus::connection::Builder::address(address.as_str())
            .build()
            .await
            .unwrap();
        match (ScreenCast { connection }).prepare().await {
            Ok((session, stream, _fd)) => {
                assert_eq!(stream, 42);
                assert_eq!(session.path.as_str(), SESSION);
                assert_eq!(
                    &*trace.lock().unwrap(),
                    &["create", "select", "start", "open"]
                );
                session.close().await;
                Ok(())
            }
            Err(error) => Err(error),
        }
    });
    (trace, result)
}

#[test]
fn portal_early_response_and_actual_session_handle() {
    let (trace, result) = run(Mode::Success);
    result.unwrap();
    assert_eq!(
        &*trace.lock().unwrap(),
        &["create", "select", "start", "open", "close"]
    );
}

#[test]
fn portal_cancellation_denial_and_bad_handles_fail_closed() {
    for (mode, expected, message) in [
        (Mode::CancelCreate, vec!["create"], "canceled"),
        (
            Mode::DenySelect,
            vec!["create", "select", "close"],
            "code 2",
        ),
        (
            Mode::DenyStart,
            vec!["create", "select", "start", "close"],
            "code 2",
        ),
        (
            Mode::WrongHandle,
            vec!["create"],
            "unexpected request handle",
        ),
        (Mode::InvalidSession, vec!["create"], "object path"),
    ] {
        let (trace, result) = run(mode);
        let error = result.unwrap_err().to_string();
        assert!(error.contains(message), "{mode:?}: {error}");
        assert_eq!(&*trace.lock().unwrap(), &expected, "{mode:?}");
    }
}

#[test]
fn screenshot_handles_early_response_and_rejection() {
    for mode in [Mode::Success, Mode::CancelScreenshot, Mode::DenyScreenshot] {
        let (_bus, address) = Bus::start();
        let trace: Trace = Arc::new(Mutex::new(Vec::new()));
        let path = std::env::temp_dir().join(format!("xcap-test-{}.png", rand::random::<u128>()));
        image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]))
            .save(&path)
            .unwrap();
        let uri = url::Url::from_file_path(&path).unwrap().to_string();
        let result = zbus::block_on(async {
            let _server = zbus::connection::Builder::address(address.as_str())
                .name("org.freedesktop.portal.Desktop")
                .serve_at(
                    DESKTOP,
                    MockScreenshot {
                        portal: MockPortal {
                            trace: trace.clone(),
                            mode,
                        },
                        uri,
                    },
                )
                .build()
                .await
                .unwrap();
            let connection = zbus::connection::Builder::address(address.as_str())
                .build()
                .await
                .unwrap();
            org_freedesktop_portal_screenshot(&connection, 0, 0, 1, 1).await
        });
        match mode {
            Mode::Success => {
                let image = result.unwrap();
                assert_eq!(image.dimensions(), (1, 1));
                assert_eq!(image.get_pixel(0, 0).0, [1, 2, 3, 255]);
                assert!(!path.exists());
            }
            Mode::CancelScreenshot | Mode::DenyScreenshot => {
                let error = result.unwrap_err();
                assert!(
                    !portal_unavailable(&error),
                    "a rejected request must never use the fallback"
                );
                let _ = std::fs::remove_file(&path);
            }
            _ => unreachable!(),
        }
        assert_eq!(&*trace.lock().unwrap(), &["screenshot"]);
    }
    let absent = crate::XCapError::ZbusError(zbus::Error::FDO(Box::new(
        zbus::fdo::Error::ServiceUnknown("portal absent".into()),
    )));
    assert!(portal_unavailable(&absent));
}

#[test]
fn missing_portal_is_the_only_fallback_case() {
    let (_bus, address) = Bus::start();
    let error = zbus::block_on(async {
        let connection = zbus::connection::Builder::address(address.as_str())
            .build()
            .await
            .unwrap();
        org_freedesktop_portal_screenshot(&connection, 0, 0, 1, 1)
            .await
            .unwrap_err()
    });
    assert!(portal_unavailable(&error), "{error}");
}

#[test]
fn worker_keeps_portal_alive_until_last_handle_is_dropped() {
    let (_bus, address) = Bus::start();
    let trace: Trace = Arc::new(Mutex::new(Vec::new()));
    let server_trace = trace.clone();
    let server_address = address.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let (stop_tx, stop_rx) = async_channel::bounded::<()>(1);
    let server = thread::spawn(move || {
        zbus::block_on(async move {
            let _server = zbus::connection::Builder::address(server_address.as_str())
                .name("org.freedesktop.portal.Desktop")
                .serve_at(
                    DESKTOP,
                    MockPortal {
                        trace: server_trace.clone(),
                        mode: Mode::Success,
                    },
                )
                .serve_at(SESSION, MockSession(server_trace))
                .build()
                .await
                .unwrap();
            ready_tx.send(()).unwrap();
            let _ = stop_rx.recv().await;
        })
    });
    ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    // Calling the synchronous bridge from another async runtime is supported.
    let (lifetime, stream, fd) = zbus::block_on(async {
        start_portal_worker(move || async move {
            Ok(ScreenCast {
                connection: zbus::connection::Builder::address(address.as_str())
                    .build()
                    .await?,
            })
        })
        .unwrap()
    });
    assert_eq!(stream, 42);
    let clone = lifetime.clone();
    drop(lifetime);
    thread::sleep(Duration::from_millis(50));
    assert_eq!(
        &*trace.lock().unwrap(),
        &["create", "select", "start", "open"]
    );
    drop(clone);
    drop(fd);
    for _ in 0..100 {
        if trace.lock().unwrap().last().map(String::as_str) == Some("close") {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        trace.lock().unwrap().last().map(String::as_str),
        Some("close")
    );
    drop(stop_tx);
    server.join().unwrap();
}
