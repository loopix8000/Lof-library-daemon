use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::Shared;

#[derive(Clone, Serialize)]
pub struct Status {
    phase: &'static str,
    current_version: String,
    version: Option<String>,
    progress: Option<u64>,
    message: Option<String>,
}

struct Inner {
    status: Status,
    pending: Option<Update>,
}

pub struct Updates {
    inner: Mutex<Inner>,
    operation: tokio::sync::Mutex<()>,
}

impl Default for Updates {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner {
                status: Status {
                    phase: "idle",
                    current_version: env!("CARGO_PKG_VERSION").into(),
                    version: None,
                    progress: None,
                    message: None,
                },
                pending: None,
            }),
            operation: tokio::sync::Mutex::new(()),
        }
    }
}

impl Updates {
    fn change(&self, action: impl FnOnce(&mut Inner)) {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        action(&mut inner);
    }

    fn failed(&self, message: String) {
        tracing::warn!("{message}");
        self.change(|inner| {
            inner.status.phase = "error";
            inner.status.message = Some(message);
            inner.status.progress = None;
        });
    }
}

#[tauri::command]
pub fn update_status(updates: State<'_, Updates>) -> Status {
    updates
        .inner
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .status
        .clone()
}

#[tauri::command]
pub async fn update_check(app: AppHandle, updates: State<'_, Updates>) -> Result<(), String> {
    let _operation = updates
        .operation
        .try_lock()
        .map_err(|_| "업데이트 작업이 진행 중입니다.")?;
    updates.change(|inner| {
        inner.status.phase = "checking";
        inner.status.message = None;
    });
    let result = async {
        let updater = app
            .updater_builder()
            .timeout(Duration::from_secs(120))
            .build()?;
        updater.check().await
    }
    .await;
    match result {
        Ok(pending) => updates.change(|inner| {
            inner.status.phase = if pending.is_some() {
                "available"
            } else {
                "latest"
            };
            inner.status.version = pending.as_ref().map(|update| update.version.clone());
            inner.status.progress = None;
            inner.pending = pending;
        }),
        Err(error) => {
            updates.failed(format!(
                "업데이트를 확인하지 못했습니다. 잠시 후 다시 시도해 주세요. ({error})"
            ));
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn update_install(
    app: AppHandle,
    updates: State<'_, Updates>,
    controller: State<'_, Shared>,
) -> Result<(), String> {
    let _operation = updates
        .operation
        .try_lock()
        .map_err(|_| "업데이트 작업이 진행 중입니다.")?;
    let update = updates
        .inner
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .pending
        .clone()
        .ok_or("설치할 업데이트가 없습니다. 먼저 업데이트를 확인해 주세요.")?;
    updates.change(|inner| {
        inner.status.phase = "downloading";
        inner.status.message = None;
        inner.status.progress = None;
    });
    let mut received = 0_u64;
    // 다운로드와 서명 검증이 끝날 때까지 서버는 계속 실행한다.
    let bytes = match update
        .download(
            |length, total| {
                received = received.saturating_add(length as u64);
                updates.change(|inner| {
                    inner.status.progress = total.filter(|total| *total > 0).map(|total| {
                        received
                            .saturating_mul(100)
                            .checked_div(total)
                            .unwrap_or(0)
                            .min(100)
                    });
                });
            },
            || {},
        )
        .await
    {
        Ok(bytes) => bytes,
        Err(error) => {
            updates.failed(format!(
                "업데이트를 내려받거나 서명을 확인하지 못했습니다. ({error})"
            ));
            return Ok(());
        }
    };
    updates.change(|inner| {
        inner.status.phase = "installing";
        inner.status.progress = Some(100);
    });
    let was_running = controller.snapshot().await.state.is_some();
    controller.stop().await;
    // Windows에서는 설치 프로그램을 실행한 뒤 앱이 종료된다.
    if let Err(error) = update.install(bytes) {
        if was_running {
            if let Err(restart_error) = controller.start().await {
                tracing::error!(
                    "업데이트 실패 후 서버를 다시 시작하지 못했습니다: {restart_error}"
                );
            }
        }
        updates.failed(format!("업데이트 설치를 시작하지 못했습니다. ({error})"));
        return Ok(());
    }
    app.restart();
}

pub fn start_checks(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        // 시작할 때와 6시간마다 새 버전을 확인한다. 설치는 사용자가 선택한다.
        loop {
            let _ = update_check(app.clone(), app.state::<Updates>()).await;
            tokio::time::sleep(Duration::from_secs(6 * 60 * 60)).await;
        }
    });
}
