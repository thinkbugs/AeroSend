//! SOAP calls against AVTransport and RenderingControl, plus DIDL-Lite.

use crate::upnp::device::{AV_TRANSPORT, RENDERING_CONTROL};
use crate::upnp::http::{self, HttpError};
use crate::upnp::media_server::mime_from_extension;
use crate::upnp::xml;
use serde::Serialize;


/// A UPnP service error, i.e. an `ErrorResponse` with a control-point-visible
/// reason. Devices refuse a cast far more often than they crash, so the code
/// has to reach the UI rather than become "it did not work".
#[derive(Debug)]
pub enum SoapError {
    /// The action was rejected with a UPnP error code.
    Rejected {
        action: String,
        code: u16,
        description: String,
    },
    Http(HttpError),
}

impl std::fmt::Display for SoapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected {
                action,
                code,
                description,
            } => write!(f, "{action} refused ({code}): {description}"),
            Self::Http(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SoapError {}

impl From<HttpError> for SoapError {
    fn from(e: HttpError) -> Self {
        Self::Http(e)
    }
}

/// The transport state a renderer reports for one instance.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TransportInfo {
    pub current: String,
    pub last: String,
}

impl TransportInfo {
    /// `PLAYING` and `TRANSITIONING` both mean media is moving; a UI that only
    /// matched PLAYING would flicker during a seek.
    pub fn is_playing(&self) -> bool {
        self.current == "PLAYING" || self.current == "TRANSITIONING"
    }

    pub fn is_stopped(&self) -> bool {
        self.current == "STOPPED" || self.current == "NO_MEDIA_PRESENT"
    }
}

/// Where playback is, in seconds, plus the track length when known.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PositionInfo {
    pub seconds: f64,
    pub total_seconds: f64,
    pub uri: String,
}

/// One `<item>` or `<container>` of a ContentDirectory response.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteItem {
    pub id: String,
    pub title: String,
    pub mime: String,
    pub url: String,
    pub is_item: bool,
}

/// Call `action` on the service at `control_url` and return the response body.
pub fn call(
    control_url: &str,
    service_type: &str,
    action: &str,
    args: &[(&str, &str)],
) -> Result<String, SoapError> {
    let body = build_envelope(service_type, action, args);
    let (host, port, path) = http::split_url(control_url)?;
    let response = match http::request(
        (host.as_str(), port),
        "POST",
        path.as_str(),
        &format!("{host}:{port}"),
        // The quoting inside the Content-Type is what several firmwares parse;
        // an unquoted `charset=utf-8` is rejected outright by some.
        Some("text/xml; charset=\"utf-8\""),
        &http::soap_action_header(service_type, action),
        body.as_bytes(),
    ) {
        Ok(response) => response,
        // The classic SOAP shape: a 500 whose body carries the device's own
        // errorCode. Reporting the status alone would lose the reason.
        Err(HttpError::Status(_, fault)) if fault.contains("errorCode") => {
            let code = xml::element_text(&fault, "errorCode")
                .and_then(|c| c.trim().parse::<u16>().ok())
                .unwrap_or(0);
            let description = xml::element_text(&fault, "errorDescription")
                .unwrap_or_else(|| "the device gave no reason".to_string());
            return Err(SoapError::Rejected {
                action: action.to_string(),
                code,
                description,
            });
        }
        Err(e) => return Err(SoapError::Http(e)),
    };
    // UPnP does not require an error status for a refused action: some devices
    // answer 200 with an in-body ErrorResponse, so the status is not the verdict.
    let text = response.text();
    if let Some(code) = xml::element_text(&text, "errorCode") {
        let code = code.trim().parse().unwrap_or(0);
        let description = xml::element_text(&text, "errorDescription")
            .unwrap_or_else(|| "the device gave no reason".to_string());
        return Err(SoapError::Rejected {
            action: action.to_string(),
            code,
            description,
        });
    }
    // A 200 with no envelope means the endpoint is not a UPnP service at all;
    // pretending otherwise would report a successful cast that did nothing.
    if !text.contains("Envelope") {
        return Err(SoapError::Http(HttpError::Protocol(format!(
            "{action} returned no SOAP envelope"
        ))));
    }
    Ok(text)
}

fn build_envelope(service_type: &str, action: &str, args: &[(&str, &str)]) -> String {
    let mut body = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>");
    body.push_str(r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body>"#);
    body.push_str(&format!(
        "<u:{action} xmlns:u=\"{service_type}\">"
    ));
    for (name, value) in args {
        body.push_str(&format!("<{name}>{}</{name}>", xml::escape(value)));
    }
    body.push_str(&format!("</u:{action}></s:Body></s:Envelope>"));
    body
}

/// The SOAPAction value for one action; sent as a header by `call`'s peers and
/// asserted by some firmwares before they dispatch.
pub fn soap_action(service_type: &str, action: &str) -> String {
    format!("\"{service_type}#{action}\"")
}

/// XML-escaped DIDL-Lite for one playable object.
///
/// Renderers differ in how strictly they read this: a TV with an empty
/// `<res>` protocolInfo refuses the stream, and one that trusts the MIME will
/// not decode what we named, so both ends are explicit.
pub fn didl_item(
    title: &str,
    url: &str,
    mime: &str,
    size_bytes: Option<u64>,
    upnp_class: &str,
) -> String {
    let size = match size_bytes {
        Some(n) => format!(r#" size="{n}""#),
        None => String::new(),
    };
    format!(
        concat!(
            r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" "#,
            r#"xmlns:dc="http://purl.org/dc/elements/1.1/" "#,
            r#"xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">"#,
            r#"<item id="0" parentID="-1" restricted="1">"#,
            r#"<dc:title>{}</dc:title>"#,
            r#"<upnp:class>object.item.{}</upnp:class>"#,
            r#"<res protocolInfo="http-get:*:{}:*"{}>{}</res>"#,
            r#"</item></DIDL-Lite>"#
        ),
        xml::escape(title),
        upnp_class,
        mime,
        size,
        xml::escape(url)
    )
}

/// Set the URI and start playback.
///
/// `Play` stays a separate action because a device may stage a URI without
/// starting it, and because a failure has to say which half happened.
pub fn play_uri(av_control_url: &str, uri: &str, metadata: &str) -> Result<(), SoapError> {
    call(
        av_control_url,
        AV_TRANSPORT,
        "SetAVTransportURI",
        &[
            ("InstanceID", "0"),
            ("CurrentURI", uri),
            ("CurrentURIMetaData", metadata),
        ],
    )?;
    simple_action(av_control_url, "Play", &[("Speed", "1")])
}

pub fn simple_action(
    av_control_url: &str,
    action: &str,
    extra: &[(&str, &str)],
) -> Result<(), SoapError> {
    let mut args = vec![("InstanceID", "0")];
    args.extend_from_slice(extra);
    call(av_control_url, AV_TRANSPORT, action, &args)?;
    Ok(())
}

pub fn transport_info(av_control_url: &str) -> Result<TransportInfo, SoapError> {
    let text = call(av_control_url, AV_TRANSPORT, "GetTransportInfo", &[("InstanceID", "0")])?;
    Ok(TransportInfo {
        current: xml::element_text(&text, "CurrentTransportState").unwrap_or_default(),
        last: xml::element_text(&text, "CurrentTransportStatus").unwrap_or_default(),
    })
}

/// Read the playhead.
///
/// `RelTime` is the honest field but some devices leave it `NOT_TIME_PRESENT`
/// while still filling `AbsTime`, so both are tried before giving up.
pub fn position_info(av_control_url: &str) -> Result<PositionInfo, SoapError> {
    let text = call(av_control_url, AV_TRANSPORT, "GetPositionInfo", &[("InstanceID", "0")])?;
    let seconds = ["RelTime", "AbsTime"]
        .iter()
        .filter_map(|tag| xml::element_text(&text, tag))
        .find_map(|v| parse_time(&v));
    let total = ["TrackDuration", "ChannelLength"]
        .iter()
        .filter_map(|tag| xml::element_text(&text, tag))
        .find_map(|v| parse_time(&v));
    Ok(PositionInfo {
        seconds: seconds.unwrap_or(0.0),
        total_seconds: total.unwrap_or(0.0),
        uri: xml::element_text(&text, "TrackURI").unwrap_or_default(),
    })
}

pub fn seek(av_control_url: &str, seconds: f64) -> Result<(), SoapError> {
    simple_action(
        av_control_url,
        "Seek",
        &[("Unit", "REL_TIME"), ("Target", &format_time(seconds))],
    )
}

pub fn set_volume(rc_control_url: &str, volume: u16) -> Result<(), SoapError> {
    call(
        rc_control_url,
        RENDERING_CONTROL,
        "SetVolume",
        &[
            ("InstanceID", "0"),
            ("Channel", "Master"),
            ("DesiredVolume", &volume.to_string()),
        ],
    )?;
    Ok(())
}

pub fn get_volume(rc_control_url: &str) -> Result<u16, SoapError> {
    let text = call(
        rc_control_url,
        RENDERING_CONTROL,
        "GetVolume",
        &[("InstanceID", "0"), ("Channel", "Master")],
    )?;
    Ok(xml::element_text(&text, "CurrentVolume")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0))
}

/// What the sink accepts, straight `GetProtocolInfo`. Used to decide whether a
/// file can be streamed as-is or has to be transcoded first.
///
/// The action belongs to ConnectionManager, so the URL has to be that service's
/// — posting it at AVTransport gets a refusal from a device that keeps its
/// services apart, which most do.
pub fn sink_protocols(cm_control_url: &str) -> Result<Vec<String>, SoapError> {
    let text = call(
        cm_control_url,
        CONNECTION_MANAGER_TYPE,
        "GetProtocolInfo",
        &[],
    )?;
    Ok(xml::element_text(&text, "Sink")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

const CONNECTION_MANAGER_TYPE: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";

/// `HH:MM:SS[.ms]` to seconds; None for `NOT_TIME_PRESENT` and blanks.
pub fn parse_time(text: &str) -> Option<f64> {
    let parts: Vec<&str> = text.trim().split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h: f64 = parts[0].parse().ok()?;
    let m: f64 = parts[1].parse().ok()?;
    let s: f64 = parts[2].parse().ok()?;
    if !(0.0..60.0).contains(&m) || !(0.0..60.0).contains(&s) {
        return None;
    }
    Some(h * 3600.0 + m * 60.0 + s)
}

pub fn format_time(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    format!(
        "{:02}:{:02}:{:02}",
        total / 3600,
        (total % 3600) / 60,
        total % 60
    )
}

/// Which UPnP class a MIME type belongs to, as devices use it to pick a player.
///
/// The names are the leaf classes of the Content Directory object list
/// (`object.item.` is added by [`didl_item`]). A device that does not
/// recognise a class does not fall back gracefully: it opens its video player
/// for a photo, and a photo has no timeline to seek on.
pub fn upnp_class_for(mime: &str) -> &'static str {
    if mime.starts_with("audio/") {
        "audioItem.musicTrack"
    } else if mime.starts_with("image/") {
        "imageItem.photo"
    } else {
        "videoItem"
    }
}

/// Browse a ContentDirectory container, following the pagination the device
/// announces.
///
/// Only used for the "cast from a server on the LAN" path; a device that
/// advertises no ContentDirectory yields nothing and the caller says so.
pub fn browse(cd_control_url: &str, object_id: &str) -> Result<Vec<RemoteItem>, SoapError> {
    let mut items = Vec::new();
    let mut offset = 0usize;
    loop {
        let text = call(
            cd_control_url,
            CONTENT_DIRECTORY_TYPE,
            "Browse",
            &[
                ("ObjectID", object_id),
                ("BrowseFlag", "BrowseDirectChildren"),
                ("Filter", "*"),
                ("StartingIndex", &offset.to_string()),
                ("MaxResults", "200"),
                ("SortCriteria", ""),
            ],
        )?;
        // The DIDL inside the reply is escaped once by SOAP; `element_text`
        // decodes that, leaving real XML to scan.
        let didl = xml::element_text(&text, "Result").unwrap_or_default();
        let page = parse_didl_items(&didl);
        let returned: usize = xml::element_text(&text, "NumberReturned")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(page.len());
        let total: usize = xml::element_text(&text, "TotalMatches")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        items.extend(page);
        offset += returned;
        if returned == 0 || offset >= total {
            break;
        }
    }
    Ok(items)
}

const CONTENT_DIRECTORY_TYPE: &str = "urn:schemas-upnp-org:service:ContentDirectory:1";

/// Read `<item>` and `<container>` entries out of a DIDL-Lite document.
pub fn parse_didl_items(didl: &str) -> Vec<RemoteItem> {
    let mut out = Vec::new();
    for tag in ["item", "container"] {
        for block in xml::element_blocks(didl, tag) {
            let url = xml::element_text(&block, "res").unwrap_or_default();
            if url.is_empty() {
                continue;
            }
            let mime = xml::element_attr(&block, "res", "protocolInfo")
                .and_then(|p| {
                    p.split(':')
                        .nth(2)
                        .filter(|m| !m.is_empty() && *m != "*")
                        .map(str::to_string)
                })
                .unwrap_or_else(|| mime_from_extension(&url));
            out.push(RemoteItem {
                id: xml::element_attr(&block, tag, "id").unwrap_or_default(),
                title: xml::element_text(&block, "dc:title")
                    .or_else(|| xml::element_text(&block, "title"))
                    .unwrap_or_else(|| url.rsplit('/').next().unwrap_or(&url).to_string()),
                is_item: tag == "item",
                mime,
                url,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};


    /// A stand-in UPnP service: answers the queued replies in order and keeps
    /// every envelope it was sent.
    struct FakeSoap {
        port: u16,
        seen: Arc<Mutex<Vec<String>>>,
        accepted: Arc<AtomicUsize>,
    }

    impl FakeSoap {
        fn new(replies: Vec<String>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let accepted = Arc::new(AtomicUsize::new(0));
            let (handle, count) = (Arc::clone(&seen), Arc::clone(&accepted));
            std::thread::spawn(move || {
                for reply in replies {
                    let Ok((mut sock, _)) = listener.accept() else {
                        break;
                    };
                    let request = crate::upnp::http::tests::read_request(&mut sock);
                    let want = count.load(Ordering::SeqCst);
                    // Wait until the reader has recorded this request, so a
                    // test that asserts on the envelope never races the reply.
                    while handle.lock().unwrap().len() < want {
                        std::thread::yield_now();
                    }
                    handle.lock().unwrap().push(request);
                    count.fetch_add(1, Ordering::SeqCst);
                    let body = reply.into_bytes();
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes());
                    let _ = sock.write_all(&body);
                }
            });
            Self {
                port,
                seen,
                accepted,
            }
        }

        fn url(&self) -> String {
            format!("http://127.0.0.1:{}/ctl/AVTransport", self.port)
        }

        /// The requests actually processed, so a test that expects two actions
        /// can prove the device received two rather than one and a half.
        fn envelopes(&self) -> Vec<String> {
            let _ = self.accepted.load(Ordering::SeqCst);
            self.seen.lock().unwrap().clone()
        }
    }

    /// The reply shape a compliant service gives for an accepted action.
    fn ok(body: &str) -> String {
        format!(r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>{body}</s:Body></s:Envelope>"#)
    }

    fn ack(action: &str, service: &str) -> String {
        ok(&format!(
            "<u:{action}Response xmlns:u=\"{service}\"></u:{action}Response>"
        ))
    }

    #[test]
    fn plays_a_uri_then_starts() {
        let fake = FakeSoap::new(vec![
            ack("SetAVTransportURI", AV_TRANSPORT),
            ack("Play", AV_TRANSPORT),
        ]);
        play_uri(&fake.url(), "http://10.0.0.2:7878/a.mp4", "<DIDL/>").unwrap();
        let sent = fake.envelopes();
        assert_eq!(sent.len(), 2, "the device must see both actions");
        assert!(sent[0].contains("SetAVTransportURI"), "{}", sent[0]);
        assert!(sent[0]
            .contains("<CurrentURI>http://10.0.0.2:7878/a.mp4</CurrentURI>"));
        // Metadata arrives escaped, which is what the spec asks for.
        assert!(sent[0].contains("&lt;DIDL/&gt;"), "{}", sent[0]);
        assert!(sent[1].contains("<Speed>1</Speed>"), "{}", sent[1]);
        assert!(
            sent[0].contains(
                "SOAPAction: \"urn:schemas-upnp-org:service:AVTransport:1#SetAVTransportURI\""
            ),
            "{}",
            sent[0]
        );
    }

    #[test]
    fn sends_the_soap_action_header() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let handle = Arc::clone(&seen);
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            *handle.lock().unwrap() = crate::upnp::http::tests::read_request(&mut sock);
            let reply = ack("GetTransportInfo", AV_TRANSPORT);
            let _ = sock.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                    reply.len()
                )
                .as_bytes(),
            );
            let _ = sock.write_all(reply.as_bytes());
        });
        transport_info(&format!("http://127.0.0.1:{port}/ctl/AVTransport")).unwrap();
        let sent = seen.lock().unwrap().clone();
        assert!(
            sent.contains(
                "SOAPAction: \"urn:schemas-upnp-org:service:AVTransport:1#GetTransportInfo\""
            ),
            "{sent}"
        );
        assert_eq!(
            soap_action(AV_TRANSPORT, "Play"),
            "\"urn:schemas-upnp-org:service:AVTransport:1#Play\""
        );
    }

    #[test]
    fn reads_transport_state() {
        let fake = FakeSoap::new(vec![ok(
            "<u:GetTransportInfoResponse xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\">\
             <CurrentTransportState>PLAYING</CurrentTransportState>\
             <CurrentTransportStatus>OK</CurrentTransportStatus></u:GetTransportInfoResponse>",
        )]);
        let info = transport_info(&fake.url()).unwrap();
        assert_eq!(info.current, "PLAYING");
        assert!(info.is_playing());
        assert!(!info.is_stopped());

        // The states a UI would otherwise get wrong.
        let mut transitioning = info.clone();
        transitioning.current = "TRANSITIONING".into();
        assert!(transitioning.is_playing(), "a seek is still playback");
        let mut paused = info.clone();
        paused.current = "PAUSED_PLAYBACK".into();
        assert!(!paused.is_playing() && !paused.is_stopped());
    }

    #[test]
    fn reads_position_and_total() {
        let fake = FakeSoap::new(vec![ok(
            "<u:GetPositionInfoResponse xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\">\
             <RelTime>00:01:30.5</RelTime><TrackDuration>00:05:00</TrackDuration>\
             <TrackURI>http://10.0.0.2/a.mp4</TrackURI></u:GetPositionInfoResponse>",
        )]);
        let p = position_info(&fake.url()).unwrap();
        assert_eq!(p.seconds, 90.5);
        assert_eq!(p.total_seconds, 300.0);
        assert_eq!(p.uri, "http://10.0.0.2/a.mp4");
    }

    #[test]
    fn falls_back_to_abstime_when_reltime_is_absent() {
        let fake = FakeSoap::new(vec![ok(
            "<u:GetPositionInfoResponse xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\">\
             <RelTime>NOT_TIME_PRESENT</RelTime><AbsTime>00:00:12</AbsTime>\
             <TrackDuration>NOT_TIME_PRESENT</TrackDuration></u:GetPositionInfoResponse>",
        )]);
        let p = position_info(&fake.url()).unwrap();
        assert_eq!(p.seconds, 12.0);
        assert_eq!(p.total_seconds, 0.0);
    }

    #[test]
    fn surfaces_a_rejected_action_with_the_devices_reason() {
        let fake = FakeSoap::new(vec![ok(
            "<s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring>\
             <detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\">\
             <errorCode>701</errorCode><errorDescription>TRANSITION_NOT_AVAILABLE</errorDescription>\
             </UPnPError></detail></s:Fault>",
        )]);
        // The device answers 200: an in-body error must still be a failure.
        let e = transport_info(&fake.url()).unwrap_err();
        match &e {
            SoapError::Rejected {
                code,
                description,
                action,
                ..
            } => {
                assert_eq!(*code, 701);
                assert_eq!(action, "GetTransportInfo");
                assert_eq!(description, "TRANSITION_NOT_AVAILABLE");
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
        assert!(e.to_string().contains("701"), "{e}");
    }

    #[test]
    fn refuses_to_call_a_non_upnp_http_server() {
        // The positive anchor above shows a real envelope parses; this proves a
        // 200 from a plain web server is not mistaken for a successful action.
        let fake = FakeSoap::new(vec!["<html>404 not found</html>".to_string()]);
        let e = transport_info(&fake.url()).unwrap_err();
        assert!(
            matches!(e, SoapError::Http(HttpError::Protocol(ref m)) if m.contains("no SOAP envelope")),
            "{e:?}"
        );
    }

    #[test]
    fn time_formatting_round_trips() {
        assert_eq!(format_time(90.4), "00:01:30");
        assert_eq!(format_time(-5.0), "00:00:00");
        assert_eq!(format_time(3725.0), "01:02:05");
        assert_eq!(parse_time("01:02:05"), Some(3725.0));
        assert_eq!(parse_time("00:00:00.25"), Some(0.25));
        assert_eq!(parse_time("NOT_TIME_PRESENT"), None);
        assert_eq!(parse_time(""), None);
        assert_eq!(parse_time("00:75:00"), None, "an out-of-range minute is not a time");
        assert_eq!(parse_time("12:34"), None);
    }

    #[test]
    fn builds_didl_with_the_protocol_info_that_persists() {
        let didl = didl_item(
            "R&D <clip>",
            "http://10.0.0.2:7878/m.mp4?x=1",
            "video/mp4",
            Some(1234),
            "videoItem",
        );
        assert!(
            didl.contains(r#"protocolInfo="http-get:*:video/mp4:*""#),
            "{didl}"
        );
        assert!(didl.contains(r#"size="1234""#));
        assert!(didl.contains("R&amp;D &lt;clip&gt;"));
        assert!(didl.contains("object.item.videoItem"));
        // A URL is data, not markup: only the XML metacharacters in it escape.
        assert!(didl.contains("http://10.0.0.2:7878/m.mp4?x=1"), "{didl}");

        // The escaped form is what goes into the SOAP body; parsing it back out
        // of an envelope has to yield the original XML.
        let wrapped = format!("<s><m>{}</m></s>", xml::escape(&didl));
        assert_eq!(xml::element_text(&wrapped, "m").unwrap(), didl);

        // Size is optional and its absence must not leave an empty attribute.
        let no_size = didl_item("t", "u", "audio/mpeg", None, upnp_class_for("audio/mpeg"));
        assert!(!no_size.contains("size="), "{no_size}");
        assert!(no_size.contains("object.item.audioItem.musicTrack"), "{no_size}");
    }

    #[test]
    fn parses_a_content_directory_page() {
        let didl = r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/">
          <container id="1" restricted="1"><dc:title>Movies</dc:title><res protocolInfo="http-get:*:inode.directory:*">http://h/1</res></container>
          <item id="7" parentID="1" restricted="1"><dc:title>Keynote &amp; Demo</dc:title>
            <res protocolInfo="http-get:*:video/x-matroska:*" size="99">http://h:8200/files/k.mkv</res></item>
          <item id="8" parentID="1" restricted="1"><dc:title>No Mime</dc:title>
            <res>http://h:8200/files/song.flac</res></item>
        </DIDL-Lite>"#;
        let items = parse_didl_items(didl);
        assert_eq!(items.len(), 3);
        let item = items.iter().find(|i| i.id == "7").unwrap();
        assert_eq!(item.title, "Keynote & Demo");
        assert_eq!(item.mime, "video/x-matroska");
        assert_eq!(item.url, "http://h:8200/files/k.mkv");
        assert!(item.is_item);
        assert_eq!(items.iter().filter(|i| !i.is_item).count(), 1);
        // With no protocolInfo the extension carries the type.
        assert_eq!(
            items.iter().find(|i| i.id == "8").unwrap().mime,
            "audio/flac"
        );
    }

    #[test]
    fn browse_follows_the_pagination_it_announces() {
        let page = |id: &str, returned: usize, total: usize| {
            ok(&format!(
                "<u:BrowseResponse xmlns:u=\"{cd}\"><Result>&lt;item id=\"{id}\"&gt;&lt;res protocolInfo=\"http-get:*:video/mp4:*\"&gt;http://h/{id}.mp4&lt;/res&gt;&lt;/item&gt;</Result>\
                 <NumberReturned>{returned}</NumberReturned><TotalMatches>{total}</TotalMatches></u:BrowseResponse>",
                cd = CONTENT_DIRECTORY_TYPE
            ))
        };
        let fake = FakeSoap::new(vec![page("1", 1, 2), page("2", 1, 2)]);
        let items = browse(&fake.url(), "0").unwrap();
        assert_eq!(items.len(), 2, "both pages collected");
        assert_eq!(items[1].url, "http://h/2.mp4");
        let sent = fake.envelopes();
        assert!(sent[1].contains("<StartingIndex>1</StartingIndex>"), "{}", sent[1]);
    }

    #[test]
    fn an_empty_response_set_ends_the_browse_at_once() {
        // The positive anchor above proves pagination walks; this proves a
        // device reporting no matches does not spin forever.
        let page = ok(&format!(
            "<u:BrowseResponse xmlns:u=\"{cd}\"><Result></Result>\
             <NumberReturned>0</NumberReturned><TotalMatches>0</TotalMatches></u:BrowseResponse>",
            cd = CONTENT_DIRECTORY_TYPE
        ));
        let fake = FakeSoap::new(vec![page]);
        assert!(browse(&fake.url(), "0").unwrap().is_empty());
        assert_eq!(fake.envelopes().len(), 1);
    }

    #[test]
    fn envelope_argument_names_survive_escapes() {
        let body = build_envelope(
            AV_TRANSPORT,
            "SetAVTransportURI",
            &[("CurrentURI", "http://h/a&b<1>")],
        );
        assert!(
            body.contains("<CurrentURI>http://h/a&amp;b&lt;1&gt;</CurrentURI>"),
            "{body}"
        );
    }

    #[test]
    fn reads_the_sink_protocols_a_device_declares() {
        let fake = FakeSoap::new(vec![ok(&format!(
            "<u:GetProtocolInfoResponse xmlns:u=\"{cm}\">\
             <Sink>http-get:*:video/mp4:*,http-get:*:audio/mpeg:*,*:*:*:*</Sink>\
             <Source></Sink></u:GetProtocolInfoResponse>",
            cm = CONNECTION_MANAGER_TYPE
        ))]);
        let sinks = sink_protocols(&fake.url()).unwrap();
        assert_eq!(sinks.len(), 3);
        assert!(sinks.contains(&"http-get:*:video/mp4:*".to_string()));
        assert!(sinks.iter().all(|s| !s.is_empty()), "no blank entries");
    }

    #[test]
    fn classes_map_to_the_player_a_device_picks() {
        // The full leaf class, not a made-up short name: a device that does not
        // recognise the class plays the item in its video player.
        assert_eq!(upnp_class_for("video/mp4"), "videoItem");
        assert_eq!(upnp_class_for("audio/flac"), "audioItem.musicTrack");
        assert_eq!(upnp_class_for("image/jpeg"), "imageItem.photo");
        // Anything unnamed stays a video item, which is what a stream is.
        assert_eq!(upnp_class_for("application/octet-stream"), "videoItem");
    }

    #[test]
    fn a_photo_and_a_song_announce_the_class_their_player_expects() {
        let photo = didl_item(
            "Holiday",
            "http://10.0.0.2:7878/m/abc.jpg",
            "image/jpeg",
            Some(4096),
            upnp_class_for("image/jpeg"),
        );
        assert!(photo.contains("<upnp:class>object.item.imageItem.photo</upnp:class>"), "{photo}");
        assert!(photo.contains(r#"protocolInfo="http-get:*:image/jpeg:*""#), "{photo}");

        let song = didl_item(
            "Encore",
            "http://10.0.0.2:7878/m/def.flac",
            "audio/flac",
            None,
            upnp_class_for("audio/flac"),
        );
        assert!(
            song.contains("<upnp:class>object.item.audioItem.musicTrack</upnp:class>"),
            "{song}"
        );
        assert!(song.contains(r#"protocolInfo="http-get:*:audio/flac:*""#), "{song}");
    }
}
