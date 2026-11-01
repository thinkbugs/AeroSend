pub mod session;

use session::StreamStats;
use tracing::info;

pub struct StreamManager {
    state: StreamState,
    stats: StreamStats,
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
            stats: StreamStats::default(),
        }
    }

    pub fn state(&self) -> &StreamState {
        &self.state
    }

    pub fn stats(&self) -> &StreamStats {
        &self.stats
    }

    pub fn start_stream(&mut self, device_id: &str) {
        info!("Starting stream to {}", device_id);
        self.state = StreamState::Connecting(device_id.to_string());
    }

    pub fn set_streaming(&mut self, device_id: &str) {
        self.state = StreamState::Streaming(device_id.to_string());
    }

    pub fn stop_stream(&mut self) {
        self.state = StreamState::Idle;
        info!("Stream stopped");
    }

    pub fn set_error(&mut self, err: String) {
        self.state = StreamState::Error(err);
    }

    pub fn update_stats(&mut self, stats: StreamStats) {
        self.stats = stats;
    }

    pub fn is_streaming(&self) -> bool {
        matches!(self.state, StreamState::Streaming(_))
    }
}
