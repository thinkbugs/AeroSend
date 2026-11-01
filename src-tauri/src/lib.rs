pub mod airplay;
pub mod annotation;
pub mod capture;
pub mod commands;
pub mod config;
pub mod stream;
pub mod upnp;

use commands::{
    cast::{
        add_renderer, cast_file, cast_url, get_cast_status, get_offered_files,
        get_renderer_volume, pick_media_file, remove_renderer,
        renderer_action, renderer_seek, renderers, scan_renderers, set_renderer_volume, stop_cast,
    },
    discovery::{add_manual_device, get_devices, remove_device, start_discovery, stop_discovery},
    stream::{
        get_encoder_status, get_stream_settings, get_stream_stats, set_stream_settings,
        stream_start, stream_stop,
    },
};
use stream::{StreamSession, StreamSettings};
use std::sync::{Arc, Mutex};
use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{Manager, WindowEvent};

pub struct AppState {
    pub session: Arc<StreamSession>,
    pub settings: Mutex<StreamSettings>,
    pub discovered_devices: Arc<Mutex<Vec<airplay::discovery::DiscoveredDevice>>>,
    pub caster: Arc<upnp::Caster>,
}

pub fn run_app() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let state = AppState {
        session: Arc::new(StreamSession::new()),
        settings: Mutex::new(StreamSettings::default()),
        discovered_devices: Arc::new(Mutex::new(Vec::new())),
        caster: Arc::new(upnp::Caster::new()),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            let handle = app.handle().clone();
            *app.state::<AppState>().settings.lock().unwrap() = config::load(&handle);

            let show = MenuItemBuilder::with_id("show", "Show AeroSend").build(app)?;
            let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;
            let menu = MenuBuilder::new(app).items(&[&show, &quit]).build()?;

            TrayIconBuilder::new()
                .tooltip("AeroSend - AirPlay Sender")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "show" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "quit" => {
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let tauri::tray::TrayIconEvent::DoubleClick { .. } = event {
                        let app = tray.app_handle();
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                })
                .icon(app.default_window_icon().unwrap().clone())
                .build(app)?;

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
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
            get_stream_settings,
            set_stream_settings,
            get_encoder_status,
            scan_renderers,
            renderers,
            add_renderer,
            remove_renderer,
            pick_media_file,
            cast_file,
            cast_url,
            get_cast_status,
            get_offered_files,
            renderer_action,
            renderer_seek,
            set_renderer_volume,
            get_renderer_volume,
            stop_cast,
        ])
        .run(tauri::generate_context!())
        .expect("error while running AeroSend");
}
