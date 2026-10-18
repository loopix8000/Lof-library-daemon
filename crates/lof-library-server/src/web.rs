//! 서버 실행에서 브라우저로 데스크톱과 같은 관리 화면을 제공한다. 아이디·비밀번호로 보호한다.
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use axum::{
    extract::State,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use lof_library_core::{control, AppState, VERSION};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;

const COOKIE: &str = "lof_session";
const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
const LOCK_AFTER: u32 = 10;
const LOCK_TIME: Duration = Duration::from_secs(60);
const AUTH_FILE: &str = "web-auth.json";

const INDEX: &str = include_str!("../../../desktop/ui/index.html");
const APP_JS: &str = include_str!("../../../desktop/ui/app.js");
const LIBRARIES_JS: &str = include_str!("../../../desktop/ui/libraries.js");
const REALTIME_JS: &str = include_str!("../../../desktop/ui/realtime.js");
const STYLE_CSS: &str = include_str!("../../../desktop/ui/style.css");
const SHIM_JS: &str = include_str!("../web/web-shim.js");
const LOGIN_HTML: &str = include_str!("../web/login.html");
const LOGIN_JS: &str = include_str!("../web/login.js");

#[derive(Clone)]
struct Web {
    core: AppState,
    auth_file: PathBuf,
    setup_code: Arc<Mutex<Option<String>>>,
    sessions: Arc<Mutex<HashMap<String, Instant>>>,
    failures: Arc<Mutex<(u32, Option<Instant>)>>,
}

pub fn auth_path(data_dir: &Path) -> PathBuf {
    data_dir.join(AUTH_FILE)
}

fn fail(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

fn random_token() -> String {
    format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
}

fn account(file: &Path) -> Option<(String, String)> {
    let value: Value = serde_json::from_slice(&std::fs::read(file).ok()?).ok()?;
    Some((
        value["username"].as_str()?.to_string(),
        value["hash"].as_str()?.to_string(),
    ))
}

fn write_account(file: &Path, username: &str, hash: &str) -> std::io::Result<()> {
    let text = json!({ "username": username, "hash": hash }).to_string();
    let temp = file.with_extension("tmp");
    std::fs::write(&temp, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(temp, file)
}

fn hash_password(password: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|_| "비밀번호를 처리하지 못했습니다.".to_string())
}

fn verify_password(hash: &str, password: &str) -> bool {
    PasswordHash::new(hash)
        .map(|parsed| Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok())
        .unwrap_or(false)
}

fn session_cookie(token: &str, max_age: u64, headers: &HeaderMap) -> HeaderValue {
    let secure = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("https"));
    HeaderValue::from_str(&format!(
        "{COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={max_age}{}",
        if secure { "; Secure" } else { "" }
    ))
    .unwrap()
}

fn token_of(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|part| part.trim().strip_prefix(&format!("{COOKIE}=")))
        .next()
        .map(str::to_string)
}

// 다른 사이트에서 보낸 변경 요청을 막는다. 브라우저가 Origin을 보내면 접속한 주소와 같아야 한다.
fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    origin
        .split_once("://")
        .map(|(_, rest)| Some(rest) == host)
        .unwrap_or(false)
}

impl Web {
    fn authed(&self, headers: &HeaderMap) -> bool {
        let Some(token) = token_of(headers) else {
            return false;
        };
        let mut sessions = self.sessions.lock().unwrap();
        sessions.retain(|_, until| *until > Instant::now());
        sessions.contains_key(&token)
    }

    fn open_session(&self, headers: &HeaderMap) -> HeaderValue {
        let token = random_token();
        self.sessions
            .lock()
            .unwrap()
            .insert(token.clone(), Instant::now() + SESSION_TTL);
        session_cookie(&token, SESSION_TTL.as_secs(), headers)
    }

    fn locked(&self) -> bool {
        let mut failures = self.failures.lock().unwrap();
        match failures.1 {
            Some(until) if until > Instant::now() => true,
            Some(_) => {
                *failures = (0, None);
                false
            }
            None => false,
        }
    }

    async fn failed(&self) {
        {
            let mut failures = self.failures.lock().unwrap();
            failures.0 += 1;
            if failures.0 >= LOCK_AFTER {
                failures.1 = Some(Instant::now() + LOCK_TIME);
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    fn succeeded(&self) {
        *self.failures.lock().unwrap() = (0, None);
    }
}

fn valid_credentials(username: &str, password: &str) -> Result<(), &'static str> {
    if username.is_empty() || username.chars().count() > 64 {
        return Err("아이디를 1~64자로 입력해 주세요.");
    }
    if password.chars().count() < 8 || password.chars().count() > 256 {
        return Err("비밀번호를 8~256자로 입력해 주세요.");
    }
    Ok(())
}

async fn session(State(web): State<Web>, headers: HeaderMap) -> Response {
    let setup = account(&web.auth_file).is_none();
    Json(json!({ "authenticated": web.authed(&headers), "setup": setup })).into_response()
}

async fn setup(State(web): State<Web>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if !same_origin(&headers) {
        return fail(StatusCode::FORBIDDEN, "허용되지 않은 요청입니다.");
    }
    if web.locked() {
        return fail(StatusCode::TOO_MANY_REQUESTS, "잠시 후 다시 시도해 주세요.");
    }
    if account(&web.auth_file).is_some() {
        return fail(StatusCode::CONFLICT, "이미 관리자 계정이 있습니다.");
    }
    let code = body["code"].as_str().unwrap_or("").trim().to_uppercase();
    let username = body["username"].as_str().unwrap_or("").trim().to_string();
    let password = body["password"].as_str().unwrap_or("").to_string();
    let expected = web.setup_code.lock().unwrap().clone();
    let matches = expected
        .as_ref()
        .is_some_and(|expected| bool::from(expected.as_bytes().ct_eq(code.as_bytes())));
    if !matches {
        web.failed().await;
        return fail(StatusCode::FORBIDDEN, "최초 설정 코드가 올바르지 않습니다.");
    }
    if let Err(message) = valid_credentials(&username, &password) {
        return fail(StatusCode::BAD_REQUEST, message);
    }
    let Ok(Ok(hash)) = tokio::task::spawn_blocking(move || hash_password(&password)).await else {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "비밀번호를 처리하지 못했습니다.");
    };
    if write_account(&web.auth_file, &username, &hash).is_err() {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "계정을 저장하지 못했습니다.");
    }
    *web.setup_code.lock().unwrap() = None;
    web.succeeded();
    let cookie = web.open_session(&headers);
    ([(header::SET_COOKIE, cookie)], Json(json!({ "ok": true }))).into_response()
}

async fn login(State(web): State<Web>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if !same_origin(&headers) {
        return fail(StatusCode::FORBIDDEN, "허용되지 않은 요청입니다.");
    }
    if web.locked() {
        return fail(StatusCode::TOO_MANY_REQUESTS, "로그인 시도가 너무 많습니다. 잠시 후 다시 시도해 주세요.");
    }
    let username = body["username"].as_str().unwrap_or("").to_string();
    let password = body["password"].as_str().unwrap_or("").to_string();
    let Some((stored_name, hash)) = account(&web.auth_file) else {
        return fail(StatusCode::CONFLICT, "먼저 관리자 계정을 만들어 주세요.");
    };
    let ok = tokio::task::spawn_blocking(move || {
        // 아이디가 달라도 같은 시간이 걸리도록 비밀번호를 항상 검증한다.
        let password_ok = verify_password(&hash, &password);
        let name_ok = bool::from(stored_name.as_bytes().ct_eq(username.as_bytes()));
        password_ok && name_ok
    })
    .await
    .unwrap_or(false);
    if !ok {
        web.failed().await;
        return fail(StatusCode::UNAUTHORIZED, "아이디 또는 비밀번호가 올바르지 않습니다.");
    }
    web.succeeded();
    let cookie = web.open_session(&headers);
    ([(header::SET_COOKIE, cookie)], Json(json!({ "ok": true }))).into_response()
}

async fn logout(State(web): State<Web>, headers: HeaderMap) -> Response {
    if !same_origin(&headers) {
        return fail(StatusCode::FORBIDDEN, "허용되지 않은 요청입니다.");
    }
    if let Some(token) = token_of(&headers) {
        web.sessions.lock().unwrap().remove(&token);
    }
    ([(header::SET_COOKIE, session_cookie("", 0, &headers))], Json(json!({ "ok": true }))).into_response()
}

async fn status(State(web): State<Web>, headers: HeaderMap) -> Response {
    if !web.authed(&headers) {
        return fail(StatusCode::UNAUTHORIZED, "로그인이 필요합니다.");
    }
    let realtime = web.core.realtime.clone();
    let Ok(peer) = tokio::task::spawn_blocking(move || realtime.info()).await else {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "상태를 확인하지 못했습니다.");
    };
    let error = peer["error"].as_str().unwrap_or("").to_string();
    let state = if !error.is_empty() {
        "error"
    } else if peer["ready"] == true {
        "running"
    } else {
        "starting"
    };
    Json(json!({
        "state": state,
        "message": if error.is_empty() { Value::Null } else { json!(error) },
        "peer": peer,
        "autostart": false,
        "version": VERSION,
        "data_dir": web.core.config.data_dir.display().to_string(),
    }))
    .into_response()
}

fn qr_svg(text: &str) -> Result<String, String> {
    let code = qrcode::QrCode::new(text.as_bytes())
        .map_err(|error| format!("QR을 만들지 못했습니다: {error}"))?;
    Ok(code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(220, 220)
        .dark_color(qrcode::render::svg::Color("#000000"))
        .light_color(qrcode::render::svg::Color("#ffffff"))
        .build())
}

async fn manage(State(web): State<Web>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if !web.authed(&headers) {
        return fail(StatusCode::UNAUTHORIZED, "로그인이 필요합니다.");
    }
    if !same_origin(&headers) {
        return fail(StatusCode::FORBIDDEN, "허용되지 않은 요청입니다.");
    }
    let action = body["action"].as_str().unwrap_or("").to_string();
    // 동기화 중지처럼 서버 수명에 관한 명령은 웹에서 허용하지 않는다.
    if !matches!(
        action.as_str(),
        "create"
            | "rename"
            | "invite"
            | "cancelInvite"
            | "remove"
            | "renamePeer"
            | "pair"
            | "leave"
            | "dismissReset"
    ) {
        return fail(StatusCode::BAD_REQUEST, "지원하지 않는 관리 명령입니다.");
    }
    match control::action(web.core.realtime.clone(), &action, body["args"].clone()).await {
        Ok(mut result) => {
            if action == "invite" {
                match qr_svg(result["code"].as_str().unwrap_or("")) {
                    Ok(svg) => result["qr_svg"] = json!(svg),
                    Err(error) => return fail(StatusCode::INTERNAL_SERVER_ERROR, &error),
                }
            }
            Json(result).into_response()
        }
        Err(error) => fail(StatusCode::BAD_REQUEST, &error),
    }
}

async fn index(State(web): State<Web>, headers: HeaderMap) -> Response {
    if !web.authed(&headers) {
        return Redirect::to("/login").into_response();
    }
    let page = INDEX.replace(
        "<script src=\"libraries.js\">",
        "<script src=\"web-shim.js\"></script><script src=\"libraries.js\">",
    );
    asset(page, "text/html; charset=utf-8")
}

fn asset(body: impl Into<String>, mime: &'static str) -> Response {
    ([(header::CONTENT_TYPE, mime)], body.into()).into_response()
}

async fn secure_headers(request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let api = request.uri().path().starts_with("/api/");
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        ),
    );
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    if api {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

pub fn router_state(core: AppState, auth_file: PathBuf) -> Router {
    let setup_code = if account(&auth_file).is_none() {
        let raw = uuid::Uuid::new_v4().simple().to_string().to_uppercase();
        let code = format!("{}-{}", &raw[..4], &raw[4..8]);
        tracing::info!("웹 관리 화면 최초 설정 코드: {code}");
        Some(code)
    } else {
        None
    };
    let web = Web {
        core,
        auth_file,
        setup_code: Arc::new(Mutex::new(setup_code)),
        sessions: Arc::default(),
        failures: Arc::new(Mutex::new((0, None))),
    };
    Router::new()
        .route("/", get(index))
        .route("/login", get(|| async { asset(LOGIN_HTML, "text/html; charset=utf-8") }))
        .route("/login.js", get(|| async { asset(LOGIN_JS, "text/javascript; charset=utf-8") }))
        .route("/web-shim.js", get(|| async { asset(SHIM_JS, "text/javascript; charset=utf-8") }))
        .route("/app.js", get(|| async { asset(APP_JS, "text/javascript; charset=utf-8") }))
        .route("/libraries.js", get(|| async { asset(LIBRARIES_JS, "text/javascript; charset=utf-8") }))
        .route("/realtime.js", get(|| async { asset(REALTIME_JS, "text/javascript; charset=utf-8") }))
        .route("/style.css", get(|| async { asset(STYLE_CSS, "text/css; charset=utf-8") }))
        .route("/api/session", get(session))
        .route("/api/setup", post(setup))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/status", get(status))
        .route("/api/manage", post(manage))
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
        .layer(axum::middleware::from_fn(secure_headers))
        .with_state(web)
}

pub async fn serve(listener: TcpListener, core: AppState, auth_file: PathBuf) {
    let closing = core.closing.clone();
    let app = router_state(core, auth_file);
    if let Err(error) = axum::serve(listener, app)
        .with_graceful_shutdown(async move { closing.cancelled().await })
        .await
    {
        tracing::error!("웹 관리 화면이 중지되었습니다: {error}");
    }
}
