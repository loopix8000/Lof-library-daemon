use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::rejection::BytesRejection;
use axum::extract::{ConnectInfo, DefaultBodyLimit, FromRequest, FromRequestParts, Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Extension, Json, Router};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::{Kind, Principal};
use crate::error::{ApiError, ApiResult};
use crate::models::{
    Capture, DeviceUpdate, DocumentMutation, Id, LibraryCreate, Login, Pair, PairingCreate, PasswordChange, PolicyUpdate,
    Resolve, Restore, Setup, Validate,
};
use crate::{AppState, SERVICE, VERSION};

const MAX_CURSOR: i64 = i64::MAX;

pub fn router(state: AppState) -> Router {
    let limit = state.config.max_request_bytes;
    let sync = Router::new()
        .route("/libraries", get(libraries).post(create_library))
        .route("/libraries/{library}/settings", get(settings).put(update_settings))
        .route("/libraries/{library}/snapshot", get(snapshot))
        .route("/libraries/{library}/changes", get(changes))
        .route("/libraries/{library}/events", get(events))
        .route("/libraries/{library}/documents/{document}", get(document).put(put_document))
        .route("/libraries/{library}/documents/{document}/history", get(histories).post(manual_history))
        .route("/libraries/{library}/documents/{document}/history/{history}", get(history))
        .route("/libraries/{library}/documents/{document}/history/{history}/restore", post(restore))
        .route("/libraries/{library}/conflicts", get(conflicts))
        .route("/libraries/{library}/conflicts/{conflict}", get(conflict))
        .route("/libraries/{library}/conflicts/{conflict}/resolve", post(resolve))
        .route("/libraries/{library}/assets/{sha256}", get(asset).put(put_asset))
        .route("/me", get(me))
        .route("/auth/logout", post(logout))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_principal));
    let mut v1 = sync.merge(Router::new().route("/pair", post(pair)));
    // 웹 관리는 Docker에서만 연다. 설치형 앱은 로그인 없이 앱 안에서 직접 관리하므로 네트워크로 관리 API를 열지 않는다.
    if state.config.web_admin {
        let admin = Router::new()
            .route("/server", get(server_info))
            .route("/password", put(change_password))
            .route("/devices", get(devices))
            .route("/devices/{device}", patch(rename_device).delete(revoke_device))
            .route("/pairing-codes", get(pairings).post(create_pairing))
            .route("/pairing-codes/{entry}", delete(cancel_pairing))
            .route_layer(middleware::from_fn_with_state(state.clone(), require_admin));
        v1 = v1
            .route("/setup", post(setup))
            .route("/auth/login", post(login))
            .nest("/admin", admin);
    }
    Router::new()
        .route("/status", get(status))
        .route("/healthz", get(health))
        .nest("/v1", v1)
        .fallback(|| async { ApiError::new(404, "not_found", "요청한 주소를 찾을 수 없습니다.") })
        .layer(DefaultBodyLimit::max(limit))
        .with_state(state)
}

// 공통 처리

fn reply(status: u16, body: Value) -> Response {
    (StatusCode::from_u16(status).unwrap_or(StatusCode::OK), Json(body)).into_response()
}

fn too_large() -> ApiError {
    ApiError::new(413, "payload_too_large", "요청 크기 제한을 초과했습니다.")
}

fn no_content() -> Response {
    StatusCode::NO_CONTENT.into_response()
}

fn bounded(value: Option<i64>, default: i64, minimum: i64, maximum: i64, label: &str) -> ApiResult<i64> {
    let value = value.unwrap_or(default);
    if value < minimum || value > maximum {
        return Err(ApiError::invalid(format!("{label}은(는) {minimum}~{maximum} 범위여야 합니다.")));
    }
    Ok(value)
}

fn sha256_path(value: &str) -> ApiResult<&str> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) {
        Ok(value)
    } else {
        Err(ApiError::invalid("첨부 자료 주소는 소문자 SHA-256 값이어야 합니다."))
    }
}

fn iso_utc(seconds: i64) -> String {
    let (days, rest) = (seconds.div_euclid(86400), seconds.rem_euclid(86400));
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rest / 3600, rest % 3600 / 60, rest % 60)
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then(|| token.to_string())
}

async fn authenticate(state: &AppState, headers: &HeaderMap) -> ApiResult<Option<Principal>> {
    let Some(token) = bearer(headers) else { return Ok(None) };
    state.blocking(move |inner| inner.auth.authenticate(&inner.store, &token)).await
}

async fn require_principal(State(state): State<AppState>, mut request: Request, next: Next) -> Result<Response, ApiError> {
    let principal = authenticate(&state, request.headers())
        .await?
        .ok_or_else(|| ApiError::new(401, "unauthorized", "유효한 인증 키가 필요합니다."))?;
    request.extensions_mut().insert(principal);
    Ok(next.run(request).await)
}

async fn require_admin(State(state): State<AppState>, mut request: Request, next: Next) -> Result<Response, ApiError> {
    let principal = authenticate(&state, request.headers())
        .await?
        .ok_or_else(|| ApiError::new(401, "unauthorized", "관리자 로그인이 필요합니다."))?;
    if !principal.admin() {
        return Err(ApiError::new(403, "forbidden", "관리자 로그인이 필요합니다."));
    }
    request.extensions_mut().insert(principal);
    Ok(next.run(request).await)
}

/// 요청을 보낸 주소. 입력 시도 제한에 사용한다.
pub struct ClientIp(pub String);

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Infallible> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|info| info.0.ip().to_string())
                .unwrap_or_else(|| "unknown".into()),
        ))
    }
}

/// JSON 본문을 읽고 범위·길이 규칙을 확인한다.
pub struct Valid<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned + Validate> FromRequest<S> for Valid<T> {
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, ApiError> {
        let Json(value) = Json::<T>::from_request(request, state).await.map_err(|rejection| {
            if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
                too_large()
            } else {
                ApiError::invalid(format!("요청 본문을 확인해 주세요: {}", rejection.body_text()))
            }
        })?;
        value.validate().map_err(ApiError::invalid)?;
        Ok(Self(value))
    }
}

/// 쿼리 문자열을 읽는다. 형식 오류는 422로 알린다.
pub struct Q<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for Q<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(value)| Self(value))
            .map_err(|rejection| ApiError::invalid(format!("쿼리 값을 확인해 주세요: {}", rejection.body_text())))
    }
}

#[derive(Deserialize)]
struct CursorQuery {
    after: Option<i64>,
    limit: Option<i64>,
}

#[derive(Deserialize)]
struct ConflictQuery {
    after: Option<String>,
    limit: Option<i64>,
}

#[derive(Deserialize)]
struct HistoryQuery {
    before: Option<i64>,
    limit: Option<i64>,
}

// 상태 확인

async fn status(State(state): State<AppState>) -> Response {
    let healthy = state.blocking(|inner| Ok(inner.store.healthy())).await.unwrap_or(false);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs() as i64).unwrap_or(0);
    let mut response = reply(
        if healthy { 200 } else { 503 },
        json!({
            "service": SERVICE, "version": VERSION,
            "server_id": state.auth.server_id, "name": state.auth.server_name,
            "status": if healthy { "ok" } else { "error" }, "database": if healthy { "ok" } else { "error" },
            "time": iso_utc(now), "uptime_seconds": state.uptime_seconds(),
        }),
    );
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn health(State(state): State<AppState>) -> Response {
    let healthy = state.blocking(|inner| Ok(inner.store.healthy())).await.unwrap_or(false);
    let mut response = reply(if healthy { 200 } else { 503 }, json!({"status": if healthy { "ok" } else { "error" }}));
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

// 인증과 기기 연결

async fn pair(State(state): State<AppState>, ClientIp(source): ClientIp, Valid(payload): Valid<Pair>) -> ApiResult<Response> {
    let result = state.pair(&source, payload.code, payload.name, payload.platform).await?;
    Ok(reply(201, result))
}

async fn me(State(state): State<AppState>, Extension(principal): Extension<Principal>) -> Json<Value> {
    Json(json!({
        "kind": principal.kind,
        "id": if principal.kind == Kind::Device { Some(principal.id.clone()) } else { None },
        "name": principal.name, "server": state.auth.identity(),
    }))
}

async fn setup(State(state): State<AppState>, ClientIp(source): ClientIp, Valid(payload): Valid<Setup>) -> ApiResult<Response> {
    let session = state.setup(&source, payload.code, payload.username, payload.password).await?;
    Ok(reply(201, session))
}

async fn login(State(state): State<AppState>, ClientIp(source): ClientIp, Valid(payload): Valid<Login>) -> ApiResult<Json<Value>> {
    Ok(Json(state.login(&source, payload.username, payload.password).await?))
}

async fn logout(State(state): State<AppState>, Extension(principal): Extension<Principal>) -> ApiResult<Response> {
    state.blocking(move |inner| inner.auth.logout(&inner.store, &principal)).await?;
    Ok(no_content())
}

// 관리

async fn change_password(
    State(state): State<AppState>,
    ClientIp(source): ClientIp,
    Valid(payload): Valid<PasswordChange>,
) -> ApiResult<Json<Value>> {
    Ok(Json(state.change_password(&source, payload.current_password, payload.new_password).await?))
}

async fn server_info(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(state.server_info().await?))
}

async fn devices(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(state.devices().await?))
}

async fn rename_device(
    State(state): State<AppState>,
    Path(device): Path<String>,
    Valid(payload): Valid<DeviceUpdate>,
) -> ApiResult<Json<Value>> {
    let device = Id::parse(&device)?;
    Ok(Json(state.rename_device(device.to_string(), payload.name).await?))
}

async fn revoke_device(State(state): State<AppState>, Path(device): Path<String>) -> ApiResult<Response> {
    let device = Id::parse(&device)?;
    state.revoke_device(device.to_string()).await?;
    Ok(no_content())
}

async fn pairings(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(state.pairings().await?))
}

fn request_origin(headers: &HeaderMap) -> String {
    let host = headers.get(header::HOST).and_then(|value| value.to_str().ok()).unwrap_or("localhost");
    let secure = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("https"));
    format!("{}://{host}", if secure { "https" } else { "http" })
}

async fn create_pairing(
    State(state): State<AppState>,
    headers: HeaderMap,
    Valid(payload): Valid<PairingCreate>,
) -> ApiResult<Response> {
    let server_url = payload.server_url.unwrap_or_else(|| request_origin(&headers));
    let entry = state.create_pairing(payload.name, server_url).await?;
    Ok(reply(201, entry))
}

async fn cancel_pairing(State(state): State<AppState>, Path(entry): Path<String>) -> ApiResult<Response> {
    let entry = Id::parse(&entry)?;
    state.cancel_pairing(entry.to_string()).await?;
    Ok(no_content())
}

// 서재와 동기화

async fn libraries(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let libraries = state.blocking(|inner| inner.store.libraries()).await?;
    Ok(Json(json!({"libraries": libraries})))
}

async fn create_library(State(state): State<AppState>, Valid(payload): Valid<LibraryCreate>) -> ApiResult<Response> {
    let library = state.write(move |inner| inner.store.create_library(&payload)).await?;
    Ok(reply(201, library))
}

async fn settings(State(state): State<AppState>, Path(library): Path<String>) -> ApiResult<Json<Value>> {
    let library = Id::parse(&library)?;
    Ok(Json(state.blocking(move |inner| inner.store.settings(&library)).await?))
}

async fn update_settings(
    State(state): State<AppState>,
    Path(library): Path<String>,
    Valid(payload): Valid<PolicyUpdate>,
) -> ApiResult<Json<Value>> {
    let library = Id::parse(&library)?;
    Ok(Json(state.write(move |inner| inner.store.update_settings(&library, &payload)).await?))
}

async fn snapshot(State(state): State<AppState>, Path(library): Path<String>) -> ApiResult<Json<Value>> {
    let library = Id::parse(&library)?;
    Ok(Json(state.blocking(move |inner| inner.store.snapshot(&library)).await?))
}

async fn changes(State(state): State<AppState>, Path(library): Path<String>, Q(query): Q<CursorQuery>) -> ApiResult<Json<Value>> {
    let library = Id::parse(&library)?;
    let after = bounded(query.after, 0, 0, MAX_CURSOR, "after")?;
    let limit = bounded(query.limit, 100, 1, 500, "limit")?;
    Ok(Json(state.blocking(move |inner| inner.store.changes(&library, after, limit)).await?))
}

/// 실시간 수신 중인 동안 접속 상태로 표시한다.
struct OnlineGuard {
    state: AppState,
    id: String,
}

impl Drop for OnlineGuard {
    fn drop(&mut self) {
        self.state.disconnect(&self.id);
    }
}

async fn events(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(library): Path<String>,
    Q(query): Q<CursorQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let library = Id::parse(&library)?;
    let mut after = bounded(query.after, 0, 0, MAX_CURSOR, "after")?;
    if let Some(value) = headers.get("last-event-id") {
        after = value
            .to_str()
            .ok()
            .and_then(|value| value.trim().parse::<i64>().ok())
            .ok_or_else(|| ApiError::invalid("Last-Event-ID가 올바른 정수가 아닙니다."))?;
        if after < 0 {
            return Err(ApiError::invalid("Last-Event-ID 범위를 확인해 주세요."));
        }
    }
    // 응답 헤더 전에도 서재와 커서를 검증해 재연결 오류를 정상 HTTP 응답으로 알린다.
    let check = library.clone();
    state.blocking(move |inner| inner.store.changes(&check, after, 1)).await?;

    let heartbeat = state.config.heartbeat;
    let stream = async_stream::stream! {
        state.connect(&principal.id);
        let _guard = OnlineGuard { state: state.clone(), id: principal.id.clone() };
        let mut receiver = state.subscribe();
        let mut cursor = after;
        yield Ok::<_, Infallible>("retry: 3000\n\n".to_string());
        loop {
            if state.closing.is_cancelled() {
                break;
            }
            // 연결 해제된 기기나 끝난 로그인의 수신은 다음 확인 때 종료한다.
            let check = principal.clone();
            if !matches!(state.blocking(move |inner| inner.auth.active(&inner.store, &check)).await, Ok(true)) {
                break;
            }
            receiver.borrow_and_update();
            let target = library.clone();
            let Ok(page) = state.blocking(move |inner| inner.store.changes(&target, cursor, 100)).await else {
                break;
            };
            let events = page["events"].as_array().cloned().unwrap_or_default();
            for event in &events {
                let data = serde_json::to_string(event).unwrap_or_default();
                yield Ok(format!("id: {}\nevent: {}\ndata: {data}\n\n", event["id"], event["type"].as_str().unwrap_or("")));
            }
            cursor = page["cursor"].as_i64().unwrap_or(cursor);
            if page["has_more"].as_bool().unwrap_or(false) {
                continue;
            }
            if events.is_empty() {
                yield Ok(": keep-alive\n\n".to_string());
            }
            tokio::select! {
                changed = receiver.changed() => if changed.is_err() { break },
                _ = tokio::time::sleep(heartbeat) => {}
                _ = state.closing.cancelled() => break,
            }
        }
    };
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(stream))
        .map_err(|_| ApiError::internal())
}

async fn document(
    State(state): State<AppState>,
    Path((library, document)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let (library, document) = (Id::parse(&library)?, Id::parse(&document)?);
    let result = state.blocking(move |inner| inner.store.read_document(&library, &document)).await?;
    let etag = format!("\"{}\"", result["version"]);
    if headers.get(header::IF_NONE_MATCH).and_then(|value| value.to_str().ok()) == Some(etag.as_str()) {
        return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response());
    }
    Ok((
        [(header::ETAG, etag), (header::CACHE_CONTROL, "no-cache".to_string())],
        Json(result),
    )
        .into_response())
}

async fn put_document(
    State(state): State<AppState>,
    Path((library, document)): Path<(String, String)>,
    Valid(payload): Valid<DocumentMutation>,
) -> ApiResult<Response> {
    let (library, document) = (Id::parse(&library)?, Id::parse(&document)?);
    let (status, result) = state.write(move |inner| inner.store.mutate_document(&library, &document, &payload)).await?;
    Ok(reply(status, result))
}

async fn conflicts(State(state): State<AppState>, Path(library): Path<String>, Q(query): Q<ConflictQuery>) -> ApiResult<Json<Value>> {
    let library = Id::parse(&library)?;
    let after = query.after.unwrap_or_default();
    if after.chars().count() > 128 {
        return Err(ApiError::invalid("after는 최대 128자입니다."));
    }
    let limit = bounded(query.limit, 100, 1, 500, "limit")?;
    Ok(Json(state.blocking(move |inner| inner.store.conflicts(&library, &after, limit)).await?))
}

async fn conflict(State(state): State<AppState>, Path((library, conflict)): Path<(String, String)>) -> ApiResult<Json<Value>> {
    let (library, conflict) = (Id::parse(&library)?, Id::parse(&conflict)?);
    Ok(Json(state.blocking(move |inner| inner.store.read_conflict(&library, &conflict)).await?))
}

async fn resolve(
    State(state): State<AppState>,
    Path((library, conflict)): Path<(String, String)>,
    Valid(payload): Valid<Resolve>,
) -> ApiResult<Response> {
    let (library, conflict) = (Id::parse(&library)?, Id::parse(&conflict)?);
    let (status, result) = state.write(move |inner| inner.store.resolve_conflict(&library, &conflict, &payload)).await?;
    Ok(reply(status, result))
}

async fn put_asset(
    State(state): State<AppState>,
    Path((library, sha256)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> ApiResult<Json<Value>> {
    let library = Id::parse(&library)?;
    let sha256 = sha256_path(&sha256)?.to_string();
    let data = body.map_err(|rejection| {
        if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE { too_large() } else { ApiError::invalid("첨부 자료를 읽지 못했습니다.") }
    })?;
    let media_type: String = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .chars()
        .take(200)
        .collect();
    Ok(Json(state.write(move |inner| inner.store.put_asset(&library, &sha256, &media_type, &data)).await?))
}

async fn asset(
    State(state): State<AppState>,
    Path((library, sha256)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let library = Id::parse(&library)?;
    let sha256 = sha256_path(&sha256)?.to_string();
    let etag = format!("\"{sha256}\"");
    let (data, media_type) = state.blocking(move |inner| inner.store.read_asset(&library, &sha256)).await?;
    let content_type = HeaderValue::from_str(&media_type).unwrap_or(HeaderValue::from_static("application/octet-stream"));
    let common = [
        (header::ETAG, HeaderValue::from_str(&etag).map_err(|_| ApiError::internal())?),
        (header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=31536000, immutable")),
        (header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")),
        (header::CONTENT_DISPOSITION, HeaderValue::from_static("attachment")),
    ];
    if headers.get(header::IF_NONE_MATCH).and_then(|value| value.to_str().ok()) == Some(etag.as_str()) {
        return Ok((StatusCode::NOT_MODIFIED, common).into_response());
    }
    Ok((common, [(header::CONTENT_TYPE, content_type)], data).into_response())
}

async fn histories(
    State(state): State<AppState>,
    Path((library, document)): Path<(String, String)>,
    Q(query): Q<HistoryQuery>,
) -> ApiResult<Json<Value>> {
    let (library, document) = (Id::parse(&library)?, Id::parse(&document)?);
    let before = bounded(query.before, MAX_CURSOR, 1, MAX_CURSOR, "before")?;
    let limit = bounded(query.limit, 100, 1, 500, "limit")?;
    Ok(Json(state.blocking(move |inner| inner.store.history_list(&library, &document, before, limit)).await?))
}

async fn manual_history(
    State(state): State<AppState>,
    Path((library, document)): Path<(String, String)>,
    Valid(payload): Valid<Capture>,
) -> ApiResult<Json<Value>> {
    let (library, document) = (Id::parse(&library)?, Id::parse(&document)?);
    Ok(Json(state.write(move |inner| inner.store.history_manual(&library, &document, &payload)).await?))
}

async fn history(
    State(state): State<AppState>,
    Path((library, document, history)): Path<(String, String, String)>,
) -> ApiResult<Json<Value>> {
    let (library, document, history) = (Id::parse(&library)?, Id::parse(&document)?, Id::parse(&history)?);
    Ok(Json(state.blocking(move |inner| inner.store.history_read(&library, &document, &history)).await?))
}

async fn restore(
    State(state): State<AppState>,
    Path((library, document, history)): Path<(String, String, String)>,
    Valid(payload): Valid<Restore>,
) -> ApiResult<Response> {
    let (library, document, history) = (Id::parse(&library)?, Id::parse(&document)?, Id::parse(&history)?);
    let (status, result) = state
        .write(move |inner| inner.store.history_restore(&library, &document, &history, &payload))
        .await?;
    Ok(reply(status, result))
}
