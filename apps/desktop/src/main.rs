//! Inkbridge for the computer: the host in the tray (Windows, Linux) or the
//! menu bar (macOS).
//!
//! It runs the same [`ib_host::service::Service`] as `inkbridge-host` in the
//! background. The tray icon shows whether a tablet is connected; its menu
//! and a small window show the details, pair tablets and manage pairings.
//! Closing the window keeps Inkbridge running; "Quit" stops it.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// Tauri hands commands their arguments (the app handle) by value.
#![allow(clippy::needless_pass_by_value)]

mod settings;

use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ib_host::service::{Service, ServiceOptions};
use ib_host::{HostStatus, TabletStatus};
use serde::Serialize;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, WindowEvent};

const TRAY_ID: &str = "inkbridge";

/// The running host, until "Quit", and what follows its status.
struct AppState {
    service: Mutex<Option<Service>>,
    tray: Mutex<Option<(TrayIcon, MenuItem<tauri::Wry>)>>,
}

impl AppState {
    fn with<T>(&self, f: impl FnOnce(&Service) -> T) -> Result<T, String> {
        let guard = self
            .service
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .as_ref()
            .map(f)
            .ok_or_else(|| "Inkbridge is stopping".to_owned())
    }
}

/// The connected tablet, for the window.
#[derive(Debug, Clone, Serialize, PartialEq)]
struct TabletView {
    name: String,
    link: &'static str,
    protocol: &'static str,
    rtt_ms: Option<f64>,
}

/// A paired tablet, for the window.
#[derive(Debug, Clone, Serialize, PartialEq)]
struct PairedView {
    name: String,
    fingerprint: String,
    last_used: String,
}

/// Everything the window shows.
#[derive(Debug, Clone, Serialize, PartialEq)]
struct StatusView {
    computer: String,
    tablet: Option<TabletView>,
    pairing_open: bool,
    paired: Vec<PairedView>,
}

/// A pairing code to show.
#[derive(Debug, Clone, Serialize)]
struct PairingView {
    svg: String,
    uri: String,
    minutes: u64,
}

fn tablet_view(t: &TabletStatus, rtt: Option<Duration>) -> TabletView {
    TabletView {
        name: t.name.clone(),
        link: if t.usb { "USB" } else { "Wi-Fi" },
        protocol: if t.tcp { "TCP" } else { "QUIC" },
        rtt_ms: rtt.map(|d| d.as_secs_f64() * 1000.0),
    }
}

fn days_ago(last_used: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    match now.saturating_sub(last_used) / 86_400 {
        0 => "today".into(),
        1 => "yesterday".into(),
        n => format!("{n} days ago"),
    }
}

async fn status_view(state: &AppState) -> Result<StatusView, String> {
    let (host, computer) = state.with(|s| (s.host().clone(), s.name().to_owned()))?;
    let status = host.status().borrow().clone();
    let mut devices = host.paired_devices().await;
    devices.sort_by_key(|d| std::cmp::Reverse(d.last_used));
    let paired = devices
        .into_iter()
        .map(|d| PairedView {
            name: d.name,
            fingerprint: d.fingerprint.to_hex(),
            last_used: days_ago(d.last_used),
        })
        .collect();
    Ok(StatusView {
        computer,
        tablet: status
            .tablet
            .as_ref()
            .map(|t| tablet_view(t, host.tablet_rtt())),
        pairing_open: status.pairing_open,
        paired,
    })
}

#[tauri::command]
async fn status(state: tauri::State<'_, AppState>) -> Result<StatusView, String> {
    status_view(&state).await
}

#[tauri::command]
async fn open_pairing(state: tauri::State<'_, AppState>) -> Result<PairingView, String> {
    let host = state.with(|s| s.host().clone())?;
    let offer = host.open_pairing().await.map_err(|e| format!("{e:#}"))?;
    let uri = offer.to_uri();
    let code = qrcode::QrCode::new(uri.as_bytes()).map_err(|e| e.to_string())?;
    let svg = code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(260, 260)
        .quiet_zone(true)
        .build();
    Ok(PairingView {
        svg,
        uri,
        minutes: ib_host::server::PAIRING_VALID.as_secs() / 60,
    })
}

#[tauri::command]
async fn close_pairing(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let host = state.with(|s| s.host().clone())?;
    host.close_pairing().await;
    Ok(())
}

#[tauri::command]
async fn forget(state: tauri::State<'_, AppState>, fingerprint: String) -> Result<(), String> {
    let host = state.with(|s| s.host().clone())?;
    let fp = ib_transport::Fingerprint::from_hex(&fingerprint).ok_or("not a fingerprint")?;
    host.forget(fp).await.map_err(|e| format!("{e:#}"))
}

/// The Advanced section of the window.
#[derive(Debug, Clone, Serialize)]
struct AdvancedView {
    adb: bool,
    autostart: bool,
    /// "windows" or "macos": which extra-screen setup applies.
    os: &'static str,
    /// Windows: whether the virtual display driver is installed.
    extra_screen_installed: bool,
}

#[tauri::command]
fn advanced(app: AppHandle) -> AdvancedView {
    use tauri_plugin_autostart::ManagerExt;
    AdvancedView {
        adb: settings::load().adb,
        autostart: app.autolaunch().is_enabled().unwrap_or(false),
        os: std::env::consts::OS,
        extra_screen_installed: ib_host::platform::extend_supported(),
    }
}

#[tauri::command]
fn set_autostart(app: AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let launcher = app.autolaunch();
    if enabled {
        launcher.enable()
    } else {
        launcher.disable()
    }
    .map_err(|e| e.to_string())
}

/// Turns USB through adb on or off; the host restarts to apply it.
#[tauri::command]
async fn set_adb(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut s = settings::load();
    s.adb = enabled;
    settings::save(&s).map_err(|e| e.to_string())?;
    restart_service(&app)
}

/// Runs a one-time setup that asks for admin rights (Windows shows its
/// prompt) without blocking the window.
async fn admin_task(f: fn() -> anyhow::Result<()>) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn install_extra_screen() -> Result<(), String> {
    #[cfg(windows)]
    {
        admin_task(|| {
            ib_host::vdd_setup::check_platform()?;
            ib_host::vdd_setup::install()
        })
        .await
    }
    #[cfg(not(windows))]
    {
        Err("only needed on Windows".into())
    }
}

#[tauri::command]
async fn remove_extra_screen() -> Result<(), String> {
    #[cfg(windows)]
    {
        admin_task(ib_host::vdd_setup::remove).await
    }
    #[cfg(not(windows))]
    {
        Err("only needed on Windows".into())
    }
}

#[tauri::command]
async fn fix_usb_routing() -> Result<(), String> {
    admin_task(|| ib_host::platform::set_usb_routing(true)).await
}

fn options() -> ServiceOptions {
    ServiceOptions {
        adb: settings::load().adb,
        ..ServiceOptions::default()
    }
}

/// Stops the host and starts it again with the current settings.
fn restart_service(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let old = state
        .service
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(old) = old {
        old.shutdown();
    }
    let service = Service::start(&options()).map_err(|e| format!("{e:#}"))?;
    *state
        .service
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(service);
    follow_status(app.clone());
    Ok(())
}

#[tauri::command]
fn quit(app: AppHandle) {
    stop(&app);
}

/// Stops the host (releasing the pen) and exits.
fn stop(app: &AppHandle) {
    if let Some(service) = app
        .state::<AppState>()
        .service
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    {
        service.shutdown();
    }
    app.exit(0);
}

fn show_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn toggle_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        if w.is_visible().unwrap_or(false) {
            let _ = w.hide();
        } else {
            show_window(app);
        }
    }
}

/// One line for the tray's tooltip and menu.
fn summary(status: &HostStatus) -> String {
    match &status.tablet {
        Some(t) => format!(
            "Connected: {} ({})",
            t.name,
            if t.usb { "USB" } else { "Wi-Fi" }
        ),
        None if status.pairing_open => "Waiting for a tablet to pair".into(),
        None => "Waiting for your tablet".into(),
    }
}

fn tray_icon(connected: bool) -> Option<Image<'static>> {
    let bytes: &[u8] = if cfg!(target_os = "macos") {
        include_bytes!("../icons/tray-template.png")
    } else if connected {
        include_bytes!("../icons/tray-connected.png")
    } else {
        include_bytes!("../icons/tray.png")
    };
    Image::from_bytes(bytes).ok()
}

fn build_tray(app: &AppHandle) -> tauri::Result<(TrayIcon, MenuItem<tauri::Wry>)> {
    let status_item = MenuItem::with_id(app, "status", "Starting…", false, None::<&str>)?;
    let open = MenuItem::with_id(app, "open", "Open Inkbridge", true, None::<&str>)?;
    let pair = MenuItem::with_id(app, "pair", "Pair a tablet…", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Inkbridge", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &status_item,
            &PredefinedMenuItem::separator(app)?,
            &open,
            &pair,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;
    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip("Inkbridge")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .icon_as_template(cfg!(target_os = "macos"))
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show_window(app),
            "pair" => {
                show_window(app);
                let _ = app.emit("pair", ());
            }
            "quit" => stop(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_window(tray.app_handle());
            }
        });
    if let Some(icon) = tray_icon(false) {
        builder = builder.icon(icon);
    }
    Ok((builder.build(app)?, status_item))
}

/// Keeps the tray and the window up to date: on every status change, and
/// every second while a tablet is connected (its round trip).
fn follow_status(app: AppHandle) {
    let Ok(mut status) = app.state::<AppState>().with(Service::status) else {
        return;
    };
    let Some((tray, status_item)) = app
        .state::<AppState>()
        .tray
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    else {
        return;
    };
    tauri::async_runtime::spawn(async move {
        let mut last_connected = None;
        let mut before = status.borrow().clone();
        loop {
            let now = status.borrow_and_update().clone();
            notify_change(&app, &before, &now);
            before = now.clone();
            let text = summary(&now);
            let _ = tray.set_tooltip(Some(format!("Inkbridge: {text}")));
            let _ = status_item.set_text(&text);
            let connected = now.tablet.is_some();
            if last_connected != Some(connected) {
                let _ = tray.set_icon(tray_icon(connected));
                last_connected = Some(connected);
            }
            if let Ok(view) = status_view(&app.state::<AppState>()).await {
                let _ = app.emit("status", view);
            }
            let tick = tokio::time::sleep(Duration::from_secs(1));
            tokio::select! {
                changed = status.changed() => if changed.is_err() { return },
                () = tick, if connected => {}
            }
        }
    });
}

/// A short system notification when a tablet connects or pairs.
fn notify_change(app: &AppHandle, before: &HostStatus, now: &HostStatus) {
    use tauri_plugin_notification::NotificationExt;
    let Some(tablet) = &now.tablet else { return };
    let paired = before.pairing_open && !now.pairing_open;
    let connected = before.tablet.as_ref().map(|t| &t.name) != Some(&tablet.name);
    let body = if paired {
        format!("{} is paired with this computer.", tablet.name)
    } else if connected {
        format!(
            "{} connected over {}.",
            tablet.name,
            if tablet.usb { "USB" } else { "Wi-Fi" }
        )
    } else {
        return;
    };
    let _ = app
        .notification()
        .builder()
        .title("Inkbridge")
        .body(body)
        .show();
}

/// Logs go to a file next to the host's identity: the app has no console.
fn init_logging() {
    let dir = ib_host::storage::config_dir();
    let _ = std::fs::create_dir_all(&dir);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    match std::fs::File::create(dir.join("inkbridge.log")) {
        Ok(file) => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(Mutex::new(file))
            .init(),
        Err(_) => tracing_subscriber::fmt().with_env_filter(filter).init(),
    }
}

/// The one-time admin setups relaunch this program with admin rights and
/// one of these flags (Windows); they run and exit before anything else.
fn run_admin_flag() -> Option<i32> {
    #[cfg(windows)]
    {
        let flag = std::env::args().nth(1)?;
        let result = match flag.as_str() {
            "--install-virtual-display" => ib_host::vdd_setup::install(),
            "--remove-virtual-display" => ib_host::vdd_setup::remove(),
            _ => return None,
        };
        Some(match result {
            Ok(()) => 0,
            Err(e) => {
                tracing::error!("{flag}: {e:#}");
                1
            }
        })
    }
    #[cfg(not(windows))]
    {
        None
    }
}

fn main() {
    // Before logging starts, which would truncate the running app's log.
    if let Some(code) = run_admin_flag() {
        std::process::exit(code);
    }
    init_logging();
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // Started again: show the running one instead.
            show_window(app);
        }))
        .plugin(tauri_plugin_autostart::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .manage(AppState {
            service: Mutex::new(None),
            tray: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            status,
            open_pairing,
            close_pairing,
            forget,
            advanced,
            set_autostart,
            set_adb,
            install_extra_screen,
            remove_extra_screen,
            fix_usb_routing,
            quit
        ])
        .on_window_event(|window, event| {
            // Closing the window keeps Inkbridge running in the tray.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            let service = tauri::async_runtime::block_on(async { Service::start(&options()) })?;
            let first_run = tauri::async_runtime::block_on(service.host().paired_count()) == 0;
            *app.state::<AppState>()
                .service
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(service);
            let tray = build_tray(app.handle())?;
            *app.state::<AppState>()
                .tray
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tray);
            follow_status(app.handle().clone());
            if first_run {
                // Nothing paired yet: the window opens on the pairing code.
                show_window(app.handle());
            }
            Ok(())
        })
        .build(tauri::generate_context!());
    match app {
        Ok(app) => app.run(|_, _| {}),
        Err(e) => tracing::error!("Inkbridge couldn't start: {e}"),
    }
}
