use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use lof_library_core::auth::Auth;
use lof_library_core::store::{Clock, Store};
use lof_library_core::{backup, connection_link, models, Config, Daemon};
use tokio::net::TcpListener;

#[derive(Parser)]
#[command(name = "lof-library-server", version, about = "Lof Writer 서재 동기화 서버")]
struct Cli {
    /// 접속을 받을 주소
    #[arg(long, env = "LOF_HOST", default_value = "0.0.0.0", global = true)]
    host: String,
    /// 접속을 받을 포트
    #[arg(long, env = "LOF_PORT", default_value_t = 8787, global = true)]
    port: u16,
    /// 원고·자료·설정·이력을 보관할 폴더
    #[arg(long, env = "LOF_DATA_DIR", default_value = "data", global = true)]
    data_dir: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// 실행 중인 서버의 DB를 온라인 백업한다. 기존 파일은 덮어쓰지 않는다.
    Backup { destination: PathBuf },
    /// 새 기기를 연결할 일회용 연결 코드를 발급한다.
    Pair {
        /// 연결할 기기의 이름
        #[arg(long, default_value = "")]
        name: String,
        /// 다른 기기에서 접속할 서버 주소. 예: http://192.168.0.10:8787
        #[arg(long)]
        server_url: Option<String>,
    },
    /// 연결된 기기 목록을 표시한다.
    Devices,
    /// 기기의 연결을 해제한다.
    Revoke { device_id: String },
    /// 웹 관리자 계정과 로그인을 지우고 새 최초 설정 코드를 발급한다.
    ResetAdmin,
    /// 상태 확인 API를 호출한다. Docker 상태 확인에 사용한다.
    Healthcheck,
}

fn config(cli: &Cli) -> Config {
    let mut config = Config::new(&cli.data_dir);
    config.web_admin = true;
    if let Ok(name) = std::env::var("LOF_SERVER_NAME") {
        if !name.trim().is_empty() {
            config.server_name = name.trim().to_string();
        }
    }
    config
}

/// 호스트에서 실행하는 관리 명령은 서버와 같은 데이터 폴더를 직접 연다. 서버가 실행 중이어도 된다.
fn host(cli: &Cli) -> Result<(Store, Auth), String> {
    let config = config(cli);
    if !config.data_dir.join("library.sqlite").is_file() {
        return Err(format!("{}에 서버 데이터가 없습니다. 먼저 서버를 실행해 주세요.", config.data_dir.display()));
    }
    let store = Store::open(&config.data_dir, Clock::default()).map_err(|error| error.to_string())?;
    let auth = Auth::load(&store, &config.server_name).map_err(|error| error.to_string())?;
    Ok((store, auth))
}

fn pair(store: &Store, auth: &Auth, name: &str, server_url: Option<&str>) -> Result<(), String> {
    if name.chars().count() > 100 {
        return Err("기기 이름은 최대 100자입니다.".into());
    }
    if let Some(url) = server_url {
        if !models::valid_server_url(url) {
            return Err("서버 주소는 http:// 또는 https://로 시작하는 주소로 입력해 주세요.".into());
        }
    }
    let entry = auth.create_pairing(store, name).map_err(|error| error.to_string())?;
    let code = entry["code"].as_str().unwrap_or_default();
    println!("연결 코드: {code}  (10분 동안 한 번만 사용)");
    match server_url {
        Some(url) => {
            let connection = connection_link(url, code);
            println!("연결 정보: {connection}\n");
            let qr = qrcode::QrCode::new(connection.as_bytes()).map_err(|error| error.to_string())?;
            println!("{}", qr.render::<qrcode::render::unicode::Dense1x2>().quiet_zone(true).build());
        }
        None => println!("Lof Writer의 서버 연결 화면에 서버 주소와 이 코드를 입력해 주세요. --server-url을 주면 연결 정보와 QR도 표시합니다."),
    }
    Ok(())
}

fn devices(store: &Store, auth: &Auth) -> Result<(), String> {
    let list = auth.devices(store, &Default::default()).map_err(|error| error.to_string())?;
    let devices = list["devices"].as_array().cloned().unwrap_or_default();
    if devices.is_empty() {
        println!("연결된 기기가 없습니다.");
    }
    for device in devices {
        let platform = device["platform"].as_str().filter(|value| !value.is_empty()).unwrap_or("-");
        println!("{}  {}  ({platform})", device["id"].as_str().unwrap_or_default(), device["name"].as_str().unwrap_or_default());
    }
    Ok(())
}

fn healthcheck(port: u16) -> Result<(), String> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(3)).map_err(|error| error.to_string())?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).map_err(|error| error.to_string())?;
    stream
        .write_all(b"GET /healthz HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n")
        .map_err(|error| error.to_string())?;
    let mut response = String::new();
    stream.read_to_string(&mut response).map_err(|error| error.to_string())?;
    let status = response.lines().next().unwrap_or_default();
    if status.split_whitespace().nth(1) == Some("200") {
        Ok(())
    } else {
        Err(format!("상태 확인 응답: {status}"))
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminate = signal(SignalKind::terminate()).expect("종료 신호를 받을 수 있어야 합니다");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn serve(cli: Cli) -> Result<(), String> {
    let daemon = Daemon::start(config(&cli)).map_err(|error| error.to_string())?;
    let listener = TcpListener::bind((cli.host.as_str(), cli.port))
        .await
        .map_err(|error| format!("{}:{} 주소를 열지 못했습니다: {error}", cli.host, cli.port))?;
    tracing::info!("http://{}:{} 에서 접속을 받습니다.", cli.host, cli.port);
    daemon.serve(listener, shutdown_signal()).await.map_err(|error| error.to_string())
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();
    let result = match &cli.command {
        None => serve(cli).await,
        Some(Command::Backup { destination }) => {
            backup::backup(&cli.data_dir, destination).map(|_| println!("서재 백업을 완료했습니다."))
        }
        Some(Command::Pair { name, server_url }) => host(&cli).and_then(|(store, auth)| pair(&store, &auth, name, server_url.as_deref())),
        Some(Command::Devices) => host(&cli).and_then(|(store, auth)| devices(&store, &auth)),
        Some(Command::Revoke { device_id }) => host(&cli).and_then(|(store, auth)| {
            auth.revoke_device(&store, device_id).map_err(|error| error.to_string())?;
            println!("기기 연결을 해제했습니다.");
            Ok(())
        }),
        Some(Command::ResetAdmin) => host(&cli).and_then(|(store, auth)| {
            let code = auth.reset_admin(&store).map_err(|error| error.to_string())?.unwrap_or_default();
            println!("관리자 계정을 초기화했습니다. 웹 관리 화면에서 최초 설정 코드 {code} 로 아이디와 비밀번호를 다시 만들어 주세요.");
            Ok(())
        }),
        Some(Command::Healthcheck) => healthcheck(cli.port),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}
