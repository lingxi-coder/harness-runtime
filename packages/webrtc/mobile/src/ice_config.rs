//! Bounded native configuration parsing; no secrets are formatted into errors.
use lingxi_webrtc_core::{IceConfig, IceServer};
use serde_json::Value;
pub(crate) fn parse(bytes: &[u8]) -> Result<IceConfig, ()> {
    if bytes.is_empty() {
        return Ok(IceConfig::default());
    }
    if bytes.len() > 65536 {
        return Err(());
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|_| ())?;
    let object = value.as_object().ok_or(())?;
    if object
        .keys()
        .any(|k| !matches!(k.as_str(), "relayOnly" | "servers"))
    {
        return Err(());
    }
    let relay_only = object
        .get("relayOnly")
        .map(|v| v.as_bool().ok_or(()))
        .transpose()?
        .unwrap_or(false);
    let mut servers = Vec::new();
    if let Some(list) = object.get("servers") {
        let list = list.as_array().ok_or(())?;
        if list.len() > 8 {
            return Err(());
        }
        for server in list {
            let o = server.as_object().ok_or(())?;
            if o.keys()
                .any(|k| !matches!(k.as_str(), "urls" | "username" | "credential"))
            {
                return Err(());
            }
            let urls = o
                .get("urls")
                .and_then(Value::as_array)
                .ok_or(())?
                .iter()
                .map(|v| v.as_str().map(str::to_owned).ok_or(()))
                .collect::<Result<Vec<_>, _>>()?;
            let text = |key: &str| {
                o.get(key)
                    .map(|v| v.as_str().map(str::to_owned).ok_or(()))
                    .transpose()
                    .map(|v| v.unwrap_or_default())
            };
            servers.push(IceServer {
                urls,
                username: text("username")?,
                credential: text("credential")?,
            });
        }
    }
    let ice = IceConfig {
        servers,
        relay_only,
    };
    ice.validate().map_err(|_| ())?;
    Ok(ice)
}
pub(crate) fn relay_candidates(sdp: &str, required: bool) -> Result<u32, i32> {
    let mut count = 0;
    for line in sdp.lines().filter(|line| line.starts_with("a=candidate:")) {
        if line.split_whitespace().nth(7) == Some("relay") {
            count += 1;
        } else if required {
            return Err(-1230);
        }
    }
    if required && count == 0 {
        return Err(-1230);
    }
    Ok(count)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_ice_config_fails_closed() {
        for bad in [
            r#"{"relayOnly":"true"}"#,
            r#"{"relay_only":true}"#,
            r#"{"relayOnly":true}"#,
            r#"{"servers":[{"urls":["turn:localhost"],"credential":"missing-user"}]}"#,
        ] {
            assert!(parse(bad.as_bytes()).is_err());
        }
        assert!(parse(&vec![b' '; 65537]).is_err());
        assert!(parse(b"").is_ok());
        let config=parse(br#"{"relayOnly":true,"servers":[{"urls":["turn:127.0.0.1:3478?transport=udp"],"username":"test","credential":"test-password"}]}"#).unwrap();
        assert!(config.relay_only);
        assert_eq!(
            relay_candidates("a=candidate:f 1 udp 1 127.0.0.1 123 typ relay\r\n", true),
            Ok(1)
        );
        assert_eq!(
            relay_candidates("a=candidate:f 1 udp 1 127.0.0.1 123 typ host\r\n", true),
            Err(-1230)
        );
        assert!(relay_candidates("", true).is_err());
    }
}
