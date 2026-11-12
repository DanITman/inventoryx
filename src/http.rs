// InventoryX — a minimal HTTP/1.1 server over a Unix socket or localhost TCP.
// One request per connection; the Unraid web server (or a browser in development) is
// the only client.
// GPL-3.0-or-later.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Arc;

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn q(&self, key: &str) -> &str { self.query.get(key).map(String::as_str).unwrap_or("") }
    pub fn q_u32(&self, key: &str) -> Option<u32> { self.q(key).parse().ok() }
    pub fn q_flag(&self, key: &str) -> bool { matches!(self.q(key), "1" | "true") }
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(v: serde_json::Value) -> Self {
        Response { status: 200, content_type: "application/json", body: v.to_string().into_bytes() }
    }
    pub fn error(status: u16, message: &str) -> Self {
        Response { status, content_type: "application/json",
                   body: serde_json::json!({ "error": message }).to_string().into_bytes() }
    }
}

pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => { out.push(b' '); i += 1; }
            b'%' => {
                let hex = b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok())
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(v) => { out.push(v); i += 3; }
                    None => { out.push(b'%'); i += 1; }
                }
            }
            c => { out.push(c); i += 1; }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Splits a request target into path and decoded query parameters.
pub fn parse_target(target: &str) -> (String, HashMap<String, String>) {
    let (path, qs) = target.split_once('?').unwrap_or((target, ""));
    let mut query = HashMap::new();
    for pair in qs.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        query.insert(percent_decode(k), percent_decode(v));
    }
    (percent_decode(&path.replace('+', "%2B")), query)
}

/// Whether a Host header names this machine. A page on another site that points its own
/// name at 127.0.0.1 (DNS rebinding) sends its own name here and is refused.
pub fn local_host(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => host.split(':').next().unwrap_or(""),
    };
    matches!(name, "localhost" | "127.0.0.1" | "::1")
}

fn host_of(head: &str) -> &str {
    head.split("\r\n").skip(1)
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.eq_ignore_ascii_case("host"))
        .map(|(_, v)| v.trim()).unwrap_or("")
}

/// Parses the request line and headers. Returns method, target and Content-Length.
pub fn parse_head(head: &str) -> Option<(String, String, usize)> {
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let target = first.next()?.to_string();
    if !first.next()?.starts_with("HTTP/") { return None; }
    let mut len = 0;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") { len = v.trim().parse().ok()?; }
        }
    }
    Some((method, target, len))
}

const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 1024 * 1024;

#[cfg(test)]
fn read_request(stream: &mut impl Read) -> Option<Request> { read_request_checked(stream, false) }

fn read_request_checked(stream: &mut impl Read, check_host: bool) -> Option<Request> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(p) = memchr::memmem::find(&buf, b"\r\n\r\n") { break p; }
        if buf.len() > MAX_HEAD { return None; }
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 { return None; }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&buf[..head_end]).ok()?;
    if check_host && !local_host(host_of(head)) { return None; }
    let (method, target, len) = parse_head(head)?;
    if len > MAX_BODY { return None; }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 { return None; }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    let (path, query) = parse_target(&target);
    Some(Request { method, path, query, body })
}

fn handle<S: Read + Write>(mut stream: S, check_host: bool, handler: &(dyn Fn(Request) -> Response + Send + Sync)) {
    let resp = match read_request_checked(&mut stream, check_host) {
        Some(req) => handler(req),
        None => Response::error(400, "Bad request"),
    };
    let reason = match resp.status { 200 => "OK", 400 => "Bad Request", 404 => "Not Found",
                                     405 => "Method Not Allowed", 409 => "Conflict", _ => "Error" };
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        resp.status, reason, resp.content_type, resp.body.len());
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&resp.body);
    let _ = stream.flush();
}

pub enum Listen {
    Unix(String),
    Tcp(String),
}

pub fn serve(listen: Listen, handler: Arc<dyn Fn(Request) -> Response + Send + Sync>) -> std::io::Result<()> {
    fn spawn<S: Read + Write + Send + 'static>(stream: S, check_host: bool,
                                               handler: Arc<dyn Fn(Request) -> Response + Send + Sync>) {
        // Layout recurses once per folder level; give it room.
        let _ = std::thread::Builder::new().stack_size(32 * 1024 * 1024)
            .spawn(move || handle(stream, check_host, handler.as_ref()));
    }
    match listen {
        Listen::Unix(path) => {
            let l = std::os::unix::net::UnixListener::bind(&path)?;
            // Only root (the Unraid web server) may connect.
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            for s in l.incoming().flatten() { spawn(s, false, handler.clone()); }
        }
        Listen::Tcp(addr) => {
            // Development only, and only for this machine.
            let local = addr.parse::<std::net::SocketAddr>().map(|a| a.ip().is_loopback()).unwrap_or(false);
            if !local {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput,
                                               "--listen only accepts a loopback address such as 127.0.0.1:7878"));
            }
            let l = std::net::TcpListener::bind(&addr)?;
            for s in l.incoming().flatten() { spawn(s, true, handler.clone()); }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_target_and_head() {
        let (path, q) = parse_target("/api/search?q=hello%20w%C3%B6rld+x&physical=1&empty");
        assert_eq!(path, "/api/search");
        assert_eq!(q["q"], "hello wörld x");
        assert_eq!(q["physical"], "1");
        assert_eq!(q["empty"], "");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        let (m, t, len) = parse_head("POST /api/scan HTTP/1.1\r\nHost: x\r\ncontent-LENGTH: 12").unwrap();
        assert_eq!((m.as_str(), t.as_str(), len), ("POST", "/api/scan", 12));
        assert!(parse_head("garbage").is_none());
        assert!(local_host("localhost:7878") && local_host("127.0.0.1") && local_host("[::1]:80"));
        assert!(!local_host("evil.example:7878") && !local_host("") && !local_host("127.0.0.1.evil.example"));
    }

    #[test]
    fn reads_body_split_across_reads() {
        let raw = b"POST /api/scan?x=1 HTTP/1.1\r\nContent-Length: 5\r\n\r\nhelloEXTRA";
        let req = read_request(&mut &raw[..]).unwrap();
        assert_eq!(req.body, b"hello");
        assert_eq!(req.q("x"), "1");
        assert!(read_request(&mut &b"GET / HTTP/1.1\r\n"[..]).is_none());
        let evil = b"GET /api/status HTTP/1.1\r\nHost: evil.example\r\n\r\n";
        assert!(read_request_checked(&mut &evil[..], true).is_none());
        let ok = b"GET /api/status HTTP/1.1\r\nhost: localhost:7878\r\n\r\n";
        assert!(read_request_checked(&mut &ok[..], true).is_some());
    }
}
