//! 같은 호스트에서만 접근하는 관리 통신. Windows 명명 파이프·Unix 소켓을 사용한다.
use crate::{realtime::Realtime, AppState};
use serde_json::{json, Value};
#[cfg(windows)]
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
pub fn address(base: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let path = std::fs::canonicalize(base)
            .unwrap_or_else(|_| base.to_path_buf())
            .to_string_lossy()
            .to_lowercase();
        let hash = hex::encode(Sha256::digest(path.as_bytes()));
        PathBuf::from(format!(r"\\.\pipe\lof-library-{}", &hash[..24]))
    }
    #[cfg(not(windows))]
    {
        base.join("control.sock")
    }
}
pub async fn action(peer: Arc<Realtime>, method: &str, args: Value) -> Result<Value, String> {
    if method == "pair" {
        return peer.pair(args).await;
    }
    if method == "delete" {
        peer.delete_library(args["id"].as_str().unwrap_or(""))
            .await?;
        return Ok(json!(true));
    }
    let method = method.to_string();
    tokio::task::spawn_blocking(move || match method.as_str() {
        "status" => Ok(peer.info()),
        "create" => peer.create_library(args["name"].as_str().unwrap_or("")),
        "rename" => {
            peer.rename_library(
                args["id"].as_str().unwrap_or(""),
                args["name"].as_str().unwrap_or(""),
            )?;
            Ok(json!(true))
        }
        "invite" => peer.invite(args["name"].as_str().unwrap_or(""), args["id"].as_str()),
        "cancelInvite" => {
            peer.cancel_invite(args["id"].as_str().unwrap_or(""));
            Ok(json!(true))
        }
        "leave" => {
            peer.leave(args["id"].as_str().unwrap_or(""))?;
            Ok(json!(true))
        }
        "dismissReset" => {
            peer.dismiss_reset()?;
            Ok(json!(true))
        }
        "remove" => {
            peer.remove_peer(
                args["vault"].as_str().unwrap_or(""),
                args["peer"].as_str().unwrap_or(""),
            )?;
            Ok(json!(true))
        }
        "renamePeer" => {
            peer.rename_peer(
                args["vault"].as_str().unwrap_or(""),
                args["peer"].as_str().unwrap_or(""),
                args["name"].as_str().unwrap_or(""),
            )?;
            Ok(json!(true))
        }
        _ => Err("지원하지 않는 관리 명령입니다.".into()),
    })
    .await
    .map_err(|_| "관리 요청을 처리하지 못했습니다.".to_string())?
}
async fn session<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    stream: S,
    state: AppState,
) {
    let mut stream = BufReader::new(stream);
    let mut bytes = Vec::new();
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        use tokio::io::AsyncReadExt;
        (&mut stream)
            .take(65_537)
            .read_until(b'\n', &mut bytes)
            .await
    })
    .await;
    if !matches!(result, Ok(Ok(_))) || bytes.len() > 65_536 || !bytes.ends_with(b"\n") {
        return;
    }
    let request: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return,
    };
    let method = request["action"].as_str().unwrap_or("");
    let result = if method == "stop" {
        state.closing.cancel();
        Ok(json!(true))
    } else {
        action(state.realtime.clone(), method, request["args"].clone()).await
    };
    let reply = match result {
        Ok(data) => json!({"ok":true,"data":data}),
        Err(error) => json!({"ok":false,"error":error}),
    };
    let _ = stream.write_all(format!("{reply}\n").as_bytes()).await;
}
pub async fn serve(state: AppState) {
    let path = address(&state.config.data_dir);
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ServerOptions;
        let mut server = match ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create(&path)
        {
            Ok(server) => server,
            Err(error) => {
                tracing::error!("로컬 관리 파이프를 열지 못했습니다: {error}");
                state.closing.cancel();
                return;
            }
        };
        loop {
            tokio::select! {_=state.closing.cancelled()=>break,result=server.connect()=>{if result.is_err(){break;}}}
            let next = match ServerOptions::new()
                .reject_remote_clients(true)
                .create(&path)
            {
                Ok(next) => next,
                Err(error) => {
                    tracing::error!("로컬 관리 연결 실패: {error}");
                    break;
                }
            };
            let accepted = std::mem::replace(&mut server, next);
            let state = state.clone();
            tokio::spawn(session(accepted, state));
        }
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        // 인스턴스 잠금 확보 뒤 이 경로의 소켓만 교체한다.
        if let Ok(metadata) = std::fs::symlink_metadata(&path) {
            use std::os::unix::fs::FileTypeExt;
            if !metadata.file_type().is_socket() {
                tracing::error!("관리 소켓 경로에 다른 파일이 있습니다.");
                state.closing.cancel();
                return;
            }
            if std::fs::remove_file(&path).is_err() {
                state.closing.cancel();
                return;
            }
        }
        let listener = match tokio::net::UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(error) => {
                tracing::error!("로컬 관리 소켓을 열지 못했습니다: {error}");
                state.closing.cancel();
                return;
            }
        };
        if std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).is_err() {
            state.closing.cancel();
            return;
        }
        loop {
            tokio::select! {_=state.closing.cancelled()=>break,result=listener.accept()=>{if let Ok((stream,_))=result{tokio::spawn(session(stream,state.clone()));}else{break;}}}
        }
    }
}
pub fn request(base: &Path, method: &str, args: Value) -> Result<Value, String> {
    use std::io::{BufRead, Read, Write};
    let path = address(base);
    #[cfg(windows)]
    let stream = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|_| "Lof Library가 실행 중이 아닙니다. 먼저 시작해 주세요.".to_string())?;
    #[cfg(not(windows))]
    let stream = std::os::unix::net::UnixStream::connect(path)
        .map_err(|_| "Lof Library가 실행 중이 아닙니다. 먼저 시작해 주세요.".to_string())?;
    let mut stream = std::io::BufReader::new(stream);
    stream
        .get_mut()
        .write_all(format!("{}\n", json!({"action":method,"args":args})).as_bytes())
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    stream
        .by_ref()
        .take(8_000_001)
        .read_until(b'\n', &mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > 8_000_000 {
        return Err("관리 응답이 너무 큽니다.".into());
    }
    let result: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if result["ok"] == true {
        Ok(result["data"].clone())
    } else {
        Err(result["error"]
            .as_str()
            .unwrap_or("관리 요청 실패")
            .to_string())
    }
}
