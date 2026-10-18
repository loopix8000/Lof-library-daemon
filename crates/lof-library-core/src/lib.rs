//! Iroh를 사용하는 Lof Library 원본 파일 동기화 피어.
pub mod backup;
pub mod control;
pub mod error;
pub mod realtime;
pub mod store;
use error::StartError;
use realtime::Realtime;
use std::{
    fs::{File, OpenOptions},
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
};
use store::Store;
use tokio_util::sync::CancellationToken;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
#[derive(Clone, Debug)]
pub struct Config {
    pub data_dir: PathBuf,
}
impl Config {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: path.into(),
        }
    }
}
pub struct InstanceLock(#[allow(dead_code)] File);
impl InstanceLock {
    pub fn acquire(directory: &Path) -> Result<Self, StartError> {
        std::fs::create_dir_all(directory)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("daemon.lock"))?;
        file.try_lock().map_err(|_| {
            StartError("이 데이터 폴더를 사용하는 Lof Library가 이미 실행 중입니다.".into())
        })?;
        Ok(Self(file))
    }
}
pub struct Inner {
    pub config: Config,
    pub realtime: Arc<Realtime>,
    pub closing: CancellationToken,
    _lock: InstanceLock,
}
#[derive(Clone)]
pub struct AppState(Arc<Inner>);
impl std::ops::Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}
pub struct Daemon {
    state: AppState,
}
impl Daemon {
    pub fn start(config: Config) -> Result<Self, StartError> {
        let lock = InstanceLock::acquire(&config.data_dir)?;
        let store = Arc::new(Store::open(&config.data_dir)?);
        let realtime =
            Realtime::new(config.data_dir.join("realtime"), store).map_err(StartError)?;
        Ok(Self {
            state: AppState(Arc::new(Inner {
                config,
                realtime,
                closing: CancellationToken::new(),
                _lock: lock,
            })),
        })
    }
    pub fn state(&self) -> AppState {
        self.state.clone()
    }
    pub async fn serve(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        let state = self.state;
        let mut peer = tokio::spawn(state.realtime.clone().run(state.closing.clone()));
        let control = tokio::spawn(control::serve(state.clone()));
        tokio::pin!(shutdown);
        tokio::select! {
            _=&mut shutdown=>{state.closing.cancel();let _=peer.await;},
            _=state.closing.cancelled()=>{let _=peer.await;},
            _=&mut peer=>{state.closing.cancel();}
        }
        state.closing.cancel();
        let _ = control.await;
        let error = state.realtime.info()["error"]
            .as_str()
            .unwrap_or("")
            .to_string();
        if error.is_empty() {
            Ok(())
        } else {
            Err(std::io::Error::other(error))
        }
    }
}
