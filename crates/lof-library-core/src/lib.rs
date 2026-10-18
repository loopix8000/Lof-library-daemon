//! Lof Writer 서재 동기화 서버 본체. Docker용 실행 파일과 설치형 관리 앱이 함께 사용한다.

pub mod api;
pub mod auth;
pub mod backup;
pub mod error;
pub mod history;
pub mod models;
pub mod store;

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use auth::{Auth, Principal, Throttle};
use error::{ApiError, ApiResult, StartError};
use store::{Clock, Store};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const SERVICE: &str = "Lof Library Daemon";

#[derive(Clone, Debug)]
pub struct Config {
    pub data_dir: PathBuf,
    /// 이전 버전의 공통 토큰. 설정하면 일반 기기 권한으로만 계속 허용한다.
    pub legacy_token: String,
    pub server_name: String,
    pub history_tick: Duration,
    pub maintenance: Duration,
    pub heartbeat: Duration,
    pub max_request_bytes: usize,
}

impl Config {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            legacy_token: String::new(),
            server_name: Self::default_server_name(),
            history_tick: Duration::from_secs(10),
            maintenance: Duration::from_secs(3600),
            heartbeat: Duration::from_secs(15),
            max_request_bytes: 8 * 1024 * 1024,
        }
    }

    pub fn default_server_name() -> String {
        hostname::get()
            .ok()
            .and_then(|name| name.into_string().ok())
            .map(|name| name.trim().chars().take(100).collect::<String>())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "Lof Library".into())
    }

    pub fn validate(&self) -> Result<(), StartError> {
        let token = &self.legacy_token;
        if !token.is_empty() && (token.len() < 32 || !token.is_ascii() || token.chars().any(char::is_whitespace)) {
            return Err(StartError("LOF_API_TOKEN을 사용하려면 공백 없는 32자 이상의 값을 설정해 주세요.".into()));
        }
        let name = self.server_name.trim();
        if name.is_empty() || name.chars().count() > 100 {
            return Err(StartError("서버 이름은 1~100자로 설정해 주세요.".into()));
        }
        if self.history_tick.is_zero() || self.maintenance.is_zero() || self.heartbeat.is_zero() {
            return Err(StartError("작업 주기는 0보다 커야 합니다.".into()));
        }
        Ok(())
    }
}

/// 같은 데이터 폴더에서 서버가 중복 실행되는 것을 막는다. 값이 사라지면 잠금도 풀린다.
pub struct InstanceLock(#[allow(dead_code)] File);

impl InstanceLock {
    pub fn acquire(directory: &Path) -> Result<Self, StartError> {
        std::fs::create_dir_all(directory)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join("daemon.lock"))?;
        file.try_lock()
            .map_err(|_| StartError("이 데이터 폴더를 사용하는 서버가 이미 실행 중입니다.".into()))?;
        Ok(Self(file))
    }
}

pub enum Gate {
    Pairing,
    Login,
}

/// 요청 처리기와 설치형 관리 앱이 함께 쓰는 서버 상태
pub struct Inner {
    pub config: Config,
    pub store: Store,
    pub auth: Auth,
    pub started: Instant,
    pub closing: CancellationToken,
    notifier: watch::Sender<u64>,
    online: Mutex<HashMap<String, usize>>,
    pair_throttle: Throttle,
    login_throttle: Throttle,
    // 저장소가 닫힌 뒤에 잠금이 풀리도록 마지막에 둔다.
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

pub fn connection_link(server_url: &str, code: &str) -> String {
    // 다른 기기에 복사하거나 QR로 보여 줄 연결 정보. 서버 주소와 일회용 코드만 담는다.
    let query = form_urlencoded::Serializer::new(String::new())
        .append_pair("server", server_url.trim_end_matches('/'))
        .append_pair("code", code)
        .finish();
    format!("lof-library://connect?{query}")
}

impl AppState {
    /// DB 작업을 작업 스레드에서 실행한다.
    pub async fn blocking<T: Send + 'static>(
        &self,
        action: impl FnOnce(&Inner) -> ApiResult<T> + Send + 'static,
    ) -> ApiResult<T> {
        let state = self.clone();
        tokio::task::spawn_blocking(move || action(&state)).await.map_err(|_| ApiError::internal())?
    }

    /// 변경을 저장한 뒤 실시간 수신 중인 기기들을 깨운다.
    pub async fn write<T: Send + 'static>(
        &self,
        action: impl FnOnce(&Inner) -> ApiResult<T> + Send + 'static,
    ) -> ApiResult<T> {
        let result = self.blocking(action).await?;
        self.notify();
        Ok(result)
    }

    pub fn notify(&self) {
        self.notifier.send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.notifier.subscribe()
    }

    pub fn online(&self) -> HashMap<String, usize> {
        self.online.lock().unwrap_or_else(|error| error.into_inner()).clone()
    }

    pub fn connect(&self, id: &str) {
        *self.online.lock().unwrap_or_else(|error| error.into_inner()).entry(id.to_string()).or_default() += 1;
    }

    pub fn disconnect(&self, id: &str) {
        let mut online = self.online.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(count) = online.get_mut(id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                online.remove(id);
            }
        }
    }

    /// 코드·비밀번호 입력 실패를 세어 일정 횟수를 넘으면 잠시 거부한다.
    pub async fn guarded<T: Send + 'static>(
        &self,
        gate: Gate,
        source: &str,
        action: impl FnOnce(&Inner) -> ApiResult<T> + Send + 'static,
    ) -> ApiResult<T> {
        let throttle = match gate {
            Gate::Pairing => &self.pair_throttle,
            Gate::Login => &self.login_throttle,
        };
        throttle.check(source)?;
        let result = self.blocking(action).await;
        if let Err(error) = &result {
            if matches!(error.status.as_u16(), 401 | 403 | 404 | 410) {
                throttle.fail(source);
            }
        }
        result
    }

    pub fn uptime_seconds(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    pub async fn server_info(&self) -> ApiResult<Value> {
        let online = self.online().len();
        self.blocking(move |inner| {
            let counts = inner.store.read(|db| {
                let count = |table: &str| db.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get::<_, i64>(0));
                Ok(json!({"libraries": count("libraries")?, "documents": count("documents")?, "devices": count("devices")?}))
            })?;
            let mut info = inner.auth.identity();
            info["setup_required"] = json!(inner.auth.setup_required(&inner.store)?);
            info["data_dir"] = json!(inner.config.data_dir);
            info["storage_bytes"] = json!(inner.store.storage_bytes());
            info["counts"] = counts;
            info["connected_devices"] = json!(online);
            Ok(info)
        })
        .await
        .map(|mut info| {
            info["uptime_seconds"] = json!(self.uptime_seconds());
            info
        })
    }

    pub async fn setup(&self, principal: Option<Principal>, code: Option<String>, password: String, source: &str) -> ApiResult<Value> {
        self.guarded(Gate::Login, source, move |inner| {
            inner.auth.setup(&inner.store, principal.as_ref(), code.as_deref(), &password)
        })
        .await
    }

    pub async fn change_password(&self, principal: Principal, current: Option<String>, new: String, source: &str) -> ApiResult<Value> {
        self.guarded(Gate::Login, source, move |inner| {
            inner.auth.change_password(&inner.store, &principal, current.as_deref(), &new)
        })
        .await
    }

    pub async fn devices(&self) -> ApiResult<Value> {
        let online = self.online();
        self.blocking(move |inner| inner.auth.devices(&inner.store, &online)).await
    }

    pub async fn rename_device(&self, device_id: String, name: String) -> ApiResult<Value> {
        self.blocking(move |inner| inner.auth.rename_device(&inner.store, &device_id, &name)).await
    }

    pub async fn revoke_device(&self, device_id: String) -> ApiResult<()> {
        // 연결 중인 실시간 수신이 다음 확인 때 바로 끊기도록 깨운다.
        self.write(move |inner| inner.auth.revoke_device(&inner.store, &device_id)).await
    }

    pub async fn create_pairing(&self, principal: Principal, name: String, role: String, server_url: String) -> ApiResult<Value> {
        let mut entry = self
            .blocking(move |inner| inner.auth.create_pairing(&inner.store, &principal, &name, &role))
            .await?;
        let server_url = server_url.trim_end_matches('/').to_string();
        entry["connection"] = json!(connection_link(&server_url, entry["code"].as_str().unwrap_or_default()));
        entry["server_url"] = json!(server_url);
        Ok(entry)
    }

    pub async fn pairings(&self) -> ApiResult<Value> {
        self.blocking(|inner| inner.auth.pairings(&inner.store)).await
    }

    pub async fn cancel_pairing(&self, entry_id: String) -> ApiResult<()> {
        self.blocking(move |inner| inner.auth.cancel_pairing(&inner.store, &entry_id)).await
    }
}

/// 시작한 서버. `serve`로 요청을 받기 시작한다.
pub struct Daemon {
    state: AppState,
}

impl Daemon {
    pub fn start(config: Config) -> Result<Self, StartError> {
        Self::start_with_clock(config, Clock::default())
    }

    pub fn start_with_clock(config: Config, clock: Clock) -> Result<Self, StartError> {
        config.validate()?;
        let lock = InstanceLock::acquire(&config.data_dir)?;
        let store = Store::open(&config.data_dir, clock)?;
        let auth = Auth::load(&store, config.server_name.trim(), &config.legacy_token)?;
        if let Some(code) = auth.prepare_setup(&store)? {
            tracing::warn!("관리자가 아직 설정되지 않았습니다. 관리 화면에서 최초 설정 코드 {code} 를 입력해 주세요.");
        }
        if !config.legacy_token.is_empty() {
            tracing::warn!("LOF_API_TOKEN은 이전 방식의 공통 토큰입니다. 모든 기기를 연결 코드로 다시 연결한 뒤 설정을 제거해 주세요.");
        }
        let (notifier, _) = watch::channel(0);
        let inner = Inner {
            config,
            store,
            auth,
            started: Instant::now(),
            closing: CancellationToken::new(),
            notifier,
            online: Mutex::new(HashMap::new()),
            pair_throttle: Throttle::new(10, 100),
            login_throttle: Throttle::new(10, 50),
            _lock: lock,
        };
        tracing::info!("서재 서버를 시작했습니다. 데이터 폴더: {}", inner.config.data_dir.display());
        Ok(Self { state: AppState(Arc::new(inner)) })
    }

    pub fn state(&self) -> AppState {
        self.state.clone()
    }

    /// 종료 신호가 올 때까지 요청을 처리한다. 실시간 수신 연결도 함께 닫는다.
    pub async fn serve(self, listener: TcpListener, shutdown: impl Future<Output = ()> + Send + 'static) -> std::io::Result<()> {
        let state = self.state;
        let scheduler = tokio::spawn(scheduler(state.clone()));
        let closing = state.closing.clone();
        let app = api::router(state.clone()).into_make_service_with_connect_info::<SocketAddr>();
        let result = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown.await;
                closing.cancel();
            })
            .await;
        state.closing.cancel();
        scheduler.abort();
        let _ = scheduler.await;
        tracing::info!("서재 서버를 종료했습니다.");
        result
    }
}

async fn scheduler(state: AppState) {
    let mut next_maintenance = Instant::now() + state.config.maintenance;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(state.config.history_tick) => {}
            _ = state.closing.cancelled() => return,
        }
        let maintain = Instant::now() >= next_maintenance;
        let result = state
            .blocking(move |inner| {
                let mut changed = inner.store.history_tick()?;
                if maintain {
                    changed += inner.store.history_maintain()?;
                    inner.auth.maintain(&inner.store)?;
                }
                Ok(changed)
            })
            .await;
        match result {
            Ok(changed) => {
                if maintain {
                    next_maintenance = Instant::now() + state.config.maintenance;
                }
                if changed > 0 {
                    state.notify();
                }
            }
            Err(error) => tracing::error!("이력 작업을 완료하지 못했습니다. 다음 주기에 재시도합니다: {error}"),
        }
    }
}
