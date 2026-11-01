//! End-to-end DLNA cast inside one process: the real control point, the real
//! media server, and a fake DMR that only knows the published protocol.
//!
//! The fake device is written without this crate's XML or HTTP code on
//! purpose — substring SOAP parsing and its own `TcpStream` media pulls — so a
//! mistake in our framing cannot hide behind the same mistake in the test.
//!
//! What this proves, in order: a renderer found by address, a local file
//! offered as a URL, DIDL-Lite the device can read, byte ranges pulled back
//! with the right content, position that advances, seek and volume that reach
//! the device, and a stop that ends the session.

use aerosend::upnp::{avt, CastError, Caster};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// What the device claims the clip lasts, and the shapes a player asks for.
const CLAIMED_DURATION: &str = "00:03:20";
const FILE_SIZE: usize = 400_000;
const HEAD: usize = 65_536;
const TAIL: usize = 65_536;

/// Byte at an offset, so a ranged read can be checked for the right content and
/// not merely the right length.
fn pattern(offset: usize) -> u8 {
    (offset % 251) as u8
}

fn split_at(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Text between `<name>` and `</name>`, or "" when absent.
fn tag(body: &str, name: &str) -> String {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let Some(start) = body.find(&open) else {
        return String::new();
    };
    let start = start + open.len();
    match body[start..].find(&close) {
        Some(len) => body[start..start + len].to_string(),
        None => String::new(),
    }
}

/// The device's own unescaping: DIDL-Lite arrives escaped inside the argument.
fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The SOAP action a body asks for, read off the envelope itself.
fn action_of(body: &str) -> String {
    match body.find("<u:") {
        Some(at) => body[at + 3..]
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_end_matches(|c| c == '>' || c == '/')
            .to_string(),
        None => String::new(),
    }
}

#[derive(Debug, Default)]
struct DeviceState {
    /// Every action, in the order the device answered it.
    actions: Vec<String>,
    /// Action name with the arguments it carried.
    calls: Vec<(String, HashMap<String, String>)>,
    uri: String,
    metadata: String,
    transport: String,
    volume: u16,
    /// Media reads: (range asked, status, Content-Range, bytes, mismatch).
    reads: Vec<(String, String, String, usize, Option<String>)>,
    errors: Vec<String>,
    /// Playback clock: where the position started and when it began running.
    base_seconds: f64,
    running_since: Option<Instant>,
}

impl DeviceState {
    fn position(&self) -> f64 {
        match self.running_since {
            Some(at) => self.base_seconds + at.elapsed().as_secs_f64(),
            None => self.base_seconds,
        }
    }
}

fn seconds_to_hms(seconds: f64) -> String {
    let whole = seconds.max(0.0) as u64;
    format!(
        "{:02}:{:02}:{:02}",
        whole / 3600,
        (whole % 3600) / 60,
        whole % 60
    )
}

fn hms_to_seconds(text: &str) -> Option<f64> {
    let mut parts = text.split(':');
    let hours: f64 = parts.next()?.parse().ok()?;
    let minutes: f64 = parts.next()?.parse().ok()?;
    let seconds: f64 = parts.next()?.parse().ok()?;
    Some(hours * 3600.0 + minutes * 60.0 + seconds)
}

/// Read one request the way a device does: framing first, then exactly the
/// body that was announced.
fn read_request(sock: &mut TcpStream) -> Option<(String, String, HashMap<String, String>, Vec<u8>)> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    let header_end = loop {
        let n = sock.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        raw.extend_from_slice(&buf[..n]);
        if let Some(at) = split_at(&raw, b"\r\n\r\n") {
            break at + 4;
        }
        if raw.len() > 4_000_000 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&raw[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("").to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let mut body = raw[header_end..].to_vec();
    let want: usize = headers
        .get("content-length")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    while body.len() < want {
        let n = sock.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    Some((method, path, headers, body))
}

/// Pull the offered file back over HTTP, as a player would: a head read for
/// the header, the tail for the index at the end, then a middle slice.
fn pull_media(state: Arc<Mutex<DeviceState>>, uri: String) {
    thread::spawn(move || {
        for spec in [
            format!("bytes=0-{}", HEAD - 1),
            format!("bytes=-{TAIL}"),
            "bytes=1000-1999".to_string(),
        ] {
            match one_read(&uri, &spec) {
                Ok((status, content_range, data)) => {
                    let mismatch = check(&spec, FILE_SIZE, &status, &content_range, &data);
                    state.lock().unwrap().reads.push((
                        spec.clone(),
                        status,
                        content_range,
                        data.len(),
                        mismatch,
                    ));
                }
                Err(e) => state.lock().unwrap().errors.push(format!("{spec}: {e}")),
            }
        }
    });
}

fn one_read(uri: &str, spec: &str) -> Result<(String, String, Vec<u8>), String> {
    let rest = uri.strip_prefix("http://").ok_or("not an http uri")?;
    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().map_err(|e| e.to_string())?),
        None => (authority.to_string(), 80),
    };
    let mut sock = TcpStream::connect((host.as_str(), port)).map_err(|e| e.to_string())?;
    sock.set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nRange: {spec}\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
    sock.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    sock.read_to_end(&mut raw).map_err(|e| e.to_string())?;
    let at = split_at(&raw, b"\r\n\r\n").ok_or("no header terminator")?;
    let head = String::from_utf8_lossy(&raw[..at]).into_owned();
    let status = head
        .split_whitespace()
        .nth(1)
        .unwrap_or("")
        .to_string();
    let content_range = head
        .lines()
        .find_map(|l| {
            l.split_once(':')
                .filter(|(k, _)| k.trim().eq_ignore_ascii_case("content-range"))
                .map(|(_, v)| v.trim().to_string())
        })
        .unwrap_or_default();
    Ok((status, content_range, raw[at + 4..].to_vec()))
}

/// Verify a read against what the range asked for, including the bytes.
fn check(spec: &str, size: usize, status: &str, content_range: &str, data: &[u8]) -> Option<String> {
    let Some(rest) = spec.strip_prefix("bytes=") else {
        return Some(format!("{spec} is not a range"));
    };
    let Some((raw_start, raw_end)) = rest.split_once('-') else {
        return Some(format!("{spec} has no start-end pair"));
    };
    let parse = |text: &str| text.trim().parse::<usize>().ok();
    let (start, end) = if raw_start.is_empty() {
        let Some(n) = parse(raw_end) else {
            return Some(format!("{spec} has no readable length"));
        };
        (size - n, size)
    } else {
        let Some(from) = parse(raw_start) else {
            return Some(format!("{spec} has no readable start"));
        };
        let to = if raw_end.is_empty() {
            size - 1
        } else {
            match parse(raw_end) {
                Some(value) => value,
                None => return Some(format!("{spec} has no readable end")),
            }
        };
        (from, to + 1)
    };
    if status != "206" {
        return Some(format!("a range request answered {status}, not 206"));
    }
    let expected_range = format!("bytes {start}-{}/{size}", end - 1);
    if content_range != expected_range {
        return Some(format!("Content-Range {content_range}, expected {expected_range}"));
    }
    if data.len() != end - start {
        return Some(format!("{} bytes back, expected {}", data.len(), end - start));
    }
    for (i, byte) in data.iter().enumerate() {
        if *byte != pattern(start + i) {
            return Some(format!(
                "byte {} is {byte:#04x}, expected {:#04x}",
                start + i,
                pattern(start + i)
            ));
        }
    }
    None
}

fn soap_reply(action: &str, service: &str, inner: &str) -> Vec<u8> {
    let body = format!(
        r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:{action}Response xmlns:u="{service}">{inner}</u:{action}Response></s:Body></s:Envelope>"#
    );
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/xml; charset=\"utf-8\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    [head.as_bytes(), body.as_bytes()].concat()
}

fn plain_reply(status: &str, body: &str) -> Vec<u8> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    [head.as_bytes(), body.as_bytes()].concat()
}

const AVT: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const RC: &str = "urn:schemas-upnp-org:service:RenderingControl:1";

fn description() -> String {
    r#"<?xml version="1.0" encoding="utf-8"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion><major>1</major><minor>1</minor></specVersion>
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
    <friendlyName>Living Room Player</friendlyName>
    <manufacturer>Third Party</manufacturer>
    <modelName>DMR-1.50</modelName>
    <UDN>uuid:fake-dmr-1</UDN>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>
        <SCPDURL>/scpd/avt.xml</SCPDURL>
        <controlURL>/ctl/AVTransport</controlURL>
        <eventSubURL>/evt/AVTransport</eventSubURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:RenderingControl</serviceId>
        <SCPDURL>/scpd/rc.xml</SCPDURL>
        <controlURL>/ctl/RenderingControl</controlURL>
        <eventSubURL>/evt/RenderingControl</eventSubURL>
      </service>
    </serviceList>
  </device>
</root>"#
        .to_string()
}

/// Start the fake renderer; returns its port and the state it accumulates.
fn spawn_device() -> (u16, Arc<Mutex<DeviceState>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let state = Arc::new(Mutex::new(DeviceState {
        transport: "NO_MEDIA_PRESENT".into(),
        volume: 10,
        ..Default::default()
    }));
    let handle = Arc::clone(&state);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut sock) = stream else { break };
            let state = Arc::clone(&handle);
            thread::spawn(move || {
                let Some((method, path, _, body)) = read_request(&mut sock) else {
                    return;
                };
                let text = String::from_utf8_lossy(&body).into_owned();
                let reply = match (method.as_str(), path.as_str()) {
                    ("GET", "/dd.xml") => plain_reply("200 OK", &description()),
                    ("POST", "/ctl/AVTransport") => avtransport(&state, &text),
                    ("POST", "/ctl/RenderingControl") => rendering_control(&state, &text),
                    ("POST", _) => soap_fault("401", "no such service"),
                    _ => plain_reply("404 Not Found", "<html>not a device document</html>"),
                };
                let _ = sock.write_all(&reply);
                let _ = sock.shutdown(std::net::Shutdown::Both);
            });
        }
    });
    (port, state)
}

/// A refused action the way a firmware sends one: a SOAP fault carrying the
/// UPnP error code.
fn soap_fault(code: &str, description: &str) -> Vec<u8> {
    let body = format!(
        r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>{code}</errorCode><errorDescription>{description}</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"#
    );
    plain_reply("500 Internal Server Error", &body)
}

fn remember(state: &Arc<Mutex<DeviceState>>, action: &str, body: &str, names: &[&str]) {
    let mut args = HashMap::new();
    for name in names {
        args.insert((*name).to_string(), unescape(&tag(body, name)));
    }
    let mut guard = state.lock().unwrap();
    guard.actions.push(action.to_string());
    guard.calls.push((action.to_string(), args));
}

fn avtransport(state: &Arc<Mutex<DeviceState>>, body: &str) -> Vec<u8> {
    let action = action_of(body);
    let mut guard = state.lock().unwrap();
    match action.as_str() {
        "SetAVTransportURI" => {
            let uri = unescape(&tag(body, "CurrentURI"));
            let metadata = unescape(&tag(body, "CurrentURIMetaData"));
            let mut args = HashMap::new();
            args.insert("CurrentURI".to_string(), uri.clone());
            args.insert("CurrentURIMetaData".to_string(), metadata.clone());
            args.insert("InstanceID".to_string(), tag(body, "InstanceID"));
            guard.uri = uri;
            guard.metadata = metadata;
            guard.actions.push(action.clone());
            guard.calls.push((action, args));
            guard.transport = "STOPPED".into();
            drop(guard);
            soap_reply("SetAVTransportURI", AVT, "")
        }
        "Play" => {
            let uri = guard.uri.clone();
            guard.transport = "PLAYING".into();
            guard.running_since = Some(Instant::now());
            drop(guard);
            remember(state, "Play", body, &["InstanceID", "Speed"]);
            // The device pulls the file itself, as any DMR would.
            pull_media(Arc::clone(state), uri);
            soap_reply("Play", AVT, "")
        }
        "Pause" => {
            guard.base_seconds = guard.position();
            guard.running_since = None;
            guard.transport = "PAUSED_PLAYBACK".into();
            drop(guard);
            remember(state, "Pause", body, &["InstanceID"]);
            soap_reply("Pause", AVT, "")
        }
        "Stop" => {
            guard.transport = "STOPPED".into();
            guard.running_since = None;
            guard.base_seconds = 0.0;
            drop(guard);
            remember(state, "Stop", body, &["InstanceID"]);
            soap_reply("Stop", AVT, "")
        }
        "Seek" => {
            let unit = tag(body, "Unit");
            let target = hms_to_seconds(&tag(body, "Target"));
            if unit == "REL_TIME" {
                if let Some(seconds) = target {
                    guard.base_seconds = seconds;
                    if guard.running_since.is_some() {
                        guard.running_since = Some(Instant::now());
                    }
                }
                guard.transport = "TRANSITIONING".into();
            }
            drop(guard);
            remember(state, "Seek", body, &["InstanceID", "Unit", "Target"]);
            soap_reply("Seek", AVT, "")
        }
        "GetTransportInfo" => {
            let transport = guard.transport.clone();
            drop(guard);
            soap_reply(
                "GetTransportInfo",
                AVT,
                &format!(
                    "<CurrentTransportState>{transport}</CurrentTransportState>\
                     <CurrentTransportStatus>OK</CurrentTransportStatus>"
                ),
            )
        }
        "GetPositionInfo" => {
            let position = seconds_to_hms(guard.position());
            let uri = guard.uri.clone();
            drop(guard);
            soap_reply(
                "GetPositionInfo",
                AVT,
                &format!(
                    "<Track>1</Track><TrackDuration>{CLAIMED_DURATION}</TrackDuration>\
                     <TrackURI>{uri}</TrackURI><RelTime>{position}</RelTime>\
                     <AbsTime>{position}</AbsTime><TrackMetaInformation>0</TrackMetaInformation>"
                ),
            )
        }
        other => {
            drop(guard);
            state.lock().unwrap().actions.push(format!("refused:{other}"));
            soap_fault("401", &format!("no such action: {other}"))
        }
    }
}

fn rendering_control(state: &Arc<Mutex<DeviceState>>, body: &str) -> Vec<u8> {
    let action = action_of(body);
    match action.as_str() {
        "SetVolume" => {
            let wanted = tag(body, "DesiredVolume").parse::<u16>().unwrap_or(0);
            state.lock().unwrap().volume = wanted;
            remember(state, "SetVolume", body, &["InstanceID", "Channel", "DesiredVolume"]);
            soap_reply("SetVolume", RC, "")
        }
        "GetVolume" => {
            let volume = state.lock().unwrap().volume;
            soap_reply("GetVolume", RC, &format!("<CurrentVolume>{volume}</CurrentVolume>"))
        }
        other => plain_reply(
            "500 Internal Server Error",
            &format!("<html>no such action {other}</html>"),
        ),
    }
}

/// Poll a condition the device or the poller has to reach, and say which one
/// went out of patience.
fn wait_for(label: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        if done() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {label}");
}

fn temp_video(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("aerosend-e2e-{}-{name}", std::process::id()));
    let bytes: Vec<u8> = (0..FILE_SIZE).map(|i| pattern(i)).collect();
    std::fs::write(&path, &bytes).unwrap();
    path
}

fn actions_seen(state: &Arc<Mutex<DeviceState>>) -> Vec<String> {
    state.lock().unwrap().actions.clone()
}

#[test]
fn a_local_file_casts_and_the_renderer_pulls_the_bytes_it_asked_for() {
    let (port, device) = spawn_device();
    let caster = Caster::new();
    let id = "uuid:fake-dmr-1";

    // 1. Manual discovery: only an address, no SSDP.
    let info = caster
        .add_by_location(&format!("127.0.0.1:{port}"))
        .expect("a device on localhost should be reachable");
    assert_eq!(info.id, id);
    assert_eq!(info.name, "Living Room Player");
    assert!(info.castable, "{info:?}");
    let control = {
        let guard = caster.renderers();
        guard
            .iter()
            .find(|r| r.id == id)
            .expect("the added device is listed")
            .base_url
            .clone()
    };
    assert_eq!(control, format!("http://127.0.0.1:{port}"));

    // 2. Offer a real file and cast it.
    let path = temp_video("clip.mp4");
    let offered = caster.offer(id, &path).expect("offering should work");
    assert_eq!(offered.size, FILE_SIZE as u64);
    assert_eq!(offered.mime, "video/mp4");
    assert!(offered.url.starts_with("http://127.0.0.1:"), "{}", offered.url);

    let status = caster.cast_file(id, &path).expect("cast should be accepted");
    assert_eq!(status.state, "PLAYING");
    assert!(status.active);

    // 3. The device must have seen the URI and DIDL it can actually read.
    wait_for("SetAVTransportURI and Play to arrive", || {
        actions_seen(&device).contains(&"SetAVTransportURI".to_string())
            && actions_seen(&device).contains(&"Play".to_string())
    });
    let (uri, metadata) = {
        let guard = device.lock().unwrap();
        (guard.uri.clone(), guard.metadata.clone())
    };
    assert_eq!(uri, offered.url, "the URI played is the URI we offered");
    assert!(
        metadata.contains(r#"protocolInfo="http-get:*:video/mp4:*""#),
        "{metadata}"
    );
    assert!(
        metadata.contains(&format!(r#"size="{FILE_SIZE}""#)),
        "a size lets the player seek: {metadata}"
    );
    assert!(metadata.contains("object.item.videoItem"), "{metadata}");

    // 4. The player pulled the file back: head, tail, and a middle slice, all
    //    with the bytes intact.
    wait_for("three ranged reads to finish", || {
        device.lock().unwrap().reads.len() == 3
    });
    let (reads, errors) = {
        let guard = device.lock().unwrap();
        (guard.reads.clone(), guard.errors.clone())
    };
    assert!(errors.is_empty(), "{errors:?}");
    let specs: Vec<&str> = reads.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(
        specs,
        vec![
            &format!("bytes=0-{}", HEAD - 1),
            &format!("bytes=-{TAIL}"),
            "bytes=1000-1999"
        ],
        "the reads a player makes, in order"
    );
    for read in &reads {
        assert_eq!(read.1, "206", "{} answered {}", read.0, read.1);
        assert!(read.4.is_none(), "{}: {:?}", read.0, read.4);
    }
    assert_eq!(reads[0].3, HEAD);
    assert_eq!(reads[1].3, TAIL, "a suffix range is the last bytes");
    assert_eq!(reads[2].3, 1000);
    let served = caster.bytes_served();
    assert!(
        served >= (HEAD + TAIL + 1000) as u64,
        "the server counted {served} bytes"
    );

    // 5. Position advances on its own: the poller reads the device's clock.
    wait_for("the poller to report a moving position", || {
        caster.status(id).map(|s| s.seconds > 0.0).unwrap_or(false)
    });
    let live = caster.status(id).unwrap();
    assert_eq!(live.state, "PLAYING");
    assert_eq!(live.total_seconds, 200.0, "00:03:20 as the device declares it");
    assert!(live.uri == offered.url);

    // 6. Seek reaches the device in the unit a player expects.
    let after_seek = caster.seek(id, 60.0).expect("seek should be accepted");
    assert_eq!(after_seek.seconds, 60.0);
    let guard = device.lock().unwrap();
    let seek = guard
        .calls
        .iter()
        .find(|(action, _)| action == "Seek")
        .expect("the device saw a Seek");
    assert_eq!(seek.1.get("Unit").map(String::as_str), Some("REL_TIME"));
    assert_eq!(seek.1.get("Target").map(String::as_str), Some("00:01:00"));
    drop(guard);

    // 7. Volume is the other service, and it round-trips.
    caster.set_volume(id, 42).expect("volume should be accepted");
    assert_eq!(device.lock().unwrap().volume, 42);

    // 8. Pause, then play again, then stop ends the session.
    let paused = caster.transport_action(id, "Pause").unwrap();
    assert_eq!(paused.state, "PAUSED_PLAYBACK");
    assert!(paused.active, "a paused cast is still ours");
    let stopped = caster.transport_action(id, "Stop").unwrap();
    assert_eq!(stopped.state, "STOPPED");
    assert!(!stopped.active);
    wait_for("the device to see the stop", || {
        actions_seen(&device).contains(&"Stop".to_string())
    });
    assert!(
        !caster
            .renderers()
            .iter()
            .any(|r| r.id == id && r.playing),
        "a stopped renderer is not shown as playing"
    );

    // 9. The same file, offered again, is registered once.
    let again = caster.offer(id, &path).unwrap();
    assert_eq!(again.url, offered.url, "one file gets one URL");
    assert_eq!(caster.offered().len(), 1);

    // 10. Forgetting the renderer makes it unusable, and a stale id says so.
    assert!(caster.remove(id));
    assert!(caster.renderers().is_empty());
    let stale = caster.cast_file(id, &path).unwrap_err();
    assert!(
        matches!(stale, CastError::UnknownRenderer(ref gone) if gone == id),
        "{stale:?}"
    );

    caster.shutdown();
    std::fs::remove_file(&path).ok();
}

#[test]
fn casting_a_url_the_renderer_reaches_itself_needs_no_local_file() {
    let (port, device) = spawn_device();
    let caster = Caster::new();
    let id = "uuid:fake-dmr-1";
    caster
        .add_by_location(&format!("http://127.0.0.1:{port}/dd.xml"))
        .expect("a named document works too");

    let remote = "http://192.168.1.50:8080/live.mp4";
    let status = caster
        .cast(id, remote, "Camera recording")
        .expect("a URL needs no media server");
    assert_eq!(status.uri, remote);
    assert_eq!(caster.bytes_served(), 0, "nothing of ours went over the wire");
    wait_for("the device to see the URL", || {
        device.lock().unwrap().uri == remote
    });
    let metadata = device.lock().unwrap().metadata.clone();
    assert!(metadata.contains("Camera recording"), "{metadata}");
    assert!(
        metadata.contains(r#"protocolInfo="http-get:*:video/mp4:*""#),
        "{metadata}"
    );
    caster.shutdown();
}

#[test]
fn a_device_that_refuses_a_cast_reports_its_own_reason() {
    // A firmware that answers a 500 with a SOAP fault when the resource is not
    // playable: the code and the device's words have to reach the user, since
    // "it did not work" teaches nothing.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        while let Ok((mut sock, _)) = listener.accept() {
            let Some((method, path, _, body)) = read_request(&mut sock) else {
                continue;
            };
            let text = String::from_utf8_lossy(&body).into_owned();
            let reply = match (method.as_str(), path.as_str()) {
                ("GET", "/dd.xml") => plain_reply("200 OK", &description()),
                ("POST", "/ctl/AVTransport") if text.contains("<u:SetAVTransportURI") => {
                    soap_fault("716", "Resource does not exist")
                }
                ("POST", "/ctl/AVTransport") => soap_reply(
                    "GetTransportInfo",
                    AVT,
                    "<CurrentTransportState>NO_MEDIA_PRESENT</CurrentTransportState>\
                     <CurrentTransportStatus>OK</CurrentTransportStatus>",
                ),
                _ => soap_reply("Unused", AVT, ""),
            };
            let _ = sock.write_all(&reply);
            let _ = sock.shutdown(std::net::Shutdown::Both);
        }
    });

    let caster = Caster::new();
    let id = "uuid:fake-dmr-1";
    let info = caster
        .add_by_location(&format!("127.0.0.1:{port}"))
        .expect("a device that refuses is still a device");
    assert!(info.castable);

    let err = caster
        .cast(id, "http://192.168.1.9:8/clip.mp4", "Clip")
        .unwrap_err();
    let message = err.to_string();
    assert!(message.contains("716"), "{message}");
    assert!(message.contains("Resource does not exist"), "{message}");
    assert!(matches!(err, CastError::Soap(_)), "{err:?}");
    // The positive anchors: the same renderer answers a plain query over the
    // same endpoint, so the refusal is the cast and not the socket, and no
    // session was seeded for a cast that never started.
    let transport =
        avt::transport_info(&format!("http://127.0.0.1:{port}/ctl/AVTransport")).unwrap();
    assert_eq!(transport.current, "NO_MEDIA_PRESENT");
    assert!(!transport.is_playing());
    assert!(caster.status(id).is_none(), "a refused cast is not a session");
    assert_eq!(caster.bytes_served(), 0);
    caster.shutdown();
}
