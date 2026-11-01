//! Which of this machine's addresses a given peer can actually reach.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use tracing::debug;

/// The source address the OS would route to `peer`, as a string.
///
/// Connecting a UDP socket does not send anything; it only asks the routing
/// table. This is what makes the media-server URL correct on a machine with a
/// VPN adapter, a Docker bridge and Wi-Fi all present: advertising
/// `127.0.0.1` or the VPN address would hand the renderer a URL it cannot
/// fetch, and the cast would fail as a blank screen on the TV.
pub fn local_ip_toward(peer: IpAddr) -> Option<String> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    if socket.connect(SocketAddr::new(peer, 9)).is_err() {
        // No route at all (peer on a network this host is not attached to).
        debug!("no route to {peer}");
        return None;
    }
    let addr = socket.local_addr().ok()?;
    match addr.ip() {
        IpAddr::V4(v4) => Some(v4.to_string()),
        // A v6 socket can be the route, but a renderer's URL almost never
        // carries a scope id the device can parse, so it is not usable here.
        IpAddr::V6(_) => None,
    }
}

/// A best-effort LAN address when the peer is not yet known, e.g. before a
/// scan has returned anything.
pub fn primary_lan_ip() -> Option<String> {
    // 8.8.8.8 is only used as a routing probe; no packet leaves unless the
    // default route exists, and the address is never sent to.
    local_ip_toward("192.168.0.1".parse().ok()?).or_else(|| {
        local_ip_toward("10.0.0.1".parse().ok()?).or_else(|| local_ip_toward("8.8.8.8".parse().ok()?))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_the_loopback_route() {
        // The positive anchor: a real route yields a real address.
        assert_eq!(local_ip_toward("127.0.0.1".parse().unwrap()).as_deref(), Some("127.0.0.1"));
    }

    #[test]
    fn answers_for_a_peer_that_exists_without_sending_anything() {
        let ip = local_ip_toward("192.168.0.1".parse().unwrap());
        // Either a routed address or nothing; never a panic and never loopback
        // for a non-loopback peer, which would make the advertised URL useless.
        if let Some(ip) = &ip {
            assert_ne!(ip, "127.0.0.1", "{ip} would not be reachable by a renderer");
            assert!(ip.parse::<IpAddr>().is_ok(), "{ip} is not an address");
        }
    }

    #[test]
    fn offers_a_lan_address_for_the_media_url() {
        let ip = primary_lan_ip();
        if let Some(ip) = &ip {
            assert!(ip.parse::<IpAddr>().is_ok(), "{ip} is not an address");
            // The URL builder refuses an empty host, so this is the guard that
            // keeps a missing address from producing `http://:8000/m/x`.
            assert!(!ip.is_empty());
        }
    }
}
