//! SSDP: how a control point finds renderers on the LAN.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};
use tracing::{debug, warn};

const SSDP_GROUP: &str = "239.255.255.250";
const SSDP_PORT: u16 = 1900;

/// Stereotype of a device that can play media we hand it.
pub const MEDIA_RENDERER: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
/// Media servers, i.e. other libraries on the LAN. Kept for the browse path.
pub const MEDIA_SERVER: &str = "urn:schemas-upnp-org:device:MediaServer:1";

/// A device that answered a search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsdpEntry {
    /// The unique service name, which is stable across reboots of a device.
    pub usn: String,
    /// Where the device description lives.
    pub location: String,
    /// `ssdp:alive` or `ssdp:byebye`.
    pub method: String,
    /// The search target this reply was made against, e.g. the renderer type.
    pub target: String,
    pub from: SocketAddr,
}

impl SsdpEntry {
    /// The device this entry describes, with the serial suffix a
    /// `…::device:MediaRenderer:1` USN carries stripped off.
    pub fn udn(&self) -> &str {
        let body = self.usn.split("::").next().unwrap_or(&self.usn);
        body.strip_prefix("uuid:").unwrap_or(body)
    }
}

/// Send one M-SEARCH round and collect the replies that arrive before `wait`
/// expires.
///
/// The socket is bound to an ephemeral port, not to 1900: on Windows a socket
/// that binds 1900 with reuse does not receive the unicast replies devices
/// send back to the request's source port, so every answer would be lost.
pub fn search(target: &str, wait: Duration) -> Vec<SsdpEntry> {
    let socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(e) => {
            warn!("SSDP socket could not bind: {e}");
            return Vec::new();
        }
    };
    let group = SocketAddr::new(
        SSDP_GROUP
            .parse::<IpAddr>()
            .expect("the SSDP group address is a literal"),
        SSDP_PORT,
    );
    let request = format!(
        "M-SEARCH * HTTP/1.1\r\n\
         HOST: {SSDP_GROUP}:{SSDP_PORT}\r\n\
         MAN: \"ssdp:discover\"\r\n\
         MX: {}\r\n\
         ST: {target}\r\n\
         \r\n",
        wait.as_secs().max(1)
    );

    // Repeat the probe: a single datagram is routinely dropped by a busy Wi-Fi
    // access point, and the devices that do answer are what matters.
    let mut sent = 0;
    for _ in 0..2 {
        match socket.send_to(request.as_bytes(), group) {
            Ok(_) => sent += 1,
            Err(e) => {
                // Multicast can be blocked outright (a VPN-tunel adapter with
                // no route to 239/8); say so once rather than per attempt.
                if sent == 0 {
                    warn!("SSDP multicast send failed: {e}");
                    break;
                }
            }
        }
    }
    if sent == 0 {
        return Vec::new();
    }

    drain(&socket, wait)
}

/// Read replies until `wait` has elapsed, ignoring anything that is not a
/// search response. Also usable with a socket already bound to 1900 to listen
/// for unsolicited `NOTIFY` advertisements.
pub fn drain(socket: &UdpSocket, wait: Duration) -> Vec<SsdpEntry> {
    let deadline = Instant::now() + wait;
    let mut buf = [0u8; 2048];
    let mut seen: Vec<SsdpEntry> = Vec::new();

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        if socket.set_read_timeout(Some(remaining)).is_err() {
            break;
        }
        match socket.recv_from(&mut buf) {
            Ok((n, from)) => {
                let text = String::from_utf8_lossy(&buf[..n]);
                if let Some(entry) = parse_reply(&text, from) {
                    if !seen.iter().any(|e| e.usn == entry.usn && e.method == entry.method) {
                        debug!("SSDP {} {} from {from}", entry.method, entry.usn);
                        seen.push(entry);
                    }
                }
            }
            // A timeout is the normal end of a search window; any other error
            // means the socket is unusable and further waiting is pointless.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => {
                debug!("SSDP receive ended: {e}");
                break;
            }
        }
    }
    seen
}

fn parse_reply(text: &str, from: SocketAddr) -> Option<SsdpEntry> {
    let mut lines = text.lines();
    let status = lines.next().unwrap_or("");
    let is_notify = status.starts_with("NOTIFY");
    if !is_notify && !status.starts_with("HTTP/1.1 200") && !status.starts_with("HTTP/1.0 200") {
        return None;
    }

    let mut location = None;
    let mut usn = None;
    let mut target = None;
    let mut method = String::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.trim().to_ascii_uppercase().as_str() {
            "LOCATION" => location = Some(value.trim().to_string()),
            "USN" => usn = Some(value.trim().to_string()),
            "ST" => target = Some(value.trim().to_string()),
            "NT" => target = target.or_else(|| Some(value.trim().to_string())),
            "NTS" => method = value.trim().to_string(),
            _ => {}
        }
    }

    // A search reply names its target in ST and has no NTS; treat it as alive.
    if method.is_empty() {
        method = "ssdp:alive".to_string();
    }
    let location = location?;
    let usn = usn.or_else(|| target.as_ref().map(|t| format!("uuid:{t}")))?;
    Some(SsdpEntry {
        usn,
        location,
        method,
        target: target.unwrap_or_default(),
        from,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(body: &str) -> Option<SsdpEntry> {
        parse_reply(body, "192.168.1.50:1900".parse().unwrap())
    }

    #[test]
    fn parses_a_search_response() {
        let e = reply(
            "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nST: urn:schemas-upnp-org:device:MediaRenderer:1\r\nUSN: uuid:deadbeef::urn:schemas-upnp-org:device:MediaRenderer:1\r\nLOCATION: http://192.168.1.50:8290/dd.xml\r\n\r\n",
        )
        .expect("a 200 with a location is a device");
        assert_eq!(e.location, "http://192.168.1.50:8290/dd.xml");
        assert_eq!(e.udn(), "deadbeef");
        assert_eq!(e.method, "ssdp:alive");
        assert_eq!(e.target, MEDIA_RENDERER);
    }

    #[test]
    fn parses_a_notify_and_honours_nts() {
        let byebye = reply(
            "NOTIFY * HTTP/1.1\r\nNT: urn:schemas-upnp-org:device:MediaRenderer:1\r\nNTS: ssdp:byebye\r\nUSN: uuid:abc\r\nLOCATION: http://10.0.0.2:49152/d.xml\r\n\r\n",
        )
        .expect("a NOTIFY carries the same headers");
        assert_eq!(byebye.method, "ssdp:byebye");
        assert_eq!(byebye.udn(), "abc");

        let alive = reply(
            "NOTIFY * HTTP/1.1\r\nNT: upnp:rootdevice\r\nNTS: ssdp:alive\r\nUSN: uuid:abc::upnp:rootdevice\r\nLOCATION: http://10.0.0.2:49152/d.xml\r\n\r\n",
        )
        .unwrap();
        assert_eq!(alive.method, "ssdp:alive");
        // The rootdevice advertisement still identifies the same box.
        assert_eq!(alive.udn(), "abc");
    }

    #[test]
    fn ignores_the_traffic_that_is_not_an_advertisement() {
        // Our own probe, echoed back by a multicast-looping stack.
        assert!(reply("M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\n\r\n").is_none());
        // A 200 with no location cannot be talked to.
        assert!(reply("HTTP/1.1 200 OK\r\nST: ssdp:all\r\n\r\n").is_none());
        assert!(reply("garbage").is_none());
    }

    #[test]
    fn header_names_are_case_insensitive_and_values_are_trimmed() {
        let e = reply(
            "HTTP/1.1 200 OK\nlocation:  http://172.16.4.9:49200/upnp/dev/1 \nUsn: uuid:1\nst: ssdp:all\n\n",
        )
        .expect("lower-case headers are legal");
        assert_eq!(e.location, "http://172.16.4.9:49200/upnp/dev/1");
    }

    #[test]
    fn a_usn_without_a_target_suffix_still_yields_the_udn() {
        let e = reply("HTTP/1.1 200 OK\r\nUSN: uuid:X-1\r\nLOCATION: http://h/d.xml\r\n\r\n").unwrap();
        assert_eq!(e.udn(), "X-1");
    }

    #[test]
    fn drain_returns_nothing_when_no_device_answers() {
        // The positive anchors for this are the two cases below: the same
        // socket path yields entries the moment a peer writes to it.
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let out = drain(&socket, Duration::from_millis(60));
        assert!(out.is_empty(), "unanswered search produced {out:?}");

        let peer = socket.local_addr().unwrap();
        let socket2 = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket2
            .send_to(
                b"HTTP/1.1 200 OK\r\nST: urn:x\r\nUSN: uuid:me\r\nLOCATION: http://127.0.0.1:1/d.xml\r\n\r\n",
                peer,
            )
            .unwrap();
        let out = drain(&socket, Duration::from_millis(300));
        assert_eq!(out.len(), 1, "a single reply must be collected");
        assert_eq!(out[0].usn, "uuid:me");
    }

    #[test]
    fn dedupes_repeated_advertisements_of_one_device() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let peer = socket.local_addr().unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let msg = b"HTTP/1.1 200 OK\r\nST: urn:x\r\nUSN: uuid:twice\r\nLOCATION: http://h/d.xml\r\n\r\n";
        sender.send_to(msg, peer).unwrap();
        sender.send_to(msg, peer).unwrap();
        let out = drain(&socket, Duration::from_millis(250));
        assert_eq!(out.len(), 1, "two adverts of one USN are one device");
    }
}
