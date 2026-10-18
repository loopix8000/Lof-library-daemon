#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod controller;

use std::fs::OpenOptions;
use std::net::IpAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};

use controller::{Controller, Settings};
use lof_library_core::auth::Principal;
use lof_library_core::error::ApiError;
use serde::Serialize;
use serde_json::Value;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, State, WindowEvent};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt as _};
use tauri_plugin_opener::OpenerExt;

type Shared = Arc<Controller>;

const HIDDEN_ARG: &str = "--hidden";
const LOG_LIMIT: u64 = 5 * 1024 * 1024;

fn text(error: ApiError) -> String {
    error.message().to_string()
}

fn show(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn init_logging(base: &Path) {
    let directory = base.join("logs");
    if std::fs::create_dir_all(&directory).is_err() {
        return;
    }
    let path = directory.join("server.log");
    // 로그가 커지면 이전 로그 하나만 남기고 새로 쓴다.
    if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() > LOG_LIMIT) {
        let _ = std::fs::rename(&path, directory.join("server.old.log"));
    }
    if let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(Mutex::new(file))
            .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
            .try_init();
    }
}

/// 다른 기기가 접속할 때 사용할 수 있는 주소 목록. 이 PC 주소를 마지막에 둔다.
fn addresses(settings: &Settings) -> Vec<String> {
    let mut list = Vec::new();
    if settings.allow_lan {
        for interface in if_addrs::get_if_addrs().unwrap_or_default() {
            if interface.is_loopback() {
                continue;
            }
            if let IpAddr::V4(ip) = interface.ip() {
                if !ip.is_link_local() {
                    list.push(format!("http://{ip}:{}", settings.port));
                }
            }
        }
        list.sort();
        list.dedup();
    }
    list.push(format!("http://127.0.0.1:{}", settings.port));
    list
}

#[derive(Serialize)]
struct Status {
    state: &'static str,
    message: Option<String>,
    settings: Settings,
    addresses: Vec<String>,
    autostart: bool,
    server: Option<Value>,
    data_dir: String,
}

#[tauri::command]
async fn server_status(app: AppHandle, controller: State<'_, Shared>) -> Result<Status, String> {
    let snapshot = controller.snapshot().await;
    let server = match &snapshot.state {
        Some(state) => state.server_info().await.ok(),
        None => None,
    };
    Ok(Status {
        state: match (&snapshot.state, &snapshot.error) {
            (Some(_), _) => "running",
            (None, Some(_)) => "error",
            (None, None) => "stopped",
        },
        message: snapshot.error,
        addresses: addresses(&snapshot.settings),
        settings: snapshot.settings,
        autostart: app.autolaunch().is_enabled().unwrap_or(false),
        server,
        data_dir: controller.data_dir.display().to_string(),
    })
}

#[tauri::command]
async fn server_start(controller: State<'_, Shared>) -> Result<(), String> {
    controller.start().await
}

#[tauri::command]
async fn server_stop(controller: State<'_, Shared>) -> Result<(), String> {
    controller.stop().await;
    Ok(())
}

#[tauri::command]
async fn server_restart(controller: State<'_, Shared>) -> Result<(), String> {
    controller.restart().await
}

#[tauri::command]
async fn settings_save(
    app: AppHandle,
    controller: State<'_, Shared>,
    port: u16,
    allow_lan: bool,
    autostart: bool,
) -> Result<(), String> {
    let launcher = app.autolaunch();
    if launcher.is_enabled().unwrap_or(false) != autostart {
        let result = if autostart { launcher.enable() } else { launcher.disable() };
        result.map_err(|error| format!("자동 실행 설정을 바꾸지 못했습니다: {error}"))?;
    }
    controller.save_settings(Settings { port, allow_lan }).await
}

#[tauri::command]
fn open_data_dir(app: AppHandle, controller: State<'_, Shared>) -> Result<(), String> {
    std::fs::create_dir_all(&controller.data_dir).map_err(|error| error.to_string())?;
    app.opener()
        .open_path(controller.data_dir.display().to_string(), None::<&str>)
        .map_err(|error| format!("데이터 폴더를 열지 못했습니다: {error}"))
}

#[tauri::command]
fn open_log(app: AppHandle, controller: State<'_, Shared>) -> Result<(), String> {
    if !controller.log_path.exists() {
        return Err("아직 기록된 로그가 없습니다.".into());
    }
    app.opener()
        .open_path(controller.log_path.display().to_string(), None::<&str>)
        .map_err(|error| format!("로그를 열지 못했습니다: {error}"))
}

#[tauri::command]
async fn admin_setup(controller: State<'_, Shared>, password: String) -> Result<(), String> {
    let state = controller.state().await?;
    state.setup(Some(Principal::local()), None, password, "local").await.map_err(text)?;
    Ok(())
}

#[tauri::command]
async fn admin_password(controller: State<'_, Shared>, password: String) -> Result<(), String> {
    if !(8..=256).contains(&password.chars().count()) {
        return Err("새 비밀번호는 8~256자로 입력해 주세요.".into());
    }
    let state = controller.state().await?;
    state.change_password(Principal::local(), None, password, "local").await.map_err(text)?;
    Ok(())
}

#[tauri::command]
async fn devices_list(controller: State<'_, Shared>) -> Result<Value, String> {
    controller.state().await?.devices().await.map_err(text)
}

#[tauri::command]
async fn device_rename(controller: State<'_, Shared>, id: String, name: String) -> Result<Value, String> {
    if name.trim().chars().count() > 100 {
        return Err("기기 이름은 최대 100자입니다.".into());
    }
    controller.state().await?.rename_device(id, name).await.map_err(text)
}

#[tauri::command]
async fn device_revoke(controller: State<'_, Shared>, id: String) -> Result<(), String> {
    controller.state().await?.revoke_device(id).await.map_err(text)
}

#[tauri::command]
async fn pairing_create(
    controller: State<'_, Shared>,
    name: String,
    role: String,
    server_url: String,
) -> Result<Value, String> {
    if name.chars().count() > 100 {
        return Err("기기 이름은 최대 100자입니다.".into());
    }
    if !["device", "admin"].contains(&role.as_str()) {
        return Err("권한을 다시 선택해 주세요.".into());
    }
    if !lof_library_core::models::valid_server_url(&server_url) {
        return Err("접속 주소를 다시 선택해 주세요.".into());
    }
    let state = controller.state().await?;
    let mut entry = state.create_pairing(Principal::local(), name, role, server_url).await.map_err(text)?;
    let connection = entry["connection"].as_str().unwrap_or_default().to_string();
    let code = qrcode::QrCode::new(connection.as_bytes()).map_err(|error| format!("QR을 만들지 못했습니다: {error}"))?;
    let svg = code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(220, 220)
        .dark_color(qrcode::render::svg::Color("#000000"))
        .light_color(qrcode::render::svg::Color("#ffffff"))
        .build();
    entry["qr_svg"] = Value::String(svg);
    Ok(entry)
}

#[tauri::command]
async fn pairing_cancel(controller: State<'_, Shared>, id: String) -> Result<(), String> {
    controller.state().await?.cancel_pairing(id).await.map_err(text)
}

fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "관리 창 열기", true, None::<&str>)?;
    let restart = MenuItem::with_id(app, "restart", "서버 다시 시작", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "종료", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &restart, &separator, &quit])?;
    let mut tray = TrayIconBuilder::with_id("main")
        .tooltip("Lof Library 서버")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show(app),
            "restart" => {
                let controller = app.state::<Shared>().inner().clone();
                tauri::async_runtime::spawn(async move {
                    let _ = controller.restart().await;
                });
            }
            "quit" => {
                let app = app.clone();
                let controller = app.state::<Shared>().inner().clone();
                tauri::async_runtime::spawn(async move {
                    controller.stop().await;
                    app.exit(0);
                });
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                show(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show(app)))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, Some(vec![HIDDEN_ARG])))
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let base = app.path().app_data_dir()?;
            init_logging(&base);
            let controller: Shared = Arc::new(Controller::new(base));
            app.manage(controller.clone());
            build_tray(app)?;
            tauri::async_runtime::spawn(async move {
                let _ = controller.start().await;
            });
            // PC 시작 때 자동 실행되면 창 없이 트레이에만 둔다.
            if !std::env::args().any(|argument| argument == HIDDEN_ARG) {
                show(app.handle());
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // 창을 닫아도 서버는 계속 실행하고 트레이에 남는다.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            server_status,
            server_start,
            server_stop,
            server_restart,
            settings_save,
            open_data_dir,
            open_log,
            admin_setup,
            admin_password,
            devices_list,
            device_rename,
            device_revoke,
            pairing_create,
            pairing_cancel,
        ])
        .run(tauri::generate_context!())
        .expect("Lof Library를 실행하지 못했습니다.");
}
