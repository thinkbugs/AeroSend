//! The control connection: one socket that speaks both RTSP and HTTP/1.1.
//!
//! AirPlay puts two dialects on the same port. RAOP is RTSP - OPTIONS, ANNOUNCE,
//! SETUP, RECORD with an SDP body. A mirroring session is plain HTTP/1.1 -
//! GET /info, POST /pair-verify, POST /setup with a binary plist. The receiver
//! accepts either framing, so one client carries both and the only difference is
//! the version token on the request line.
//!
//! Bodies are bytes, never strings: an fp-setup message is 164 opaque bytes and a
//! SETUP reply is a binary plist. Content-Length is always sent, and a reply is
//! read as headers plus exactly that many bytes, which keeps the socket on a
//! message boundary for the next request. A peer that closes mid-session reads as
//! None rather than a truncated body.

use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::airplay::plist::Plist;

/// A LAN peer answers in milliseconds or is not answering; a long timeout only
/// makes a dead receiver look like a slow one.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// How often a mirroring session asks the receiver a question it does not need
/// an answer to.
///
/// Mirroring puts no traffic on the control connection, so a session that never
/// asks anything leaves that socket silent for minutes or hours — and silence is
/// what an access point, a firewall or a NAT table reaps a connection over. The
/// reported idle timeouts spread from 30 s to 10 min, so the interval has to be
/// shorter than the smallest of them rather than tuned to one device.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Fails naming both the step and the status, because a 451 on ANNOUNCE and a
    /// 461 on SETUP mean different things to fix.
    pub fn ok(&self, what: &str) -> Result<&Self> {
        if !(200..300).contains(&self.status) {
            bail!("{what} was refused: {} {}", self.status, self.reason);
        }
        Ok(self)
    }

    /// The plist a mirroring reply carries. A receiver that answers a plist
    /// request with an empty body is refusing, so that is reported separately
    /// rather than becoming a parse error.
    pub fn plist(&self, what: &str) -> Result<Plist> {
        if self.body.is_empty() {
            bail!("{what} answered {} with no body", self.status);
        }
        Plist::parse(&self.body)
            .map_err(|error| anyhow::anyhow!("{what} reply is not a plist: {error}"))
    }
}

/// One live control connection plus its request sequence number.
pub struct Control {
    writer: TcpStream,
    reader: BufReader<TcpStream>,
    /// Shared with [`Pacer`] so the keepalive and the requests on this handle
    /// never put the same CSeq on the wire twice.
    cseq: Arc<AtomicU32>,
    host: String,
    port: u16,
}

/// The control connection's two halves, held somewhere other than [`Control`].
///
/// A mirroring session spends its whole life sending pictures down a different
/// socket, so keeping this one alive cannot be driven by frame arrival: a static
/// screen produces no frames at all, and the connection the network is watching
/// would go quiet exactly when nothing is happening. It gets its own thread,
/// which needs handles that outlive a borrow of the session.
pub struct Pacer {
    writer: TcpStream,
    reader: BufReader<TcpStream>,
    cseq: Arc<AtomicU32>,
}

impl Pacer {
    /// One `OPTIONS` and its reply, on the thread that keeps the connection alive.
    ///
    /// The reply is read here even though nothing in the sender wants it. An
    /// answer left in the buffer is one more byte the receiver wrote and nobody
    /// took, and a session long enough piles up enough of them to close the
    /// receive window — at which point the receiver can no longer answer, which
    /// it reports as a control connection it has lost.
    pub fn beat(&mut self) -> Result<()> {
        let seq = self.cseq.fetch_add(1, Ordering::Relaxed) + 1;
        let message = format!(
            "OPTIONS / RTSP/1.0\r\nCSeq: {seq}\r\nUser-Agent: AeroSend/1.0\r\nContent-Length: 0\r\n\r\n"
        );
        self.writer
            .write_all(message.as_bytes())
            .context("send keepalive")?;
        // Bounded by the connection's read deadline: a receiver that has stopped
        // answering costs one beat, and the caller turns that into one log line.
        read_response(&mut self.reader)
            .context("read keepalive reply")?
            .context("the receiver closed the control connection")?;
        Ok(())
    }
}

impl Control {
    pub fn connect(host: &str, port: u16) -> Result<Self> {
        let addr = format!("{host}:{port}");
        let stream = TcpStream::connect(&addr).with_context(|| format!("connect to {addr}"))?;
        stream
            .set_read_timeout(Some(READ_TIMEOUT))
            .context("set read timeout")?;
        stream.set_nodelay(true).context("set TCP_NODELAY")?;
        let writer = stream.try_clone().context("clone control socket")?;
        Ok(Self {
            writer,
            reader: BufReader::new(stream),
            cseq: Arc::new(AtomicU32::new(0)),
            host: host.to_string(),
            port,
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// OPTIONS * RTSP/1.0 - the RAOP dialect.
    pub fn rtsp(
        &mut self,
        method: &str,
        uri: &str,
        headers: &[(&str, String)],
        body: &[u8],
    ) -> Result<Response> {
        self.request("RTSP/1.0", method, uri, headers, body)
    }

    /// POST /setup HTTP/1.1 - the mirroring dialect.
    pub fn http(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, String)],
        body: &[u8],
    ) -> Result<Response> {
        self.request("HTTP/1.1", method, path, headers, body)
    }

    /// Hands out the keepalive half of this connection.
    ///
    /// The caller owns the returned [`Pacer`] and is the only reader on the
    /// socket while it lives: dropping it before the session sends `TEARDOWN` is
    /// what keeps two threads from taking bytes off one stream and leaving a
    /// half-read reply in front of the next one.
    pub fn pacer(&self) -> Result<Pacer> {
        Ok(Pacer {
            writer: self.writer.try_clone().context("clone control writer")?,
            reader: BufReader::new(
                self.reader
                    .get_ref()
                    .try_clone()
                    .context("clone control reader")?,
            ),
            cseq: Arc::clone(&self.cseq),
        })
    }

    fn request(
        &mut self,
        version: &str,
        method: &str,
        uri: &str,
        headers: &[(&str, String)],
        body: &[u8],
    ) -> Result<Response> {
        let seq = self.cseq.fetch_add(1, Ordering::Relaxed) + 1;
        let mut message = format!("{method} {uri} {version}\r\nCSeq: {seq}\r\n");
        for (name, value) in headers {
            message.push_str(&format!("{name}: {value}\r\n"));
        }
        message.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));

        let step = format!("{method} {uri}");
        self.writer
            .write_all(message.as_bytes())
            .with_context(|| format!("send {step}"))?;
        self.writer
            .write_all(body)
            .with_context(|| format!("send {step} body"))?;
        self.writer.flush()?;

        read_response(&mut self.reader)
            .with_context(|| format!("{step} on {}:{}", self.host, self.port))?
            .ok_or_else(|| anyhow::anyhow!("{}:{} closed before answering {step}", self.host, self.port))
    }
}

/// Reads one response: leading blank lines and interleaved server requests are
/// skipped, then the status line, headers and exactly Content-Length bytes.
fn read_response(reader: &mut BufReader<TcpStream>) -> Result<Option<Response>> {
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let text = line.trim_end_matches(['\r', '\n']).to_string();
        if text.is_empty() {
            continue;
        }
        if !text.starts_with("RTSP/") && !text.starts_with("HTTP/") {
            bail!("expected a status line, got {text:?}");
        }

        let mut parts = text.splitn(3, ' ');
        parts.next();
        let status: u16 = parts
            .next()
            .context("status line has no code")?
            .parse()
            .context("status code is not a number")?;
        let reason = parts.next().unwrap_or_default().to_string();

        let mut headers = Vec::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let name = line.trim_end_matches(['\r', '\n']).to_string();
            if name.is_empty() {
                break;
            }
            if let Some((key, value)) = name.split_once(':') {
                headers.push((key.trim().to_string(), value.trim().to_string()));
            }
        }

        let length: usize = headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; length];
        if length > 0 {
            reader.read_exact(&mut body)?;
        }
        return Ok(Some(Response {
            status,
            reason,
            headers,
            body,
        }));
    }
}

/// The first number of a key=low-high pair in a Transport header. A value above
/// 0x7FFF is a port that arrived through a signed slot, so it is masked back.
pub fn transport_port(transport: &str, key: &str) -> Option<u16> {
    let value = transport
        .split(';')
        .map(str::trim)
        .find(|part| part.starts_with(&format!("{key}=")))?
        .split_once('=')?
        .1;
    let raw: i64 = value.split('-').next()?.parse().ok()?;
    Some((raw as u64 & 0xFFFF) as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, TcpListener};
    use std::sync::{Arc, Mutex};
    use std::thread;

    /// A receiver that answers every request and remembers its CSeq.
    fn receiver(replies: bool) -> (u16, Arc<Mutex<Vec<u32>>>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let server_seen = Arc::clone(&seen);
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            if !replies {
                // Gone before the first byte of the first request is answered.
                drop(stream);
                return;
            }
            let mut reader = BufReader::new(stream);
            loop {
                // One request is its line plus headers until the blank line; the
                // keepalive sends no body, so this is the whole message.
                let mut cseq = None;
                let mut line = String::new();
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let text = line.trim_end_matches(['\r', '\n']);
                    if text.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = text.split_once(':') {
                        if name.eq_ignore_ascii_case("cseq") {
                            cseq = value.trim().parse().ok();
                        }
                    }
                }
                let seq = cseq.expect("every request on this connection carries a CSeq");
                server_seen.lock().unwrap().push(seq);
                let reply = format!("RTSP/1.0 200 OK\r\nCSeq: {seq}\r\n\r\n");
                if reader.get_mut().write_all(reply.as_bytes()).is_err() {
                    return;
                }
            }
        });
        (port, seen, server)
    }

    /// The keepalive shares the connection with the session's own requests, so it
    /// counts from where they left off rather than restarting at one, and it takes
    /// its reply off the socket instead of leaving it unread.
    #[test]
    fn a_pacer_beats_the_control_connection_and_keeps_counting() {
        let (port, seen, server) = receiver(true);
        let mut control = Control::connect("127.0.0.1", port).unwrap();
        control.rtsp("OPTIONS", "/", &[], &[]).unwrap();

        let mut pacer = control.pacer().unwrap();
        pacer.beat().expect("the keepalive reaches the receiver");
        pacer.beat().expect("the keepalive reaches the receiver again");

        // The session's last request goes through the handle the pacer was
        // cloned from, and still gets its answer.
        control
            .rtsp("TEARDOWN", "/", &[], &[])
            .expect("the pacer left the request path intact");

        assert_eq!(seen.lock().unwrap().clone(), vec![1, 2, 3, 4]);
        drop(pacer);
        drop(control);
        let _ = server.join();
    }

    /// A beat is not a beat unless the answer came back. Writing alone reports
    /// success against a receiver that hung up a second ago, and answers nobody
    /// reads pile up until the receiver cannot write any more.
    #[test]
    fn a_beat_fails_when_the_receiver_has_closed() {
        let (port, _seen, _server) = receiver(false);
        let control = Control::connect("127.0.0.1", port).unwrap();
        let mut pacer = control.pacer().unwrap();
        pacer.beat().expect_err("a closed control connection is a failed beat");
    }
}
