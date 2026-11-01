use crate::airplay::discovery::DiscoveredDevice;
use crate::AppState;
use serde::Serialize;
use tauri::State;
use tracing::{info, error};

#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    #[serde(rename = "type")]
    pub device_type: String,
    pub features: String,
    pub model: Option<String>,
}

impl From<&DiscoveredDevice> for DeviceInfo {
    fn from(d: &DiscoveredDevice) -> Self {
        let features = d
            .properties
            .get("features")
            .cloned()
            .unwrap_or_default();
        let model = d.properties.get("model").cloned();

        Self {
            id: d.id.clone(),
            name: d.name.clone(),
            host: d.host.clone(),
            port: d.port,
            device_type: d.device_type.clone(),
            features,
            model,
        }
    }
}

static DISCOVERY: std::sync::LazyLock<std::sync::Mutex<Option<crate::airplay::discovery::DiscoveryService>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

#[tauri::command]
pub async fn start_discovery(state: State<'_, AppState>) -> Result<(), String> {
    info!("Starting device discovery");

    let mut discovery = DISCOVERY.lock().map_err(|e| e.to_string())?;

    if discovery.is_some() {
        return Ok(());
    }

    let mut svc = crate::airplay::discovery::DiscoveryService::new()
        .map_err(|e| format!("Failed to create discovery service: {}", e))?;

    let mut rx = svc.start();
    *discovery = Some(svc);

    let devices = std::sync::Arc::clone(&state.discovered_devices);

    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                crate::airplay::discovery::DiscoveryEvent::DeviceFound(device) => {
                    info!("Device found: {} at {}:{}", device.name, device.host, device.port);
                    if let Ok(mut devs) = devices.lock() {
                        if !devs.iter().any(|d: &DiscoveredDevice| d.id == device.id) {
                            devs.push(device);
                        }
                    }
                }
                crate::airplay::discovery::DiscoveryEvent::DeviceLost(id) => {
                    info!("Device lost: {}", id);
                    if let Ok(mut devs) = devices.lock() {
                        devs.retain(|d| d.id != id);
                    }
                }
            }
        }
    });

    Ok(())
}

#[tauri::command]
pub async fn stop_discovery() -> Result<(), String> {
    info!("Stopping device discovery");
    if let Ok(mut discovery) = DISCOVERY.lock() {
        if let Some(svc) = discovery.take() {
            svc.stop();
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn get_devices(state: State<'_, AppState>) -> Result<Vec<DeviceInfo>, String> {
    let devices = state
        .discovered_devices
        .lock()
        .map_err(|e| e.to_string())?;

    Ok(devices.iter().map(DeviceInfo::from).collect())
}
