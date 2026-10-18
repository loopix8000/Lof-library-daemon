use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use hmac::{Hmac, Mac};
use rand::rngs::OsRng;
use rand::{Rng, RngCore};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::error::{ApiError, ApiResult};
use crate::store::{new_id, not_found, Store};
use crate::VERSION;

const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
pub const PAIRING_SECONDS: f64 = 10.0 * 60.0;
pub const SESSION_SECONDS: f64 = 12.0 * 60.0 * 60.0;
const SEEN_INTERVAL: f64 = 60.0;
const KEEP_USED_SECONDS: f64 = 24.0 * 60.0 * 60.0;
const SCRYPT_LOG_N: u8 = 15;
const SCRYPT_R: u32 = 8;
const SCRYPT_P: u32 = 1;
const LOGIN_FAILED: &str = "아이디 또는 비밀번호가 올바르지 않습니다.";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// 웹 관리 화면에 아이디·비밀번호로 로그인한 관리자
    Session,
    /// 연결 코드로 등록한 기기. 동기화만 할 수 있다.
    Device,
}

#[derive(Clone, Debug, Serialize)]
pub struct Principal {
    pub kind: Kind,
    pub id: String,
    pub name: String,
}

impl Principal {
    pub fn admin(&self) -> bool {
        self.kind == Kind::Session
    }
}

pub fn random_token(prefix: &str) -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    format!("{prefix}{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

pub fn new_code() -> String {
    let value: String = (0..8).map(|_| CODE_ALPHABET[OsRng.gen_range(0..CODE_ALPHABET.len())] as char).collect();
    format!("{}-{}", &value[..4], &value[4..])
}

pub fn normalize_code(code: &str) -> String {
    code.chars().filter(char::is_ascii_alphanumeric).map(|char| char.to_ascii_uppercase()).collect()
}

fn equal(left: &str, right: &str) -> bool {
    left.as_bytes().ct_eq(right.as_bytes()).into()
}

pub fn hash_password(password: &str) -> String {
    let mut salt = [0u8; 16];
    OsRng.fill_bytes(&mut salt);
    let params = scrypt::Params::new(SCRYPT_LOG_N, SCRYPT_R, SCRYPT_P, 32).expect("scrypt 설정이 올바릅니다");
    let mut derived = [0u8; 32];
    scrypt::scrypt(password.as_bytes(), &salt, &params, &mut derived).expect("출력 길이가 올바릅니다");
    format!("scrypt${}${SCRYPT_R}${SCRYPT_P}${}${}", 1u64 << SCRYPT_LOG_N, hex::encode(salt), hex::encode(derived))
}

pub fn verify_password(password: &str, stored: &str) -> bool {
    let parts: Vec<&str> = stored.split('$').collect();
    let [_, n, r, p, salt, expected] = parts[..] else { return false };
    let (Ok(n), Ok(r), Ok(p), Ok(salt), Ok(expected)) =
        (n.parse::<u64>(), r.parse::<u32>(), p.parse::<u32>(), hex::decode(salt), hex::decode(expected))
    else {
        return false;
    };
    if !n.is_power_of_two() || expected.is_empty() || expected.len() > 64 {
        return false;
    }
    let Ok(params) = scrypt::Params::new(n.trailing_zeros() as u8, r, p, expected.len()) else { return false };
    let mut derived = vec![0u8; expected.len()];
    if scrypt::scrypt(password.as_bytes(), &salt, &params, &mut derived).is_err() {
        return false;
    }
    derived.ct_eq(&expected).into()
}

/// 인증 실패를 출처별·전체로 세어 연결 코드와 비밀번호 대입을 늦춘다.
pub struct Throttle {
    limit: usize,
    total: usize,
    window: Duration,
    failures: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl Throttle {
    pub fn new(limit: usize, total: usize) -> Self {
        Self { limit, total, window: Duration::from_secs(600), failures: Mutex::new(HashMap::new()) }
    }

    fn count(failures: &mut HashMap<String, VecDeque<Instant>>, key: &str, window: Duration) -> usize {
        let Some(queue) = failures.get_mut(key) else { return 0 };
        while queue.front().is_some_and(|time| time.elapsed() >= window) {
            queue.pop_front();
        }
        let count = queue.len();
        if count == 0 {
            failures.remove(key);
        }
        count
    }

    pub fn check(&self, source: &str) -> ApiResult<()> {
        let mut failures = self.failures.lock().unwrap_or_else(|error| error.into_inner());
        if Self::count(&mut failures, source, self.window) >= self.limit
            || Self::count(&mut failures, "*", self.window) >= self.total
        {
            return Err(ApiError::new(429, "too_many_attempts", "입력 시도가 너무 많습니다. 잠시 후 다시 시도해 주세요."));
        }
        Ok(())
    }

    pub fn fail(&self, source: &str) {
        let mut failures = self.failures.lock().unwrap_or_else(|error| error.into_inner());
        let now = Instant::now();
        for key in [source, "*"] {
            failures.entry(key.to_string()).or_default().push_back(now);
        }
    }
}

/// 서버 키, 관리자 계정과 로그인, 기기 인증 키, 일회용 연결 코드를 서로 분리해 관리한다.
pub struct Auth {
    secret: Vec<u8>,
    pub server_id: String,
    pub server_name: String,
}

fn get(db: &Connection, key: &str) -> ApiResult<Option<String>> {
    Ok(db.query_row("SELECT value FROM server WHERE key=?1", [key], |row| row.get(0)).optional()?)
}

fn put(db: &Connection, key: &str, value: &str) -> ApiResult<()> {
    db.execute(
        "INSERT INTO server VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn device_row(row: &rusqlite::Row) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": row.get::<_, String>("id")?, "name": row.get::<_, String>("name")?,
        "platform": row.get::<_, String>("platform")?,
        "created_at": row.get::<_, f64>("created_at")?, "last_seen_at": row.get::<_, Option<f64>>("last_seen_at")?,
    }))
}

impl Auth {
    pub fn load(store: &Store, server_name: &str) -> ApiResult<Self> {
        // 서버 키는 최초 기동 때 한 번 만들고 DB에 보관해 재기동·업데이트·백업 복구 후에도 기기 연결을 유지한다.
        let (secret, server_id) = store.write(|db| {
            let secret = match get(db, "secret")? {
                Some(secret) => secret,
                None => {
                    let mut bytes = [0u8; 32];
                    OsRng.fill_bytes(&mut bytes);
                    let secret = hex::encode(bytes);
                    put(db, "secret", &secret)?;
                    secret
                }
            };
            let server_id = match get(db, "server_id")? {
                Some(server_id) => server_id,
                None => {
                    let server_id = new_id();
                    put(db, "server_id", &server_id)?;
                    server_id
                }
            };
            Ok((secret, server_id))
        })?;
        Ok(Self {
            secret: hex::decode(secret).map_err(|_| ApiError::internal())?,
            server_id,
            server_name: server_name.to_string(),
        })
    }

    pub fn hash(&self, purpose: &str, value: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.secret).expect("HMAC은 모든 키 길이를 허용합니다");
        mac.update(purpose.as_bytes());
        mac.update(b":");
        mac.update(value.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    pub fn identity(&self) -> Value {
        json!({"id": self.server_id, "name": self.server_name, "version": VERSION})
    }

    fn session(&self, store: &Store, db: &Connection) -> ApiResult<Value> {
        let token = random_token("lofs_");
        let now = store.clock.now();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3)",
            params![self.hash("session", &token), now, now + SESSION_SECONDS],
        )?;
        Ok(json!({"token": token, "expires_at": now + SESSION_SECONDS}))
    }

    // 관리자 계정. 웹 관리 화면에서만 사용하며, 설치형 앱은 로그인 없이 관리한다.

    pub fn setup_required(&self, store: &Store) -> ApiResult<bool> {
        store.read(|db| Ok(get(db, "admin_password")?.is_none()))
    }

    /// 관리자 계정이 없으면 새 최초 설정 코드를 만든다. 이전 코드는 더 이상 쓸 수 없다.
    pub fn prepare_setup(&self, store: &Store) -> ApiResult<Option<String>> {
        store.write(|db| {
            if get(db, "admin_password")?.is_some() {
                return Ok(None);
            }
            let code = new_code();
            put(db, "setup_code", &self.hash("setup", &normalize_code(&code)))?;
            Ok(Some(code))
        })
    }

    /// 관리자 계정과 로그인을 지우고 새 최초 설정 코드를 발급한다. 호스트의 명령줄에서만 실행한다.
    pub fn reset_admin(&self, store: &Store) -> ApiResult<Option<String>> {
        store.write(|db| {
            db.execute("DELETE FROM server WHERE key IN ('admin_username','admin_password','setup_code')", [])?;
            db.execute("DELETE FROM sessions", [])?;
            Ok(())
        })?;
        self.prepare_setup(store)
    }

    pub fn setup(&self, store: &Store, code: &str, username: &str, password: &str) -> ApiResult<Value> {
        let hashed = hash_password(password);
        store.write(|db| {
            if get(db, "admin_password")?.is_some() {
                return Err(ApiError::new(409, "already_configured", "관리자 계정이 이미 있습니다."));
            }
            let expected = get(db, "setup_code")?;
            let provided = self.hash("setup", &normalize_code(code));
            if !expected.is_some_and(|expected| equal(&expected, &provided)) {
                return Err(ApiError::new(403, "setup_code_invalid", "최초 설정 코드가 올바르지 않습니다. 서버 로그의 코드를 확인해 주세요."));
            }
            put(db, "admin_username", username.trim())?;
            put(db, "admin_password", &hashed)?;
            db.execute("DELETE FROM server WHERE key='setup_code'", [])?;
            self.session(store, db)
        })
    }

    pub fn login(&self, store: &Store, username: &str, password: &str) -> ApiResult<Value> {
        let (stored_username, stored_password) = store.read(|db| Ok((get(db, "admin_username")?, get(db, "admin_password")?)))?;
        let Some(stored_password) = stored_password else {
            return Err(ApiError::new(409, "setup_required", "먼저 서버 로그의 최초 설정 코드로 관리자 계정을 만들어 주세요."));
        };
        // 아이디가 틀려도 비밀번호 확인을 똑같이 거쳐 어느 쪽이 틀렸는지 응답 시간으로 드러나지 않게 한다.
        let name_matches = equal(username.trim(), stored_username.as_deref().unwrap_or_default());
        let password_matches = verify_password(password, &stored_password);
        if !(name_matches && password_matches) {
            return Err(ApiError::new(401, "invalid_credentials", LOGIN_FAILED));
        }
        store.write(|db| self.session(store, db))
    }

    pub fn logout(&self, store: &Store, principal: &Principal) -> ApiResult<()> {
        if principal.kind == Kind::Session {
            store.write(|db| {
                db.execute("DELETE FROM sessions WHERE token_hash=?1", [&principal.id])?;
                Ok(())
            })?;
        }
        Ok(())
    }

    pub fn change_password(&self, store: &Store, current: &str, new: &str) -> ApiResult<Value> {
        let stored = store.read(|db| get(db, "admin_password"))?
            .ok_or_else(|| ApiError::new(409, "setup_required", "먼저 관리자 계정을 만들어 주세요."))?;
        if !verify_password(current, &stored) {
            return Err(ApiError::new(403, "invalid_password", "현재 비밀번호가 올바르지 않습니다."));
        }
        let hashed = hash_password(new);
        // 비밀번호를 바꾸면 다른 브라우저의 로그인을 모두 끝내고 요청한 쪽에만 새 로그인을 준다.
        store.write(|db| {
            put(db, "admin_password", &hashed)?;
            db.execute("DELETE FROM sessions", [])?;
            self.session(store, db)
        })
    }

    pub fn admin_username(&self, store: &Store) -> ApiResult<Option<String>> {
        store.read(|db| get(db, "admin_username"))
    }

    // 인증

    pub fn authenticate(&self, store: &Store, token: &str) -> ApiResult<Option<Principal>> {
        let now = store.clock.now();
        if token.starts_with("lofs_") {
            let hashed = self.hash("session", token);
            let expires: Option<f64> = store.read(|db| {
                Ok(db.query_row("SELECT expires_at FROM sessions WHERE token_hash=?1", [&hashed], |row| row.get(0)).optional()?)
            })?;
            return Ok(expires
                .filter(|expires| *expires > now)
                .map(|_| Principal { kind: Kind::Session, id: hashed, name: "관리자".into() }));
        }
        if !token.starts_with("lofd_") {
            return Ok(None);
        }
        let hashed = self.hash("device", token);
        let row: Option<(String, String, Option<f64>)> = store.read(|db| {
            Ok(db
                .query_row("SELECT id,name,last_seen_at FROM devices WHERE token_hash=?1", [&hashed], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .optional()?)
        })?;
        let Some((id, name, last_seen)) = row else { return Ok(None) };
        // 마지막 접속 시각은 일정 간격으로만 기록해 요청마다 쓰기가 생기지 않게 한다.
        if last_seen.is_none_or(|seen| now - seen >= SEEN_INTERVAL) {
            store.write(|db| {
                db.execute("UPDATE devices SET last_seen_at=?1 WHERE id=?2", params![now, id])?;
                Ok(())
            })?;
        }
        Ok(Some(Principal { kind: Kind::Device, id, name }))
    }

    /// 연결 해제한 기기나 끝난 로그인의 실시간 수신을 끊기 위해 상태를 다시 확인한다.
    pub fn active(&self, store: &Store, principal: &Principal) -> ApiResult<bool> {
        let now = store.clock.now();
        store.read(|db| match principal.kind {
            Kind::Device => {
                Ok(db.query_row("SELECT 1 FROM devices WHERE id=?1", [&principal.id], |_| Ok(())).optional()?.is_some())
            }
            Kind::Session => {
                let expires: Option<f64> = db
                    .query_row("SELECT expires_at FROM sessions WHERE token_hash=?1", [&principal.id], |row| row.get(0))
                    .optional()?;
                Ok(expires.is_some_and(|expires| expires > now))
            }
        })
    }

    // 기기

    pub fn devices(&self, store: &Store, online: &HashMap<String, usize>) -> ApiResult<Value> {
        let devices = store.read(|db| {
            let mut statement = db.prepare("SELECT * FROM devices ORDER BY created_at,id")?;
            let rows = statement.query_map([], device_row)?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        let devices: Vec<Value> = devices
            .into_iter()
            .map(|mut device| {
                let connected = device["id"].as_str().is_some_and(|id| online.get(id).copied().unwrap_or(0) > 0);
                device["connected"] = Value::Bool(connected);
                device
            })
            .collect();
        Ok(json!({"devices": devices}))
    }

    pub fn rename_device(&self, store: &Store, device_id: &str, name: &str) -> ApiResult<Value> {
        let name = name.trim();
        if name.is_empty() {
            return Err(ApiError::invalid("기기 이름을 입력해 주세요."));
        }
        store.write(|db| {
            if db.execute("UPDATE devices SET name=?1 WHERE id=?2", params![name, device_id])? == 0 {
                return Err(not_found("device_not_found", "연결 기기를 찾을 수 없습니다."));
            }
            Ok(db.query_row("SELECT * FROM devices WHERE id=?1", [device_id], device_row)?)
        })
    }

    pub fn revoke_device(&self, store: &Store, device_id: &str) -> ApiResult<()> {
        store.write(|db| {
            if db.execute("DELETE FROM devices WHERE id=?1", [device_id])? == 0 {
                return Err(not_found("device_not_found", "연결 기기를 찾을 수 없습니다."));
            }
            Ok(())
        })
    }

    // 연결 코드

    pub fn create_pairing(&self, store: &Store, name: &str) -> ApiResult<Value> {
        let code = new_code();
        let now = store.clock.now();
        let entry = json!({"id": new_id(), "name": name.trim(), "created_at": now, "expires_at": now + PAIRING_SECONDS});
        store.write(|db| {
            db.execute(
                "INSERT INTO pairing_codes VALUES(?1,?2,?3,'device','host',?4,?5,NULL,NULL)",
                params![entry["id"].as_str(), self.hash("pair", &normalize_code(&code)), name.trim(), now,
                        now + PAIRING_SECONDS],
            )?;
            Ok(())
        })?;
        let mut result = entry;
        result["code"] = Value::String(code);
        Ok(result)
    }

    pub fn pairings(&self, store: &Store) -> ApiResult<Value> {
        let now = store.clock.now();
        store.read(|db| {
            let mut statement = db.prepare(
                "SELECT id,name,created_at,expires_at FROM pairing_codes
                 WHERE used_at IS NULL AND expires_at>?1 ORDER BY created_at",
            )?;
            let entries = statement
                .query_map([now], |row| {
                    Ok(json!({
                        "id": row.get::<_, String>(0)?, "name": row.get::<_, String>(1)?,
                        "created_at": row.get::<_, f64>(2)?, "expires_at": row.get::<_, f64>(3)?,
                    }))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(json!({"entries": entries}))
        })
    }

    pub fn cancel_pairing(&self, store: &Store, entry_id: &str) -> ApiResult<()> {
        store.write(|db| {
            if db.execute("DELETE FROM pairing_codes WHERE id=?1 AND used_at IS NULL", [entry_id])? == 0 {
                return Err(not_found("pairing_code_not_found", "취소할 연결 코드가 없습니다."));
            }
            Ok(())
        })
    }

    pub fn pair(&self, store: &Store, code: &str, name: &str, platform: &str) -> ApiResult<Value> {
        let token = random_token("lofd_");
        let now = store.clock.now();
        let hashed = self.hash("pair", &normalize_code(code));
        let device = store.write(|db| {
            let row: Option<(String, String, Option<f64>, f64)> = db
                .query_row(
                    "SELECT id,name,used_at,expires_at FROM pairing_codes WHERE code_hash=?1",
                    [&hashed],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()?;
            let Some((entry_id, entry_name, used_at, expires_at)) = row else {
                return Err(not_found("pairing_code_invalid", "연결 코드를 확인해 주세요."));
            };
            if used_at.is_some() {
                return Err(ApiError::new(410, "pairing_code_used", "이미 사용한 연결 코드입니다. 서버에서 새 코드를 발급해 주세요."));
            }
            if expires_at <= now {
                return Err(ApiError::new(410, "pairing_code_expired", "연결 코드의 유효 시간이 지났습니다. 서버에서 새 코드를 발급해 주세요."));
            }
            let device_name = [name.trim(), entry_name.as_str()]
                .into_iter()
                .find(|value| !value.is_empty())
                .unwrap_or("새 기기")
                .to_string();
            let device = json!({
                "id": new_id(), "name": device_name, "platform": platform.trim(),
                "created_at": now, "last_seen_at": now,
            });
            db.execute(
                "INSERT INTO devices VALUES(?1,?2,'device',?3,?4,?5,?6)",
                params![device["id"].as_str(), device_name, platform.trim(), self.hash("device", &token), now, now],
            )?;
            db.execute(
                "UPDATE pairing_codes SET used_at=?1, device_id=?2 WHERE id=?3",
                params![now, device["id"].as_str(), entry_id],
            )?;
            Ok(device)
        })?;
        Ok(json!({"device": device, "token": token, "server": self.identity()}))
    }

    pub fn maintain(&self, store: &Store) -> ApiResult<usize> {
        let now = store.clock.now();
        store.write(|db| {
            let codes = db.execute(
                "DELETE FROM pairing_codes WHERE coalesce(used_at, expires_at)<?1",
                [now - KEEP_USED_SECONDS],
            )?;
            Ok(codes + db.execute("DELETE FROM sessions WHERE expires_at<=?1", [now])?)
        })
    }
}
