use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Request};
use axum::Router;
use http_body_util::BodyExt;
use lof_library_core::backup::backup;
use lof_library_core::history::changed_chars;
use lof_library_core::models::DocumentMutation;
use lof_library_core::store::Clock;
use lof_library_core::{api, AppState, Config, Daemon, InstanceLock};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt;

const BASE: &str = "/v1/libraries/library";
const ADMIN: &str = "library-admin";
const PASSWORD: &str = "관리자-비밀번호";

/// 호스트가 발급한 연결 코드로 시험용 기기를 등록하고 기기 인증 키를 돌려준다.
fn device_token(state: &AppState) -> String {
    let code = state.auth.create_pairing(&state.store, "시험 기기").unwrap()["code"].as_str().unwrap().to_string();
    state.auth.pair(&state.store, &code, "", "test").unwrap()["token"].as_str().unwrap().to_string()
}

struct Reply {
    status: u16,
    headers: HeaderMap,
    body: Value,
}

async fn call(router: &Router, method: &str, uri: &str, body: Option<Value>, token: Option<&str>, headers: &[(&str, &str)]) -> Reply {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let body = match body {
        Some(value) => {
            builder = builder.header("content-type", "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    send(router, builder.body(body).unwrap()).await
}

async fn send(router: &Router, request: Request<Body>) -> Reply {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let raw = response.into_body().collect().await.unwrap().to_bytes();
    Reply { status, headers, body: serde_json::from_slice(&raw).unwrap_or(Value::String(String::from_utf8_lossy(&raw).into())) }
}

struct Harness {
    dir: tempfile::TempDir,
    clock: Clock,
    state: Option<AppState>,
    router: Option<Router>,
    serial: usize,
    token: String,
}

impl Harness {
    async fn new() -> Self {
        let dir = tempfile::Builder::new().prefix("lof-daemon-test-").tempdir().unwrap();
        let clock = Clock::default();
        clock.set(1_700_000_000.0);
        let mut harness = Self { dir, clock, state: None, router: None, serial: 0, token: String::new() };
        harness.boot();
        harness.token = device_token(harness.state());
        let created = harness.post("/v1/libraries", json!({"id": "library", "name": "시험 서재"})).await;
        assert_eq!(created.status, 201, "{:?}", created.body);
        harness
    }

    fn config(&self) -> Config {
        let mut config = Config::new(self.dir.path());
        config.web_admin = true;
        config.server_name = "시험 서버".into();
        config.history_tick = Duration::from_secs(3600);
        config
    }

    fn boot(&mut self) {
        let daemon = Daemon::start_with_clock(self.config(), self.clock.clone()).unwrap();
        let state = daemon.state();
        self.router = Some(api::router(state.clone()));
        self.state = Some(state);
    }

    fn restart(&mut self) {
        self.router = None;
        self.state = None;
        self.boot();
    }

    fn state(&self) -> &AppState {
        self.state.as_ref().unwrap()
    }

    fn router(&self) -> &Router {
        self.router.as_ref().unwrap()
    }

    async fn request(&self, method: &str, uri: &str, body: Option<Value>, token: Option<&str>) -> Reply {
        call(self.router(), method, uri, body, token, &[]).await
    }

    async fn get(&self, uri: &str) -> Reply {
        self.request("GET", uri, None, Some(&self.token)).await
    }

    async fn post(&self, uri: &str, body: Value) -> Reply {
        self.request("POST", uri, Some(body), Some(&self.token)).await
    }

    async fn put(&self, uri: &str, body: Value) -> Reply {
        self.request("PUT", uri, Some(body), Some(&self.token)).await
    }

    async fn put_doc(&mut self, body: &str, version: i64, extra: Value) -> Reply {
        self.serial += 1;
        let mut payload = json!({
            "title": "원고", "body": body, "device_id": "pc-a",
            "mutation_id": format!("mutation-{}", self.serial), "expected_version": version,
        });
        for (key, value) in extra.as_object().cloned().unwrap_or_default() {
            payload[key] = value;
        }
        self.put(&format!("{BASE}/documents/doc"), payload).await
    }

    async fn policy(&self, values: Value) -> Value {
        let current = self.get(&format!("{BASE}/settings")).await.body;
        let mut policy = current["history"].clone();
        for (key, value) in values.as_object().cloned().unwrap_or_default() {
            policy[key] = value;
        }
        let response = self
            .put(&format!("{BASE}/settings"), json!({"expected_version": current["settings_version"], "history": policy}))
            .await;
        assert_eq!(response.status, 200, "{:?}", response.body);
        response.body
    }

    async fn histories(&self) -> Vec<Value> {
        let response = self.get(&format!("{BASE}/documents/doc/history")).await;
        assert_eq!(response.status, 200, "{:?}", response.body);
        response.body["entries"].as_array().cloned().unwrap()
    }

    fn tick(&self) -> usize {
        self.state().store.history_tick().unwrap()
    }

    fn maintain(&self) -> usize {
        self.state().store.history_maintain().unwrap()
    }

    /// 호스트의 명령줄에서 최초 설정 코드를 다시 발급받아 웹 관리자 계정을 만들고 로그인 키를 돌려준다.
    async fn admin(&self) -> String {
        let state = self.state();
        let code = state.auth.reset_admin(&state.store).unwrap().unwrap();
        let setup = json!({"code": code, "username": ADMIN, "password": PASSWORD});
        let response = self.request("POST", "/v1/setup", Some(setup), None).await;
        assert_eq!(response.status, 201, "{:?}", response.body);
        response.body["token"].as_str().unwrap().to_string()
    }
}

#[tokio::test]
async fn public_status_and_authentication() {
    let harness = Harness::new().await;
    for route in ["/status", "/healthz"] {
        let response = harness.request("GET", route, None, None).await;
        assert_eq!(response.status, 200);
        assert_eq!(response.body["status"], "ok");
        assert!(!response.body.to_string().contains(&harness.token));
    }
    let status = harness.request("GET", "/status", None, None).await.body;
    assert_eq!(status["name"], "시험 서버");
    assert_eq!(harness.request("GET", "/v1/libraries", None, None).await.status, 401);
    assert_eq!(harness.request("GET", "/v1/libraries", None, Some("lofd_wrong-token")).await.status, 401);
    // 연결 기기의 키로는 관리 API를 쓸 수 없다.
    assert_eq!(harness.request("GET", "/v1/admin/server", None, Some(&harness.token)).await.status, 403);
}

#[tokio::test]
async fn versions_retries_and_noop() {
    let mut harness = Harness::new().await;
    let first = harness.put_doc("초안", 0, json!({"mutation_id": "stable"})).await;
    assert_eq!(first.status, 201, "{:?}", first.body);
    let cursor = harness.get(&format!("{BASE}/snapshot")).await.body["cursor"].clone();
    let retry = harness.put_doc("초안", 0, json!({"mutation_id": "stable"})).await;
    assert_eq!(retry.body, first.body);
    let same = harness.put_doc("초안", 0, json!({})).await;
    assert_eq!(same.body["changed"], false);
    assert_eq!(same.body["document"]["version"], 1);
    assert_eq!(harness.get(&format!("{BASE}/snapshot")).await.body["cursor"], cursor);
    let reused = harness.put_doc("다른 내용", 0, json!({"mutation_id": "stable"})).await;
    assert_eq!(reused.status, 409);
    assert_eq!(reused.body["error"], "mutation_reused");
    assert_eq!(harness.get(&format!("{BASE}/documents/doc")).await.headers["etag"], "\"1\"");
    let cached = call(harness.router(), "GET", &format!("{BASE}/documents/doc"), None, Some(&harness.token), &[("if-none-match", "\"1\"")]).await;
    assert_eq!(cached.status, 304);
}

#[tokio::test]
async fn conflicting_edit_is_preserved_and_can_be_resolved() {
    let mut harness = Harness::new().await;
    harness.put_doc("초안", 0, json!({})).await;
    harness.put_doc("서버 수정", 1, json!({})).await;
    let offline = json!({"device_id": "pc-b", "mutation_id": "offline"});
    let conflict = harness.put_doc("오프라인 수정", 1, offline.clone()).await;
    assert_eq!(conflict.status, 409);
    let conflict_id = conflict.body["conflict_id"].as_str().unwrap().to_string();
    assert_eq!(harness.put_doc("오프라인 수정", 1, offline).await.body, conflict.body);
    let saved = harness.get(&format!("{BASE}/conflicts/{conflict_id}")).await.body;
    assert_eq!(saved["incoming"]["body"], "오프라인 수정");
    assert_eq!(harness.get(&format!("{BASE}/documents/doc")).await.body["body"], "서버 수정");
    let request = json!({"expected_version": 2, "device_id": "pc-b", "mutation_id": "resolve", "choice": "incoming"});
    let resolved = harness.post(&format!("{BASE}/conflicts/{conflict_id}/resolve"), request.clone()).await;
    assert_eq!(resolved.status, 200, "{:?}", resolved.body);
    assert_eq!(harness.get(&format!("{BASE}/documents/doc")).await.body["body"], "오프라인 수정");
    assert_eq!(harness.post(&format!("{BASE}/conflicts/{conflict_id}/resolve"), request).await.body, resolved.body);
    assert_eq!(harness.get(&format!("{BASE}/conflicts")).await.body["entries"], json!([]));
}

#[tokio::test]
async fn two_devices_cannot_overwrite_same_version() {
    let mut harness = Harness::new().await;
    harness.put_doc("초안", 0, json!({})).await;
    let store = &harness.state().store;
    let requests: Vec<DocumentMutation> = ["첫 번째 변경", "두 번째 변경"]
        .iter()
        .enumerate()
        .map(|(index, body)| {
            serde_json::from_value(json!({
                "title": "원고", "body": body, "expected_version": 1,
                "device_id": format!("pc-{index}"), "mutation_id": "concurrent",
            }))
            .unwrap()
        })
        .collect();
    let mut statuses: Vec<u16> = std::thread::scope(|scope| {
        let handles: Vec<_> = requests
            .iter()
            .map(|request| scope.spawn(move || store.mutate_document("library", "doc", request).unwrap().0))
            .collect();
        handles.into_iter().map(|handle| handle.join().unwrap()).collect()
    });
    statuses.sort();
    assert_eq!(statuses, vec![200, 409]);
    assert_eq!(store.conflicts("library", "", 100).unwrap()["entries"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn history_requires_elapsed_time_and_character_changes() {
    let mut harness = Harness::new().await;
    harness.policy(json!({"min_minutes": 1, "min_changed_chars": 5})).await;
    harness.put_doc("초안", 0, json!({})).await;
    harness.clock.advance(10.0);
    harness.put_doc(&format!("초안{}", "가".repeat(10)), 1, json!({})).await;
    assert_eq!(harness.tick(), 0);
    harness.clock.advance(51.0);
    assert_eq!(harness.tick(), 1);
    assert_eq!(harness.histories().await.len(), 2);
    harness.clock.advance(1000.0);
    assert_eq!(harness.tick(), 0);
    harness.put_doc(&format!("초안{}나", "가".repeat(10)), 2, json!({})).await;
    assert_eq!(harness.tick(), 0);
    harness.put_doc(&format!("초안{}나다라마바", "가".repeat(10)), 3, json!({})).await;
    assert_eq!(harness.tick(), 1);
    assert_eq!(harness.histories().await.len(), 3);
}

#[tokio::test]
async fn same_length_edit_and_revert() {
    let mut harness = Harness::new().await;
    harness.policy(json!({"min_minutes": 1, "min_changed_chars": 2})).await;
    harness.put_doc("가나다", 0, json!({})).await;
    harness.clock.advance(61.0);
    harness.put_doc("가마바", 1, json!({})).await;
    assert_eq!(harness.tick(), 1);
    harness.put_doc("전혀 다른 원고", 2, json!({})).await;
    harness.put_doc("가마바", 3, json!({})).await;
    harness.clock.advance(61.0);
    assert_eq!(harness.tick(), 0);
    assert_eq!(harness.histories().await.len(), 2);
}

#[tokio::test]
async fn manual_history_deduplicates_and_restore_preserves_current() {
    let mut harness = Harness::new().await;
    harness.put_doc("처음 원고", 0, json!({})).await;
    let original = harness.histories().await[0]["id"].as_str().unwrap().to_string();
    let capture = harness.post(&format!("{BASE}/documents/doc/history"), json!({"expected_version": 1, "device_id": "pc-a"})).await;
    assert_eq!(capture.body["created"], false);
    let histories = harness.histories().await;
    assert_eq!(histories.len(), 1);
    assert_eq!(histories[0]["protected"], true);
    harness.clock.advance(1.0);
    harness.put_doc("복원 전의 원고", 1, json!({})).await;
    let request = json!({"expected_version": 2, "device_id": "pc-a", "mutation_id": "restore"});
    let route = format!("{BASE}/documents/doc/history/{original}/restore");
    let result = harness.post(&route, request.clone()).await;
    assert_eq!(result.status, 200, "{:?}", result.body);
    assert_eq!(result.body["document"]["version"], 3);
    assert_eq!(harness.get(&format!("{BASE}/documents/doc")).await.body["body"], "처음 원고");
    let histories = harness.histories().await;
    assert_eq!(histories.len(), 3);
    let previous = histories.iter().find(|row| row["reason"] == "before_restore").unwrap();
    let saved = harness.get(&format!("{BASE}/documents/doc/history/{}", previous["id"].as_str().unwrap())).await.body;
    assert_eq!(saved["snapshot"]["body"], "복원 전의 원고");
    assert_eq!(harness.post(&route, request).await.body, result.body);
}

#[tokio::test]
async fn shared_settings_reject_stale_writes_and_apply_new_threshold() {
    let mut harness = Harness::new().await;
    let policy = harness.policy(json!({"min_minutes": 1, "min_changed_chars": 100})).await;
    harness.put_doc("가", 0, json!({})).await;
    harness.clock.advance(61.0);
    harness.put_doc("가나다", 1, json!({})).await;
    assert_eq!(harness.tick(), 0);
    harness.policy(json!({"min_changed_chars": 2})).await;
    assert_eq!(harness.tick(), 1);
    let stale = harness
        .put(&format!("{BASE}/settings"), json!({"expected_version": policy["settings_version"], "history": policy["history"]}))
        .await;
    assert_eq!(stale.status, 409);
    assert_eq!(harness.get(&format!("{BASE}/settings")).await.body["history"]["min_changed_chars"], 2);
}

#[tokio::test]
async fn smart_pruning_keeps_fixed_buckets_and_manual_history() {
    let mut harness = Harness::new().await;
    harness.clock.set(120_000.0);
    harness
        .policy(json!({"min_minutes": 1, "min_changed_chars": 1,
                       "smart": {"enabled": true, "keep_recent_minutes": 60, "bucket_minutes": 10}}))
        .await;
    harness.put_doc("a", 0, json!({})).await;
    for version in 1..=5 {
        harness.clock.set(120_000.0 + version as f64 * 60.0);
        harness.put_doc(&"a".repeat(version + 1), version as i64, json!({})).await;
        assert_eq!(harness.tick(), 1);
        if version == 2 {
            harness.post(&format!("{BASE}/documents/doc/history"), json!({"expected_version": version + 1, "device_id": "pc-a"})).await;
        }
    }
    assert_eq!(harness.histories().await.len(), 6);
    harness.clock.advance(7200.0);
    assert_eq!(harness.maintain(), 4);
    let histories = harness.histories().await;
    assert_eq!(histories.len(), 2);
    let mut reasons: Vec<&str> = histories.iter().map(|row| row["reason"].as_str().unwrap()).collect();
    reasons.sort();
    assert_eq!(reasons, vec!["automatic", "manual"]);
    assert_eq!(harness.maintain(), 0);
}

#[tokio::test]
async fn retention_keeps_latest_and_protected_unless_opted_in() {
    let mut harness = Harness::new().await;
    harness.policy(json!({"retention_days": 1})).await;
    harness.put_doc("처음", 0, json!({})).await;
    harness.post(&format!("{BASE}/documents/doc/history"), json!({"expected_version": 1, "device_id": "pc-a"})).await;
    harness.clock.advance(100.0);
    harness.put_doc("두 번째", 1, json!({})).await;
    harness.post(&format!("{BASE}/documents/doc/history"), json!({"expected_version": 2, "device_id": "pc-a"})).await;
    harness.clock.advance(86400.0 * 3.0);
    assert_eq!(harness.maintain(), 0);
    harness.policy(json!({"expire_protected": true})).await;
    assert_eq!(harness.maintain(), 1);
    assert_eq!(harness.histories().await.len(), 1);
}

#[tokio::test]
async fn change_feed_pagination_and_future_cursor() {
    let mut harness = Harness::new().await;
    let cursor = harness.get(&format!("{BASE}/snapshot")).await.body["cursor"].as_i64().unwrap();
    harness.put_doc("첫 원고", 0, json!({})).await;
    let first = harness.get(&format!("{BASE}/changes?after={cursor}&limit=1")).await.body;
    assert_eq!(first["has_more"], true);
    let second = harness.get(&format!("{BASE}/changes?after={}&limit=1", first["cursor"])).await.body;
    assert!(second["events"][0]["id"].as_i64() > first["events"][0]["id"].as_i64());
    assert_eq!(second["has_more"], false);
    assert_eq!(harness.get(&format!("{BASE}/changes?after=999999")).await.status, 409);
    assert_eq!(harness.get(&format!("{BASE}/events?after=999999")).await.status, 409);
    let invalid = call(harness.router(), "GET", &format!("{BASE}/events"), None, Some(&harness.token), &[("last-event-id", "invalid")]).await;
    assert_eq!(invalid.status, 422);
}

#[tokio::test]
async fn assets_are_hash_addressed_and_noop_uploads_do_not_emit() {
    use sha2::Digest;
    let harness = Harness::new().await;
    let data = "첨부 자료".as_bytes().to_vec();
    let sha256 = hex::encode(sha2::Sha256::digest(&data));
    let route = format!("{BASE}/assets/{sha256}");
    let upload = |content: Vec<u8>| {
        Request::builder()
            .method("PUT")
            .uri(&route)
            .header("authorization", format!("Bearer {}", harness.token))
            .body(Body::from(content))
            .unwrap()
    };
    assert_eq!(send(harness.router(), upload(b"wrong".to_vec())).await.status, 422);
    assert_eq!(send(harness.router(), upload(data.clone())).await.body["created"], true);
    let cursor = harness.get(&format!("{BASE}/snapshot")).await.body["cursor"].clone();
    assert_eq!(send(harness.router(), upload(data.clone())).await.body["created"], false);
    assert_eq!(harness.get(&format!("{BASE}/snapshot")).await.body["cursor"], cursor);
    let downloaded = harness.router().clone().oneshot(
        Request::builder().uri(&route).header("authorization", format!("Bearer {}", harness.token)).body(Body::empty()).unwrap(),
    ).await.unwrap();
    assert_eq!(downloaded.into_body().collect().await.unwrap().to_bytes().to_vec(), data);
    let etag = format!("\"{sha256}\"");
    let cached = call(harness.router(), "GET", &route, None, Some(&harness.token), &[("if-none-match", &etag)]).await;
    assert_eq!(cached.status, 304);
}

#[tokio::test]
async fn persistence_on_restart() {
    let mut harness = Harness::new().await;
    let first = harness.put_doc("재시작 보존", 0, json!({"mutation_id": "persisted"})).await;
    let server_id = harness.state().auth.server_id.clone();
    harness.restart();
    assert_eq!(harness.state().auth.server_id, server_id);
    assert_eq!(harness.get(&format!("{BASE}/documents/doc")).await.body["body"], "재시작 보존");
    assert_eq!(harness.put_doc("재시작 보존", 0, json!({"mutation_id": "persisted"})).await.body, first.body);
    assert_eq!(harness.histories().await.len(), 1);
}

#[tokio::test]
async fn deleted_content_is_preserved_and_can_be_restored() {
    let mut harness = Harness::new().await;
    harness.put_doc("삭제해도 남길 원고", 0, json!({})).await;
    let original = harness.histories().await[0]["id"].as_str().unwrap().to_string();
    harness.put_doc("", 1, json!({"deleted": true})).await;
    let deleted = harness.get(&format!("{BASE}/documents/doc")).await.body;
    assert_eq!(deleted["deleted"], true);
    assert_eq!(deleted["body"], "삭제해도 남길 원고");
    let result = harness
        .post(&format!("{BASE}/documents/doc/history/{original}/restore"),
              json!({"expected_version": 2, "device_id": "pc-a", "mutation_id": "undelete"}))
        .await;
    assert_eq!(result.body["document"]["deleted"], false);
}

#[tokio::test]
async fn single_process_lock() {
    let harness = Harness::new().await;
    assert!(InstanceLock::acquire(harness.dir.path()).is_err());
    assert!(Daemon::start(harness.config()).is_err());
}

#[tokio::test]
async fn request_size_limit_and_validation() {
    let harness = Harness::new().await;
    let large = Request::builder()
        .method("PUT")
        .uri(format!("{BASE}/assets/{}", "0".repeat(64)))
        .header("authorization", format!("Bearer {}", harness.token))
        .body(Body::from(vec![b'x'; 9 * 1024 * 1024]))
        .unwrap();
    assert_eq!(send(harness.router(), large).await.status, 413);
    assert_eq!(harness.post("/v1/libraries", json!({"id": "../other", "name": "잘못된 경로"})).await.status, 422);
    assert_eq!(harness.put(&format!("{BASE}/settings"), json!({"expected_version": 1, "history": {"min_minutes": 0}})).await.status, 422);
    assert_eq!(harness.post("/v1/libraries", json!({"id": "extra", "name": "추가 필드", "unknown": true})).await.status, 422);
}

#[tokio::test]
async fn chunked_upload_cannot_bypass_request_limit() {
    let harness = Harness::new().await;
    let chunks = async_stream::stream! {
        yield Ok::<_, std::convert::Infallible>(vec![b'x'; 4 * 1024 * 1024]);
        yield Ok(vec![b'y'; 4 * 1024 * 1024 + 1]);
    };
    let request = Request::builder()
        .method("PUT")
        .uri(format!("{BASE}/assets/{}", "0".repeat(64)))
        .header("authorization", format!("Bearer {}", harness.token))
        .body(Body::from_stream(chunks))
        .unwrap();
    assert_eq!(send(harness.router(), request).await.status, 413);
}

#[tokio::test]
async fn write_receipts_do_not_store_hidden_history_copies() {
    let mut harness = Harness::new().await;
    harness.put_doc("보관 조건을 충족하지 않은 본문", 0, json!({})).await;
    harness.put_doc("보관 조건을 충족하지 않은 수정본", 1, json!({})).await;
    let receipts = harness
        .state()
        .store
        .read(|db| {
            let mut statement = db.prepare("SELECT response FROM mutations")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .unwrap();
    for receipt in receipts {
        let receipt: Value = serde_json::from_str(&receipt).unwrap();
        assert!(receipt["document"].get("body").is_none());
    }
    assert_eq!(harness.histories().await.len(), 1);
}

#[test]
fn character_count_uses_unicode_characters_and_replacements() {
    assert_eq!(changed_chars(&json!({"body": "가😀나", "deleted": false}), &json!({"body": "가🦊나", "deleted": false})), 2);
    assert_eq!(changed_chars(&json!({"body": "가나다", "deleted": false}), &json!({"body": "가나다", "deleted": false})), 0);
    let long = "가".repeat(30_000);
    assert_eq!(changed_chars(&json!({"body": long, "deleted": false}), &json!({"body": "", "deleted": false})), 30_000);
}

#[tokio::test]
async fn running_database_backup_is_complete_and_never_overwrites() {
    let mut harness = Harness::new().await;
    harness.put_doc("백업할 원고", 0, json!({})).await;
    let destination = harness.dir.path().join("backup.sqlite");
    backup(harness.dir.path(), &destination).unwrap();
    let copy = rusqlite::Connection::open(&destination).unwrap();
    let body: String = copy.query_row("SELECT body FROM documents", [], |row| row.get(0)).unwrap();
    assert_eq!(body, "백업할 원고");
    assert!(backup(harness.dir.path(), &destination).is_err());
}

// 인증과 기기 연결

#[tokio::test]
async fn web_admin_signs_in_with_username_and_password() {
    let harness = Harness::new().await;
    let wrong_code = json!({"code": "AAAA-AAAA", "username": ADMIN, "password": PASSWORD});
    assert_eq!(harness.request("POST", "/v1/setup", Some(wrong_code.clone()), None).await.status, 403);
    let session = harness.admin().await;
    assert!(session.starts_with("lofs_"));
    assert_eq!(harness.request("POST", "/v1/setup", Some(wrong_code), None).await.status, 409);

    // 아이디와 비밀번호 중 어느 쪽이 틀렸는지 구분하지 않는다.
    let wrong_name = harness.request("POST", "/v1/auth/login", Some(json!({"username": "other", "password": PASSWORD})), None).await;
    let wrong_password = harness.request("POST", "/v1/auth/login", Some(json!({"username": ADMIN, "password": "틀린 비밀번호"})), None).await;
    assert_eq!((wrong_name.status, wrong_password.status), (401, 401));
    assert_eq!(wrong_name.body, wrong_password.body);
    let login = harness.request("POST", "/v1/auth/login", Some(json!({"username": ADMIN, "password": PASSWORD})), None).await;
    assert_eq!(login.status, 200, "{:?}", login.body);
    let token = login.body["token"].as_str().unwrap().to_string();
    let server = harness.request("GET", "/v1/admin/server", None, Some(&token)).await;
    assert_eq!((server.status, server.body["admin_username"].clone()), (200, json!(ADMIN)));

    let change = json!({"current_password": PASSWORD, "new_password": "새-관리자-비밀번호"});
    let changed = harness.request("PUT", "/v1/admin/password", Some(change), Some(&token)).await;
    assert_eq!(changed.status, 200, "{:?}", changed.body);
    assert_eq!(harness.request("GET", "/v1/admin/server", None, Some(&token)).await.status, 401);
    let renewed = changed.body["token"].as_str().unwrap().to_string();
    assert_eq!(harness.request("POST", "/v1/auth/logout", None, Some(&renewed)).await.status, 204);
    assert_eq!(harness.request("GET", "/v1/admin/server", None, Some(&renewed)).await.status, 401);
    harness.clock.advance(13.0 * 3600.0);
    assert_eq!(harness.request("GET", "/v1/admin/server", None, Some(&session)).await.status, 401);
}

#[tokio::test]
async fn installed_app_opens_no_web_admin() {
    let dir = tempfile::Builder::new().prefix("lof-daemon-test-").tempdir().unwrap();
    let daemon = Daemon::start(Config::new(dir.path())).unwrap();
    let state = daemon.state();
    let router = api::router(state.clone());
    for route in ["/v1/setup", "/v1/auth/login"] {
        assert_eq!(call(&router, "POST", route, Some(json!({})), None, &[]).await.status, 404);
    }
    assert_eq!(call(&router, "GET", "/v1/admin/server", None, None, &[]).await.status, 404);
    // 설치형 앱은 로그인 없이 같은 프로세스에서 코드를 발급한다.
    let created = state.create_pairing("노트북".into(), "http://192.168.0.2:8787".into()).await.unwrap();
    assert!(created["connection"].as_str().unwrap().starts_with("lof-library://connect?server=http%3A%2F%2F192.168.0.2%3A8787"));
    let paired = call(&router, "POST", "/v1/pair", Some(json!({"code": created["code"]})), None, &[]).await;
    assert_eq!(paired.status, 201, "{:?}", paired.body);
}

#[tokio::test]
async fn pairing_codes_issue_device_keys_once() {
    let harness = Harness::new().await;
    let admin = harness.admin().await;
    let created = harness
        .request("POST", "/v1/admin/pairing-codes", Some(json!({"name": "노트북", "server_url": "http://192.168.0.2:8787"})), Some(&admin))
        .await;
    assert_eq!(created.status, 201, "{:?}", created.body);
    let code = created.body["code"].as_str().unwrap().to_string();
    assert_eq!(harness.request("GET", "/v1/admin/server", None, Some(&admin)).await.status, 200);
    assert_eq!(harness.request("GET", "/v1/admin/pairing-codes", None, Some(&admin)).await.body["entries"].as_array().unwrap().len(), 1);

    let paired = harness.request("POST", "/v1/pair", Some(json!({"code": code.to_lowercase().replace('-', ""), "platform": "windows"})), None).await;
    assert_eq!(paired.status, 201, "{:?}", paired.body);
    assert_eq!(paired.body["device"]["name"], "노트북");
    let device = paired.body["token"].as_str().unwrap().to_string();
    assert!(device.starts_with("lofd_"));
    let used = harness.request("POST", "/v1/pair", Some(json!({"code": code})), None).await;
    assert_eq!((used.status, used.body["error"].clone()), (410, json!("pairing_code_used")));

    assert_eq!(harness.request("GET", "/v1/libraries", None, Some(&device)).await.status, 200);
    assert_eq!(harness.request("GET", "/v1/admin/devices", None, Some(&device)).await.status, 403);
    let me = harness.request("GET", "/v1/me", None, Some(&device)).await.body;
    assert_eq!(me["kind"], "device");
    let device_id = me["id"].as_str().unwrap().to_string();

    let renamed = harness.request("PATCH", &format!("/v1/admin/devices/{device_id}"), Some(json!({"name": "작업용 노트북"})), Some(&admin)).await;
    assert_eq!(renamed.body["name"], "작업용 노트북");
    let devices = harness.request("GET", "/v1/admin/devices", None, Some(&admin)).await.body;
    let laptop = devices["devices"].as_array().unwrap().iter().find(|device| device["id"] == device_id.as_str()).unwrap();
    assert_eq!(laptop["platform"], "windows");
    assert_eq!(harness.request("DELETE", &format!("/v1/admin/devices/{device_id}"), None, Some(&admin)).await.status, 204);
    assert_eq!(harness.request("GET", "/v1/libraries", None, Some(&device)).await.status, 401);

    let expiring = harness.request("POST", "/v1/admin/pairing-codes", Some(json!({})), Some(&admin)).await.body;
    harness.clock.advance(601.0);
    let expired = harness.request("POST", "/v1/pair", Some(json!({"code": expiring["code"]})), None).await;
    assert_eq!((expired.status, expired.body["error"].clone()), (410, json!("pairing_code_expired")));
}

#[tokio::test]
async fn repeated_wrong_codes_are_throttled() {
    let harness = Harness::new().await;
    for _ in 0..10 {
        assert_eq!(harness.request("POST", "/v1/pair", Some(json!({"code": "ZZZZ-ZZZZ"})), None).await.status, 404);
    }
    assert_eq!(harness.request("POST", "/v1/pair", Some(json!({"code": "ZZZZ-ZZZZ"})), None).await.status, 429);
}

// 실시간 수신

async fn open_events(address: std::net::SocketAddr, token: &str, path: &str, last_event_id: Option<i64>) -> Lines<BufReader<TcpStream>> {
    let mut stream = TcpStream::connect(address).await.unwrap();
    let resume = last_event_id.map(|id| format!("Last-Event-ID: {id}\r\n")).unwrap_or_default();
    let request = format!("GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\n{resume}\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut lines = BufReader::new(stream).lines();
    let status = lines.next_line().await.unwrap().unwrap();
    assert!(status.contains(" 200 "), "{status}");
    lines
}

async fn next_event(lines: &mut Lines<BufReader<TcpStream>>) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let line = lines.next_line().await.unwrap().expect("연결이 끊겼습니다");
            if let Some(data) = line.strip_prefix("data: ") {
                return serde_json::from_str::<Value>(data).unwrap();
            }
        }
    })
    .await
    .expect("이벤트를 받지 못했습니다")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_events_and_resume() {
    let dir = tempfile::Builder::new().prefix("lof-daemon-test-").tempdir().unwrap();
    let mut config = Config::new(dir.path());
    config.heartbeat = Duration::from_millis(100);
    let daemon = Daemon::start(config).unwrap();
    let token = device_token(&daemon.state());
    let router = api::router(daemon.state());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, signal) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(daemon.serve(listener, async {
        let _ = signal.await;
    }));

    assert_eq!(call(&router, "GET", "/status", None, None, &[]).await.body["status"], "ok");
    call(&router, "POST", "/v1/libraries", Some(json!({"id": "library", "name": "실시간 서재"})), Some(&token), &[]).await;
    let cursor = call(&router, "GET", &format!("{BASE}/snapshot"), None, Some(&token), &[]).await.body["cursor"].as_i64().unwrap();

    let mut lines = open_events(address, &token, &format!("{BASE}/events?after={cursor}"), None).await;
    let document = json!({"title": "원고", "body": "실시간 변경", "device_id": "pc", "mutation_id": "live", "expected_version": 0});
    call(&router, "PUT", &format!("{BASE}/documents/doc"), Some(document), Some(&token), &[]).await;
    let event = next_event(&mut lines).await;
    assert_eq!(event["type"], "document.updated");
    assert_eq!(event["data"]["id"], "doc");
    drop(lines);

    let mut lines = open_events(address, &token, &format!("{BASE}/events"), event["id"].as_i64()).await;
    let replayed = next_event(&mut lines).await;
    assert_eq!(replayed["type"], "history.created");
    assert!(replayed["id"].as_i64() > event["id"].as_i64());
    drop(lines);

    stop.send(()).unwrap();
    drop(router);
    tokio::time::timeout(Duration::from_secs(5), server).await.expect("검증용 서버가 종료되지 않았습니다").unwrap().unwrap();
}
