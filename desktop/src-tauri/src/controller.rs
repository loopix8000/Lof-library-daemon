use std::path::PathBuf;
use std::time::Duration;

use lof_library_core::{AppState, Config, Daemon};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub port: u16,
    /// 다른 기기에서 접속할 수 있도록 모든 네트워크 주소에서 접속을 받는다.
    pub allow_lan: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { port: 8787, allow_lan: true }
    }
}

struct Running {
    state: AppState,
    stop: oneshot::Sender<()>,
    task: JoinHandle<std::io::Result<()>>,
}

#[derive(Default)]
struct Inner {
    settings: Settings,
    running: Option<Running>,
    error: Option<String>,
}

pub struct Snapshot {
    pub settings: Settings,
    pub state: Option<AppState>,
    pub error: Option<String>,
}

/// 관리 앱 안에서 서재 서버를 시작·중지하고 설정을 보관한다.
pub struct Controller {
    inner: Mutex<Inner>,
    pub data_dir: PathBuf,
    pub log_path: PathBuf,
    settings_path: PathBuf,
}

impl Controller {
    pub fn new(base: PathBuf) -> Self {
        let settings_path = base.join("settings.json");
        let settings = std::fs::read_to_string(&settings_path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self {
            inner: Mutex::new(Inner { settings, ..Inner::default() }),
            data_dir: base.join("data"),
            log_path: base.join("logs").join("server.log"),
            settings_path,
        }
    }

    pub async fn snapshot(&self) -> Snapshot {
        let mut inner = self.inner.lock().await;
        if inner.running.as_ref().is_some_and(|running| running.task.is_finished()) {
            inner.running = None;
            inner.error = Some("서버가 예기치 않게 종료되었습니다. 로그를 확인한 뒤 다시 시작해 주세요.".into());
        }
        Snapshot {
            settings: inner.settings.clone(),
            state: inner.running.as_ref().map(|running| running.state.clone()),
            error: inner.error.clone(),
        }
    }

    pub async fn state(&self) -> Result<AppState, String> {
        self.snapshot().await.state.ok_or_else(|| "서버가 실행 중이 아닙니다. 먼저 서버를 시작해 주세요.".into())
    }

    pub async fn start(&self) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        self.start_locked(&mut inner).await
    }

    pub async fn stop(&self) {
        let mut inner = self.inner.lock().await;
        Self::stop_locked(&mut inner).await;
        inner.error = None;
    }

    pub async fn restart(&self) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        Self::stop_locked(&mut inner).await;
        self.start_locked(&mut inner).await
    }

    pub async fn save_settings(&self, settings: Settings) -> Result<(), String> {
        if settings.port < 1024 {
            return Err("포트는 1024~65535 사이로 지정해 주세요.".into());
        }
        if let Some(directory) = self.settings_path.parent() {
            std::fs::create_dir_all(directory).map_err(|error| format!("설정 폴더를 만들지 못했습니다: {error}"))?;
        }
        let text = serde_json::to_string_pretty(&settings).map_err(|error| error.to_string())?;
        std::fs::write(&self.settings_path, text).map_err(|error| format!("설정을 저장하지 못했습니다: {error}"))?;
        let mut inner = self.inner.lock().await;
        let changed = inner.settings != settings;
        inner.settings = settings;
        if changed && inner.running.is_some() {
            Self::stop_locked(&mut inner).await;
            return self.start_locked(&mut inner).await;
        }
        Ok(())
    }

    async fn start_locked(&self, inner: &mut Inner) -> Result<(), String> {
        if inner.running.as_ref().is_some_and(|running| !running.task.is_finished()) {
            return Ok(());
        }
        inner.running = None;
        let settings = inner.settings.clone();
        let result = async {
            let daemon = Daemon::start(Config::new(&self.data_dir)).map_err(|error| error.to_string())?;
            let host = if settings.allow_lan { "0.0.0.0" } else { "127.0.0.1" };
            let listener = TcpListener::bind((host, settings.port)).await.map_err(|error| {
                format!("{}번 포트를 열지 못했습니다. 다른 프로그램이 사용 중인지 확인해 주세요. ({error})", settings.port)
            })?;
            let state = daemon.state();
            let (stop, signal) = oneshot::channel::<()>();
            let task = tokio::spawn(daemon.serve(listener, async {
                let _ = signal.await;
            }));
            tracing::info!("{host}:{} 에서 접속을 받습니다.", settings.port);
            Ok::<_, String>(Running { state, stop, task })
        }
        .await;
        match result {
            Ok(running) => {
                inner.running = Some(running);
                inner.error = None;
                Ok(())
            }
            Err(message) => {
                tracing::error!("서버를 시작하지 못했습니다: {message}");
                inner.error = Some(message.clone());
                Err(message)
            }
        }
    }

    async fn stop_locked(inner: &mut Inner) {
        let Some(running) = inner.running.take() else { return };
        let _ = running.stop.send(());
        drop(running.state);
        // 실시간 수신 연결을 닫고 진행 중인 요청을 마칠 때까지 기다린다.
        if tokio::time::timeout(Duration::from_secs(10), running.task).await.is_err() {
            tracing::warn!("서버 종료가 늦어지고 있습니다.");
        }
    }
}
