use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::airplay::rtsp::RtspClient;
use crate::airplay::rtp::{RtpPacketizer, RtcpSenderReport};
use crate::airplay::{StreamConfig, StreamStatistics};
use crate::capture::encoder::{EncoderType, VideoEncoder};
use crate::capture::screen::ScreenCapture;

pub struct StreamManager {
    state: StreamState,
    stats: StreamStatistics,
    start_time: Option<Instant>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamState {
    Idle,
    Connecting(String),
    Streaming(String),
    Error(String),
}

impl StreamManager {
    pub fn new() -> Self {
        Self {
            state: StreamState::Idle,
            stats: StreamStatistics::default(),
            start_time: None,
        }
    }

    pub fn state(&self) -> &StreamState {
        &self.state
    }

    pub fn stats(&self) -> &StreamStatistics {
        &self.stats
    }

    pub fn set_state(&mut self, state: StreamState) {
        info!("Stream state: {:?}", state);
        self.state = state;
    }

    pub fn start_stream(&mut self, device_id: &str) -> Result<()> {
        self.set_state(StreamState::Connecting(device_id.to_string()));
        self.start_time = Some(Instant::now());
        self.stats = StreamStatistics::default();
        self.stats.codec = "H.264".to_string();
        Ok(())
    }

    pub fn stop_stream(&mut self) {
        if let Some(start) = self.start_time {
            self.stats.duration_secs = start.elapsed().as_secs();
        }
        self.set_state(StreamState::Idle);
        self.start_time = None;
        info!("Stream stopped. Total duration: {}s", self.stats.duration_secs);
    }

    pub fn update_stats(&mut self, fps: f64, bitrate: f64, latency_ms: f64) {
        self.stats.fps = fps;
        self.stats.bitrate = bitrate;
        self.stats.latency_ms = latency_ms;
        if let Some(start) = self.start_time {
            self.stats.duration_secs = start.elapsed().as_secs();
        }
    }

    pub fn is_streaming(&self) -> bool {
        matches!(self.state, StreamState::Streaming(_))
    }
}
