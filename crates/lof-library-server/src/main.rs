use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use lof_library_core::auth::Auth;
use lof_library_core::store::{Clock, Store};
use lof_library_core::{backup, Config, Daemon};
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
    /// 관리자 비밀번호와 로그인을 지우고 새 최초 설정 코드를 발급한다.
    ResetAdmin,
    /// 상태 확인 API를 호출한다. Docker 상태 확인에 사용한다.
    Healthcheck,
}

fn config(cli: &Cli) -> Config {
    let mut config = Config::new(&cli.data_dir);
    config.legacy_token = std::env::var("LOF_API_TOKEN").unwrap_or_default().trim().to_string();
    if let Ok(name) = std::env::var("LOF_SERVER_NAME") {
        if !name.trim().is_empty() {
            config.server_name = name.trim().to_string();
        }
    }
    config
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
        Some(Command::ResetAdmin) => (|| {
            let config = config(&cli);
            let store = Store::open(&config.data_dir, Clock::default()).map_err(|error| error.to_string())?;
            let auth = Auth::load(&store, &config.server_name, "").map_err(|error| error.to_string())?;
            let code = auth.reset_admin(&store).map_err(|error| error.to_string())?.unwrap_or_default();
            println!("관리자 설정을 초기화했습니다. 관리 화면에서 최초 설정 코드 {code} 를 입력해 주세요.");
            Ok(())
        })(),
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
