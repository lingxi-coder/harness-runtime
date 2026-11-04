use std::{
    env::{self, var_os},
    path::{Path, PathBuf},
};

use futures_lite::StreamExt;
use image::{open, RgbaImage};
use lazy_static::lazy_static;
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use url::Url;
use xcb::{
    randr::{GetMonitors, MonitorInfoBuf, Output},
    x::{Atom, InternAtom, ScreenBuf},
    ConnResult, Connection as XcbConnection, Xid,
};
use zbus::{Connection as ZBusConnection, DynamicType, OwnedObjectPath, OwnedVariant, Proxy, Type};

use crate::{error::XCapResult, XCapError};

lazy_static! {
    static ref XCB_CONNECTION_AND_INDEX: ConnResult<(XcbConnection, i32)> = {
        let display_name = env::var("DISPLAY").unwrap_or("DISPLAY:1".to_string());
        XcbConnection::connect(Some(display_name.as_str()))
    };
}

pub fn get_xcb_connection_and_index() -> XCapResult<&'static (XcbConnection, i32)> {
    XCB_CONNECTION_AND_INDEX.as_ref().map_err(XCapError::new)
}

pub fn wayland_detect() -> bool {
    let xdg_session_type = var_os("XDG_SESSION_TYPE")
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    let wayland_display = var_os("WAYLAND_DISPLAY")
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    xdg_session_type.eq("wayland") || wayland_display.to_lowercase().contains("wayland")
}

pub fn get_current_screen_buf() -> XCapResult<ScreenBuf> {
    let (conn, index) = get_xcb_connection_and_index()?;

    let setup = conn.get_setup();

    let screen = setup
        .roots()
        .nth(*index as usize)
        .ok_or_else(|| XCapError::new("Not found screen"))?;

    Ok(screen.to_owned())
}

pub fn get_monitor_info_buf(output: Output) -> XCapResult<MonitorInfoBuf> {
    let (conn, _) = get_xcb_connection_and_index()?;

    let screen_buf = get_current_screen_buf()?;

    let get_monitors_cookie = conn.send_request(&GetMonitors {
        window: screen_buf.root(),
        get_active: true,
    });

    let get_monitors_reply = conn.wait_for_reply(get_monitors_cookie)?;

    let monitor_info_iterator = get_monitors_reply.monitors();

    for monitor_info in monitor_info_iterator {
        for &item in monitor_info.outputs() {
            if item == output {
                return Ok(monitor_info.to_owned());
            }
        }
    }
    Err(XCapError::new("Not found monitor"))
}

pub fn get_atom(name: &str) -> XCapResult<Atom> {
    let (conn, _) = get_xcb_connection_and_index()?;
    let atom_cookie = conn.send_request(&InternAtom {
        only_if_exists: true,
        name: name.as_bytes(),
    });
    let atom_reply = conn.wait_for_reply(atom_cookie)?;
    let atom = atom_reply.atom();

    if atom.is_none() {
        return Err(XCapError::new(format!("{} not supported", name)));
    }

    Ok(atom)
}

pub(super) fn png_to_rgba_image<T>(
    filename: T,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> XCapResult<RgbaImage>
where
    T: AsRef<Path>,
{
    let mut dynamic_image = open(filename)?;
    dynamic_image = dynamic_image.crop(x as u32, y as u32, width as u32, height as u32);
    Ok(dynamic_image.to_rgba8())
}

/// uri 转换为 path
pub(super) fn safe_uri_to_path(uri: &str) -> XCapResult<PathBuf> {
    let url = Url::parse(uri)?;

    if url.scheme() != "file" {
        return Err(XCapError::new("Uri scheme is not file"));
    }

    // 获取已解码的路径
    let decoded_path = percent_decode_str(url.path())
        .decode_utf8_lossy()
        .to_string();

    let path = PathBuf::from(&decoded_path);

    Ok(path)
}

pub(super) async fn get_zbus_portal_request(
    conn: &ZBusConnection,
    handle_token: &str,
) -> XCapResult<Proxy<'static>> {
    let unique_identifier = conn
        .unique_name()
        .ok_or(XCapError::new("Get DBus unique name failed"))?
        .trim_start_matches(':')
        .replace('.', "_");

    let path =
        format!("/org/freedesktop/portal/desktop/request/{unique_identifier}/{handle_token}");

    let request = Proxy::new(
        conn,
        "org.freedesktop.portal.Desktop",
        path,
        "org.freedesktop.portal.Request",
    )
    .await?;

    Ok(request)
}

/// Subscribe before the method call: a portal may emit Response before returning
/// its request handle. Reject unexpected handles rather than wait on the wrong
/// object and miss an already-emitted signal.
pub(super) async fn call_portal<T, B>(
    conn: &ZBusConnection,
    portal: &Proxy<'_>,
    method: &str,
    handle_token: &str,
    body: &B,
) -> XCapResult<T>
where
    T: for<'de> Deserialize<'de> + Type,
    B: Serialize + DynamicType,
{
    let request = get_zbus_portal_request(conn, handle_token).await?;
    let mut response = request.receive_signal("Response").await?;
    let returned: OwnedObjectPath = portal.call(method, body).await?;
    if returned.as_str() != request.path().as_str() {
        return Err(XCapError::new(format!(
            "Portal returned unexpected request handle {returned} (expected {})",
            request.path()
        )));
    }

    let message = response
        .next()
        .await
        .ok_or(XCapError::new("Failed get response"))?;

    let body = message.body();
    // A cancellation/denial normally has an empty dictionary. Inspect the
    // status before decoding success-only required fields.
    let (code, _): (u32, std::collections::HashMap<String, OwnedVariant>) = body
        .deserialize()
        .map_err(|error| XCapError::new(format!("Portal {method} response status: {error}")))?;
    match code {
        0 => {
            let (_, result): (u32, T) = body.deserialize().map_err(|error| {
                XCapError::new(format!("Portal {method} response result: {error}"))
            })?;
            Ok(result)
        }
        1 => Err(XCapError::new("Portal request canceled")),
        code => Err(XCapError::new(format!(
            "Portal request denied or failed (code {code})"
        ))),
    }
}

pub(super) fn portal_token() -> String {
    format!("xcap_{:032x}", rand::random::<u128>())
}

pub(super) fn portal_unavailable(error: &XCapError) -> bool {
    matches!(
        error,
        XCapError::ZbusError(zbus::Error::FDO(error))
            if matches!(
                error.as_ref(),
                zbus::fdo::Error::ServiceUnknown(_)
                    | zbus::fdo::Error::NameHasNoOwner(_)
                    | zbus::fdo::Error::UnknownInterface(_)
                    | zbus::fdo::Error::UnknownMethod(_)
            )
    )
}
