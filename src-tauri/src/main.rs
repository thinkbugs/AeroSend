#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod airplay;
mod annotation;
mod capture;
mod commands;
mod stream;

use commands::{
    discovery::{get_devices, start_discovery, stop_discovery},
    stream::{get_stream_stats, stream_start, stream_stop},
};
use stream::StreamManager;
use std::sync::{Arc, Mutex};

pub struct AppState {
    stream_manager: Mutex<StreamManager>,
    discovered_devices: Arc<Mutex<Vec<airplay::discovery::DiscoveredDevice>>>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let state = AppState {
        stream_manager: Mutex::new(StreamManager::new()),
        discovered_devices: Arc::new(Mutex::new(Vec::new())),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            start_discovery,
            stop_discovery,
            get_devices,
            stream_start,
            stream_stop,
            get_stream_stats,
        ])
        .run(tauri::generate_context!())
        .expect("error while running AeroSend");
}
