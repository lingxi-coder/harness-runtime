//! CONNECT-target parsing + bounded direct dial (`http-proxy.js:265-279`,
//! `parent-proxy.js:415-437`).

use std::time::Duration;

use tokio::net::TcpStream;

/// Default connect timeout (`parent-proxy.js:21` `CONNECT_TIMEOUT_MS`).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Parse a CONNECT request-target `host:port` (or `[ipv6]:port`) into
/// `(hostname, port)`. Port must be 1..=65535. (`parseConnectTarget`.)
#[must_use]
pub fn parse_connect_target(target: &str) -> Option<(String, u16)> {
    let (host, port_str) = if let Some(rest) = target.strip_prefix('[') {
        let (h, after) = rest.split_once(']')?;
        (h.to_string(), after.strip_prefix(':')?)
    } else {
        let (h, p) = target.rsplit_once(':')?;
        // reject if host part itself contains a colon (would be an unbracketed v6)
        if h.contains(':') {
            return None;
        }
        (h.to_string(), p)
    };
    let port: u32 = port_str.parse().ok()?;
    if !(1..=65535).contains(&port) {
        return None;
    }
    Some((host, u16::try_from(port).ok()?))
}

/// Dial `host:port` directly with a bounded timeout (`dialDirect`).
///
/// # Errors
/// Returns the connect error or a timeout.
pub async fn dial_direct(host: &str, port: u16) -> std::io::Result<TcpStream> {
    match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port))).await {
        Ok(r) => r,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "connect timed out",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_connect_target_host_and_ipv6() {
        // http-proxy.js:265-279
        assert_eq!(
            parse_connect_target("example.com:443"),
            Some(("example.com".into(), 443))
        );
        assert_eq!(
            parse_connect_target("[::1]:8443"),
            Some(("::1".into(), 8443))
        );
        assert_eq!(parse_connect_target("host:1"), Some(("host".into(), 1)));
        // invalid
        assert_eq!(parse_connect_target("noport"), None);
        assert_eq!(parse_connect_target("host:0"), None);
        assert_eq!(parse_connect_target("host:65536"), None);
        assert_eq!(parse_connect_target("host:abc"), None);
    }
}
