pub mod discovery;
pub mod h264;
pub mod rtsp;
pub mod rtp;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AirPlayDevice {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub device_type: String,
    pub features: u64,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamConfig {
    pub device: AirPlayDevice,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub audio_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamStatistics {
    pub fps: f64,
    pub bitrate: f64,
    pub latency_ms: f64,
    pub packet_loss: f64,
    pub resolution: String,
    pub codec: String,
    pub duration_secs: u64,
    pub frames_sent: u64,
    pub bytes_sent: u64,
}

impl Default for StreamStatistics {
    fn default() -> Self {
        Self {
            fps: 0.0,
            bitrate: 0.0,
            latency_ms: 0.0,
            packet_loss: 0.0,
            resolution: "0x0".to_string(),
            codec: "H.264".to_string(),
            duration_secs: 0,
            frames_sent: 0,
            bytes_sent: 0,
        }
    }
}
