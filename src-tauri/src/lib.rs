pub mod airplay;
pub mod annotation;
pub mod capture;
pub mod commands;
pub mod stream;

use commands::{
    discovery::{add_manual_device, get_devices, remove_device, start_discovery, stop_discovery},
    stream::{get_stream_stats, stream_start, stream_stop},
};
use stream::StreamManager;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

pub struct AppState {
    pub stream_manager: Mutex<StreamManager>,
    pub discovered_devices: Arc<Mutex<Vec<airplay::discovery::DiscoveredDevice>>>,
    pub stream_running: Arc<AtomicBool>,
}

pub fn run_app() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let state = AppState {
        stream_manager: Mutex::new(StreamManager::new()),
        discovered_devices: Arc::new(Mutex::new(Vec::new())),
        stream_running: Arc::new(AtomicBool::new(false)),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            start_discovery,
            stop_discovery,
            get_devices,
            add_manual_device,
            remove_device,
            stream_start,
            stream_stop,
            get_stream_stats,
        ])
        .run(tauri::generate_context!())
        .expect("error while running AeroSend");
}
