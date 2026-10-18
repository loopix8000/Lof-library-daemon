#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod controller;
mod updater;

use std::fs::OpenOptions;
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use controller::{Controller, Settings};
use lof_library_core::error::ApiError;
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

const PUBLIC_IP_SERVICES: [&str; 2] = ["https://api.ipify.org", "https://ipv4.icanhazip.com"];
const PUBLIC_IP_TTL: Duration = Duration::from_secs(30 * 60);
const PUBLIC_IP_RETRY: Duration = Duration::from_secs(60);

#[derive(Serialize)]
struct Address {
    url: String,
    kind: &'static str,
}

fn kind_of(ip: Ipv4Addr) -> Option<&'static str> {
    let [first, second, ..] = ip.octets();
    if ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() {
        None
    } else if ip.is_private() {
        Some("lan")
    } else if first == 100 && (64..128).contains(&second) {
        // 100.64.0.0/10은 Tailscale 같은 VPN이 사용하는 주소 범위다.
        Some("vpn")
    } else {
        Some("public")
    }
}

/// 다른 기기가 접속할 때 사용할 수 있는 주소 목록. 같은 네트워크·VPN·인터넷·이 PC 순서로 둔다.
fn addresses(settings: &Settings, public: Option<Ipv4Addr>) -> Vec<Address> {
    let mut found = Vec::new();
    if settings.allow_lan {
        for interface in if_addrs::get_if_addrs().unwrap_or_default() {
            if let IpAddr::V4(ip) = interface.ip() {
                if let Some(kind) = kind_of(ip) {
                    found.push((kind, ip));
                }
            }
        }
        if let Some(ip) = public {
            found.push(("public", ip));
        }
    }
    let rank = |kind: &str| ["lan", "vpn", "public"].iter().position(|item| *item == kind).unwrap_or(3);
    found.sort_by_key(|(kind, ip)| (rank(kind), *ip));
    found.dedup_by_key(|(_, ip)| *ip);
    let mut list: Vec<Address> = found
        .into_iter()
        .map(|(kind, ip)| Address { url: format!("http://{ip}:{}", settings.port), kind })
        .collect();
    list.push(Address { url: format!("http://127.0.0.1:{}", settings.port), kind: "local" });
    list
}

/// 외부에서 접속할 공인 IP. 공유기 안쪽에서는 외부 서비스에 물어봐야 알 수 있다.
#[derive(Default)]
struct PublicIp {
    value: Option<Ipv4Addr>,
    checked: Option<Instant>,
    checking: bool,
}

type SharedIp = Arc<Mutex<PublicIp>>;

async fn fetch_public_ip() -> Option<Ipv4Addr> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(5)).build().ok()?;
    for service in PUBLIC_IP_SERVICES {
        let Ok(response) = client.get(service).send().await else { continue };
        let Ok(text) = response.text().await else { continue };
        if let Ok(ip) = text.trim().parse::<Ipv4Addr>() {
            if kind_of(ip) == Some("public") {
                return Some(ip);
            }
        }
    }
    None
}

/// 저장한 공인 IP가 오래됐으면 백그라운드에서 다시 확인한다.
fn ensure_public_ip(public: &SharedIp, force: bool) {
    {
        let mut state = public.lock().unwrap_or_else(|error| error.into_inner());
        let wait = if state.value.is_some() { PUBLIC_IP_TTL } else { PUBLIC_IP_RETRY };
        if state.checking || (!force && state.checked.is_some_and(|checked| checked.elapsed() < wait)) {
            return;
        }
        state.checking = true;
    }
    let public = public.clone();
    tauri::async_runtime::spawn(async move {
        let value = fetch_public_ip().await;
        let mut state = public.lock().unwrap_or_else(|error| error.into_inner());
        state.value = value;
        state.checked = Some(Instant::now());
        state.checking = false;
    });
}

#[derive(Serialize)]
struct Status {
    state: &'static str,
    message: Option<String>,
    settings: Settings,
    addresses: Vec<Address>,
    /// 공인 IP 확인 상태: checking, ok, unavailable. 다른 기기 접속을 막았으면 없음.
    public_ip: Option<&'static str>,
    autostart: bool,
    server: Option<Value>,
    data_dir: String,
}

#[tauri::command]
fn public_ip_refresh(public: State<'_, SharedIp>) {
    ensure_public_ip(&public, true);
}

#[tauri::command]
async fn server_status(app: AppHandle, controller: State<'_, Shared>, public: State<'_, SharedIp>) -> Result<Status, String> {
    let snapshot = controller.snapshot().await;
    let server = match &snapshot.state {
        Some(state) => state.server_info().await.ok(),
        None => None,
    };
    let (public_ip, public_state) = if snapshot.settings.allow_lan {
        ensure_public_ip(&public, false);
        let state = public.lock().unwrap_or_else(|error| error.into_inner());
        let label = match (state.value, state.checking || state.checked.is_none()) {
            (Some(_), _) => "ok",
            (None, true) => "checking",
            (None, false) => "unavailable",
        };
        (state.value, Some(label))
    } else {
        (None, None)
    };
    Ok(Status {
        state: match (&snapshot.state, &snapshot.error) {
            (Some(_), _) => "running",
            (None, Some(_)) => "error",
            (None, None) => "stopped",
        },
        message: snapshot.error,
        addresses: addresses(&snapshot.settings, public_ip),
        public_ip: public_state,
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

fn qr_svg(text: &str) -> Result<String, String> {
    let code = qrcode::QrCode::new(text.as_bytes()).map_err(|error| format!("QR을 만들지 못했습니다: {error}"))?;
    Ok(code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(220, 220)
        .dark_color(qrcode::render::svg::Color("#000000"))
        .light_color(qrcode::render::svg::Color("#ffffff"))
        .build())
}

/// 기기 이름은 연결하는 기기가 직접 보내므로 받지 않는다.
#[tauri::command]
async fn pairing_create(controller: State<'_, Shared>, server_url: String) -> Result<Value, String> {
    if !lof_library_core::models::valid_server_url(&server_url) {
        return Err("접속 주소를 다시 선택해 주세요.".into());
    }
    let state = controller.state().await?;
    let mut entry = state.create_pairing(String::new(), server_url).await.map_err(text)?;
    entry["qr_svg"] = Value::String(qr_svg(entry["connection"].as_str().unwrap_or_default())?);
    Ok(entry)
}

/// 이미 발급한 코드로 다른 접속 주소의 연결 정보와 QR만 다시 만든다.
#[tauri::command]
fn pairing_link(code: String, server_url: String) -> Result<Value, String> {
    if !lof_library_core::models::valid_server_url(&server_url) {
        return Err("접속 주소를 다시 선택해 주세요.".into());
    }
    let connection = lof_library_core::connection_link(&server_url, &code);
    Ok(json!({ "connection": connection, "server_url": server_url.trim_end_matches('/'), "qr_svg": qr_svg(&connection)? }))
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
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let base = app.path().app_data_dir()?;
            init_logging(&base);
            let controller: Shared = Arc::new(Controller::new(base));
            app.manage(controller.clone());
            app.manage::<SharedIp>(Arc::default());
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
            public_ip_refresh,
            server_start,
            server_stop,
            server_restart,
            settings_save,
            open_data_dir,
            open_log,
            devices_list,
            device_rename,
            device_revoke,
            pairing_create,
            pairing_link,
            pairing_cancel,
        ])
        .run(tauri::generate_context!())
        .expect("Lof Library를 실행하지 못했습니다.");
}
