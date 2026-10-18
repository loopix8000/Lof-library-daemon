use clap::{Parser, Subcommand};
use lof_library_core::{control, Config, Daemon};
use serde_json::{json, Value};
use std::{path::PathBuf, process::ExitCode};
mod web;
#[derive(Parser)]
#[command(
    name = "lof-library-server",
    version,
    about = "Iroh 원본 파일 동기화 피어"
)]
struct Cli {
    #[arg(long, env = "LOF_DATA_DIR", default_value = "data", global = true)]
    data_dir: PathBuf,
    /// 웹 관리 화면을 열 주소(예: 0.0.0.0:8787). 생략하면 웹 관리 화면을 켜지 않는다.
    #[arg(long, env = "LOF_WEB_ADDR", global = true)]
    web_addr: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    /// 실행 중인 피어 상태를 확인한다.
    Status,
    /// 등록한 서재를 표시한다.
    Libraries,
    /// 원본 파일을 보관할 서재를 만든다.
    Create {
        name: String,
    },
    /// 이 서재의 동기화 그룹에 초대할 일회용 페어링 코드와 QR을 발급한다. 그룹이 없으면 새로 만든다.
    Invite {
        library_id: String,
    },
    /// 다른 Writer 또는 Library의 페어링 코드로 그룹에 가입한다. 코드 생략 시 표준 입력에서 읽는다.
    Pair {
        library_id: String,
        #[arg(long)]
        code: Option<String>,
    },
    /// 이 서재를 동기화 그룹에서 뺀다.
    Leave {
        library_id: String,
    },
    CancelInvite {
        library_id: String,
    },
    Rename {
        library_id: String,
        name: String,
    },
    Devices,
    Revoke {
        library_id: String,
        peer_id: String,
    },
    Stop,
    /// 웹 관리 화면의 관리자 계정을 지운다. 서버를 다시 시작하면 새 최초 설정 코드가 나온다.
    WebReset,
    /// 로컬 관리 통신으로 실행 상태를 확인한다.
    Healthcheck,
    /// 중지된 피어의 원본·이력·관리 DB를 새 폴더에 백업한다.
    Backup {
        destination: PathBuf,
    },
}
async fn shutdown() {
    #[cfg(unix)]
    {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {_=signal.recv()=>{},_=tokio::signal::ctrl_c()=>{}}
        } else {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
fn manage(cli: &Cli, action: &str, args: Value) -> Result<Value, String> {
    control::request(&cli.data_dir, action, args)
}
#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let result = if let Some(command) = &cli.command {
        (|| -> Result<(), String> {
            let result = match command {
                Command::Status => manage(&cli, "status", json!({}))?,
                Command::Libraries => manage(&cli, "status", json!({}))?["libraries"].clone(),
                Command::Create { name } => manage(&cli, "create", json!({"name":name}))?,
                Command::Invite { library_id } => {
                    let result = manage(&cli, "invite", json!({"id":library_id}))?;
                    let code = result["code"]
                        .as_str()
                        .ok_or("페어링 코드를 받지 못했습니다.")?;
                    if let Ok(qr) = qrcode::QrCode::new(code.as_bytes()) {
                        eprintln!(
                            "{}",
                            qr.render::<qrcode::render::unicode::Dense1x2>().build()
                        );
                    }
                    result
                }
                Command::Pair { library_id, code } => {
                    let code = match code {
                        Some(code) => code.clone(),
                        None => {
                            let mut code = String::new();
                            std::io::stdin()
                                .read_line(&mut code)
                                .map_err(|e| e.to_string())?;
                            code.trim().to_string()
                        }
                    };
                    manage(&cli, "pair", json!({"id":library_id,"code":code}))?
                }
                Command::Leave { library_id } => manage(&cli, "leave", json!({"id":library_id}))?,
                Command::CancelInvite { library_id } => {
                    manage(&cli, "cancelInvite", json!({"id":library_id}))?
                }
                Command::Rename { library_id, name } => {
                    manage(&cli, "rename", json!({"id":library_id,"name":name}))?
                }
                Command::Devices => {
                    let libraries = manage(&cli, "status", json!({}))?;
                    json!(libraries["libraries"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .flat_map(|l| l["peers"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(move |p| json!({"library":l["id"],"peer":p})))
                        .collect::<Vec<_>>())
                }
                Command::Revoke {
                    library_id,
                    peer_id,
                } => manage(&cli, "remove", json!({"vault":library_id,"peer":peer_id}))?,
                Command::Stop => manage(&cli, "stop", json!({}))?,
                Command::WebReset => {
                    match std::fs::remove_file(web::auth_path(&cli.data_dir)) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.to_string()),
                    }
                    json!("웹 관리자 계정을 지웠습니다. 서버를 다시 시작하면 새 최초 설정 코드가 표시됩니다.")
                }
                Command::Healthcheck => {
                    let state = manage(&cli, "status", json!({}))?;
                    if state["ready"] != true
                        || state["error"].as_str().is_some_and(|e| !e.is_empty())
                    {
                        return Err("Iroh 피어가 준비되지 않았습니다.".into());
                    }
                    return Ok(());
                }
                Command::Backup { destination } => {
                    lof_library_core::backup::backup(&cli.data_dir, destination)?;
                    json!({"path":destination})
                }
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?
            );
            Ok(())
        })()
    } else {
        match Daemon::start(Config::new(&cli.data_dir)) {
            Ok(daemon) => {
                let web = match &cli.web_addr {
                    Some(address) => match tokio::net::TcpListener::bind(address).await {
                        Ok(listener) => {
                            tracing::info!("웹 관리 화면: http://{address}");
                            Some(tokio::spawn(web::serve(
                                listener,
                                daemon.state(),
                                web::auth_path(&cli.data_dir),
                            )))
                        }
                        Err(error) => {
                            eprintln!("웹 관리 화면 주소({address})를 열지 못했습니다: {error}");
                            return ExitCode::FAILURE;
                        }
                    },
                    None => None,
                };
                let result = daemon.serve(shutdown()).await.map_err(|e| e.to_string());
                if let Some(web) = web {
                    let _ = web.await;
                }
                result
            }
            Err(e) => Err(e.to_string()),
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
