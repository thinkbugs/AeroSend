//! The cast controller: what the UI drives.
//!
//! Discovery, the media server that makes a local file fetchable, and one
//! playback state per renderer. Blocking UPnP calls (each one is a socket round
//! trip with a timeout) run here rather than in a Tauri command body, so the
//! commands can be `spawn_blocking` wrappers without knowing the details.

use crate::upnp::avt::{self, SoapError};
use crate::upnp::device::{self, Renderer, AV_TRANSPORT, RENDERING_CONTROL};
use crate::upnp::media_server::{MediaFile, MediaServer};
use crate::upnp::net;
use crate::upnp::ssdp::{self, SsdpEntry};
use serde::Serialize;
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::collections::HashSet;
use tracing::{debug, info, warn};

/// How long one scan window stays open for replies.
pub const SCAN_WINDOW: std::time::Duration = std::time::Duration::from_millis(2500);
/// Playback position refresh. A renderer is a remote box; polling at 1 Hz is
/// what its own progress bar looks like anyway.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1000);

/// A renderer as the UI sees it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RendererInfo {
    pub id: String,
    pub name: String,
    pub model: String,
    pub manufacturer: String,
    pub base_url: String,
    /// Whether this device can be cast to at all.
    pub castable: bool,
    pub reachable: bool,
    pub error: Option<String>,
    /// True while one of our casts is live on it.
    pub playing: bool,
}

/// The state of the cast on one renderer.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CastStatus {
    pub renderer_id: String,
    pub title: String,
    pub uri: String,
    /// `PLAYING`, `PAUSED_PLAYBACK`, `STOPPED`, `NO_MEDIA_PRESENT` or
    /// `unknown` when the device would not say.
    pub state: String,
    pub seconds: f64,
    pub total_seconds: f64,
    /// Bytes this machine has handed to renderers overall.
    pub bytes_served: u64,
    pub active: bool,
    pub error: Option<String>,
}

/// One entry in the local queue that has been offered to the media server.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OfferedFile {
    pub path: String,
    pub title: String,
    pub url: String,
    pub mime: String,
    pub size: u64,
}

#[derive(Debug)]
pub enum CastError {
    /// The renderer is not known to the controller (a stale selection).
    UnknownRenderer(String),
    /// Known, but it publishes no AVTransport, so it is not a player.
    NotCastable(String),
    NoLanAddress,
    Io(String),
    Soap(String),
}

impl std::fmt::Display for CastError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownRenderer(id) => write!(f, "renderer {id} is not in the list; rescan"),
            Self::NotCastable(name) => write!(f, "{name} does not offer AVTransport"),
            Self::NoLanAddress => write!(f, "no LAN address: is this machine on a network?"),
            Self::Io(m) => write!(f, "{m}"),
            Self::Soap(m) => write!(f, "{m}"),
        }
    }
}

impl From<std::io::Error> for CastError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

impl From<SoapError> for CastError {
    fn from(e: SoapError) -> Self {
        Self::Soap(e.to_string())
    }
}

/// The stop flags of every running poller, keyed by renderer, so a new cast
/// replaces its own old poller instead of racing it.
type Pollers = HashMap<String, Arc<AtomicBool>>;

pub struct Caster {
    renderers: Arc<Mutex<HashMap<String, SsdpEntry>>>,
    described: Arc<Mutex<HashMap<String, Renderer>>>,
    status: Arc<Mutex<HashMap<String, CastStatus>>>,
    pollers: Arc<Mutex<Pollers>>,
    media: Arc<Mutex<Option<Arc<MediaServer>>>>,
    offered: Arc<Mutex<Vec<OfferedFile>>>,
    /// Scans in a row a device has missed. Two is the threshold because a
    /// single lost datagram on Wi-Fi is routine, and a device that answered
    /// the first and misses the second is gone.
    misses: Arc<Mutex<HashMap<String, u8>>>,
    /// Ids the user typed in by hand. These are pinned: a device behind a
    /// switch that swallows multicast never answers a scan, and dropping it
    /// because of that would make manual entry useless.
    manual: Arc<Mutex<HashSet<String>>>,
}

impl Default for Caster {
    fn default() -> Self {
        Self::new()
    }
}

impl Caster {
    pub fn new() -> Self {
        Self {
            renderers: Arc::new(Mutex::new(HashMap::new())),
            described: Arc::new(Mutex::new(HashMap::new())),
            status: Arc::new(Mutex::new(HashMap::new())),
            pollers: Arc::new(Mutex::new(HashMap::new())),
            media: Arc::new(Mutex::new(None)),
            offered: Arc::new(Mutex::new(Vec::new())),
            misses: Arc::new(Mutex::new(HashMap::new())),
            manual: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Find renderers.
    ///
    /// Already-known devices survive a scan that missed them, so a cast in
    /// progress is never dropped from the list because of one lost datagram.
    pub fn scan(&self) -> Vec<RendererInfo> {
        let entries = ssdp::search(ssdp::MEDIA_RENDERER, SCAN_WINDOW);
        info!("SSDP found {} renderer advertisements", entries.len());
        let mut out = Vec::new();
        let mut seen = HashSet::new();

        for entry in entries {
            let id = entry.usn.split("::").next().unwrap_or(&entry.usn).to_string();
            let known = self.renderers.lock().unwrap().contains_key(&id);
            if !known {
                self.renderers.lock().unwrap().insert(id.clone(), entry.clone());
            }
            seen.insert(id.clone());
            // Description is cached: fetching it per scan would re-hit every
            // device on the LAN once a second for the life of the app. A device
            // that was unreachable is retried, since that is usually a sleeping
            // box that has woken.
            let fresh = {
                let d = self.described.lock().unwrap();
                match d.get(&id) {
                    Some(r) => !r.reachable,
                    None => true,
                }
            };
            if fresh {
                let renderer = self.describe(&entry, &id);
                self.described.lock().unwrap().insert(id.clone(), renderer);
            }
            if !known {
                let guard = self.described.lock().unwrap();
                if let Some(r) = guard.get(&id) {
                    out.push(self.info(r));
                }
            }
        }

        self.prune_missing(&seen);
        out
    }

    /// Forget devices that stopped answering, unless we are still casting to
    /// them — a renderer that is playing does not need to answer a search.
    fn prune_missing(&self, seen: &HashSet<String>) {
        // The key list is copied out first: asking for a second guard while the
        // first is still alive in the same statement self-deadlocks.
        let ids: Vec<String> = self.renderers.lock().unwrap().keys().cloned().collect();
        let mut misses = self.misses.lock().unwrap();
        let pinned = self.manual.lock().unwrap();
        let mut stale = Vec::new();
        for id in &ids {
            if pinned.contains(id) {
                misses.remove(id);
                continue;
            }
            if seen.contains(id) {
                misses.remove(id);
                continue;
            }
            let count = misses.entry(id.clone()).or_insert(0);
            *count += 1;
            if *count >= 2 {
                stale.push(id.clone());
            }
        }
        let playing = self
            .status
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, s)| s.active)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in stale {
            if playing.contains(&id) {
                continue;
            }
            misses.remove(&id);
            self.renderers.lock().unwrap().remove(&id);
            self.described.lock().unwrap().remove(&id);
            self.status.lock().unwrap().remove(&id);
            self.stop_polling(&id);
        }
    }

    fn describe(&self, entry: &SsdpEntry, id: &str) -> Renderer {
        match device::describe(entry) {
            Ok(mut r) => {
                if let Some(base) = origin(&entry.location) {
                    device::resolve(&mut r, &base);
                }
                r.id = id.to_string();
                info!("renderer {} at {}", r.name, r.base_url);
                r
            }
            Err(e) => {
                // Keeping an unreachable entry is deliberate: the UI shows the
                // device with its reason instead of silently dropping it, so a
                // firewall rule looks like a firewall rule and not an absence.
                warn!("renderer {id} did not answer ({e})");
                Renderer {
                    id: id.to_string(),
                    name: id.to_string(),
                    model: String::new(),
                    manufacturer: String::new(),
                    base_url: entry.location.clone(),
                    services: Vec::new(),
                    reachable: false,
                    error: Some(e.to_string()),
                }
            }
        }
    }

    pub fn renderers(&self) -> Vec<RendererInfo> {
        let guard = self.described.lock().unwrap();
        guard.values().map(|r| self.info(r)).collect()
    }

    /// Add a renderer the user named instead of one SSDP announced, accepting a
    /// bare `192.168.1.9:8290`, an address, or a full description URL.
    ///
    /// The result is the same shape a scan gives, so the list needs no second
    /// code path; a device with no AVTransport is still shown, as not castable,
    /// rather than reported as missing.
    pub fn add_by_location(&self, input: &str) -> Result<RendererInfo, CastError> {
        let renderer = device::describe_by_address(input)
            .map_err(|e| CastError::Io(e.to_string()))?;
        let id = renderer.id.clone();
        let entry = SsdpEntry {
            usn: id.clone(),
            location: renderer.base_url.clone(),
            method: "aerosend:manual".into(),
            target: ssdp::MEDIA_RENDERER.into(),
            // No SSDP packet arrived, so there is no source address to keep.
            from: "0.0.0.0:0".parse().unwrap(),
        };
        self.renderers.lock().unwrap().insert(id.clone(), entry);
        self.described.lock().unwrap().insert(id.clone(), renderer.clone());
        self.manual.lock().unwrap().insert(id);
        Ok(self.info(&renderer))
    }

    /// Forget a renderer, and stop following its playback position.
    pub fn remove(&self, renderer_id: &str) -> bool {
        let known = self.renderers.lock().unwrap().remove(renderer_id).is_some();
        self.described.lock().unwrap().remove(renderer_id);
        self.misses.lock().unwrap().remove(renderer_id);
        self.manual.lock().unwrap().remove(renderer_id);
        self.status.lock().unwrap().remove(renderer_id);
        self.stop_polling(renderer_id);
        known
    }

    fn info(&self, r: &Renderer) -> RendererInfo {
        let playing = self
            .status
            .lock()
            .unwrap()
            .get(&r.id)
            .map(|s| s.active && s.state == "PLAYING")
            .unwrap_or(false);
        RendererInfo {
            id: r.id.clone(),
            name: r.name.clone(),
            model: r.model.clone(),
            manufacturer: r.manufacturer.clone(),
            base_url: r.base_url.clone(),
            castable: r.has_av_transport(),
            reachable: r.reachable,
            error: r.error.clone(),
            playing,
        }
    }

    /// Register a local file with the media server and return its URL.
    ///
    /// The address used is the one routed toward *this* renderer, which is why
    /// the peer is a parameter rather than a global guess.
    pub fn offer(&self, renderer_id: &str, path: &Path) -> Result<OfferedFile, CastError> {
        let peer_ip = self.peer_ip(renderer_id)?;
        let host = net::local_ip_toward(peer_ip).ok_or(CastError::NoLanAddress)?;
        let server = self.ensure_media_server()?;
        let file: MediaFile = server
            .offer(path)
            .map_err(|e| CastError::Io(e.message))?;
        let url = file
            .url_for(&host, server.port())
            .map_err(|e| CastError::Io(e.message))?;
        let entry = OfferedFile {
            path: path.display().to_string(),
            title: file.title.clone(),
            url: url.clone(),
            mime: file.mime.clone(),
            size: file.size,
        };
        {
            let mut offered = self.offered.lock().unwrap();
            if !offered.iter().any(|o| o.url == entry.url) {
                offered.push(entry.clone());
            }
        }
        Ok(entry)
    }

    pub fn offered(&self) -> Vec<OfferedFile> {
        self.offered.lock().unwrap().clone()
    }

    fn ensure_media_server(&self) -> Result<Arc<MediaServer>, CastError> {
        let mut guard = self.media.lock().unwrap();
        if let Some(server) = guard.as_ref() {
            return Ok(Arc::clone(server));
        }
        let server = Arc::new(
            MediaServer::start().map_err(|e| CastError::Io(format!("media server: {}", e.message)))?,
        );
        server.serve();
        info!("media server listening on :{}", server.port());
        *guard = Some(Arc::clone(&server));
        Ok(server)
    }

    /// The renderer's own address, taken from its advertised LOCATION.
    fn peer_ip(&self, renderer_id: &str) -> Result<IpAddr, CastError> {
        let entry = self
            .renderers
            .lock()
            .unwrap()
            .get(renderer_id)
            .cloned()
            .ok_or_else(|| CastError::UnknownRenderer(renderer_id.to_string()))?;
        host_of(&entry.location).ok_or_else(|| {
            CastError::Io(format!(
                "{} is not an address a local file can be served to",
                entry.location
            ))
        })
    }

    /// Push a URI — either a file already offered or any http URL the renderer
    /// can reach — and start playback.
    pub fn cast(&self, renderer_id: &str, uri: &str, title: &str) -> Result<CastStatus, CastError> {
        let av = {
            let guard = self.described.lock().unwrap();
            let r = guard
                .get(renderer_id)
                .ok_or_else(|| CastError::UnknownRenderer(renderer_id.to_string()))?;
            r.control_url(AV_TRANSPORT)
                .map(|s| s.to_string())
                .ok_or_else(|| CastError::NotCastable(r.name.clone()))?
        };

        // A file we are serving has an id for a URL, not an extension, so its
        // real type is only knowable from the offer.
        let mime = self.mime_for(uri);
        let metadata = avt::didl_item(
            title,
            uri,
            &mime,
            self.size_of(uri),
            avt::upnp_class_for(&mime),
        );
        info!("casting {title} to {renderer_id}: {uri}");
        avt::play_uri(&av, uri, &metadata)?;

        let status = CastStatus {
            renderer_id: renderer_id.to_string(),
            title: title.to_string(),
            uri: uri.to_string(),
            state: "PLAYING".to_string(),
            seconds: 0.0,
            total_seconds: 0.0,
            bytes_served: self.bytes_served(),
            active: true,
            error: None,
        };
        self.status
            .lock()
            .unwrap()
            .insert(renderer_id.to_string(), status.clone());
        self.start_polling(renderer_id, &av);
        Ok(status)
    }

    /// Cast a local file end to end: offer it, then push it.
    pub fn cast_file(&self, renderer_id: &str, path: &Path) -> Result<CastStatus, CastError> {
        let offered = self.offer(renderer_id, path)?;
        self.cast(renderer_id, &offered.url, &offered.title)
            .map_err(|e| {
                // A refusal after the file was registered is still worth the
                // name in the log.
                warn!("cast of {} failed: {e}", offered.title);
                e
            })
    }

    pub fn transport_action(&self, renderer_id: &str, action: &str) -> Result<CastStatus, CastError> {
        let av = self.av_url(renderer_id)?;
        avt::simple_action(&av, action, &[])?;
        // The device's own answer is the truth about what happened.
        let info = avt::transport_info(&av).unwrap_or_default();
        let position = avt::position_info(&av).unwrap_or_default();
        let stopped = info.is_stopped();
        let mut status = self
            .status
            .lock()
            .unwrap()
            .get(renderer_id)
            .cloned()
            .unwrap_or_default();
        status.renderer_id = renderer_id.to_string();
        status.state = info.current;
        status.seconds = position.seconds;
        status.total_seconds = position.total_seconds;
        status.active = !stopped;
        if action == "Stop" {
            status.active = false;
            self.stop_polling(renderer_id);
        } else {
            self.start_polling(renderer_id, &av);
        }
        self.status
            .lock()
            .unwrap()
            .insert(renderer_id.to_string(), status.clone());
        Ok(status)
    }

    pub fn seek(&self, renderer_id: &str, seconds: f64) -> Result<CastStatus, CastError> {
        let av = self.av_url(renderer_id)?;
        avt::seek(&av, seconds)?;
        let mut guard = self.status.lock().unwrap();
        if let Some(status) = guard.get_mut(renderer_id) {
            status.seconds = seconds;
        }
        Ok(guard.get(renderer_id).cloned().unwrap_or_default())
    }

    pub fn set_volume(&self, renderer_id: &str, volume: u16) -> Result<(), CastError> {
        avt::set_volume(&self.rc_url(renderer_id)?, volume)?;
        Ok(())
    }

    /// The volume the device itself reports, so the UI can show its number
    /// rather than the last one we happened to send.
    pub fn volume(&self, renderer_id: &str) -> Result<u16, CastError> {
        Ok(avt::get_volume(&self.rc_url(renderer_id)?)?)
    }

    fn rc_url(&self, renderer_id: &str) -> Result<String, CastError> {
        let guard = self.described.lock().unwrap();
        let r = guard
            .get(renderer_id)
            .ok_or_else(|| CastError::UnknownRenderer(renderer_id.to_string()))?;
        r.control_url(RENDERING_CONTROL)
            .map(|s| s.to_string())
            .ok_or_else(|| CastError::NotCastable(r.name.clone()))
    }

    fn av_url(&self, renderer_id: &str) -> Result<String, CastError> {
        let guard = self.described.lock().unwrap();
        let r = guard
            .get(renderer_id)
            .ok_or_else(|| CastError::UnknownRenderer(renderer_id.to_string()))?;
        r.control_url(AV_TRANSPORT)
            .map(|s| s.to_string())
            .ok_or_else(|| CastError::NotCastable(r.name.clone()))
    }

    fn size_of(&self, uri: &str) -> Option<u64> {
        let id = served_id(uri)?;
        let guard = self.media.lock().unwrap();
        let server = guard.as_ref()?;
        server
            .files()
            .iter()
            .find(|f| f.id == id)
            .map(|f| f.size)
    }

    /// The MIME to announce for a URI in DIDL-Lite.
    ///
    /// A file we serve is addressed by an id we minted (`/m/45bd36a3.mkv`); the
    /// extension on it is decoration for the receiver, and the registered file
    /// is the authority — a device that renamed the clip would otherwise be
    /// trusted over the bytes we actually have.
    fn mime_for(&self, uri: &str) -> String {
        let registered = served_id(uri).and_then(|id| {
            self.media
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|server| {
                    server
                        .files()
                        .iter()
                        .find(|f| f.id == id)
                        .map(|f| f.mime.clone())
                })
        });
        registered.unwrap_or_else(|| mime_of(uri))
    }

    /// Poll one renderer's position until the cast stops or the device stops
    /// answering, so a progress bar reflects the TV rather than our own clock.
    fn start_polling(&self, renderer_id: &str, av_url: &str) {
        let key = renderer_id.to_string();
        let already = self
            .pollers
            .lock()
            .unwrap()
            .get(&key)
            .map(|flag| flag.load(Ordering::SeqCst))
            .unwrap_or(false);
        if already {
            return;
        }
        let flag = Arc::new(AtomicBool::new(true));
        self.pollers.lock().unwrap().insert(key.clone(), Arc::clone(&flag));

        let status = Arc::clone(&self.status);
        let pollers = Arc::clone(&self.pollers);
        let av = av_url.to_string();
        std::thread::spawn(move || {
            while flag.load(Ordering::SeqCst) {
                std::thread::sleep(POLL_INTERVAL);
                if !flag.load(Ordering::SeqCst) {
                    break;
                }
                match avt::transport_info(&av) {
                    Ok(info) => {
                        let position = avt::position_info(&av).unwrap_or_default();
                        let stopped = info.is_stopped();
                        let state = info.current;
                        let mut guard = status.lock().unwrap();
                        if let Some(s) = guard.get_mut(&key) {
                            s.state = state;
                            s.seconds = position.seconds;
                            s.total_seconds = position.total_seconds;
                            // A device that stopped on its own (end of file)
                            // ends the session without us guessing a duration.
                            s.active = !stopped;
                        }
                        if stopped {
                            break;
                        }
                    }
                    Err(e) => {
                        // One lost poll is a Wi-Fi hiccup; the next tick retries.
                        // The flag is cleared so a dead renderer does not spin.
                        warn!("position poll failed: {e}");
                        if let Some(s) = status.lock().unwrap().get_mut(&key) {
                            s.error = Some(e.to_string());
                        }
                        break;
                    }
                }
            }
            pollers.lock().unwrap().remove(&key);
        });
    }

    fn stop_polling(&self, renderer_id: &str) {
        if let Some(flag) = self.pollers.lock().unwrap().remove(renderer_id) {
            flag.store(false, Ordering::SeqCst);
        }
    }

    pub fn status(&self, renderer_id: &str) -> Option<CastStatus> {
        let mut s = self.status.lock().unwrap().get(renderer_id).cloned()?;
        s.bytes_served = self.bytes_served();
        Some(s)
    }

    pub fn all_status(&self) -> Vec<CastStatus> {
        let bytes = self.bytes_served();
        self.status
            .lock()
            .unwrap()
            .values()
            .map(|s| {
                let mut s = s.clone();
                s.bytes_served = bytes;
                s
            })
            .collect()
    }

    pub fn bytes_served(&self) -> u64 {
        self.media
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.bytes_served())
            .unwrap_or(0)
    }

    /// Stop every cast and close the pollers; called on shutdown so a renderer
    /// is not left playing after the window is gone.
    pub fn shutdown(&self) {
        for id in self.status.lock().unwrap().keys().cloned() {
            if let Ok(av) = self.av_url(&id) {
                if let Err(e) = avt::simple_action(&av, "Stop", &[]) {
                    debug!("stop of {id} failed: {e}");
                }
            }
        }
        for flag in self.pollers.lock().unwrap().values() {
            flag.store(false, Ordering::SeqCst);
        }
        self.pollers.lock().unwrap().clear();
        if let Some(server) = self.media.lock().unwrap().as_ref() {
            server.stop();
        }
    }
}

/// `http://host:port/anything` to `http://host:port`.
fn origin(location: &str) -> Option<String> {
    let rest = location.strip_prefix("http://")?;
    let end = rest.find('/')?;
    Some(format!("http://{}", &rest[..end]))
}

/// The host part of a LOCATION, as an address, or None when the device
/// advertised a hostname or an address this controller cannot serve to.
fn host_of(location: &str) -> Option<IpAddr> {
    let rest = location
        .strip_prefix("http://")
        .or_else(|| location.strip_prefix("https://"))
        .unwrap_or(location);
    let host = rest.split('/').next().unwrap_or(rest);
    let host = host.split(':').next().unwrap_or(host);
    host.trim_matches(['[', ']']).parse().ok()
}

/// The MIME a DMR should be told about a URI we did not register ourselves.
///
/// Extension-based, for the same reason the hub's own queue does it: an
/// address like `/media/12` carries no hint, and `application/octet-stream`
/// makes most devices refuse the stream outright.
fn mime_of(uri: &str) -> String {
    crate::upnp::media_server::mime_from_extension(uri)
}

/// The media-server id inside a URI we minted, or `None` for any other address.
///
/// The advertised URL is `/m/<id>.<ext>` — the extension is there so a receiver
/// that reads only the address can tell a photo from a film — so the id is what
/// precedes the first dot, query or fragment.
fn served_id(uri: &str) -> Option<&str> {
    let rest = uri.split("/m/").nth(1)?;
    let id = rest.split(['?', '#', '.']).next().unwrap_or("");
    (!id.is_empty()).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_and_host_extraction_handle_the_shapes_devices_advertise() {
        assert_eq!(
            origin("http://192.168.1.9:8290/dd.xml").as_deref(),
            Some("http://192.168.1.9:8290")
        );
        assert_eq!(
            origin("http://192.168.1.9:8290"),
            None,
            "no path means no split point"
        );
        assert_eq!(
            host_of("http://192.168.1.9:8290/dd.xml").map(|a| a.to_string()).as_deref(),
            Some("192.168.1.9")
        );
        assert_eq!(
            host_of("http://10.0.0.7/d.xml").map(|a| a.to_string()).as_deref(),
            Some("10.0.0.7")
        );
        // A name is not something a local file can be served behind without a
        // resolver, and v6 has no scope id a renderer can parse: both refuse
        // the cast instead of advertising a URL that would silently 404.
        assert!(host_of("http://tv.local:8290/d.xml").is_none());
        assert!(host_of("http://[fe80::1%257]:9/d.xml").is_none());
        assert!(host_of("nonsense").is_none());
    }

    #[test]
    fn mime_of_follows_the_extension_of_a_uri() {
        assert_eq!(mime_of("http://h:1/m/abc"), "application/octet-stream");
        assert_eq!(mime_of("Z:/videos/clip.mkv"), "video/x-matroska");
    }

    #[test]
    fn a_served_file_is_announced_with_its_real_type() {
        // The served URL is `/m/<id>.<ext>`: the extension is a hint for a
        // receiver that reads only the address, and the registered file stays
        // the authority on what the bytes are.
        let caster = Caster::new();
        let path = temp_media_file("mime.mp4");
        let served = caster.ensure_media_server().unwrap();
        let file = served.offer(&path).unwrap();
        let url = file.url_for("192.168.0.9", served.port()).unwrap();
        assert!(url.ends_with(&format!("/m/{}.mp4", file.id)), "{url}");
        assert_eq!(caster.mime_for(&url), "video/mp4");
        assert_eq!(caster.size_of(&url), Some(file.size));
        // A URL from somewhere else still gets the extension reading.
        assert_eq!(caster.mime_for("http://h/stream.mkv"), "video/x-matroska");
        assert_eq!(caster.size_of("http://h/stream.mkv"), None);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_served_id_survives_the_extension_and_a_query_a_device_adds() {
        assert_eq!(served_id("http://h:1/m/45bd36a3.jpg"), Some("45bd36a3"));
        assert_eq!(served_id("http://h:1/m/45bd36a3"), Some("45bd36a3"));
        assert_eq!(served_id("http://h:1/m/45bd36a3.mp4?x=1"), Some("45bd36a3"));
        assert_eq!(served_id("http://h:1/m/45bd36a3.mp4#t=10"), Some("45bd36a3"));
        // Not ours: no id to look up, so the caller reads the extension instead.
        assert_eq!(served_id("http://h/files/a.mkv"), None);
        assert_eq!(served_id("http://h/m/"), None);
    }

    /// A real file on disk: the media server reads its length and extension, so
    /// a zero-byte or extensionless name would not exercise the offer path.
    fn temp_media_file(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        // The pid keeps concurrent runs apart, and it has to come before the
        // extension — a suffix after `.mp4` makes the file unrecognisable.
        path.push(format!("aerosend-{}-{name}", std::process::id()));
        std::fs::write(&path, [0x5Au8; 2048]).unwrap();
        path
    }

    #[test]
    fn unknown_and_uncastable_renderers_are_named_not_blamed_on_the_file() {
        let caster = Caster::new();
        let e = caster.cast("uuid:gone", "http://h/a.mp4", "A").unwrap_err();
        assert!(
            matches!(e, CastError::UnknownRenderer(ref id) if id == "uuid:gone"),
            "{e:?}"
        );
        let e = caster.transport_action("uuid:gone", "Play").unwrap_err();
        assert!(matches!(e, CastError::UnknownRenderer(_)), "{e:?}");
    }

    #[test]
    fn a_device_without_avtransport_is_reported_as_not_castable() {
        let caster = Caster::new();
        let renderer = Renderer {
            id: "uuid:x".into(),
            name: "Smart Bulb".into(),
            model: String::new(),
            manufacturer: String::new(),
            base_url: "http://192.168.1.2:80".into(),
            services: vec![device::ServiceEndpoint {
                service_type: device::RENDERING_CONTROL.into(),
                service_id: "urn:upnp-org:serviceId:RenderingControl".into(),
                control_url: "http://192.168.1.2:80/ctl/rc".into(),
            }],
            reachable: true,
            error: None,
        };
        caster.described.lock().unwrap().insert("uuid:x".into(), renderer);
        let info = caster.renderers();
        assert_eq!(info.len(), 1);
        assert!(!info[0].castable, "a bulb cannot be cast to");
        let e = caster.cast("uuid:x", "http://h/a.mp4", "A").unwrap_err();
        assert!(
            matches!(e, CastError::NotCastable(ref name) if name == "Smart Bulb"),
            "{e:?}"
        );
    }

    #[test]
    fn status_is_absent_until_a_cast_exists_and_complete_after_one() {
        let caster = Caster::new();
        assert!(caster.status("uuid:none").is_none());
        assert!(caster.all_status().is_empty());
        let mut s = CastStatus::default();
        s.renderer_id = "uuid:one".into();
        s.active = true;
        s.state = "PLAYING".into();
        caster.status.lock().unwrap().insert("uuid:one".into(), s);
        let read = caster.status("uuid:one").unwrap();
        assert!(read.active);
        assert_eq!(caster.all_status().len(), 1);
    }

    #[test]
    fn a_device_that_misses_two_scans_is_forgotten_and_one_that_misses_one_is_not() {
        let caster = Caster::new();
        let id = "uuid:gone";
        caster.renderers.lock().unwrap().insert(
            id.into(),
            SsdpEntry {
                usn: format!("{id}::urn:schemas-upnp-org:device:MediaRenderer:1"),
                location: "http://192.168.1.5/d.xml".into(),
                method: "ssdp:alive".into(),
                target: ssdp::MEDIA_RENDERER.into(),
                from: "192.168.1.5:1900".parse().unwrap(),
            },
        );
        caster.described.lock().unwrap().insert(
            id.into(),
            Renderer {
                id: id.into(),
                name: "Gone".into(),
                model: String::new(),
                manufacturer: String::new(),
                base_url: String::new(),
                services: Vec::new(),
                reachable: true,
                error: None,
            },
        );
        let flag = Arc::new(AtomicBool::new(true));
        caster.pollers.lock().unwrap().insert(id.into(), Arc::clone(&flag));

        let empty = HashSet::new();
        caster.prune_missing(&empty);
        assert_eq!(caster.renderers().len(), 1, "a single lost reply keeps the device");
        assert!(flag.load(Ordering::SeqCst));

        caster.prune_missing(&empty);
        assert!(caster.renderers().is_empty(), "the second miss removes it");
        assert!(!flag.load(Ordering::SeqCst), "and its poller was stopped");
    }

    #[test]
    fn a_device_still_playing_is_kept_even_when_it_stops_answering() {
        let caster = Caster::new();
        let id = "uuid:busy";
        caster.renderers.lock().unwrap().insert(
            id.into(),
            SsdpEntry {
                usn: id.into(),
                location: "http://192.168.1.5/d.xml".into(),
                method: "ssdp:alive".into(),
                target: ssdp::MEDIA_RENDERER.into(),
                from: "192.168.1.5:1900".parse().unwrap(),
            },
        );
        caster.status.lock().unwrap().insert(
            id.into(),
            CastStatus {
                renderer_id: id.into(),
                title: "Film".into(),
                uri: "http://192.168.1.20:9/m/x".into(),
                state: "PLAYING".into(),
                seconds: 10.0,
                total_seconds: 100.0,
                bytes_served: 0,
                active: true,
                error: None,
            },
        );
        let empty = HashSet::new();
        for _ in 0..5 {
            caster.prune_missing(&empty);
        }
        assert_eq!(
            caster.status(id).map(|s| s.state),
            Some("PLAYING".to_string()),
            "the progress of a live cast must survive a silent device"
        );
    }

    #[test]
    fn a_device_that_answers_has_its_miss_counter_cleared() {
        let caster = Caster::new();
        let id = "uuid/flaky";
        caster.renderers.lock().unwrap().insert(
            id.into(),
            SsdpEntry {
                usn: id.into(),
                location: "http://192.168.1.5/d.xml".into(),
                method: "ssdp:alive".into(),
                target: ssdp::MEDIA_RENDERER.into(),
                from: "192.168.1.5:1900".parse().unwrap(),
            },
        );
        // A scan that lists a device also caches its description, so the
        // user-visible list is what a prune has to leave standing.
        caster
            .described
            .lock()
            .unwrap()
            .insert(id.into(), flaky_renderer(id));
        let empty = HashSet::new();
        caster.prune_missing(&empty);
        assert_eq!(caster.misses.lock().unwrap().get(id), Some(&1));
        let mut seen = HashSet::new();
        seen.insert(id.to_string());
        caster.prune_missing(&seen);
        assert!(!caster.misses.lock().unwrap().contains_key(id), "the streak was reset");
        // Two more isolated losses must still not drop it.
        caster.prune_missing(&empty);
        caster.prune_missing(&seen);
        caster.prune_missing(&empty);
        assert_eq!(caster.renderers().len(), 1, "one miss in a streak is not a farewell");
        assert_eq!(caster.misses.lock().unwrap().get(id), Some(&1));
    }

    /// The description a real scan leaves in the cache for a castable renderer.
    fn flaky_renderer(id: &str) -> Renderer {
        Renderer {
            id: id.into(),
            name: "Salon TV".into(),
            model: String::new(),
            manufacturer: String::new(),
            base_url: "http://192.168.1.5:8290".into(),
            services: vec![device::ServiceEndpoint {
                service_type: device::AV_TRANSPORT.into(),
                service_id: "urn:upnp-org:serviceId:AVTransport".into(),
                control_url: "http://192.168.1.5:8290/ctl/AVTransport".into(),
            }],
            reachable: true,
            error: None,
        }
    }

    #[test]
    fn a_pinned_renderer_survives_scans_that_never_see_it() {
        // A renderer behind a switch that swallows multicast answers no scan at
        // all; the entry the user typed must not be pruned for being quiet.
        let caster = Caster::new();
        let id = "uuid:pinned";
        caster.renderers.lock().unwrap().insert(
            id.into(),
            SsdpEntry {
                usn: id.into(),
                location: "http://192.168.1.5:8290".into(),
                method: "aerosend:manual".into(),
                target: ssdp::MEDIA_RENDERER.into(),
                from: "0.0.0.0:0".parse().unwrap(),
            },
        );
        caster
            .described
            .lock()
            .unwrap()
            .insert(id.into(), flaky_renderer(id));
        caster.manual.lock().unwrap().insert(id.into());

        let empty = HashSet::new();
        caster.prune_missing(&empty);
        caster.prune_missing(&empty);
        assert_eq!(caster.renderers().len(), 1, "a pinned device is not a stale one");
        assert!(caster.misses.lock().unwrap().get(id).is_none());

        assert!(caster.remove(id), "the user's own entry can be deleted");
        assert!(caster.renderers().is_empty());
        assert!(!caster.remove(id), "deleting it twice is not a success");
    }

    #[test]
    fn offering_needs_a_renderer_we_know_the_address_of() {
        // The positive anchors are in test_upnp_e2e, where a full scan and cast
        // run against a fake DMR; this proves a stale id fails early.
        let caster = Caster::new();
        let e = caster
            .offer("uuid:missing", Path::new("C:/x.mp4"))
            .unwrap_err();
        assert!(matches!(e, CastError::UnknownRenderer(_)), "{e:?}");
    }
}
