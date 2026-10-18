#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod controller;
mod updater;

use std::fs::OpenOptions;
use std::path::Path;
use std::sync::{Arc, Mutex};

use controller::Controller;
use serde::Serialize;
use serde_json::{json, Value};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, State, WindowEvent};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt as _};
use tauri_plugin_opener::OpenerExt;

type Shared = Arc<Controller>;

const HIDDEN_ARG: &str = "--hidden";
const LOG_LIMIT: u64 = 5 * 1024 * 1024;

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

#[derive(Serialize)]
struct Status {
    state: &'static str,
    message: Option<String>,
    peer: Option<Value>,
    autostart: bool,
    version: &'static str,
    data_dir: String,
}
#[tauri::command]
async fn server_status(app: AppHandle, controller: State<'_, Shared>) -> Result<Status, String> {
    let snapshot = controller.snapshot().await;
    let peer = if let Some(state) = &snapshot.state {
        let rt = state.realtime.clone();
        Some(
            tokio::task::spawn_blocking(move || rt.info())
                .await
                .map_err(|_| "피어 상태 확인 실패")?,
        )
    } else {
        None
    };
    let phase = if snapshot.error.is_some() {
        "error"
    } else if snapshot.state.is_none() {
        "stopped"
    } else if peer.as_ref().is_some_and(|p| p["ready"] == true) {
        "running"
    } else {
        "starting"
    };
    Ok(Status {
        state: phase,
        message: snapshot.error,
        peer,
        autostart: app.autolaunch().is_enabled().unwrap_or(false),
        version: env!("CARGO_PKG_VERSION"),
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
fn settings_save(app: AppHandle, autostart: bool) -> Result<(), String> {
    let launcher = app.autolaunch();
    if launcher.is_enabled().unwrap_or(false) != autostart {
        let result = if autostart {
            launcher.enable()
        } else {
            launcher.disable()
        };
        result.map_err(|e| format!("자동 실행 설정 변경 실패: {e}"))?;
    }
    Ok(())
}
#[tauri::command]
async fn open_library(
    app: AppHandle,
    controller: State<'_, Shared>,
    id: String,
) -> Result<(), String> {
    let path = controller.state().await?.realtime.library_path(&id)?;
    app.opener()
        .open_path(path.display().to_string(), None::<&str>)
        .map_err(|e| e.to_string())
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

fn qr_svg(text: &str) -> Result<String, String> {
    let code = qrcode::QrCode::new(text.as_bytes())
        .map_err(|error| format!("QR을 만들지 못했습니다: {error}"))?;
    Ok(code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(220, 220)
        .dark_color(qrcode::render::svg::Color("#000000"))
        .light_color(qrcode::render::svg::Color("#ffffff"))
        .build())
}

#[tauri::command]
async fn realtime_manage(
    controller: State<'_, Shared>,
    action: String,
    args: Value,
) -> Result<Value, String> {
    let state = controller.state().await?;
    let mut result =
        lof_library_core::control::action(state.realtime.clone(), &action, args).await?;
    if action == "invite" {
        result["qr_svg"] = json!(qr_svg(result["code"].as_str().unwrap_or(""))?);
    }
    Ok(result)
}

fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "관리 창 열기", true, None::<&str>)?;
    let restart = MenuItem::with_id(app, "restart", "동기화 다시 시작", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "종료", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &restart, &separator, &quit])?;
    let mut tray = TrayIconBuilder::with_id("main")
        .tooltip("Lof Library 실시간 동기화")
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
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
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
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show(app)
        }))
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec![HIDDEN_ARG]),
        ))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let base = app.path().app_data_dir()?;
            init_logging(&base);
            let controller: Shared = Arc::new(Controller::new(base));
            app.manage(controller.clone());
            app.manage(updater::Updates::default());
            build_tray(app)?;
            updater::start_checks(app.handle().clone());
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
            updater::update_status,
            updater::update_check,
            updater::update_install,
            server_status,
            server_start,
            server_stop,
            server_restart,
            settings_save,
            open_data_dir,
            open_library,
            open_log,
            realtime_manage,
        ])
        .run(tauri::generate_context!())
        .expect("Lof Library를 실행하지 못했습니다.");
}
