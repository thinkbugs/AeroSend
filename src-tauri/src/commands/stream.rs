use crate::stream::session::{StreamSession, StreamStats};
use crate::AppState;
use serde::Serialize;
use std::sync::atomic::Ordering;
use tauri::State;
use tracing::{error, info};

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
    pub frames_captured: u64,
    pub bytes_sent: u64,
}

impl From<&StreamStats> for StreamStatsResponse {
    fn from(s: &StreamStats) -> Self {
        Self {
            fps: s.fps,
            bitrate: s.bitrate_mbps,
            latency: s.latency_ms,
            packet_loss: 0.0,
            resolution: "1920x1080".to_string(),
            codec: "H.264".to_string(),
            duration: 0,
            frames_captured: s.frames_captured,
            bytes_sent: s.bytes_sent,
        }
    }
}

#[tauri::command]
pub async fn stream_start(
    device_id: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    info!("Starting stream to device: {}", device_id);

    let parts: Vec<&str> = device_id.split(':').collect();
    if parts.len() < 2 {
        return Err("Invalid device ID format".to_string());
    }
    let host = parts[0].to_string();
    let port: u16 = parts[1].parse().unwrap_or(7000);

    {
        let mut manager = state.stream_manager.lock().map_err(|e| e.to_string())?;
        manager.start_stream(&device_id);
    }

    if state.stream_running.load(Ordering::SeqCst) {
        return Err("Already streaming".to_string());
    }

    state.stream_running.store(true, Ordering::SeqCst);
    let running = state.stream_running.clone();

    tokio::spawn(async move {
        let mut session = StreamSession::new();
        if let Err(e) = session.start(&host, port, 1920, 1080, 30, 4_000_000).await {
            error!("Stream session error: {}", e);
        }
        running.store(false, Ordering::SeqCst);
    });

    Ok(())
}

#[tauri::command]
pub async fn stream_stop(state: State<'_, AppState>) -> Result<(), String> {
    info!("Stopping stream");
    state.stream_running.store(false, Ordering::SeqCst);
    let mut manager = state.stream_manager.lock().map_err(|e| e.to_string())?;
    manager.stop_stream();
    Ok(())
}

#[tauri::command]
pub async fn get_stream_stats(state: State<'_, AppState>) -> Result<StreamStatsResponse, String> {
    let manager = state.stream_manager.lock().map_err(|e| e.to_string())?;
    Ok(StreamStatsResponse::from(manager.stats()))
}
