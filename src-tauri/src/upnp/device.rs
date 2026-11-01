//! Device description: the XML a renderer serves at its LOCATION, and the
//! control endpoints taken from it.

use crate::upnp::http::{self, HttpError};
use crate::upnp::ssdp::SsdpEntry;
use crate::upnp::xml;
use serde::Serialize;

pub const AV_TRANSPORT: &str = "urn:schemas-upnp-org:service:AVTransport:1";
pub const RENDERING_CONTROL: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
pub const CONNECTION_MANAGER: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";

/// Everything needed to talk to one renderer.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Renderer {
    /// `uuid:…` device id, stable across reboots.
    pub id: String,
    pub name: String,
    pub model: String,
    pub manufacturer: String,
    /// The `http://host:port` origin, for display and for the UI's own links.
    pub base_url: String,
    /// Absolute control endpoints, one per service this device publishes.
    pub services: Vec<ServiceEndpoint>,
    /// Whether the description was fetched and parsed successfully. A renderer
    /// is listed even when it is not, so the user can see and retry it.
    pub reachable: bool,
    /// Why not, when unreachable.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceEndpoint {
    /// `urn:schemas-upnp-org:service:AVTransport:1` — what a caller asks for.
    pub service_type: String,
    /// `urn:upnp-org:serviceId:AVTransport` — what some devices key on instead.
    pub service_id: String,
    pub control_url: String,
}

impl Renderer {
    /// The control endpoint for a service, matched on either name.
    ///
    /// Both spellings turn up in real descriptions and a device that omits
    /// `serviceType` would otherwise look uncastable.
    pub fn control_url(&self, service: &str) -> Option<&str> {
        self.services
            .iter()
            .find(|s| s.service_type == service || s.service_id == service)
            .map(|s| s.control_url.as_str())
    }

    pub fn has_av_transport(&self) -> bool {
        self.control_url(AV_TRANSPORT).is_some()
    }
}

/// Fetch and parse a device description.
pub fn describe(entry: &SsdpEntry) -> Result<Renderer, HttpError> {
    let (host, port, path) = http::split_url(&entry.location)?;
    let response = http::request(
        (host.as_str(), port),
        "GET",
        path.as_str(),
        &format!("{host}:{port}"),
        None,
        "",
        &[],
    )?;
    let mut renderer = parse_description(&response.text(), &entry.usn);
    // The advertisement's origin is what makes a relative `controlURL` usable;
    // without this step every device serving a bare path is unusable.
    let origin = match entry.location.find("://") {
        Some(at) => match entry.location[at + 3..].find('/') {
            Some(slash) => entry.location[..at + 3 + slash].to_string(),
            None => entry.location.clone(),
        },
        None => String::new(),
    };
    resolve(&mut renderer, &origin);
    Ok(renderer)
}

/// Turn a description document into a renderer. Control URLs are still the
/// device's own (often relative) strings until [`resolve`] runs.
pub fn parse_description(xml_text: &str, usn: &str) -> Renderer {
    let services = {
        let mut services = Vec::new();
        for block in xml::element_blocks(xml_text, "service") {
            let service_type = xml::element_text(&block, "serviceType").unwrap_or_default();
            let service_id = xml::element_text(&block, "serviceId").unwrap_or_default();
            let Some(url) = xml::element_text(&block, "controlURL") else {
                continue;
            };
            if service_type.is_empty() && service_id.is_empty() {
                continue;
            }
            services.push(ServiceEndpoint {
                service_type,
                service_id,
                control_url: url,
            });
        }
        services
    };

    Renderer {
        id: match xml::element_text(xml_text, "UDN") {
            // The document's own UDN is the strongest id: it survives a change
            // of address and matches what SSDP reports for the same box.
            Some(udn) if !udn.is_empty() => udn,
            _ => usn.split("::").next().unwrap_or(usn).to_string(),
        },
        name: xml::element_text(xml_text, "friendlyName").unwrap_or_else(|| "Renderer".into()),
        model: xml::element_text(xml_text, "modelName").unwrap_or_default(),
        manufacturer: xml::element_text(xml_text, "manufacturer").unwrap_or_default(),
        base_url: String::new(),
        services,
        reachable: true,
        error: None,
    }
}

/// Document paths a device is likely to serve its description from.
const CANDIDATE_PATHS: [&str; 3] = ["/dd.xml", "/description.xml", "/"];

/// Fetch a description from an address the user typed.
///
/// Manual entry has to work from `192.168.1.9:8290` alone, because SSDP is
/// blocked often enough to matter: AP isolation, a switch that does not snoop
/// multicast, a VPN that owns the route. So the standard document paths are
/// probed in order, and a URL that already names a document is used as given.
pub fn describe_by_address(input: &str) -> Result<Renderer, HttpError> {
    let input = input.trim();
    let with_scheme = if input.contains("://") {
        input.to_string()
    } else {
        format!("http://{input}")
    };
    let (host, port, path) = http::split_url(&with_scheme)?;
    let base = format!("http://{host}:{port}");
    let candidates: Vec<String> = if path == "/" || path.is_empty() {
        CANDIDATE_PATHS.iter().map(|p| (*p).to_string()).collect()
    } else {
        vec![path]
    };

    let mut last = HttpError::Protocol(format!("nothing answered at {input}"));
    for candidate in candidates {
        let text = match http::request(
            (host.as_str(), port),
            "GET",
            &candidate,
            &format!("{host}:{port}"),
            None,
            "",
            &[],
        ) {
            Ok(r) => r.text(),
            Err(e) => {
                last = e;
                continue;
            }
        };
        // A door that answers with a login page is not a device description.
        if !text.contains("<root") {
            last = HttpError::Protocol(format!("{base}{candidate} is not a UPnP description"));
            continue;
        }
        let mut renderer = parse_description(&text, "");
        resolve(&mut renderer, &base);
        if renderer.services.is_empty() {
            last = HttpError::Protocol(format!("{base}{candidate} lists no services"));
            continue;
        }
        if renderer.id.is_empty() {
            renderer.id = renderer.base_url.clone();
        }
        return Ok(renderer);
    }
    Err(last)
}

/// Fill in absolute endpoints; `base` is the `http://host:port` of LOCATION.
///
/// Split out because two shapes occur in the wild: a bare path (`/ctl/AVT`)
/// that belongs to the advertising origin, and a full URL a device may emit
/// when its control server lives elsewhere (a TV that proxies to a stick).
pub fn resolve(renderer: &mut Renderer, base: &str) {
    renderer.base_url = base.to_string();
    for service in &mut renderer.services {
        let url = &service.control_url;
        if url.starts_with("http://") || url.starts_with("https://") {
            continue;
        }
        let path = if url.starts_with('/') {
            url.clone()
        } else {
            // Some firmware omits the leading slash.
            format!("/{url}")
        };
        service.control_url = format!("{}{}", base.trim_end_matches('/'), path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(location: &str) -> SsdpEntry {
        SsdpEntry {
            usn: "uuid:R-1::urn:schemas-upnp-org:device:MediaRenderer:1".into(),
            location: location.into(),
            method: "ssdp:alive".into(),
            target: crate::upnp::ssdp::MEDIA_RENDERER.into(),
            from: "192.168.1.9:1900".parse().unwrap(),
        }
    }

    const DOCUMENT: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion><major>1</major><minor>1</minor></specVersion>
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
    <friendlyName>Salon TV</friendlyName>
    <manufacturer>HORIZON</manufacturer>
    <modelName>Meeting Hub</modelName>
    <UDN>uuid:R-1</UDN>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>
        <controlURL>/ctl/AVTransport</controlURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:RenderingControl</serviceId>
        <controlURL>ctl/RenderingControl</controlURL>
      </service>
    </serviceList>
  </device>
</root>"#;

    #[test]
    fn reads_the_fields_that_identify_a_device() {
        let mut r = parse_description(DOCUMENT, entry("http://192.168.1.9:8290/dd.xml").usn.as_str());
        resolve(&mut r, "http://192.168.1.9:8290");
        assert_eq!(r.name, "Salon TV");
        assert_eq!(r.manufacturer, "HORIZON");
        assert_eq!(r.model, "Meeting Hub");
        assert_eq!(r.id, "uuid:R-1");
        assert!(r.has_av_transport());
    }

    #[test]
    fn pairs_each_control_url_with_its_own_service() {
        let mut r = parse_description(DOCUMENT, "uuid:R-1");
        resolve(&mut r, "http://192.168.1.9:8290");
        assert_eq!(
            r.control_url("urn:upnp-org:serviceId:AVTransport"),
            Some("http://192.168.1.9:8290/ctl/AVTransport")
        );
        // The missing leading slash is repaired, not dropped.
        assert_eq!(
            r.control_url("urn:upnp-org:serviceId:RenderingControl"),
            Some("http://192.168.1.9:8290/ctl/RenderingControl")
        );
        assert_eq!(r.control_url("urn:upnp-org:serviceId:Nope"), None);
    }

    #[test]
    fn keeps_an_absolute_control_url_as_is() {
        let doc = r#"<root><device><friendlyName>X</friendlyName>
          <service><serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>
          <controlURL>http://192.168.1.99:1234/avt</controlURL></service>
        </device></root>"#;
        let mut r = parse_description(doc, "uuid:X");
        resolve(&mut r, "http://192.168.1.9:8290");
        assert_eq!(r.control_url("urn:upnp-org:serviceId:AVTransport"), Some("http://192.168.1.99:1234/avt"));
    }

    #[test]
    fn a_service_block_without_a_control_url_is_skipped_not_misaligned() {
        let doc = r#"<root>
          <service><serviceId>A</serviceId></service>
          <service><serviceId>B</serviceId><controlURL>/b</controlURL></service>
        </root>"#;
        let mut r = parse_description(doc, "uuid:Y");
        resolve(&mut r, "http://h:1");
        assert_eq!(r.services.len(), 1);
        assert_eq!(r.services[0].service_id, "B");
        assert_eq!(r.services[0].control_url, "http://h:1/b");
    }

    #[test]
    fn describe_fetches_over_http_and_resolves() {
        let document = DOCUMENT.to_string();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let _ = crate::upnp::http::tests::read_request(&mut sock);
            let body = document.as_bytes();
            let head = format!("HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\n\r\n", body.len());
            let _ = std::io::Write::write_all(&mut sock, head.as_bytes());
            let _ = std::io::Write::write_all(&mut sock, body);
        });
        let e = entry(&format!("http://127.0.0.1:{port}/dd.xml"));
        let r = describe(&e).expect("a served description should parse");
        assert_eq!(r.name, "Salon TV");
        assert_eq!(r.base_url, format!("http://127.0.0.1:{port}"));
        assert!(r.control_url(AV_TRANSPORT).unwrap().ends_with("/ctl/AVTransport"));
    }

    #[test]
    fn describe_reports_a_dead_device_instead_of_hanging() {
        // Port 1 is closed on this machine; the bound connect timeout is what
        // makes this a fast failure rather than a stalled UI.
        let e = entry("http://127.0.0.1:1/dd.xml");
        let err = describe(&e).unwrap_err();
        assert!(matches!(err, HttpError::Io(_)), "{err:?}");
    }

    #[test]
    fn base_url_is_the_advertisement_origin() {
        // `resolve` is called with the origin taken from LOCATION; check the
        // trimming of a trailing slash so paths never double up.
        let mut r = parse_description(DOCUMENT, "uuid:R-1");
        resolve(&mut r, "http://192.168.1.9:8290/");
        assert_eq!(
            r.control_url("urn:upnp-org:serviceId:AVTransport"),
            Some("http://192.168.1.9:8290/ctl/AVTransport")
        );
    }

    /// A device that answers HTTP; `routes` maps a request path to the body it
    /// serves, anything else gets a 404. Runs until the test process ends.
    fn serve_document(routes: Vec<(&'static str, &'static str)>) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            while let Ok((mut sock, _)) = listener.accept() {
                let request = crate::upnp::http::tests::read_request(&mut sock);
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                let body = routes
                    .iter()
                    .find(|(p, _)| *p == path)
                    .map(|(_, b)| *b)
                    .unwrap_or("<html>not found</html>");
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = std::io::Write::write_all(&mut sock, reply.as_bytes());
                let _ = sock.shutdown(std::net::Shutdown::Both);
            }
        });
        port
    }

    #[test]
    fn manual_entry_finds_the_document_behind_a_bare_address() {
        // This device only serves its description at /description.xml, so a
        // probe of the usual paths is the only way in from `host:port` alone.
        let port = serve_document(vec![("/description.xml", DOCUMENT)]);
        let r = describe_by_address(&format!("127.0.0.1:{port}")).expect("a probe should land");
        assert_eq!(r.id, "uuid:R-1");
        assert_eq!(r.name, "Salon TV");
        assert_eq!(r.base_url, format!("http://127.0.0.1:{port}"));
        assert!(r.has_av_transport());
    }

    #[test]
    fn manual_entry_accepts_a_full_description_url() {
        let port = serve_document(vec![("/x/upnp/rootdevice.xml", DOCUMENT)]);
        let r = describe_by_address(&format!("http://127.0.0.1:{port}/x/upnp/rootdevice.xml"))
            .expect("a named path is used as given");
        assert_eq!(r.model, "Meeting Hub");
    }

    #[test]
    fn manual_entry_refuses_a_port_that_is_not_a_renderer() {
        // A web admin page answers 200 on every path; treating that as a
        // device would put an uncastable entry in the list.
        let port = serve_document(vec![("/", "<html><body>Login</body></html>")]);
        let e = describe_by_address(&format!("127.0.0.1:{port}")).unwrap_err();
        assert!(
            matches!(e, HttpError::Protocol(ref m) if m.contains("not a UPnP description")),
            "{e:?}"
        );
    }
}
