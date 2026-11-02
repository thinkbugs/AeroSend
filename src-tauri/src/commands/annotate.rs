//! Commands behind the annotation overlay: open it, close it, lock the pointer
//! through it, and write the ink out as a PNG.

use crate::annotation;
use crate::AppState;
use serde::Serialize;
use std::sync::atomic::Ordering;
use tauri::{AppHandle, State};
use tracing::info;

/// What the overlay is doing, so the main window can show the same truth the
/// desktop layer holds instead of guessing from its last click.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnnotationState {
    pub open: bool,
    pub passthrough: bool,
}

fn state_of(app: &AppHandle, state: &AppState) -> AnnotationState {
    AnnotationState {
        open: annotation::is_open(app),
        passthrough: state.annotation_passthrough.load(Ordering::SeqCst),
    }
}

#[tauri::command]
pub async fn start_annotation(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<AnnotationState, String> {
    annotation::open(&app)?;
    // A fresh window is interactive; a leftover flag from the last session would
    // hand the mouse through an overlay the user expects to draw on.
    state
        .annotation_passthrough
        .store(false, Ordering::SeqCst);
    annotation::set_passthrough(&app, false)?;
    Ok(state_of(&app, &state))
}

#[tauri::command]
pub async fn stop_annotation(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<AnnotationState, String> {
    annotation::close(&app);
    state
        .annotation_passthrough
        .store(false, Ordering::SeqCst);
    Ok(state_of(&app, &state))
}

#[tauri::command]
pub async fn get_annotation_state(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<AnnotationState, String> {
    Ok(state_of(&app, &state))
}

/// `true` lets clicks fall through to the applications under the ink.
#[tauri::command]
pub async fn set_annotation_passthrough(
    app: AppHandle,
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<AnnotationState, String> {
    if !annotation::is_open(&app) {
        return Err("批注层尚未打开".to_string());
    }
    annotation::set_passthrough(&app, enabled)?;
    state
        .annotation_passthrough
        .store(enabled, Ordering::SeqCst);
    info!("Annotation passthrough: {}", enabled);
    Ok(state_of(&app, &state))
}

/// Save the ink as a PNG through the native Windows save dialog.
///
/// The canvas is the only copy of the drawing, so the bytes come from the
/// overlay as a data URL. `None` is the user cancelling the dialog.
#[tauri::command]
pub async fn save_annotation_png(png_base64: String) -> Result<Option<String>, String> {
    let bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        png_base64.trim(),
    )
    .map_err(|e| format!("无法读取画布图像：{e}"))?;

    let written = tokio::task::spawn_blocking(move || -> Result<Option<String>, String> {
        let path = match rfd::FileDialog::new()
            .set_title("将批注保存为 PNG")
            .set_file_name("aerosend-annotation.png")
            .add_filter("PNG 图片", &["png"])
            .save_file()
        {
            Some(path) => path,
            None => return Ok(None),
        };
        std::fs::write(&path, &bytes).map_err(|e| format!("写入 {} 失败：{e}", path.display()))?;
        Ok(Some(path.to_string_lossy().into_owned()))
    })
    .await
    .map_err(|e| e.to_string())??;

    if let Some(path) = &written {
        info!("Annotation saved to {}", path);
    }
    Ok(written)
}
