use crate::airplay::discovery::DiscoveredDevice;
use crate::AppState;
use serde::Serialize;
use tauri::State;
use tracing::info;

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
        .map_err(|e| format!("创建设备发现服务失败：{}", e))?;

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

#[tauri::command]
pub async fn add_manual_device(
    host: String,
    port: Option<u16>,
    name: Option<String>,
    state: State<'_, AppState>,
) -> Result<DeviceInfo, String> {
    let port = port.unwrap_or(7000);
    let name = name.unwrap_or_else(|| format!("Device @ {}", host));
    let device_type = "airplay".to_string();
    let id = format!("{}:{}:{}", host, port, device_type);

    let device = DiscoveredDevice {
        id: id.clone(),
        name: name.clone(),
        host: host.clone(),
        port,
        device_type: device_type.clone(),
        properties: {
            let mut m = std::collections::HashMap::new();
            m.insert("manual".to_string(), "true".to_string());
            m
        },
    };

    info!("Manual device added: {} at {}:{}", name, host, port);

    let mut devices = state.discovered_devices.lock().map_err(|e| e.to_string())?;
    if !devices.iter().any(|d| d.id == id) {
        devices.push(device);
    }

    let d = devices.iter().find(|d| d.id == id).unwrap();
    Ok(DeviceInfo::from(d))
}

#[tauri::command]
pub async fn remove_device(device_id: String, state: State<'_, AppState>) -> Result<(), String> {
    let mut devices = state.discovered_devices.lock().map_err(|e| e.to_string())?;
    devices.retain(|d| d.id != device_id);
    info!("Device removed: {}", device_id);
    Ok(())
}
