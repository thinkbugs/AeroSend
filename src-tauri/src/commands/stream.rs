use crate::airplay::StreamStatistics;
use crate::stream::StreamManager;
use crate::AppState;
use serde::Serialize;
use tauri::State;
use tracing::{info, error};

#[derive(Debug, Clone, Serialize)]
pub struct StreamStatsResponse {
    pub fps: f64,
    pub bitrate: f64,
    pub latency: f64,
    #[serde(rename = "packetLoss")]
    pub packet_loss: f64,
    pub resolution: String,
    pub codec: String,
    pub duration: u64,
}

impl From<&StreamStatistics> for StreamStatsResponse {
    fn from(s: &StreamStatistics) -> Self {
        Self {
            fps: s.fps,
            bitrate: s.bitrate,
            latency: s.latency_ms,
            packet_loss: s.packet_loss,
            resolution: s.resolution.clone(),
            codec: s.codec.clone(),
            duration: s.duration_secs,
        }
    }
}

#[tauri::command]
pub async fn stream_start(
    device_id: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    info!("Starting stream to device: {}", device_id);

    let mut manager = state.stream_manager.lock().map_err(|e| e.to_string())?;
    manager
        .start_stream(&device_id)
        .map_err(|e| format!("Failed to start stream: {}", e))?;

    Ok(())
}

#[tauri::command]
pub async fn stream_stop(state: State<'_, AppState>) -> Result<(), String> {
    info!("Stopping stream");

    let mut manager = state.stream_manager.lock().map_err(|e| e.to_string())?;
    manager.stop_stream();

    Ok(())
}

#[tauri::command]
pub async fn get_stream_stats(state: State<'_, AppState>) -> Result<StreamStatsResponse, String> {
    let manager = state.stream_manager.lock().map_err(|e| e.to_string())?;
    Ok(StreamStatsResponse::from(manager.stats()))
}
