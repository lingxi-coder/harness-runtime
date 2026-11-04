use std::{
    collections::HashMap,
    future::Future,
    io::Cursor,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc,
    },
    thread,
};

use pipewire::{
    context::Context,
    keys::{MEDIA_CATEGORY, MEDIA_ROLE, MEDIA_TYPE},
    main_loop::MainLoop,
    properties,
    spa::{
        param::{
            format::{FormatProperties, MediaSubtype, MediaType},
            format_utils,
            video::{VideoFormat, VideoInfoRaw},
            ParamType,
        },
        pod::{self, serialize::PodSerializer, Pod},
        utils::{Direction, Fraction, Rectangle, SpaTypes},
    },
    stream::{Stream, StreamFlags},
};
use zbus::{
    Connection, DeserializeDict, OwnedFd, OwnedObjectPath, OwnedVariant, Proxy, Type, Variant,
};

use crate::{video_recorder::Frame, XCapError, XCapResult};

use super::{
    impl_monitor::ImplMonitor,
    utils::{call_portal, portal_token},
};

#[allow(dead_code)]
#[derive(DeserializeDict, Type, Debug)]
#[zbus(signature = "dict")]
pub struct ScreenCastStartStream {
    pub id: Option<String>,
    pub position: Option<(i32, i32)>,
    pub size: Option<(i32, i32)>,
    pub source_type: Option<u32>,
    pub mapping_id: Option<String>,
}

#[derive(DeserializeDict, Type, Debug)]
#[zbus(signature = "dict")]
pub struct ScreenCastStartResponse {
    pub streams: Option<Vec<(u32, ScreenCastStartStream)>>,
    #[allow(dead_code)]
    pub restore_token: Option<String>,
}

#[derive(DeserializeDict, Type, Debug)]
#[zbus(signature = "dict")]
struct ScreenCastCreateResponse {
    // The portal protocol deliberately encodes this object path as a string.
    session_handle: String,
}

#[derive(Debug)]
struct PortalSession {
    connection: Connection,
    path: OwnedObjectPath,
}

impl PortalSession {
    async fn close(&self) {
        if let Err(error) = self
            .connection
            .call_method(
                Some("org.freedesktop.portal.Desktop"),
                &self.path,
                Some("org.freedesktop.portal.Session"),
                "Close",
                &(),
            )
            .await
        {
            log::debug!("Could not close screen cast session: {}", error);
        }
    }
}

/// https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html
struct ScreenCast {
    connection: Connection,
}

impl ScreenCast {
    async fn new() -> XCapResult<Self> {
        Ok(Self {
            connection: Connection::session().await?,
        })
    }

    async fn proxy(&self) -> XCapResult<Proxy<'static>> {
        Ok(Proxy::new(
            &self.connection,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.ScreenCast",
        )
        .await?)
    }

    async fn create_session(&self) -> XCapResult<PortalSession> {
        let proxy = self.proxy().await?;
        let handle_token = portal_token();
        let session_token = portal_token();
        let mut options = HashMap::new();
        options.insert("handle_token", Variant::from(&handle_token));
        options.insert("session_handle_token", Variant::from(&session_token));
        let response: ScreenCastCreateResponse = call_portal(
            &self.connection,
            &proxy,
            "CreateSession",
            &handle_token,
            &options,
        )
        .await?;
        let path = OwnedObjectPath::try_from(response.session_handle)?;
        Ok(PortalSession {
            connection: self.connection.clone(),
            path,
        })
    }

    async fn select_sources(&self, session: &PortalSession) -> XCapResult<()> {
        let proxy = self.proxy().await?;
        let handle_token = portal_token();
        let mut options = HashMap::new();
        options.insert("handle_token", Variant::from(&handle_token));
        options.insert("types", Variant::from(1_u32));
        options.insert("multiple", Variant::from(false));
        let _: HashMap<String, OwnedVariant> = call_portal(
            &self.connection,
            &proxy,
            "SelectSources",
            &handle_token,
            &(&session.path, options),
        )
        .await?;
        Ok(())
    }

    async fn start(&self, session: &PortalSession) -> XCapResult<ScreenCastStartResponse> {
        let proxy = self.proxy().await?;
        let handle_token = portal_token();
        let mut options = HashMap::new();
        options.insert("handle_token", Variant::from(&handle_token));
        call_portal(
            &self.connection,
            &proxy,
            "Start",
            &handle_token,
            &(&session.path, "", options),
        )
        .await
    }

    async fn open_pipe_wire_remote(&self, session: &PortalSession) -> XCapResult<OwnedFd> {
        let proxy = self.proxy().await?;
        let options: HashMap<&str, Variant<'_>> = HashMap::new();
        Ok(proxy
            .call("OpenPipeWireRemote", &(&session.path, options))
            .await?)
    }

    async fn prepare(self) -> XCapResult<(PortalSession, u32, OwnedFd)> {
        let session = self.create_session().await?;
        let result = async {
            self.select_sources(&session).await?;
            let response = self.start(&session).await?;
            let stream_id = response
                .streams
                .and_then(|streams| streams.first().map(|stream| stream.0))
                .ok_or_else(|| XCapError::new("Stream ID not found"))?;
            let fd = self.open_pipe_wire_remote(&session).await?;
            Ok::<_, XCapError>((stream_id, fd))
        }
        .await;
        match result {
            Ok((stream_id, fd)) => Ok((session, stream_id, fd)),
            Err(error) => {
                session.close().await;
                Err(error)
            }
        }
    }
}

fn start_portal_worker<F, Fut>(connect: F) -> XCapResult<(async_channel::Sender<()>, u32, OwnedFd)>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = XCapResult<ScreenCast>>,
{
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (portal_lifetime, stop_rx) = async_channel::bounded::<()>(1);
    thread::spawn(move || {
        zbus::block_on(async move {
            match async { connect().await?.prepare().await }.await {
                Ok((session, stream_id, fd)) => {
                    if ready_tx.send(Ok((stream_id, fd))).is_ok() {
                        let _ = stop_rx.recv().await;
                    }
                    session.close().await;
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                }
            }
        })
    });
    let (stream_id, fd) = ready_rx
        .recv()
        .map_err(|_| XCapError::new("Screen cast D-Bus worker stopped"))??;
    Ok((portal_lifetime, stream_id, fd))
}

#[derive(Debug, Clone)]
pub struct WaylandVideoRecorder {
    #[allow(dead_code)]
    monitor: ImplMonitor,
    sender: Sender<Frame>,
    is_running: Arc<AtomicBool>,
    // The worker owns the D-Bus connection until the last recorder is dropped.
    _portal_lifetime: async_channel::Sender<()>,
}

#[derive(Clone)]
struct ListenerUserData {
    pub format: VideoInfoRaw,
}

impl WaylandVideoRecorder {
    pub fn new(monitor: ImplMonitor) -> XCapResult<(Self, Receiver<Frame>)> {
        let (sender, receiver) = mpsc::channel();

        // xcap exposes a synchronous API, including to async applications.
        // Never enter zbus::block_on on the caller's runtime thread.
        let (portal_lifetime, stream_id, fd) = start_portal_worker(ScreenCast::new)?;

        let recorder = Self {
            monitor,
            sender,
            is_running: Arc::new(AtomicBool::new(false)),
            _portal_lifetime: portal_lifetime,
        };

        recorder.pipewire_capturer(stream_id, fd.into())?;

        Ok((recorder, receiver))
    }

    fn pipewire_capturer(&self, stream_id: u32, fd: std::os::fd::OwnedFd) -> XCapResult<()> {
        let sender = self.sender.clone();
        let is_running = self.is_running.clone();

        thread::spawn(move || {
            pipewire::init();

            let main_loop = MainLoop::new(None)?;
            let context = Context::new(&main_loop)?;
            let core = context.connect_fd(fd, None)?;

            let user_data = ListenerUserData {
                format: Default::default(),
            };

            let stream = Stream::new(
                &core,
                "XCap",
                properties::properties! {
                    *MEDIA_TYPE => "Video",
                    *MEDIA_CATEGORY => "Capture",
                    *MEDIA_ROLE => "Screen",
                },
            )?;

            let _listener = stream
                .add_local_listener_with_user_data(user_data)
                .param_changed(|_, user_data, id, param| {
                    let Some(param) = param else {
                        return;
                    };

                    if id != ParamType::Format.as_raw() {
                        return;
                    }

                    let (media_type, media_subtype) = match format_utils::parse_format(param) {
                        Ok(v) => v,
                        Err(err) => {
                            log::error!("Failed to parse format: {:?}", err);
                            return;
                        }
                    };

                    if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
                        return;
                    }

                    if let Err(err) = user_data.format.parse(param) {
                        log::error!("Failed to parse format: {:?}", err);
                    }
                })
                .process(move |stream, user_data| {
                    let state = is_running.load(Ordering::Relaxed);
                    if !state {
                        return;
                    }
                    match stream.dequeue_buffer() {
                        None => log::info!("stream.dequeue_buffer() returned None"),
                        Some(mut buffer) => {
                            let datas = buffer.datas_mut();
                            if datas.is_empty() {
                                return;
                            }
                            let size = user_data.format.size();
                            if let Some(frame_data) = datas[0].data() {
                                let buffer = match user_data.format.format() {
                                    VideoFormat::RGB => {
                                        let mut buf =
                                            vec![0; (size.width * size.height * 4) as usize];
                                        for (src, dst) in
                                            frame_data.chunks_exact(3).zip(buf.chunks_exact_mut(4))
                                        {
                                            dst[0] = src[0];
                                            dst[1] = src[1];
                                            dst[2] = src[2];
                                            dst[3] = 255;
                                        }

                                        buf
                                    }
                                    VideoFormat::RGBA => frame_data.to_vec(),
                                    VideoFormat::RGBx => frame_data.to_vec(),
                                    VideoFormat::BGRx => {
                                        let mut buf = frame_data.to_vec();
                                        for src in buf.chunks_exact_mut(4) {
                                            src.swap(0, 2);
                                        }

                                        buf
                                    }
                                    _ => {
                                        log::error!(
                                            "Unsupported format: {:?}",
                                            user_data.format.format()
                                        );
                                        return;
                                    }
                                };

                                let _ = sender.send(Frame::new(size.width, size.height, buffer));
                            }
                        }
                    }
                })
                .register()?;

            let obj = pod::object!(
                SpaTypes::ObjectParamFormat,
                ParamType::EnumFormat,
                pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
                pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
                pod::property!(
                    FormatProperties::VideoFormat,
                    Choice,
                    Enum,
                    Id,
                    VideoFormat::RGB,
                    VideoFormat::RGBA,
                    VideoFormat::RGBx,
                    VideoFormat::BGRx,
                    // VideoFormat::YUY2,
                    // VideoFormat::I420,
                ),
                pod::property!(
                    FormatProperties::VideoSize,
                    Choice,
                    Range,
                    Rectangle,
                    Rectangle {
                        width: 128,
                        height: 128
                    },
                    Rectangle {
                        width: 1,
                        height: 1
                    },
                    Rectangle {
                        width: 4096,
                        height: 4096
                    }
                ),
                pod::property!(
                    FormatProperties::VideoFramerate,
                    Choice,
                    Range,
                    Fraction,
                    Fraction { num: 24, denom: 1 },
                    Fraction { num: 0, denom: 1 },
                    Fraction {
                        num: 1000,
                        denom: 1
                    }
                ),
            );
            let values =
                PodSerializer::serialize(Cursor::new(Vec::new()), &pod::Value::Object(obj))
                    .map_err(XCapError::new)?
                    .0
                    .into_inner();

            let mut params =
                [Pod::from_bytes(&values).ok_or(XCapError::new("Failed to create Pod"))?];

            stream.connect(
                Direction::Input,
                Some(stream_id),
                StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
                &mut params,
            )?;

            main_loop.run();

            Result::<(), XCapError>::Ok(())
        });

        Ok(())
    }

    pub fn start(&self) -> XCapResult<()> {
        self.is_running.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub fn stop(&self) -> XCapResult<()> {
        self.is_running.store(false, Ordering::Relaxed);
        Ok(())
    }
}

#[cfg(test)]
#[path = "portal_tests.rs"]
mod portal_tests;
