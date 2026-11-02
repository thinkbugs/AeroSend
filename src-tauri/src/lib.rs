pub mod airplay;
pub mod annotation;
pub mod capture;
pub mod commands;
pub mod config;
pub mod stream;
pub mod upnp;

use commands::{
    annotate::{
        get_annotation_state, save_annotation_png, set_annotation_passthrough, start_annotation,
        stop_annotation,
    },
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
use std::sync::atomic::AtomicBool;
use stream::{MirrorStream, StreamSession, StreamSettings};
use std::sync::{Arc, Mutex};
use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{Manager, WindowEvent};

pub struct AppState {
    pub session: Arc<StreamSession>,
    /// The AirPlay 2 screen path, for a receiver that advertises a display.
    pub mirror: Arc<MirrorStream>,
    pub settings: Mutex<StreamSettings>,
    pub discovered_devices: Arc<Mutex<Vec<airplay::discovery::DiscoveredDevice>>>,
    pub caster: Arc<upnp::Caster>,
    /// Whether the overlay is currently letting the mouse through to the
    /// desktop. The window owns that switch, this is the copy the UI reads.
    pub annotation_passthrough: AtomicBool,
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
        mirror: Arc::new(MirrorStream::new()),
        settings: Mutex::new(StreamSettings::default()),
        discovered_devices: Arc::new(Mutex::new(Vec::new())),
        caster: Arc::new(upnp::Caster::new()),
        annotation_passthrough: AtomicBool::new(false),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            let handle = app.handle().clone();
            *app.state::<AppState>().settings.lock().unwrap() = config::load(&handle);

            let show = MenuItemBuilder::with_id("show", "显示 AeroSend").build(app)?;
            let quit = MenuItemBuilder::with_id("quit", "退出").build(app)?;
            let menu = MenuBuilder::new(app).items(&[&show, &quit]).build()?;

            TrayIconBuilder::new()
                .tooltip("AeroSend - AirPlay 投屏")
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
            // Only the main window is "mined to tray" on close. The overlay has
            // to actually go away, or a hidden transparent window keeps eating
            // the top of the desktop.
            if window.label() != "main" {
                return;
            }
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
            start_annotation,
            stop_annotation,
            get_annotation_state,
            set_annotation_passthrough,
            save_annotation_png,
        ])
        .run(tauri::generate_context!())
        .expect("error while running AeroSend");
}
