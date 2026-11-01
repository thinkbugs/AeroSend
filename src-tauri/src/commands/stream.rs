use crate::config;
use crate::stream::{StreamSettings, StreamStats};
use crate::AppState;
use serde::Serialize;
use std::sync::atomic::Ordering;
use std::time::Instant;
use tauri::{AppHandle, State};
use tracing::{error, info};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamStatsResponse {
    pub fps: f64,
    /// Measured video bitrate in megabits per second.
    pub bitrate: f64,
    pub latency: f64,
    pub resolution: String,
    pub codec: String,
    pub duration: u64,
    pub frames_captured: u64,
    pub frames_dropped: u64,
    pub bytes_sent: u64,
    pub audio_frames_sent: u64,
    pub running: bool,
    pub error: Option<String>,
}

impl StreamStatsResponse {
    fn new(s: &StreamStats, running: bool) -> Self {
        Self {
            fps: s.fps,
            bitrate: s.bitrate_mbps,
            latency: s.latency_ms,
            resolution: s.resolution.clone(),
            codec: "H.264 / AAC".to_string(),
            duration: 0,
            frames_captured: s.frames_captured,
            frames_dropped: s.frames_dropped,
            bytes_sent: s.bytes_sent,
            audio_frames_sent: s.audio_frames_sent,
            running,
            error: s.error.clone(),
        }
    }
}

/// When the current session began, for the elapsed-time readout.
static SESSION_STARTED: std::sync::LazyLock<Mutex<Option<Instant>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));
type Mutex<T> = std::sync::Mutex<T>;

#[tauri::command]
pub async fn stream_start(device_id: String, state: State<'_, AppState>) -> Result<(), String> {
    let mut parts = device_id.split(':');
    let host = parts.next().unwrap_or_default().to_string();
    let port: u16 = parts
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(7000);
    if host.is_empty() {
        return Err("Invalid device ID".to_string());
    }

    let running = state.session.running_flag();
    if running.load(Ordering::SeqCst) {
        return Err("A stream is already running".to_string());
    }

    let settings = *state.settings.lock().map_err(|e| e.to_string())?;
    info!("Connecting to {host}:{port} with {settings:?}");
    running.store(true, Ordering::SeqCst);
    *SESSION_STARTED.lock().unwrap() = Some(Instant::now());

    let session = state.session.clone();
    tauri::async_runtime::spawn_blocking(move || {
        if let Err(e) = session.start(&host, port, &settings) {
            error!("Stream session ended with an error: {e:#}");
        }
        running.store(false, Ordering::SeqCst);
    });

    Ok(())
}

#[tauri::command]
pub async fn stream_stop(state: State<'_, AppState>) -> Result<(), String> {
    info!("Stopping stream");
    *SESSION_STARTED.lock().unwrap() = None;
    state.session.stop();
    Ok(())
}

#[tauri::command]
pub async fn get_stream_stats(state: State<'_, AppState>) -> Result<StreamStatsResponse, String> {
    let running = state.session.running_flag().load(Ordering::SeqCst);
    let mut resp = StreamStatsResponse::new(&state.session.stats(), running);
    if let Some(started) = *SESSION_STARTED.lock().unwrap() {
        resp.duration = started.elapsed().as_secs();
    }
    Ok(resp)
}

#[tauri::command]
pub async fn get_stream_settings(state: State<'_, AppState>) -> Result<StreamSettings, String> {
    Ok(*state.settings.lock().map_err(|e| e.to_string())?)
}

/// Single entry point for encoder and transport choices; the values are saved
/// for the next run and apply to the next connection.
#[tauri::command]
pub async fn set_stream_settings(
    settings: StreamSettings,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    if settings.fps == 0 || settings.fps > 120 {
        return Err("FPS must be between 1 and 120".to_string());
    }
    if !(500_000..=40_000_000).contains(&settings.bitrate) {
        return Err("Bitrate must be between 0.5 and 40 Mbps".to_string());
    }

    info!("Stream settings updated: {settings:?}");
    *state.settings.lock().map_err(|e| e.to_string())? = settings;
    if let Err(e) = config::save(&app, &settings) {
        error!("Could not persist settings: {e}");
    }
    Ok(())
}

/// Which H.264 encoders the ffmpeg build on this machine really provides.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EncoderStatus {
    pub ffmpeg: String,
    pub x264: bool,
    pub nvenc: bool,
    pub qsv: bool,
}

static ENCODER_STATUS: std::sync::LazyLock<Mutex<Option<EncoderStatus>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));

#[tauri::command]
pub async fn get_encoder_status() -> Result<EncoderStatus, String> {
    // Copy the cached value out first: an `if let` scrutinee keeps its mutex
    // guard alive through the whole statement, including a future else branch.
    let cached = ENCODER_STATUS.lock().unwrap().clone();
    if let Some(cached) = cached {
        return Ok(cached);
    }
    let status = tauri::async_runtime::spawn_blocking(probe_encoders)
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| {
            error!("{e}");
            e
        })?;
    *ENCODER_STATUS.lock().unwrap() = Some(status.clone());
    Ok(status)
}

fn probe_encoders() -> Result<EncoderStatus, String> {
    let probe = crate::capture::encoder::probe_encoders()?;
    Ok(EncoderStatus {
        ffmpeg: probe.ffmpeg,
        x264: probe.x264,
        nvenc: probe.nvenc,
        qsv: probe.qsv,
    })
}
