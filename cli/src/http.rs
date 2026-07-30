//! A very small HTTP/1.1 client, for one caller and one server.
//!
//! The CLI talks to nothing but the daemon on the loopback interface: no TLS, no
//! redirects, no chunked responses, no keep-alive (every request asks the server
//! to close, which is also what makes "read to EOF" a complete body). That is
//! roughly eighty lines here versus reqwest + tokio + rustls in the binary, and
//! this binary is the one an install script hands to a stranger.
//!
//! Anything that needs a real HTTP client — talking to the asale server,
//! refreshing provider tokens — is the daemon's job, not this one's.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

pub struct Reply {
    pub status: u16,
    pub body: String,
}

impl Reply {
    pub fn json(&self) -> Option<serde_json::Value> {
        serde_json::from_str(&self.body).ok()
    }
}

/// Where to dial for a *bind* address.
///
/// `asaled --bind 0.0.0.0:9700` (web mode on a headless box) listens on every
/// interface, and the unspecified address is not a destination — dialling
/// 0.0.0.0 fails outright on some platforms and is a different host on others.
/// The CLI always administers the local daemon, so it swaps in the loopback
/// address of the same family.
pub fn dial_addr(bind: &str) -> SocketAddr {
    let parsed = bind
        .parse::<SocketAddr>()
        .ok()
        .or_else(|| bind.to_socket_addrs().ok().and_then(|mut it| it.next()))
        .unwrap_or_else(|| SocketAddr::from((Ipv4Addr::LOCALHOST, 9700)));
    if !parsed.ip().is_unspecified() {
        return parsed;
    }
    let loopback = match parsed.ip() {
        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
    };
    SocketAddr::new(loopback, parsed.port())
}

fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
    timeout: Duration,
) -> std::io::Result<Reply> {
    let mut sock = TcpStream::connect_timeout(&addr, timeout)?;
    sock.set_read_timeout(Some(timeout))?;
    sock.set_write_timeout(Some(timeout))?;

    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nUser-Agent: asale-cli/{}\r\nConnection: close\r\n",
        env!("CARGO_PKG_VERSION")
    );
    if let Some(t) = token {
        head.push_str(&format!("X-Asale-Token: {t}\r\n"));
    }
    match body {
        // Content-Type is not decoration: /rpc only accepts application/json
        // (axum's `Json` extractor rejects anything else).
        Some(b) => head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{b}",
            b.len()
        )),
        None => head.push_str("\r\n"),
    }
    sock.write_all(head.as_bytes())?;
    sock.flush()?;

    let mut raw = Vec::new();
    sock.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw).into_owned();

    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let body = match text.find("\r\n\r\n") {
        Some(i) => text[i + 4..].to_string(),
        None => String::new(),
    };
    Ok(Reply { status, body })
}

pub fn get(addr: SocketAddr, path: &str, timeout: Duration) -> Option<Reply> {
    request(addr, "GET", path, None, None, timeout).ok()
}

/// One daemon RPC. `None` means the daemon did not answer at all — the caller
/// distinguishes that from an answer it did not like.
pub fn rpc(addr: SocketAddr, cmd: &str, token: &str, timeout: Duration) -> Option<Reply> {
    request(addr, "POST", &format!("/rpc/{cmd}"), Some(token), Some("{}"), timeout).ok()
}

/// True when something answers `/healthz` — the same probe the desktop shell
/// uses before deciding whether to start its own in-process daemon.
pub fn healthy(addr: SocketAddr) -> bool {
    matches!(get(addr, "/healthz", Duration::from_millis(1200)), Some(r) if r.status == 200)
}

/// Wait for the daemon to stop answering, up to `deadline`.
pub fn wait_gone(addr: SocketAddr, deadline: Duration) -> bool {
    let start = std::time::Instant::now();
    loop {
        if !healthy(addr) {
            return true;
        }
        if start.elapsed() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unspecified_bind_is_dialled_on_loopback() {
        assert_eq!(dial_addr("0.0.0.0:9700"), SocketAddr::from((Ipv4Addr::LOCALHOST, 9700)));
        assert_eq!(dial_addr("[::]:9701"), SocketAddr::from((Ipv6Addr::LOCALHOST, 9701)));
    }

    #[test]
    fn explicit_bind_is_left_alone() {
        assert_eq!(dial_addr("127.0.0.1:9788"), SocketAddr::from((Ipv4Addr::LOCALHOST, 9788)));
        assert_eq!(dial_addr("192.168.1.4:9700").port(), 9700);
    }

    #[test]
    fn garbage_falls_back_to_the_default_port() {
        assert_eq!(dial_addr("not-an-address"), SocketAddr::from((Ipv4Addr::LOCALHOST, 9700)));
    }
}
