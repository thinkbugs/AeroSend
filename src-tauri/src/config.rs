//! Persisted user settings, so the chosen encoder, fps and bitrate survive a
//! restart. Written as JSON in the app config directory.

use crate::stream::StreamSettings;
use anyhow::{Context, Result};
use std::path::PathBuf;
use tauri::{AppHandle, Manager};
use tracing::{info, warn};

const FILE_NAME: &str = "settings.json";

fn config_file(app: &AppHandle) -> Result<PathBuf> {
    let dir = app.path().app_config_dir().context("No app config directory")?;
    std::fs::create_dir_all(&dir).with_context(|| format!("Creating {}", dir.display()))?;
    Ok(dir.join(FILE_NAME))
}

/// Saved settings, or the defaults when nothing readable is on disk.
pub fn load(app: &AppHandle) -> StreamSettings {
    let path = match config_file(app) {
        Ok(path) => path,
        Err(e) => {
            warn!("{e:#}");
            return StreamSettings::default();
        }
    };
    match std::fs::read(&path)
        .with_context(|| format!("Reading {}", path.display()))
        .and_then(|bytes| {
            serde_json::from_slice::<StreamSettings>(&bytes).context("Parsing settings JSON")
        })
    {
        Ok(settings) => {
            info!("Loaded settings from {}", path.display());
            settings
        }
        Err(e) => {
            warn!("{e:#}; using defaults");
            StreamSettings::default()
        }
    }
}

pub fn save(app: &AppHandle, settings: &StreamSettings) -> Result<()> {
    let path = config_file(app)?;
    let text = serde_json::to_string_pretty(settings).context("Serializing settings")?;
    // Write-then-rename so a crash mid-write cannot leave a truncated file.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &text).with_context(|| format!("Writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("Replacing {}", path.display()))?;
    Ok(())
}
