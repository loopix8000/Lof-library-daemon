use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{Map, Value};

/// API 응답으로 그대로 내보내는 오류. 본문은 항상 `error`(코드)와 `message`(안내)를 포함한다.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub body: Map<String, Value>,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(status: u16, code: &str, message: impl Into<String>) -> Self {
        let mut body = Map::new();
        body.insert("error".into(), Value::String(code.into()));
        body.insert("message".into(), Value::String(message.into()));
        Self { status: StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), body }
    }

    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.body.insert(key.into(), value.into());
        self
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(422, "invalid_request", message)
    }

    pub fn internal() -> Self {
        Self::new(500, "internal_error", "서버 작업을 완료하지 못했습니다. 잠시 후 다시 시도해 주세요.")
    }

    pub fn code(&self) -> &str {
        self.body.get("error").and_then(Value::as_str).unwrap_or("")
    }

    pub fn message(&self) -> &str {
        self.body.get("message").and_then(Value::as_str).unwrap_or("")
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message())
    }
}

impl std::error::Error for ApiError {}

impl From<rusqlite::Error> for ApiError {
    fn from(error: rusqlite::Error) -> Self {
        tracing::error!("데이터베이스 작업을 완료하지 못했습니다: {error}");
        Self::new(503, "database_unavailable", "저장소를 사용할 수 없습니다. 잠시 후 다시 시도해 주세요.")
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(error: serde_json::Error) -> Self {
        tracing::error!("저장된 JSON을 읽지 못했습니다: {error}");
        Self::internal()
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let unauthorized = self.status == StatusCode::UNAUTHORIZED;
        let mut response = (self.status, Json(Value::Object(self.body))).into_response();
        if unauthorized {
            response.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

/// 서버를 시작하지 못한 이유. 실행 파일과 관리 앱이 사용자에게 그대로 보여 준다.
#[derive(Debug)]
pub struct StartError(pub String);

impl std::fmt::Display for StartError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for StartError {}

impl From<std::io::Error> for StartError {
    fn from(error: std::io::Error) -> Self {
        Self(format!("데이터 폴더를 사용할 수 없습니다: {error}"))
    }
}

impl From<rusqlite::Error> for StartError {
    fn from(error: rusqlite::Error) -> Self {
        Self(format!("데이터베이스를 열지 못했습니다: {error}"))
    }
}

impl From<ApiError> for StartError {
    fn from(error: ApiError) -> Self {
        Self(error.message().to_string())
    }
}
