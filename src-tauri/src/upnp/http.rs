//! A minimal HTTP/1.1 client over a plain TCP socket.
//!
//! Deliberately not an HTTP crate: LAN peers must be reached directly, and the
//! clients most people run (Clash, a corporate proxy) leak into the request
//! through `HTTP_PROXY`, which makes a renderer on `192.168.x.x` unreachable.
//! A socket cannot be proxied by accident, and both directions here are small,
//! single-shot, machine-generated messages.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Connect, request-read and total-response bounds. A device that is present in
/// SSDP but asleep must not stall a cast, so every call is timeboxed.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);
const IO_TIMEOUT: Duration = Duration::from_millis(4000);

#[derive(Debug)]
pub enum HttpError {
    Io(String),
    /// The peer answered, but not with something this client can use.
    Protocol(String),
    Status(u16, String),
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(m) => write!(f, "network: {m}"),
            Self::Protocol(m) => write!(f, "malformed response: {m}"),
            Self::Status(code, body) => write!(f, "HTTP {code}: {}", body.trim()),
        }
    }
}

impl std::error::Error for HttpError {}

impl From<std::io::Error> for HttpError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k == &name)
            .map(|(_, v)| v.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Issue a request and read the whole response.
///
/// `content_type` is sent only when the body is non-empty; some DMRs reject a
/// POST that announces no type, and GETs must not carry one.
pub fn request(
    addr: impl ToSocketAddrs,
    method: &str,
    path_and_query: &str,
    host_header: &str,
    content_type: Option<&str>,
    extra_headers: &str,
    body: &[u8],
) -> Result<Response, HttpError> {
    let sock = connect(addr)?;
    send_and_read(
        sock,
        method,
        path_and_query,
        host_header,
        content_type,
        extra_headers,
        body,
    )
}

/// The SOAPAction a UPnP service dispatches on; some firmwares refuse the
/// POST outright without it.
pub fn soap_action_header(service_type: &str, action: &str) -> String {
    format!("SOAPAction: \"{service_type}#{action}\"\r\n")
}

fn connect(addr: impl ToSocketAddrs) -> Result<TcpStream, HttpError> {
    let addrs: Vec<_> = addr
        .to_socket_addrs()
        .map_err(|e| HttpError::Io(e.to_string()))?
        .collect();
    if addrs.is_empty() {
        return Err(HttpError::Io("address resolved to nothing".into()));
    }
    // A name can resolve to several records (IPv6 first) and only one may be
    // reachable on a LAN without a v6 route, so each gets its own attempt.
    let mut last = HttpError::Io("no address to connect to".into());
    for sa in &addrs {
        match TcpStream::connect_timeout(sa, CONNECT_TIMEOUT) {
            Ok(sock) => {
                sock.set_read_timeout(Some(IO_TIMEOUT))?;
                sock.set_write_timeout(Some(IO_TIMEOUT))?;
                return Ok(sock);
            }
            Err(e) => last = HttpError::Io(format!("{sa}: {e}")),
        }
    }
    Err(last)
}

fn send_and_read(
    mut sock: TcpStream,
    method: &str,
    path: &str,
    host: &str,
    content_type: Option<&str>,
    extra_headers: &str,
    body: &[u8],
) -> Result<Response, HttpError> {
    let mut head = format!("{} {} HTTP/1.1\r\nHost: {}\r\n", method, path, host);
    if !body.is_empty() {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
        if let Some(ct) = content_type {
            head.push_str(&format!("Content-Type: {}\r\n", ct));
        }
    }
    head.push_str(extra_headers);
    // UPnP devices keep one connection per request; saying close means a peer
    // that ignores keep-alive does not leave us waiting on a dead socket.
    head.push_str("Connection: close\r\nAccept: */*\r\n\r\n");

    sock.write_all(head.as_bytes())?;
    if !body.is_empty() {
        sock.write_all(body)?;
    }
    sock.flush()?;

    let mut raw = Vec::with_capacity(8 * 1024);
    sock.read_to_end(&mut raw)?;

    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| HttpError::Protocol("no header terminator".into()))?;
    // The terminator belongs to the headers; leaving it in `rest` would hand
    // every caller a body that starts with a blank line.
    let (head_bytes, rest) = raw.split_at(split + 4);
    let text = String::from_utf8_lossy(head_bytes).into_owned();
    let mut lines = text.lines();
    let status_line = lines
        .next()
        .ok_or_else(|| HttpError::Protocol("empty response".into()))?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| HttpError::Protocol(format!("bad status line: {status_line}")))?;

    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }

    let body = read_body(rest, &headers)?;
    if !(200..300).contains(&status) {
        return Err(HttpError::Status(
            status,
            String::from_utf8_lossy(&body).into_owned(),
        ));
    }
    Ok(Response {
        status,
        headers,
        body,
    })
}

/// `read_to_end` already drained the socket, so `rest` is the entire payload;
/// the framing headers are only consulted to explain a truncated body.
fn read_body(rest: &[u8], headers: &[(String, String)]) -> Result<Vec<u8>, HttpError> {
    let value = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };
    if let Some(enc) = value("transfer-encoding") {
        if enc.to_ascii_lowercase().contains("chunked") {
            return decode_chunked(rest);
        }
    }
    if let Some(len) = value("content-length") {
        if let Ok(want) = len.trim().parse::<usize>() {
            if rest.len() < want {
                return Err(HttpError::Protocol(format!(
                    "body truncated: {want} bytes promised, {} read",
                    rest.len()
                )));
            }
            // A device that ignores `Connection: close` may already have sent
            // the next response; the announced length is the body, nothing more.
            return Ok(rest[..want].to_vec());
        }
    }
    Ok(rest.to_vec())
}

/// Strip chunk framing; devices that chunk a SOAP reply are common enough
/// (some firmware only serves chunked) that a naive read yields binary noise.
fn decode_chunked(data: &[u8]) -> Result<Vec<u8>, HttpError> {
    let mut out = Vec::new();
    let mut pos = 0;
    loop {
        let Some(rel) = find_line_end(&data[pos..]) else {
            return Err(HttpError::Protocol("chunk size line missing".into()));
        };
        let line = String::from_utf8_lossy(&data[pos..pos + rel]).into_owned();
        pos += rel + 2;
        let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| HttpError::Protocol(format!("bad chunk size: {line:?}")))?;
        if size == 0 {
            break;
        }
        let end = pos + size;
        if end > data.len() {
            return Err(HttpError::Protocol("chunk body truncated".into()));
        }
        out.extend_from_slice(&data[pos..end]);
        pos = end;
        // Skip the CRLF that follows each chunk.
        if data.get(pos..pos + 2) == Some(b"\r\n") {
            pos += 2;
        }
    }
    Ok(out)
}

fn find_line_end(data: &[u8]) -> Option<usize> {
    data.windows(2).position(|w| w == b"\r\n")
}

/// Split a `http://host:port/path` URL into the parts a raw request needs.
pub fn split_url(url: &str) -> Result<(String, u16, String), HttpError> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| HttpError::Protocol(format!("not an http URL: {url}")))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().map_err(|_| HttpError::Protocol(format!("bad port in {url}")))?),
        None => (authority, 80),
    };
    Ok((host.to_string(), port, path.to_string()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    /// Read exactly one HTTP request, the way a device does: headers up to the
    /// blank line, then the body the request announces.
    ///
    /// Test peers that use `read_to_end` instead hang: this client sends
    /// `Connection: close` but never half-closes, so the peer waits for an EOF
    /// that only arrives when our own read timeout fires.
    pub(crate) fn read_request(sock: &mut TcpStream) -> String {
        let mut raw = Vec::new();
        let mut byte = [0u8; 1];
        let mut header_end = None;
        while header_end.is_none() {
            match sock.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => raw.push(byte[0]),
            }
            if raw.ends_with(b"\r\n\r\n") {
                header_end = Some(raw.len());
            }
        }
        let Some(end) = header_end else {
            return String::from_utf8_lossy(&raw).into_owned();
        };
        let lower = String::from_utf8_lossy(&raw[..end]).to_ascii_lowercase();
        let announced = lower
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; announced];
        let mut got = 0;
        while got < announced {
            match sock.read(&mut body[got..]) {
                Ok(0) | Err(_) => break,
                Ok(n) => got += n,
            }
        }
        raw.extend_from_slice(&body[..got]);
        String::from_utf8_lossy(&raw).into_owned()
    }

    /// A peer that answers with exactly the bytes a given device would, so the
    /// framing paths can be tested without a real renderer.
    fn serve_once(reply: &'static str) -> TcpListener {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let accepted = listener.try_clone().unwrap();
        std::thread::spawn(move || {
            let (mut sock, _) = accepted.accept().unwrap();
            // Take the request before answering, or the write races a client
            // that has not finished sending.
            let _ = read_request(&mut sock);
            let _ = sock.write_all(reply.as_bytes());
            let _ = sock.shutdown(std::net::Shutdown::Both);
        });
        listener
    }

    fn addr(listener: &TcpListener) -> std::net::SocketAddr {
        listener.local_addr().unwrap()
    }

    #[test]
    fn reads_a_content_length_body() {
        let l = serve_once("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello");
        let r = request(addr(&l), "GET", "/", "x", None, "", &[]).unwrap();
        assert_eq!((r.status, r.text().as_str()), (200, "hello"));
        // The blank line that ends the headers is framing, not body.
        assert!(!r.body.starts_with(b"\r\n"), "{:?}", r.body);
        assert_eq!(r.header("Content-Length"), Some("5"));
        assert_eq!(r.header("content-length"), Some("5"));
        assert_eq!(r.header("missing"), None);
    }

    #[test]
    fn reads_a_chunked_body() {
        let l = serve_once(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
        );
        let r = request(addr(&l), "GET", "/", "x", None, "", &[]).unwrap();
        assert_eq!(r.text(), "hello world");
    }

    #[test]
    fn surfaces_error_status_with_the_body() {
        let l = serve_once("HTTP/1.1 500 Internal Server Error\r\nContent-Length: 4\r\n\r\noops");
        let e = request(addr(&l), "POST", "/", "x", Some("text/xml"), "", b"body").unwrap_err();
        match e {
            HttpError::Status(500, body) => assert_eq!(body, "oops"),
            other => panic!("expected a status error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_a_truncated_body_rather_than_returning_it() {
        let l = serve_once("HTTP/1.1 200 OK\r\nContent-Length: 900\r\n\r\nshort");
        let e = request(addr(&l), "GET", "/", "x", None, "", &[]).unwrap_err();
        assert!(
            matches!(e, HttpError::Protocol(ref m) if m.contains("truncated")),
            "{e:?}"
        );
    }

    #[test]
    fn splits_urls_with_and_without_a_port() {
        assert_eq!(
            split_url("http://192.168.1.9:8290/dd.xml").unwrap(),
            ("192.168.1.9".into(), 8290, "/dd.xml".into())
        );
        assert_eq!(
            split_url("http://10.0.0.5/desc.xml").unwrap(),
            ("10.0.0.5".into(), 80, "/desc.xml".into())
        );
        assert_eq!(split_url("http://10.0.0.5").unwrap().2, "/");
        assert!(split_url("https://10.0.0.5/x").is_err());
    }

    #[test]
    fn sends_the_body_and_the_announced_content_type() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let cap = std::sync::Arc::clone(&captured);
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            *cap.lock().unwrap() = read_request(&mut sock);
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
        });
        let r = request(
            ("127.0.0.1", port),
            "POST",
            "/ctl/AVTransport",
            &format!("127.0.0.1:{port}"),
            Some("text/xml; charset=\"utf-8\""),
            "",
            b"<Envelope/>",
        )
        .unwrap();
        assert_eq!(r.text(), "ok");
        let sent = captured.lock().unwrap().clone();
        assert!(sent.contains("POST /ctl/AVTransport HTTP/1.1"), "{sent}");
        assert!(sent.contains("Content-Length: 11"), "{sent}");
        assert!(sent.contains("Content-Type: text/xml"), "{sent}");
        assert!(sent.ends_with("<Envelope/>"), "{sent}");
    }
}
