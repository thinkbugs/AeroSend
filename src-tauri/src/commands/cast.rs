//! Tauri commands for the media cast path.
//!
//! Every UPnP call here is a socket round trip with a timeout, so each command
//! runs on the blocking pool instead of the async runtime's task threads.

use crate::AppState;
use crate::upnp::{caster::CastError, CastStatus, OfferedFile, RendererInfo};
use serde::Serialize;
use std::path::PathBuf;
use tauri::State;
use tracing::{error, info};

/// Commands return this shape so the UI can show the device's own words rather
/// than a generic failure.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CastErrorInfo {
    pub message: String,
}

impl From<CastError> for CastErrorInfo {
    fn from(e: CastError) -> Self {
        Self {
            message: e.to_string(),
        }
    }
}

#[tauri::command]
pub async fn scan_renderers(state: State<'_, AppState>) -> Result<Vec<RendererInfo>, CastErrorInfo> {
    let caster = std::sync::Arc::clone(&state.caster);
    tokio::task::spawn_blocking(move || {
        caster.scan();
        caster.renderers()
    })
    .await
    .map_err(|e| CastErrorInfo {
        message: e.to_string(),
    })
}

#[tauri::command]
pub async fn renderers(state: State<'_, AppState>) -> Result<Vec<RendererInfo>, CastErrorInfo> {
    Ok(state.caster.renderers())
}

/// The native Windows file dialog, opened from the media cast page.
///
/// The dialog is a modal loop of its own, so it belongs on the blocking pool;
/// `None` is the user pressing Cancel, which is not an error.
///
/// One filter per media kind, because the page plays all three: a single
/// "everything" list makes a user who wants a photo scroll through the video
/// folder, and a folder of raw files has no extension to announce.
#[tauri::command]
pub async fn pick_media_file() -> Result<Option<String>, CastErrorInfo> {
    let picked = tokio::task::spawn_blocking(|| {
        rfd::FileDialog::new()
            .set_title("选择要投屏的媒体文件")
            .add_filter(
                "视频",
                &[
                    "mp4", "m4v", "mov", "mkv", "avi", "mpg", "mpeg", "ts", "m2ts", "webm", "flv",
                    "wmv",
                ],
            )
            .add_filter(
                "音乐",
                &["mp3", "m4a", "aac", "flac", "wav", "ogg", "oga", "opus", "wma"],
            )
            .add_filter("图片", &["jpg", "jpeg", "png", "gif", "webp", "bmp"])
            .add_filter("所有文件", &["*"])
            .pick_file()
            .map(|path| path.to_string_lossy().into_owned())
    })
    .await
    .map_err(|e| CastErrorInfo {
        message: e.to_string(),
    })?;
    Ok(picked)
}

/// Add a renderer by address (`192.168.1.9:8290` or a description URL), for the
/// networks where SSDP does not work: AP isolation, a switch that does not
/// snoop multicast, a VPN that owns the route.
#[tauri::command]
pub async fn add_renderer(
    location: String,
    state: State<'_, AppState>,
) -> Result<RendererInfo, CastErrorInfo> {
    let caster = std::sync::Arc::clone(&state.caster);
    let shown = location.clone();
    let result = tokio::task::spawn_blocking(move || caster.add_by_location(&location))
        .await
        .map_err(|e| CastErrorInfo {
            message: e.to_string(),
        })?;
    match &result {
        Ok(r) => info!("added renderer {} at {}", r.name, r.base_url),
        Err(e) => error!("could not add renderer {shown}: {e}"),
    }
    result.map_err(Into::into)
}

/// Forget a renderer, whether it was announced or added by hand.
#[tauri::command]
pub async fn remove_renderer(
    renderer_id: String,
    state: State<'_, AppState>,
) -> Result<bool, CastErrorInfo> {
    Ok(state.caster.remove(&renderer_id))
}

/// Cast a local file: register it with the media server, then hand the URL to
/// the renderer and start it.
#[tauri::command]
pub async fn cast_file(
    renderer_id: String,
    path: String,
    state: State<'_, AppState>,
) -> Result<CastStatus, CastErrorInfo> {
    let caster = std::sync::Arc::clone(&state.caster);
    let file = PathBuf::from(&path);
    let id_for_log = renderer_id.clone();
    let result = tokio::task::spawn_blocking(move || caster.cast_file(&renderer_id, &file))
        .await
        .map_err(|e| CastErrorInfo {
            message: e.to_string(),
        })?;
    match &result {
        Ok(status) => info!("casting {} to {id_for_log}", status.title),
        Err(e) => error!("cast to {id_for_log} failed: {e}"),
    }
    result.map_err(Into::into)
}

/// Cast a URL the renderer can already reach — e.g. a stream served by another
/// device on the LAN, which needs no local file at all.
#[tauri::command]
pub async fn cast_url(
    renderer_id: String,
    url: String,
    title: String,
    state: State<'_, AppState>,
) -> Result<CastStatus, CastErrorInfo> {
    let caster = std::sync::Arc::clone(&state.caster);
    tokio::task::spawn_blocking(move || {
        caster
            .cast(&renderer_id, &url, &title)
            .map_err(Into::into)
    })
    .await
    .map_err(|e| CastErrorInfo {
        message: e.to_string(),
    })?
}

#[tauri::command]
pub async fn get_cast_status(
    renderer_id: String,
    state: State<'_, AppState>,
) -> Result<Option<CastStatus>, CastErrorInfo> {
    Ok(state.caster.status(&renderer_id))
}

#[tauri::command]
pub async fn get_offered_files(state: State<'_, AppState>) -> Result<Vec<OfferedFile>, CastErrorInfo> {
    Ok(state.caster.offered())
}

/// Play, Pause or Stop on one renderer.
#[tauri::command]
pub async fn renderer_action(
    renderer_id: String,
    action: String,
    state: State<'_, AppState>,
) -> Result<CastStatus, CastErrorInfo> {
    let allowed = ["Play", "Pause", "Stop"];
    if !allowed.contains(&action.as_str()) {
        return Err(CastErrorInfo {
            message: format!("unsupported action {action}"),
        });
    }
    let caster = std::sync::Arc::clone(&state.caster);
    tokio::task::spawn_blocking(move || caster.transport_action(&renderer_id, &action).map_err(Into::into))
        .await
        .map_err(|e| CastErrorInfo {
            message: e.to_string(),
        })?
}

#[tauri::command]
pub async fn renderer_seek(
    renderer_id: String,
    seconds: f64,
    state: State<'_, AppState>,
) -> Result<CastStatus, CastErrorInfo> {
    let caster = std::sync::Arc::clone(&state.caster);
    tokio::task::spawn_blocking(move || caster.seek(&renderer_id, seconds).map_err(Into::into))
        .await
        .map_err(|e| CastErrorInfo {
            message: e.to_string(),
        })?
}

#[tauri::command]
pub async fn set_renderer_volume(
    renderer_id: String,
    volume: u16,
    state: State<'_, AppState>,
) -> Result<(), CastErrorInfo> {
    if volume > 100 {
        return Err(CastErrorInfo {
            message: "volume is a percentage".into(),
        });
    }
    let caster = std::sync::Arc::clone(&state.caster);
    tokio::task::spawn_blocking(move || caster.set_volume(&renderer_id, volume).map_err(Into::into))
        .await
        .map_err(|e| CastErrorInfo {
            message: e.to_string(),
        })?
}

/// The volume the device reports for itself, asked when a renderer is selected
/// rather than guessed from what we last sent.
#[tauri::command]
pub async fn get_renderer_volume(
    renderer_id: String,
    state: State<'_, AppState>,
) -> Result<u16, CastErrorInfo> {
    let caster = std::sync::Arc::clone(&state.caster);
    tokio::task::spawn_blocking(move || caster.volume(&renderer_id).map_err(Into::into))
        .await
        .map_err(|e| CastErrorInfo {
            message: e.to_string(),
        })?
}

#[tauri::command]
pub async fn stop_cast(state: State<'_, AppState>) -> Result<(), CastErrorInfo> {
    let caster = std::sync::Arc::clone(&state.caster);
    tokio::task::spawn_blocking(move || caster.shutdown())
        .await
        .map_err(|e| CastErrorInfo {
            message: e.to_string(),
        })
}
