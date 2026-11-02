//! A single-purpose HTTP file server: the reason a renderer can play a file
//! that lives on this machine.
//!
//! A DMR fetches the URI we hand it with plain HTTP, and video players issue
//! byte-range requests to seek before they have read the end of the file. A
//! server that ignores `Range` makes the device download the whole thing and,
//! for a large file, the device gives up. So range handling is not an
//! optimisation here, it is the entry requirement.

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tracing::{debug, warn};

/// How much of a ranged read to serve at once. A renderer pulling an MP4 at
/// 20 Mbps needs ~2.5 MB/s; 256 KB per socket read keeps that comfortably.
const COPY_CHUNK: usize = 256 * 1024;

/// Files offered for casting, addressed by an opaque id.
///
/// Ids rather than paths in the URL: a path in a query string invites traversal
/// and needs escaping, whereas a token we minted can only name something the
/// user already chose.
#[derive(Debug, Clone)]
pub struct MediaFile {
    pub id: String,
    pub path: PathBuf,
    pub title: String,
    pub mime: String,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct ServeError {
    pub message: String,
}

/// Serves `GET`/`HEAD` for registered files on a loopback-plus-LAN port.
pub struct MediaServer {
    listener: TcpListener,
    port: u16,
    files: Arc<std::sync::Mutex<Vec<MediaFile>>>,
    running: Arc<AtomicBool>,
    /// Bytes handed out, so the UI can show whether the device is really
    /// pulling the stream rather than having refused it.
    bytes_served: Arc<AtomicU64>,
    requests: Arc<AtomicU64>,
}

impl MediaServer {
    /// Bind an ephemeral port and start accepting.
    ///
    /// Bound to `0.0.0.0` because the renderer is a different machine; a
    /// loopback bind would advertise a URL nothing off this host can reach.
    pub fn start() -> Result<Self, ServeError> {
        let listener = TcpListener::bind("0.0.0.0:0").map_err(|e| ServeError {
            message: format!("media server could not bind: {e}"),
        })?;
        let port = listener.local_addr().map_err(|e| ServeError {
            message: format!("media server has no address: {e}"),
        })?;
        listener
            .set_nonblocking(true)
            .map_err(|e| ServeError {
                message: format!("media server socket: {e}"),
            })?;
        Ok(Self {
            listener,
            port: port.port(),
            files: Arc::new(std::sync::Mutex::new(Vec::new())),
            running: Arc::new(AtomicBool::new(true)),
            bytes_served: Arc::new(AtomicU64::new(0)),
            requests: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Register a file and return what is needed to cast it: the id, the size,
    /// the MIME and a `url_for(host, port)` away from a usable URL.
    pub fn offer(&self, path: &Path) -> Result<MediaFile, ServeError> {
        let meta = std::fs::metadata(path).map_err(|e| ServeError {
            message: format!("{}: {e}", path.display()),
        })?;
        if !meta.is_file() {
            return Err(ServeError {
                message: format!("{} is not a file", path.display()),
            });
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "media".to_string());
        let file = MediaFile {
            id: format!("{:08x}", fnv_hash(&path.to_string_lossy()) ^ meta.len() as u32),
            path: path.to_path_buf(),
            title: name.clone(),
            mime: mime_from_extension(&name),
            size: meta.len(),
        };
        let mut files = self.files.lock().unwrap();
        if !files.iter().any(|f| f.id == file.id) {
            files.push(file.clone());
        }
        debug!("offering {} ({} bytes) as /m/{}", name, file.size, file.id);
        Ok(file)
    }

    /// Spawn the accept loop. Each connection is handled on its own thread
    /// because a renderer keeps a connection open for the whole film.
    pub fn serve(&self) {
        let files = Arc::clone(&self.files);
        let running = Arc::clone(&self.running);
        let bytes = Arc::clone(&self.bytes_served);
        let requests = Arc::clone(&self.requests);
        let listener = match self.listener.try_clone() {
            Ok(l) => l,
            Err(e) => {
                warn!("media server could not clone the listener: {e}");
                return;
            }
        };
        std::thread::spawn(move || {
            while running.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((sock, addr)) => {
                        let (files, bytes, requests) =
                            (Arc::clone(&files), Arc::clone(&bytes), Arc::clone(&requests));
                        std::thread::spawn(move || {
                            if let Err(e) = handle(sock, &files, &bytes, &requests) {
                                debug!("media connection with {addr} ended: {e}");
                            }
                        });
                    }
                    // The listener is non-blocking: no pending connection is
                    // the normal case, not an error.
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    Err(e) => {
                        if running.load(Ordering::SeqCst) {
                            warn!("media server accept failed: {e}");
                        }
                        return;
                    }
                }
            }
        });
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    /// Total bytes written to renderers since start.
    pub fn bytes_served(&self) -> u64 {
        self.bytes_served.load(Ordering::SeqCst)
    }

    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::SeqCst)
    }

    pub fn files(&self) -> Vec<MediaFile> {
        self.files.lock().unwrap().clone()
    }
}

impl MediaFile {
    /// The castable URL for this file.
    ///
    /// The id carries the address, and the original extension is appended to
    /// it: a receiver that only has the URL to go on (`/m/45bd36a3` says
    /// nothing) picks its player from that extension, and a photo handed over
    /// as a nameless stream is a black screen with a working progress bar.
    pub fn url_for(&self, host: &str, port: u16) -> Result<String, ServeError> {
        if host.is_empty() {
            return Err(ServeError {
                message: "no LAN address to advertise for the media server".into(),
            });
        }
        Ok(format!(
            "http://{host}:{port}/m/{}{}",
            self.id,
            suffix_of(&self.path)
        ))
    }
}

/// The extension part of a cast URL, kept to what a URL can carry.
///
/// A device-named extension is decoration next to the id, so anything odd
/// (a space, a dot-dot, twenty characters) is dropped rather than encoded; the
/// server ignores it on the way back in anyway.
fn suffix_of(path: &Path) -> String {
    let raw = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let clean: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect();
    if clean.is_empty() {
        String::new()
    } else {
        format!(".{clean}")
    }
}

/// Handle one connection: read the request line, then serve or refuse.
pub fn handle(
    mut sock: TcpStream,
    files: &std::sync::Mutex<Vec<MediaFile>>,
    bytes: &AtomicU64,
    requests: &AtomicU64,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(sock.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    if request_line.is_empty() {
        return Ok(());
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_ascii_uppercase();
    let target = parts.next().unwrap_or("/").to_string();

    // Drain the headers: a renderer's GET carries Range here, and the socket
    // must not be left with unread bytes for whatever the device sends next.
    let mut range_header: Option<String> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
        let (name, value) = match line.split_once(':') {
            Some(p) => p,
            None => continue,
        };
        if name.trim().eq_ignore_ascii_case("range") {
            range_header = Some(value.trim().to_string());
        }
    }

    requests.fetch_add(1, Ordering::SeqCst);
    if method != "GET" && method != "HEAD" {
        return respond(&mut sock, "405 Method Not Allowed", &[], b"");
    }

    let Some(id) = target.strip_prefix("/m/") else {
        // A trailing query is legal in a URL and some devices append one;
        // the id is whatever precedes it.
        return respond(&mut sock, "404 Not Found", &[], b"");
    };
    let id = id.split(['?', '#']).next().unwrap_or(id);
    // The advertised URL ends in the file's extension so that a receiver can
    // choose a player from the address alone; ids are hex and hold no dot, so
    // the part before the first one is the whole id.
    let id = id.split('.').next().unwrap_or(id);
    let file = {
        let guard = files.lock().unwrap();
        guard.iter().find(|f| f.id == id).cloned()
    };
    let Some(file) = file else {
        debug!("media request for unknown id {id}");
        return respond(&mut sock, "404 Not Found", &[], b"");
    };

    let range = parse_range(range_header.as_deref(), file.size);
    match range {
        RangeRequest::Unsatisfiable => {
            // RFC 7233: tell the player the real length or it will retry the
            // same impossible range forever.
            respond(
                &mut sock,
                "416 Range Not Satisfiable",
                &[format!("Content-Range: bytes */{}", file.size)],
                b"",
            )
        }
        RangeRequest::Whole | RangeRequest::Part(_) => {
            let span = match range {
                RangeRequest::Part(s) => Some(s),
                _ => None,
            };
            stream_file(&mut sock, &file, span, bytes, method == "HEAD")
        }
    }
}

/// A byte range to serve.
///
/// `end` is exclusive, so an empty file has a legal full span (`0..0`) and
/// `len` cannot underflow; HTTP's inclusive end appears only where the
/// `Content-Range` header is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: u64,
    pub end: u64,
}

impl Span {
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }
    pub fn is_full(size: u64) -> Self {
        Self { start: 0, end: size }
    }
}

/// What a `Range` header asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeRequest {
    /// No range present: serve the whole file.
    Whole,
    Part(Span),
    /// A range present but impossible for this size, which is a 416, not a 200.
    Unsatisfiable,
}

/// Interpret a `Range` header against a known size.
///
/// Only the first range is honoured and open-ended tails are clamped: that is
/// what every media player asks for, and a multi-range reply would have to
/// carry a MIME boundary nobody parses.
pub fn parse_range(header: Option<&str>, size: u64) -> RangeRequest {
    let Some(spec) = header.and_then(|h| h.strip_prefix("bytes=")) else {
        return RangeRequest::Whole;
    };
    if size == 0 {
        return RangeRequest::Unsatisfiable;
    }
    let first = spec.split(',').next().unwrap_or("").trim();
    let Some((raw_start, raw_end)) = first.split_once('-') else {
        return RangeRequest::Unsatisfiable;
    };
    if raw_start.is_empty() {
        // A suffix range: the last N bytes.
        let Ok(n) = raw_end.trim().parse::<u64>() else {
            return RangeRequest::Unsatisfiable;
        };
        if n == 0 {
            return RangeRequest::Unsatisfiable;
        }
        return RangeRequest::Part(Span {
            start: size - n.min(size),
            end: size,
        });
    }
    let Ok(start) = raw_start.trim().parse::<u64>() else {
        return RangeRequest::Unsatisfiable;
    };
    if start >= size {
        return RangeRequest::Unsatisfiable;
    }
    // `end` is exclusive: a closed range names its last byte, so the byte
    // after it is the end of what we serve.
    let end = if raw_end.trim().is_empty() {
        size
    } else {
        match raw_end.trim().parse::<u64>() {
            Ok(e) => {
                if e >= size {
                    size
                } else {
                    e + 1
                }
            }
            Err(_) => return RangeRequest::Unsatisfiable,
        }
    };
    if end <= start {
        return RangeRequest::Unsatisfiable;
    }
    RangeRequest::Part(Span { start, end })
}

fn stream_file(
    sock: &mut TcpStream,
    file: &MediaFile,
    range: Option<Span>,
    bytes: &AtomicU64,
    head_only: bool,
) -> std::io::Result<()> {
    let mut handle = std::fs::File::open(&file.path)?;
    let actual_size = handle.metadata()?.len();
    // A file that changed since it was offered must not be served with stale
    // numbers: the device would read past what we promised.
    if actual_size != file.size {
        return respond(
            sock,
            "409 Conflict",
            &[],
            b"file changed since it was offered",
        );
    }

    let span = range.unwrap_or_else(|| Span::is_full(file.size));
    let partial = range.is_some();
    let status = if partial {
        "206 Partial Content"
    } else {
        "200 OK"
    };
    let mut extra = vec![
        format!("Content-Type: {}", file.mime),
        "Accept-Ranges: bytes".to_string(),
        format!("Content-Length: {}", span.len()),
    ];
    if partial {
        extra.push(format!(
            "Content-Range: bytes {}-{}/{}",
            span.start,
            span.end - 1,
            file.size
        ));
    }

    if head_only {
        // HEAD carries the same headers and no body; a player that only wants
        // the length still gets a truthful answer.
        return respond(sock, status, &extra, b"");
    }

    let head = format!("HTTP/1.1 {status}\r\n{}\r\n\r\n", extra.join("\r\n"));
    sock.write_all(head.as_bytes())?;
    bytes.fetch_add(head.len() as u64, Ordering::SeqCst);

    handle.seek(SeekFrom::Start(span.start))?;
    let mut remaining = span.len();
    let mut buf = vec![0u8; COPY_CHUNK];
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        match handle.read(&mut buf[..want]) {
            Ok(0) => break,
            Ok(n) => {
                sock.write_all(&buf[..n])?;
                bytes.fetch_add(n as u64, Ordering::SeqCst);
                remaining -= n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    sock.flush()?;
    Ok(())
}

fn respond(
    sock: &mut TcpStream,
    status: &str,
    extra: &[String],
    body: &[u8],

) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n", body.len());
    for line in extra {
        head.push_str(line);
        head.push_str("\r\n");
    }
    head.push_str("Connection: close\r\n\r\n");
    sock.write_all(head.as_bytes())?;
    sock.write_all(body)?;
    sock.flush()
}

/// MIME by extension, with the aliases renderers actually ask for.
///
/// Deliberately a table rather than a crate: the set a DMR will play is small,
/// and a wrong guess here (`.m4v` as `video/mp4`) is what makes a TV show a
/// black screen with working audio.
pub fn mime_from_extension(name_or_url: &str) -> String {
    let tail = name_or_url
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(name_or_url)
        .split(['?', '#'])
        .next()
        .unwrap_or("");
    let ext = match tail.rfind('.') {
        Some(i) => tail[i + 1..].to_ascii_lowercase(),
        None => return "application/octet-stream".to_string(),
    };
    match ext.as_str() {
        "mp4" | "m4v" | "mov" => "video/mp4",
        "mp2ts" | "ts" | "m2ts" => "video/mp2ts",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "mpg" | "mpeg" => "video/mpeg",
        "webm" => "video/webm",
        "flv" => "video/x-flv",
        "wmv" => "video/x-ms-wmv",
        "mp3" => "audio/mpeg",
        "aac" | "m4a" => "audio/mp4",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "ogg" | "oga" => "audio/ogg",
        "wma" => "audio/x-ms-wma",
        "opus" => "audio/opus",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// A stable 32-bit hash, used for the file id.
fn fnv_hash(text: &str) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for b in text.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;
    use std::time::Duration;

    fn temp_file(name: &str, content: &[u8]) -> PathBuf {
        let mut path = std::env::temp_dir();
        // The pid keeps concurrent runs apart, but it has to come before the
        // extension: a suffix after `.mp4` makes the file unrecognisable.
        path.push(format!("aerosend-{}-{name}", std::process::id()));
        std::fs::write(&path, content).unwrap();
        path
    }

    /// Serve `files` and return a connected socket plus the raw response.
    fn request(path: &str, headers: &[&str], files: MutexVec) -> (Vec<u8>, Vec<u8>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let store = Arc::clone(&files.0);
        std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let bytes = Arc::new(AtomicU64::new(0));
            let requests = Arc::new(AtomicU64::new(0));
            let _ = handle(sock, &store, &bytes, &requests);
        });
        let mut sock = TcpStream::connect(("127.0.0.1", port)).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
        for h in headers {
            req.push_str(h);
            req.push_str("\r\n");
        }
        req.push_str("\r\n");
        sock.write_all(req.as_bytes()).unwrap();
        let mut raw = Vec::new();
        sock.read_to_end(&mut raw).unwrap();
        let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        (raw[..split].to_vec(), raw[split + 4..].to_vec())
    }

    /// The store a request is served from, shared with the handler thread.
    struct MutexVec(Arc<std::sync::Mutex<Vec<MediaFile>>>);

    fn store(files: Vec<MediaFile>) -> MutexVec {
        MutexVec(Arc::new(std::sync::Mutex::new(files)))
    }

    #[test]
    fn serves_a_whole_file_when_no_range_is_asked_for() {
        let path = temp_file("whole.mp4", b"0123456789abcdefghij");
        let file = MediaFile {
            id: "abc".into(),
            path: path.clone(),
            title: "whole.mp4".into(),
            mime: "video/mp4".into(),
            size: 20,
        };
        let (head, body) = request("/m/abc", &[], store(vec![file]));
        let head = String::from_utf8_lossy(&head).into_owned();
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
        assert!(head.contains("Content-Length: 20"), "{head}");
        assert!(head.contains("Accept-Ranges: bytes"), "{head}");
        assert!(head.contains("Content-Type: video/mp4"), "{head}");
        assert_eq!(body, b"0123456789abcdefghij");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn honours_an_open_ended_range() {
        let path = temp_file("tail.mp4", b"0123456789abcdefghij");
        let file = MediaFile {
            id: "t".into(),
            path: path.clone(),
            title: "tail.mp4".into(),
            mime: "video/mp4".into(),
            size: 20,
        };
        let (head, body) = request("/m/t", &["Range: bytes=10-"], store(vec![file]));
        let head = String::from_utf8_lossy(&head).into_owned();
        assert!(head.starts_with("HTTP/1.1 206 Partial Content"), "{head}");
        assert!(head.contains("Content-Range: bytes 10-19/20"), "{head}");
        assert!(head.contains("Content-Length: 10"), "{head}");
        assert_eq!(body, b"abcdefghij");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn honours_a_closed_range_which_is_how_a_player_reads_the_moov_atom() {
        let path = temp_file("head.mp4", b"0123456789abcdefghij");
        let file = MediaFile {
            id: "h".into(),
            path: path.clone(),
            title: "head.mp4".into(),
            mime: "video/mp4".into(),
            size: 20,
        };
        let (head, body) = request("/m/h", &["Range: bytes=0-4"], store(vec![file]));
        let head = String::from_utf8_lossy(&head).into_owned();
        assert!(head.contains("Content-Range: bytes 0-4/20"), "{head}");
        assert_eq!(body, b"01234");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn honours_a_suffix_range() {
        let path = temp_file("suffix.mp4", b"0123456789abcdefghij");
        let file = MediaFile {
            id: "s".into(),
            path: path.clone(),
            title: "s.mp4".into(),
            mime: "video/mp4".into(),
            size: 20,
        };
        let (_, body) = request("/m/s", &["Range: bytes=-5"], store(vec![file]));
        assert_eq!(body, b"fghij");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn refuses_an_out_of_range_request_with_the_sizes_it_knows() {
        let path = temp_file("oor.mp4", b"0123456789");
        let file = MediaFile {
            id: "o".into(),
            path: path.clone(),
            title: "o.mp4".into(),
            mime: "video/mp4".into(),
            size: 10,
        };
        // RFC 7233: unsatisfiable range gets 416 plus the real length.
        let (head, _) = request("/m/o", &["Range: bytes=99-120"], store(vec![file]));
        let head = String::from_utf8_lossy(&head).into_owned();
        assert!(head.starts_with("HTTP/1.1 416"), "{head}");
        assert!(head.contains("Content-Range: bytes */10"), "{head}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn unknown_ids_and_other_paths_are_404_not_a_hang() {
        let (head, _) = request("/m/nope", &[], store(vec![]));
        assert!(String::from_utf8_lossy(&head).starts_with("HTTP/1.1 404"));
        let (head, _) = request("/", &[], store(vec![]));
        assert!(String::from_utf8_lossy(&head).starts_with("HTTP/1.1 404"));
    }

    #[test]
    fn a_query_string_on_the_url_does_not_hide_the_file() {
        // Devices routinely append `?` cache-busters; the id is what precedes it.
        let path = temp_file("q.mp4", b"payload");
        let file = MediaFile {
            id: "qq".into(),
            path: path.clone(),
            title: "q.mp4".into(),
            mime: "video/mp4".into(),
            size: 7,
        };
        let (_, body) = request("/m/qq?x=1", &[], store(vec![file]));
        assert_eq!(body, b"payload");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_file_that_changed_since_it_was_offered_is_not_served_stale() {
        let path = temp_file("grew.mp4", b"ten_______");
        let file = MediaFile {
            id: "g".into(),
            path: path.clone(),
            title: "g.mp4".into(),
            mime: "video/mp4".into(),
            size: 3,
        };
        let (head, _) = request("/m/g", &[], store(vec![file]));
        assert!(
            String::from_utf8_lossy(&head).starts_with("HTTP/1.1 409"),
            "the advertised length no longer matches the file"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn range_parsing_covers_the_shapes_players_send() {
        assert_eq!(parse_range(None, 100), RangeRequest::Whole);
        // A header we cannot read as a range is not a range request; the file
        // goes out whole rather than the connection being refused.
        assert_eq!(parse_range(Some("items=0-1"), 100), RangeRequest::Whole);
        assert_eq!(
            parse_range(Some("bytes=0-49"), 100),
            RangeRequest::Part(Span { start: 0, end: 50 })
        );
        assert_eq!(
            parse_range(Some("bytes=50-"), 100),
            RangeRequest::Part(Span { start: 50, end: 100 })
        );
        assert_eq!(
            parse_range(Some("bytes=-20"), 100),
            RangeRequest::Part(Span { start: 80, end: 100 })
        );
        // Clamped to the real end, as the spec requires.
        assert_eq!(
            parse_range(Some("bytes=0-1000"), 100),
            RangeRequest::Part(Span { start: 0, end: 100 })
        );
        assert_eq!(
            parse_range(Some("bytes=2-5"), 10),
            RangeRequest::Part(Span { start: 2, end: 6 })
        );
        // The span that goes on the wire: `bytes=0-49` is fifty bytes, not 49.
        assert_eq!(Span { start: 0, end: 50 }.len(), 50);
        for bad in [
            "bytes=999-1000",
            "bytes=-0",
            "bytes=garbage",
            "bytes=abc-def",
            "bytes=60-50",
        ] {
            assert_eq!(
                parse_range(Some(bad), 100),
                RangeRequest::Unsatisfiable,
                "{bad} is not playable"
            );
        }
        // Nothing to slice out of an empty file, but a plain GET still works.
        assert_eq!(parse_range(Some("bytes=0-10"), 0), RangeRequest::Unsatisfiable);
        assert_eq!(parse_range(None, 0), RangeRequest::Whole);
        assert_eq!(Span { start: 0, end: 0 }.len(), 0);
        assert_eq!(Span::is_full(10).len(), 10);
        assert_eq!(Span::is_full(0).len(), 0);
    }

    #[test]
    fn maps_extensions_that_a_tv_needs_named_exactly() {
        assert_eq!(mime_from_extension("clip.MP4"), "video/mp4");
        assert_eq!(mime_from_extension("http://h/p/show.m3u8?k=1"), "application/octet-stream");
        assert_eq!(mime_from_extension("/x/y.mkv"), "video/x-matroska");
        assert_eq!(mime_from_extension("a.m4v"), "video/mp4");
        assert_eq!(mime_from_extension("track.flac"), "audio/flac");
        // The kinds the media page now offers beyond video: a wrong guess here
        // sends a photo to the TV's video player.
        assert_eq!(mime_from_extension("Holiday.JPEG"), "image/jpeg");
        assert_eq!(mime_from_extension("/x/y.png"), "image/png");
        assert_eq!(mime_from_extension("cover.webp"), "image/webp");
        assert_eq!(mime_from_extension("scan.bmp"), "image/bmp");
        assert_eq!(mime_from_extension("art.avif"), "image/avif");
        assert_eq!(mime_from_extension("voice.m4a"), "audio/mp4");
        assert_eq!(mime_from_extension("song.opus"), "audio/opus");
        assert_eq!(mime_from_extension("old.wma"), "audio/x-ms-wma");
        assert_eq!(mime_from_extension("noext"), "application/octet-stream");
        // A dot inside a directory name must not be read as an extension.
        assert_eq!(mime_from_extension("/v1.2/movie"), "application/octet-stream");
    }

    #[test]
    fn offers_a_real_file_and_advertises_a_lan_url() {
        let path = temp_file("offer.mp4", b"0123456789");
        let server = MediaServer::start().unwrap();
        let file = server.offer(&path).unwrap();
        assert_eq!(file.size, 10);
        assert_eq!(file.mime, "video/mp4");
        assert_eq!(
            file.title,
            format!("aerosend-{}-offer.mp4", std::process::id())
        );
        let url = file.url_for("192.168.0.42", server.port()).unwrap();
        assert!(url.starts_with("http://192.168.0.42:"), "{url}");
        assert_eq!(url, format!("http://192.168.0.42:{}/m/{}.mp4", server.port(), file.id));

        // Offering twice yields the same id, so a repeated cast does not pile
        // up duplicate registrations.
        let again = server.offer(&path).unwrap();
        assert_eq!(again.id, file.id);
        assert_eq!(server.files().len(), 1);

        // A missing file is refused before any URL is handed out.
        assert!(server.offer(Path::new("Z:/definitely/not/here.mp4")).is_err());
        assert!(file.url_for("", 1).is_err(), "an empty host cannot be advertised");
        server.stop();
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_photo_and_a_song_are_advertised_by_their_extension_and_still_served() {
        let server = MediaServer::start().unwrap();
        let photo_path = temp_file("Holiday.JPG", b"jpeg-bytes");
        let song_path = temp_file("encore.flac", b"flac-bytes");
        let photo = server.offer(&photo_path).unwrap();
        let song = server.offer(&song_path).unwrap();

        // Both the type we announce and the one spelled out in the address.
        assert_eq!(photo.mime, "image/jpeg");
        assert_eq!(song.mime, "audio/flac");
        assert_eq!(
            photo.url_for("192.168.0.42", server.port()).unwrap(),
            format!("http://192.168.0.42:{}/m/{}.jpg", server.port(), photo.id),
            "an uppercase extension is lowercased, not dropped"
        );
        assert_eq!(
            song.url_for("192.168.0.42", server.port()).unwrap(),
            format!("http://192.168.0.42:{}/m/{}.flac", server.port(), song.id)
        );

        // The suffixed address still resolves: the id is what precedes the
        // extension, and the Content-Type is the registered one.
        let (head, body) = request(&format!("/m/{}.jpg", photo.id), &[], store(server.files()));
        let head = String::from_utf8_lossy(&head).into_owned();
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
        assert!(head.contains("Content-Type: image/jpeg"), "{head}");
        assert_eq!(body, b"jpeg-bytes");

        server.stop();
        std::fs::remove_file(&photo_path).ok();
        std::fs::remove_file(&song_path).ok();
    }

    #[test]
    fn an_extension_that_cannot_live_in_a_url_is_cleaned_rather_than_encoded() {
        assert_eq!(suffix_of(Path::new("/x/clip.mp4")), ".mp4");
        assert_eq!(suffix_of(Path::new("/x/noext")), "");
        // Spaces and punctuation cannot appear bare in a path segment.
        assert_eq!(suffix_of(Path::new("/x/tour.a b!c")), ".abc");
        assert_eq!(suffix_of(Path::new("/x/tour.abcdefghij")), ".abcdefgh");
        // A trailing dot has no extension at all.
        assert_eq!(suffix_of(Path::new("/x/tour.")), "");
    }

    #[test]
    fn serves_a_ranged_read_over_the_real_socket_and_counts_bytes() {
        let path = temp_file("live.mp4", b"ABCDEFGHIJ");
        let server = MediaServer::start().unwrap();
        let file = server.offer(&path).unwrap();
        server.serve();
        let url = file.url_for("127.0.0.1", server.port()).unwrap();
        let (host, port, p) = crate::upnp::http::split_url(&url).unwrap();
        let r = crate::upnp::http::request(
            (host.as_str(), port),
            "GET",
            p.as_str(),
            &format!("{host}:{port}"),
            None,
            "Range: bytes=2-5\r\n",
            &[],
        )
        .unwrap();
        // A 206 is a success and must not be read as an error by the client.
        assert_eq!(r.status, 206);
        assert_eq!(r.body, b"CDEF", "the range should have been honoured");
        assert_eq!(r.header("content-range"), Some("bytes 2-5/10"));
        assert_eq!(r.header("accept-ranges"), Some("bytes"));
        assert_eq!(server.requests(), 1);
        // Headers plus the 4 requested bytes.
        assert!(server.bytes_served() >= 4, "{}", server.bytes_served());
        server.stop();
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_head_request_carries_headers_and_no_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let path = temp_file("headonly.mp4", b"0123456789");
        let file = MediaFile {
            id: "hd".into(),
            path: path.clone(),
            title: "h.mp4".into(),
            mime: "video/mp4".into(),
            size: 10,
        };
        let store = Arc::new(std::sync::Mutex::new(vec![file]));
        let s = Arc::clone(&store);
        std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let _ = handle(
                sock,
                &s,
                &Arc::new(AtomicU64::new(0)),
                &Arc::new(AtomicU64::new(0)),
            );
        });
        let mut sock = TcpStream::connect(("127.0.0.1", port)).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        sock.write_all(b"HEAD /m/hd HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut raw = Vec::new();
        sock.read_to_end(&mut raw).unwrap();
        let text = String::from_utf8_lossy(&raw).into_owned();
        assert!(text.contains("200 OK"), "{text}");
        assert!(text.contains("Content-Length: 10"), "{text}");
        assert!(!text.contains("0123456789"), "HEAD must not send the body");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn rejects_a_method_a_renderer_should_not_use() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let _ = handle(
                sock,
                &Arc::new(std::sync::Mutex::new(Vec::new())),
                &Arc::new(AtomicU64::new(0)),
                &Arc::new(AtomicU64::new(0)),
            );
        });
        let mut sock = TcpStream::connect(("127.0.0.1", port)).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        sock.write_all(b"POST /m/x HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut raw = Vec::new();
        sock.read_to_end(&mut raw).unwrap();
        let head = String::from_utf8_lossy(&raw).into_owned();
        assert!(head.starts_with("HTTP/1.1 405"), "{head}");
    }
}
