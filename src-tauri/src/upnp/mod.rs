//! UPnP AV: casting a video, a photo or a song to a media renderer.
//!
//! The AirPlay path in `stream` encodes live pixels; this path does the
//! opposite — the file leaves untouched and the renderer pulls it over HTTP
//! while AVTransport drives playback. Two roles are therefore in play at once:
//! this app is a control point (it issues SOAP) and a media server (it serves
//! the bytes a local file is asked for).

pub mod avt;
pub mod caster;
pub mod device;
pub mod http;
pub mod media_server;
pub mod net;
pub mod ssdp;
pub mod xml;

pub use caster::{CastError, CastStatus, Caster, OfferedFile, RendererInfo};
