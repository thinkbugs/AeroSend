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
    /// The session carries audio only: `_raop._tcp` is the speaker transport.
    pub audio_only: bool,
    pub duration: u64,
    pub frames_captured: u64,
    pub frames_dropped: u64,
    /// Pictures dropped because the receiver stopped taking them, as opposed to
    /// `frames_dropped`, which is the local encoder backlog.
    pub frames_stalled: u64,
    pub bytes_sent: u64,
    pub audio_frames_sent: u64,
    pub running: bool,
    pub error: Option<String>,
}

impl StreamStatsResponse {
    fn new(s: &StreamStats, running: bool, mirror: bool) -> Self {
        Self {
            fps: s.fps,
            bitrate: s.bitrate_mbps,
            latency: s.latency_ms,
            resolution: s.resolution.clone(),
            codec: if mirror {
                "H.264 (AirPlay 2)".to_string()
            } else {
                "L16 / 44.1 kHz".to_string()
            },
            audio_only: !mirror,
            duration: 0,
            frames_captured: s.frames_captured,
            frames_dropped: s.frames_dropped,
            frames_stalled: s.frames_stalled,
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

/// Which of the two AirPlay transports the running session uses. They share one
/// port on the receiver and differ only in what the sender does after `RECORD`,
/// so the choice is made once at start and the stats readout follows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transport {
    Raop,
    Mirror,
}

static TRANSPORT: std::sync::LazyLock<Mutex<Option<Transport>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));

/// A receiver that publishes an identity can take a mirroring session; one that
/// does not is a speaker, and the RAOP path is the only one it answers.
fn mirrors_screen(device_id: &str, state: &AppState) -> bool {
    state
        .discovered_devices
        .lock()
        .map(|devices| {
            devices
                .iter()
                .any(|device| device.id == device_id && device.mirrors_screen())
        })
        .unwrap_or(false)
}

#[tauri::command]
pub async fn stream_start(device_id: String, state: State<'_, AppState>) -> Result<(), String> {
    let mut parts = device_id.split(':');
    let host = parts.next().unwrap_or_default().to_string();
    let port: u16 = parts
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(7000);
    if host.is_empty() {
        return Err("设备 ID 无效".to_string());
    }

    let transport = if mirrors_screen(&device_id, &state) {
        Transport::Mirror
    } else {
        Transport::Raop
    };
    let (running, busy) = match transport {
        Transport::Mirror => (state.mirror.running_flag(), state.session.running_flag()),
        Transport::Raop => (state.session.running_flag(), state.mirror.running_flag()),
    };
    if running.load(Ordering::SeqCst) || busy.load(Ordering::SeqCst) {
        return Err("已有投屏会话在运行".to_string());
    }

    let settings = *state.settings.lock().map_err(|e| e.to_string())?;
    if transport == Transport::Raop && !settings.audio {
        return Err(
            "该设备是 AirPlay 扬声器（RAOP），只承载音频；\
             请先在设置中开启系统声音再投送"
                .to_string(),
        );
    }
    info!("Connecting to {host}:{port} with {settings:?} over {transport:?}");
    running.store(true, Ordering::SeqCst);
    *TRANSPORT.lock().unwrap() = Some(transport);
    *SESSION_STARTED.lock().unwrap() = Some(Instant::now());

    match transport {
        Transport::Mirror => {
            let session = state.mirror.clone();
            tauri::async_runtime::spawn_blocking(move || {
                if let Err(e) = session.start(&host, port, &settings) {
                    error!("Mirroring session ended with an error: {e:#}");
                }
                running.store(false, Ordering::SeqCst);
            });
        }
        Transport::Raop => {
            let session = state.session.clone();
            tauri::async_runtime::spawn_blocking(move || {
                if let Err(e) = session.start(&host, port, &settings) {
                    error!("Stream session ended with an error: {e:#}");
                }
                running.store(false, Ordering::SeqCst);
            });
        }
    }

    Ok(())
}

#[tauri::command]
pub async fn stream_stop(state: State<'_, AppState>) -> Result<(), String> {
    info!("Stopping stream");
    *SESSION_STARTED.lock().unwrap() = None;
    *TRANSPORT.lock().unwrap() = None;
    state.session.stop();
    state.mirror.stop();
    Ok(())
}

#[tauri::command]
pub async fn get_stream_stats(state: State<'_, AppState>) -> Result<StreamStatsResponse, String> {
    let transport = *TRANSPORT.lock().unwrap();
    let (stats, running) = match transport {
        Some(Transport::Mirror) => (state.mirror.stats(), state.mirror.running_flag()),
        _ => (state.session.stats(), state.session.running_flag()),
    };
    let mut resp = StreamStatsResponse::new(
        &stats,
        running.load(Ordering::SeqCst),
        transport == Some(Transport::Mirror),
    );
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
        return Err("帧率必须在 1 到 120 之间".to_string());
    }
    if !(500_000..=40_000_000).contains(&settings.bitrate) {
        return Err("码率必须在 0.5 到 40 Mbps 之间".to_string());
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
