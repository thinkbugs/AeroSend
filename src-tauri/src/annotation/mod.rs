//! The annotation overlay.
//!
//! A second window: transparent, undecorated, always on top, covering the
//! monitor we mirror. It is deliberately not composited into the stream by us —
//! the screen duplicator already grabs whatever is on top of the desktop, so ink
//! drawn here leaves through the same path as any other window and shows up on
//! the receiver without a second encoder or a second protocol.
//!
//! The one thing only the desktop layer can do is hand input back to the
//! applications underneath: `set_ignore_cursor_events` makes the whole window
//! click-through, which is what "lock" means in the toolbar.

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};
use tracing::info;

pub const OVERLAY_LABEL: &str = "annotate";

/// Whether the overlay window exists right now.
pub fn is_open(app: &AppHandle) -> bool {
    app.get_webview_window(OVERLAY_LABEL).is_some()
}

/// Create the overlay, or show the one already there.
///
/// Fullscreen rather than "primary monitor size at (0, 0)": the builder takes
/// logical pixels and the capture path takes physical ones, and on a display at
/// 150 % scaling those two disagree. Letting the window manager cover the
/// monitor keeps the ink aligned with what the duplicator grabs.
pub fn open(app: &AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(OVERLAY_LABEL) {
        window.show().map_err(|e| e.to_string())?;
        window.set_focus().map_err(|e| e.to_string())?;
        return Ok(());
    }

    let window = WebviewWindowBuilder::new(
        app,
        OVERLAY_LABEL,
        WebviewUrl::App("annotate.html".into()),
    )
    .title("AeroSend Annotation")
    .inner_size(1.0, 1.0)
    .decorations(false)
    .transparent(true)
    .always_on_top(true)
    .skip_taskbar(true)
    .resizable(false)
    .maximizable(false)
    .minimizable(false)
    .closable(false)
    .shadow(false)
    .fullscreen(true)
    .visible(true)
    .build()
    .map_err(|e| e.to_string())?;

    info!("Annotation overlay opened: {}", window.label());
    Ok(())
}

/// Destroy the overlay. Ink is not kept: an annotation is a live gesture, and a
/// half-finished drawing should not reappear over next week's deck.
pub fn close(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(OVERLAY_LABEL) {
        let _ = window.close();
        info!("Annotation overlay closed");
    }
}

/// Hand the mouse back to whatever is underneath the ink.
pub fn set_passthrough(app: &AppHandle, enabled: bool) -> Result<(), String> {
    let window = app
        .get_webview_window(OVERLAY_LABEL)
        .ok_or_else(|| "The annotation overlay is not open".to_string())?;
    window
        .set_ignore_cursor_events(enabled)
        .map_err(|e| e.to_string())
}
