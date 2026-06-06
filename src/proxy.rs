//! Outbound proxy support for SSH / SFTP connections (issue #7).
//!
//! Establishes the TCP stream to the target host **through a proxy**, then the
//! caller hands that stream to `russh::client::connect_stream`.  Both proxy
//! kinds end up as a transparent `TcpStream`:
//!
//! * **SOCKS5** (`socks5://` / `socks5h://`) via `tokio-socks`; after the
//!   handshake we unwrap to the inner `TcpStream`.
//! * **HTTP / HTTPS CONNECT** (`http://` / `https://`): we issue an HTTP
//!   `CONNECT host:port` and reuse the same socket as the tunnel.
//!
//! The proxy is taken from the per-session setting, falling back to the standard
//! `ALL_PROXY` / `all_proxy` environment variable.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ProxyKind {
    Socks5,
    Http,
}

#[derive(Clone, Debug)]
pub struct ProxyConfig {
    kind: ProxyKind,
    host: String,
    port: u16,
    auth: Option<(String, String)>, // (user, password)
}

/// Resolve the proxy for a session: the explicit `session_proxy` string if set,
/// otherwise the `ALL_PROXY` / `all_proxy` environment variable.  Returns `None`
/// for a direct connection.
pub fn resolve(session_proxy: &str) -> Option<ProxyConfig> {
    let s = session_proxy.trim();
    if !s.is_empty() {
        return parse(s);
    }
    for var in ["ALL_PROXY", "all_proxy"] {
        if let Ok(v) = std::env::var(var) {
            if !v.trim().is_empty() {
                return parse(v.trim());
            }
        }
    }
    None
}

/// Parse a proxy URL: `scheme://[user:pass@]host:port`.
fn parse(url: &str) -> Option<ProxyConfig> {
    let (scheme, rest) = url.split_once("://").unwrap_or(("socks5", url));
    let kind = match scheme.to_ascii_lowercase().as_str() {
        "socks5" | "socks5h" | "socks" => ProxyKind::Socks5,
        "http" | "https" => ProxyKind::Http,
        _ => return None,
    };
    // Optional userinfo before '@'.
    let (auth, hostport) = match rest.rsplit_once('@') {
        Some((userinfo, hp)) => {
            let (u, p) = userinfo.split_once(':').unwrap_or((userinfo, ""));
            (Some((u.to_string(), p.to_string())), hp)
        }
        None => (None, rest),
    };
    let hostport = hostport.trim_end_matches('/');
    let (host, port) = hostport.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    if host.is_empty() {
        return None;
    }
    Some(ProxyConfig {
        kind,
        host: host.to_string(),
        port,
        auth,
    })
}

/// Human-readable description of where we're connecting (for status messages).
pub fn describe(cfg: &ProxyConfig) -> String {
    let scheme = match cfg.kind {
        ProxyKind::Socks5 => "socks5",
        ProxyKind::Http => "http",
    };
    format!("{}://{}:{}", scheme, cfg.host, cfg.port)
}

/// Open a TCP stream to `target_host:target_port` through the proxy.
pub async fn connect(cfg: &ProxyConfig, target_host: &str, target_port: u16) -> Result<TcpStream> {
    match cfg.kind {
        ProxyKind::Socks5 => connect_socks5(cfg, target_host, target_port).await,
        ProxyKind::Http => connect_http(cfg, target_host, target_port).await,
    }
}

async fn connect_socks5(cfg: &ProxyConfig, host: &str, port: u16) -> Result<TcpStream> {
    use tokio_socks::tcp::Socks5Stream;
    let proxy = (cfg.host.as_str(), cfg.port);
    let target = (host, port);
    let stream = match &cfg.auth {
        Some((u, p)) => Socks5Stream::connect_with_password(proxy, target, u, p)
            .await
            .context("SOCKS5 proxy connect failed")?,
        None => Socks5Stream::connect(proxy, target)
            .await
            .context("SOCKS5 proxy connect failed")?,
    };
    // After the handshake the underlying socket is a transparent tunnel.
    Ok(stream.into_inner())
}

async fn connect_http(cfg: &ProxyConfig, host: &str, port: u16) -> Result<TcpStream> {
    let mut s = TcpStream::connect((cfg.host.as_str(), cfg.port))
        .await
        .with_context(|| format!("connect to HTTP proxy {}:{} failed", cfg.host, cfg.port))?;

    let mut req = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n");
    if let Some((u, p)) = &cfg.auth {
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}"));
        req.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    req.push_str("Proxy-Connection: keep-alive\r\n\r\n");
    s.write_all(req.as_bytes())
        .await
        .context("write CONNECT to proxy")?;

    // Read response headers up to the blank line, bounded.
    let mut buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        let n = s.read(&mut byte).await.context("read proxy response")?;
        if n == 0 {
            bail!("proxy closed the connection during CONNECT");
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > 8192 {
            bail!("proxy CONNECT response too large");
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let status_line = head.lines().next().unwrap_or("");
    // Expect "HTTP/1.x 200 ...".
    let ok = status_line
        .split_whitespace()
        .nth(1)
        .map(|c| c == "200")
        .unwrap_or(false);
    if !ok {
        return Err(anyhow!("proxy CONNECT rejected: {}", status_line.trim()));
    }
    Ok(s)
}
