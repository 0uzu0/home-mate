//! Minimal HTTP/1.1 client (loopback management API + LAN package fetch).
//!
//! No TLS, no redirects, no keep-alive: M8a distribution is LAN `http://` only
//! (user decision) and the management API is loopback. Chunked responses are
//! decoded because axum may use them; everything is bounded.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

struct Url {
    host: String,
    port: u16,
    path: String,
}

fn parse_url(url: &str) -> Result<Url, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("only http:// URLs are supported ({url})"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(format!("URL has no host: {url}"));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>().map_err(|_| format!("bad port in {url}"))?,
        ),
        None => (authority.to_string(), 80),
    };
    Ok(Url {
        host,
        port,
        path: path.to_string(),
    })
}

pub fn request(
    method: &str,
    url: &str,
    body: Option<&[u8]>,
    timeout: Duration,
) -> Result<Response, String> {
    request_with_token(method, url, body, None, timeout)
}

pub fn request_with_token(
    method: &str,
    url: &str,
    body: Option<&[u8]>,
    token: Option<&str>,
    timeout: Duration,
) -> Result<Response, String> {
    let u = parse_url(url)?;
    let addr = (u.host.as_str(), u.port)
        .to_socket_addrs()
        .map_err(|e| format!("resolve {}: {e}", u.host))?
        .next()
        .ok_or_else(|| format!("resolve {}: no address", u.host))?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|e| format!("connect {}:{}: {e}", u.host, u.port))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;

    let mut head = format!(
        "{method} {} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\nAccept: */*\r\n",
        u.path, u.host, u.port
    );
    if let Some(t) = token.map(str::trim).filter(|t| !t.is_empty()) {
        if t.contains('\r') || t.contains('\n') {
            return Err("invalid characters in bearer token".into());
        }
        head.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    if let Some(b) = body {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            b.len()
        ));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    if let Some(b) = body {
        stream
            .write_all(b)
            .map_err(|e| format!("write body: {e}"))?;
    }
    stream.flush().ok();

    let mut raw = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    let cap = 64 * 1024 * 1024;
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > cap {
                    return Err("response exceeds 64 MiB".into());
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Err(format!(
                    "timeout waiting for response after {} bytes: {e}",
                    raw.len()
                ));
            }
            Err(e) => return Err(format!("read: {e}")),
        }
    }

    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("malformed HTTP response (no header terminator)")?;
    let header_text = String::from_utf8_lossy(&raw[..split]).to_string();
    let body_raw = &raw[split + 4..];
    let mut lines = header_text.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| format!("malformed HTTP status line {status_line:?}"))?;

    let mut chunked = false;
    let mut content_length: Option<usize> = None;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let k = k.trim().to_ascii_lowercase();
        let v = v.trim();
        if k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
            chunked = true;
        }
        if k == "content-length" {
            content_length = v.parse().ok();
        }
    }

    let body = if chunked {
        decode_chunked(body_raw)?
    } else if let Some(n) = content_length {
        if body_raw.len() < n {
            return Err(format!(
                "truncated HTTP body: got {} of {n} bytes",
                body_raw.len()
            ));
        }
        body_raw[..n].to_vec()
    } else {
        body_raw.to_vec()
    };
    Ok(Response { status, body })
}

fn decode_chunked(mut raw: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let Some(eol) = raw.windows(2).position(|w| w == b"\r\n") else {
            return Err("truncated chunked body".into());
        };
        let size_line = std::str::from_utf8(&raw[..eol]).map_err(|_| "bad chunk size")?;
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| format!("bad chunk size {size_hex:?}"))?;
        raw = &raw[eol + 2..];
        if size == 0 {
            return Ok(out);
        }
        if raw.len() < size + 2 {
            return Err("truncated chunk".into());
        }
        if out.len() + size > 64 * 1024 * 1024 {
            return Err("chunked body exceeds 64 MiB".into());
        }
        out.extend_from_slice(&raw[..size]);
        raw = &raw[size + 2..];
    }
}

pub fn get_json_with_token(
    url: &str,
    token: Option<&str>,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    let r = request_with_token("GET", url, None, token, timeout)?;
    if r.status != 200 {
        return Err(format!("GET {url} → HTTP {}", r.status));
    }
    serde_json::from_slice(&r.body).map_err(|e| format!("GET {url}: invalid JSON: {e}"))
}

pub fn post_json_with_token(
    url: &str,
    value: &serde_json::Value,
    token: Option<&str>,
    timeout: Duration,
) -> Result<Response, String> {
    let body = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    request_with_token("POST", url, Some(&body), token, timeout)
}

/// Fetch a package over the LAN into `path`. Returns the byte count.
pub fn download(url: &str, path: &std::path::Path, timeout: Duration) -> Result<u64, String> {
    let r = request("GET", url, None, timeout)?;
    if r.status != 200 {
        return Err(format!("GET {url} → HTTP {}", r.status));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let mut f =
        std::fs::File::create(path).map_err(|e| format!("create {}: {e}", path.display()))?;
    f.write_all(&r.body)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    f.sync_all()
        .map_err(|e| format!("sync {}: {e}", path.display()))?;
    Ok(r.body.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_parse_with_and_without_port() {
        let a = parse_url("http://192.168.1.27:8000/pkg.tar").unwrap();
        assert_eq!(
            (a.host.as_str(), a.port, a.path.as_str()),
            ("192.168.1.27", 8000, "/pkg.tar")
        );
        let b = parse_url("http://127.0.0.1/api/v0/status").unwrap();
        assert_eq!(
            (b.host.as_str(), b.port, b.path.as_str()),
            ("127.0.0.1", 80, "/api/v0/status")
        );
        assert!(parse_url("https://example.com/x").is_err());
    }

    #[test]
    fn chunk_decoding_handles_terminator_and_truncation() {
        let decoded = decode_chunked(b"4\r\nwiki\r\n5\r\npedia\r\n0\r\n\r\n").unwrap();
        assert_eq!(decoded, b"wikipedia");
        assert!(decode_chunked(b"5\r\nab").is_err());
    }

    #[test]
    fn request_with_token_sends_bearer_and_rejects_truncated_content_length() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut s1, _) = listener.accept().unwrap();
            let mut buf = [0u8; 2048];
            let n = s1.read(&mut buf).unwrap();
            let req1 = String::from_utf8_lossy(&buf[..n]).to_string();
            s1.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\n{\"ok\":1}\n")
                .unwrap();
            drop(s1);

            let (mut s2, _) = listener.accept().unwrap();
            let _ = s2.read(&mut buf).unwrap();
            s2.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 50\r\n\r\nshort")
                .unwrap();
            drop(s2);
            req1
        });
        let url = format!("http://{addr}/api/v0/status");
        let ok = get_json_with_token(&url, Some("hmr_test_secret_token"), Duration::from_secs(2))
            .unwrap();
        assert_eq!(ok["ok"], 1);
        let err = request("GET", &url, None, Duration::from_secs(2))
            .err()
            .expect("truncated Content-Length must fail");
        assert!(err.contains("truncated HTTP body"), "{err}");
        let seen_req = handle.join().unwrap();
        assert!(
            seen_req.contains("Authorization: Bearer hmr_test_secret_token\r\n"),
            "missing bearer header in {seen_req:?}"
        );
    }
}
