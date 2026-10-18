use lof_library_core::{AppState, Config, Daemon};
use std::path::PathBuf;
use tokio::{
    sync::{oneshot, Mutex},
    task::JoinHandle,
};
struct Running {
    state: AppState,
    stop: oneshot::Sender<()>,
    task: JoinHandle<std::io::Result<()>>,
}
#[derive(Default)]
struct Inner {
    running: Option<Running>,
    error: Option<String>,
}
pub struct Snapshot {
    pub state: Option<AppState>,
    pub error: Option<String>,
}
pub struct Controller {
    inner: Mutex<Inner>,
    pub data_dir: PathBuf,
    pub log_path: PathBuf,
}
impl Controller {
    pub fn new(base: PathBuf) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            data_dir: base.join("data"),
            log_path: base.join("logs").join("server.log"),
        }
    }
    pub async fn snapshot(&self) -> Snapshot {
        let mut inner = self.inner.lock().await;
        if inner.running.as_ref().is_some_and(|r| r.task.is_finished()) {
            let error = inner
                .running
                .as_ref()
                .map(|r| {
                    r.state.realtime.info()["error"]
                        .as_str()
                        .unwrap_or("")
                        .to_string()
                })
                .unwrap_or_default();
            inner.running = None;
            inner.error = Some(if error.is_empty() {
                "피어 실행이 종료되었습니다. 로그를 확인한 뒤 다시 시작해 주세요.".into()
            } else {
                error
            });
        }
        Snapshot {
            state: inner.running.as_ref().map(|r| r.state.clone()),
            error: inner.error.clone(),
        }
    }
    pub async fn state(&self) -> Result<AppState, String> {
        self.snapshot()
            .await
            .state
            .ok_or("피어를 먼저 시작해 주세요.".into())
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
    async fn start_locked(&self, inner: &mut Inner) -> Result<(), String> {
        if inner
            .running
            .as_ref()
            .is_some_and(|r| !r.task.is_finished())
        {
            return Ok(());
        }
        inner.running = None;
        let daemon = match Daemon::start(Config::new(&self.data_dir)) {
            Ok(d) => d,
            Err(e) => {
                let message = e.to_string();
                inner.error = Some(message.clone());
                return Err(message);
            }
        };
        let state = daemon.state();
        let (stop, signal) = oneshot::channel();
        let task = tokio::spawn(daemon.serve(async {
            let _ = signal.await;
        }));
        inner.running = Some(Running { state, stop, task });
        inner.error = None;
        Ok(())
    }
    async fn stop_locked(inner: &mut Inner) {
        if let Some(running) = inner.running.take() {
            let _ = running.stop.send(());
            drop(running.state);
            let _ = running.task.await;
        }
    }
}
