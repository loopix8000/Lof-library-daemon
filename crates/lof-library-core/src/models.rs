use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::error::{ApiError, ApiResult};

/// 요청 본문을 받은 뒤 범위와 길이를 확인한다. 실패하면 422 안내 문구를 돌려준다.
pub trait Validate {
    fn validate(&self) -> Result<(), String> {
        Ok(())
    }
}

fn length(value: &str, minimum: usize, maximum: usize, label: &str) -> Result<(), String> {
    let count = value.chars().count();
    if count < minimum || count > maximum {
        return Err(if minimum > 0 {
            format!("{label}은(는) {minimum}~{maximum}자로 입력해 주세요.")
        } else {
            format!("{label}은(는) 최대 {maximum}자입니다.")
        });
    }
    Ok(())
}

fn range(value: i64, minimum: i64, maximum: i64, label: &str) -> Result<(), String> {
    if value < minimum || value > maximum {
        return Err(format!("{label}은(는) {minimum}~{maximum} 범위여야 합니다."));
    }
    Ok(())
}

/// 서재·문서·기기·요청 ID. 영문, 숫자, `_`, `-`만 쓰며 최대 128자다.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct Id(String);

impl Id {
    pub fn parse(value: &str) -> ApiResult<Self> {
        let valid = !value.is_empty()
            && value.len() <= 128
            && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
        if valid {
            Ok(Self(value.to_string()))
        } else {
            Err(ApiError::invalid("ID에는 영문, 숫자, _, -만 사용하며 최대 128자입니다."))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for Id {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Id {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Id::parse(&value).map_err(|error| de::Error::custom(error.message()))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SmartHistory {
    pub enabled: bool,
    pub keep_recent_minutes: i64,
    pub bucket_minutes: i64,
}

impl Default for SmartHistory {
    fn default() -> Self {
        Self { enabled: false, keep_recent_minutes: 60, bucket_minutes: 10 }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HistoryPolicy {
    pub min_minutes: i64,
    pub min_changed_chars: i64,
    pub retention_days: Option<i64>,
    pub expire_protected: bool,
    pub smart: SmartHistory,
}

impl Default for HistoryPolicy {
    fn default() -> Self {
        Self {
            min_minutes: 10,
            min_changed_chars: 200,
            retention_days: None,
            expire_protected: false,
            smart: SmartHistory::default(),
        }
    }
}

impl Validate for HistoryPolicy {
    fn validate(&self) -> Result<(), String> {
        range(self.min_minutes, 1, 10080, "이력 생성 간격(분)")?;
        range(self.min_changed_chars, 1, 1_000_000, "변경 글자 수")?;
        if let Some(days) = self.retention_days {
            range(days, 1, 36500, "이력 보관 기간(일)")?;
        }
        range(self.smart.keep_recent_minutes, 1, 525_600, "최근 이력 전체 보관 시간(분)")?;
        range(self.smart.bucket_minutes, 1, 525_600, "이력 정리 간격(분)")
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryCreate {
    pub id: Id,
    pub name: String,
    #[serde(default)]
    pub history: HistoryPolicy,
}

impl Validate for LibraryCreate {
    fn validate(&self) -> Result<(), String> {
        length(&self.name, 1, 200, "서재 이름")?;
        self.history.validate()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyUpdate {
    pub expected_version: i64,
    pub history: HistoryPolicy,
}

impl Validate for PolicyUpdate {
    fn validate(&self) -> Result<(), String> {
        range(self.expected_version, 1, i64::MAX, "expected_version")?;
        self.history.validate()
    }
}

fn default_kind() -> String {
    "episode".into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentMutation {
    pub device_id: Id,
    pub mutation_id: Id,
    pub expected_version: i64,
    #[serde(default = "default_kind")]
    pub kind: String,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub metadata: Map<String, Value>,
    #[serde(default)]
    pub deleted: bool,
}

impl DocumentMutation {
    /// 저장된 문서 내용(kind·title·body·metadata·deleted)에 요청 식별 정보를 붙여 새 변경을 만든다.
    pub fn from_content(content: &Value, device_id: &Id, mutation_id: &Id, expected_version: i64) -> ApiResult<Self> {
        let mut value = content.clone();
        let object = value.as_object_mut().ok_or_else(ApiError::internal)?;
        object.insert("device_id".into(), json!(device_id));
        object.insert("mutation_id".into(), json!(mutation_id));
        object.insert("expected_version".into(), json!(expected_version));
        Ok(serde_json::from_value(value)?)
    }

    pub fn content(&self) -> Value {
        json!({
            "kind": self.kind, "title": self.title, "body": self.body,
            "metadata": self.metadata, "deleted": self.deleted,
        })
    }
}

impl Validate for DocumentMutation {
    fn validate(&self) -> Result<(), String> {
        range(self.expected_version, 0, i64::MAX, "expected_version")?;
        if !["episode", "memo", "plot", "project"].contains(&self.kind.as_str()) {
            return Err("kind는 episode, memo, plot, project 중 하나입니다.".into());
        }
        length(&self.title, 0, 1000, "제목")?;
        length(&self.body, 0, 1_000_000, "본문")
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capture {
    pub expected_version: i64,
    pub device_id: Id,
}

impl Validate for Capture {
    fn validate(&self) -> Result<(), String> {
        range(self.expected_version, 1, i64::MAX, "expected_version")
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Restore {
    pub device_id: Id,
    pub mutation_id: Id,
    pub expected_version: i64,
}

impl Validate for Restore {
    fn validate(&self) -> Result<(), String> {
        range(self.expected_version, 1, i64::MAX, "expected_version")
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resolve {
    pub device_id: Id,
    pub mutation_id: Id,
    pub expected_version: i64,
    pub choice: String,
}

impl Validate for Resolve {
    fn validate(&self) -> Result<(), String> {
        range(self.expected_version, 1, i64::MAX, "expected_version")?;
        if !["current", "incoming"].contains(&self.choice.as_str()) {
            return Err("choice는 current 또는 incoming입니다.".into());
        }
        Ok(())
    }
}

/// 관리자 아이디는 영문·숫자·`_`·`-`·`.`로 3~32자다.
fn username(value: &str) -> Result<(), String> {
    let value = value.trim();
    let valid = (3..=32).contains(&value.len())
        && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
    if valid {
        Ok(())
    } else {
        Err("아이디는 영문, 숫자, _, -, .으로 3~32자 입력해 주세요.".into())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Setup {
    pub code: String,
    pub username: String,
    pub password: String,
}

impl Validate for Setup {
    fn validate(&self) -> Result<(), String> {
        length(&self.code, 4, 32, "설정 코드")?;
        username(&self.username)?;
        length(&self.password, 8, 256, "비밀번호")
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Login {
    pub username: String,
    pub password: String,
}

impl Validate for Login {
    fn validate(&self) -> Result<(), String> {
        length(&self.username, 1, 64, "아이디")?;
        length(&self.password, 1, 256, "비밀번호")
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasswordChange {
    pub current_password: String,
    pub new_password: String,
}

impl Validate for PasswordChange {
    fn validate(&self) -> Result<(), String> {
        length(&self.current_password, 1, 256, "현재 비밀번호")?;
        length(&self.new_password, 8, 256, "새 비밀번호")
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceUpdate {
    pub name: String,
}

impl Validate for DeviceUpdate {
    fn validate(&self) -> Result<(), String> {
        length(self.name.trim(), 1, 100, "기기 이름")
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingCreate {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub server_url: Option<String>,
}

pub fn valid_server_url(value: &str) -> bool {
    let rest = value.strip_prefix("https://").or_else(|| value.strip_prefix("http://"));
    match rest {
        Some(rest) => {
            let host = rest.split('/').next().unwrap_or("");
            value.len() <= 500
                && !host.is_empty()
                && !value.chars().any(|char| char.is_whitespace() || char == '?' || char == '#')
        }
        None => false,
    }
}

impl Validate for PairingCreate {
    fn validate(&self) -> Result<(), String> {
        length(&self.name, 0, 100, "기기 이름")?;
        if let Some(url) = &self.server_url {
            if !valid_server_url(url) {
                return Err("서버 주소는 http:// 또는 https://로 시작하는 주소로 입력해 주세요.".into());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pair {
    pub code: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub platform: String,
}

impl Validate for Pair {
    fn validate(&self) -> Result<(), String> {
        length(&self.code, 4, 32, "연결 코드")?;
        length(&self.name, 0, 100, "기기 이름")?;
        length(&self.platform, 0, 50, "플랫폼")
    }
}
