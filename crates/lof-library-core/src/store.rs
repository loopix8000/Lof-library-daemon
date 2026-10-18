use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension, Row, TransactionBehavior};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::error::{ApiError, ApiResult, StartError};
use crate::history;
use crate::models::{DocumentMutation, Id, LibraryCreate, PolicyUpdate, Resolve};

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS libraries (
        id TEXT PRIMARY KEY, name TEXT NOT NULL, policy TEXT NOT NULL,
        settings_version INTEGER NOT NULL, created_at REAL NOT NULL
    );
    CREATE TABLE IF NOT EXISTS documents (
        library_id TEXT NOT NULL REFERENCES libraries(id), id TEXT NOT NULL,
        kind TEXT NOT NULL, title TEXT NOT NULL, body TEXT NOT NULL,
        metadata TEXT NOT NULL, deleted INTEGER NOT NULL,
        version INTEGER NOT NULL, content_hash TEXT NOT NULL,
        device_id TEXT NOT NULL, updated_at REAL NOT NULL,
        PRIMARY KEY (library_id, id)
    );
    CREATE TABLE IF NOT EXISTS changes (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        library_id TEXT NOT NULL REFERENCES libraries(id),
        type TEXT NOT NULL, payload TEXT NOT NULL, created_at REAL NOT NULL
    );
    CREATE INDEX IF NOT EXISTS changes_library ON changes(library_id, id);
    CREATE TABLE IF NOT EXISTS mutations (
        library_id TEXT NOT NULL REFERENCES libraries(id),
        device_id TEXT NOT NULL, mutation_id TEXT NOT NULL,
        fingerprint TEXT NOT NULL, status INTEGER NOT NULL, response TEXT NOT NULL,
        PRIMARY KEY (library_id, device_id, mutation_id)
    );
    CREATE TABLE IF NOT EXISTS conflicts (
        id TEXT PRIMARY KEY, library_id TEXT NOT NULL, document_id TEXT NOT NULL,
        base_version INTEGER NOT NULL, server_version INTEGER NOT NULL,
        device_id TEXT NOT NULL, incoming TEXT NOT NULL, created_at REAL NOT NULL,
        resolved_at REAL,
        FOREIGN KEY (library_id, document_id) REFERENCES documents(library_id, id)
    );
    CREATE INDEX IF NOT EXISTS conflicts_library ON conflicts(library_id, resolved_at);
    CREATE TABLE IF NOT EXISTS assets (
        library_id TEXT NOT NULL REFERENCES libraries(id), sha256 TEXT NOT NULL,
        media_type TEXT NOT NULL, data BLOB NOT NULL, created_at REAL NOT NULL,
        PRIMARY KEY (library_id, sha256)
    );
    CREATE TABLE IF NOT EXISTS histories (
        seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT UNIQUE NOT NULL,
        library_id TEXT NOT NULL, document_id TEXT NOT NULL,
        document_version INTEGER NOT NULL, content_hash TEXT NOT NULL,
        snapshot TEXT NOT NULL, reason TEXT NOT NULL, protected INTEGER NOT NULL,
        created_at REAL NOT NULL,
        FOREIGN KEY (library_id, document_id) REFERENCES documents(library_id, id)
    );
    CREATE INDEX IF NOT EXISTS history_document ON histories(library_id, document_id, seq);
    CREATE TABLE IF NOT EXISTS history_state (
        library_id TEXT NOT NULL, document_id TEXT NOT NULL,
        snapshot TEXT NOT NULL, content_hash TEXT NOT NULL, recorded_at REAL NOT NULL,
        checked_version INTEGER NOT NULL, checked_policy INTEGER NOT NULL,
        changed_chars INTEGER NOT NULL,
        PRIMARY KEY (library_id, document_id),
        FOREIGN KEY (library_id, document_id) REFERENCES documents(library_id, id)
    );
    CREATE TABLE IF NOT EXISTS server (key TEXT PRIMARY KEY, value TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS devices (
        id TEXT PRIMARY KEY, name TEXT NOT NULL, role TEXT NOT NULL, platform TEXT NOT NULL,
        token_hash TEXT UNIQUE NOT NULL, created_at REAL NOT NULL, last_seen_at REAL
    );
    CREATE TABLE IF NOT EXISTS pairing_codes (
        id TEXT PRIMARY KEY, code_hash TEXT UNIQUE NOT NULL, name TEXT NOT NULL, role TEXT NOT NULL,
        created_by TEXT NOT NULL, created_at REAL NOT NULL, expires_at REAL NOT NULL,
        used_at REAL, device_id TEXT
    );
    CREATE TABLE IF NOT EXISTS sessions (
        token_hash TEXT PRIMARY KEY, created_at REAL NOT NULL, expires_at REAL NOT NULL
    );
    PRAGMA user_version=2;
";

/// UTC Unix 초 단위 시계. 검증 코드에서는 시각을 고정하고 앞당길 수 있다.
#[derive(Clone, Default)]
pub struct Clock(Arc<Mutex<Option<f64>>>);

impl Clock {
    pub fn now(&self) -> f64 {
        if let Some(fixed) = *self.0.lock().unwrap_or_else(|error| error.into_inner()) {
            return fixed;
        }
        SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs_f64()).unwrap_or(0.0)
    }

    pub fn set(&self, value: f64) {
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) = Some(value);
    }

    pub fn advance(&self, seconds: f64) {
        let now = self.now();
        self.set(now + seconds);
    }
}

pub fn encode(value: &Value) -> String {
    // serde_json의 기본 Map은 키를 정렬하므로 같은 내용은 항상 같은 문자열과 해시가 된다.
    serde_json::to_string(value).unwrap_or_default()
}

pub fn digest(value: &Value) -> String {
    hex::encode(Sha256::digest(encode(value).as_bytes()))
}

pub fn content_of(document: &Value) -> Value {
    json!({
        "kind": document["kind"], "title": document["title"], "body": document["body"],
        "metadata": document["metadata"], "deleted": document["deleted"],
    })
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

pub fn not_found(code: &str, message: &str) -> ApiError {
    ApiError::new(404, code, message)
}

pub fn document_from_row(row: &Row) -> rusqlite::Result<Value> {
    let metadata: String = row.get("metadata")?;
    Ok(json!({
        "id": row.get::<_, String>("id")?,
        "library_id": row.get::<_, String>("library_id")?,
        "kind": row.get::<_, String>("kind")?,
        "title": row.get::<_, String>("title")?,
        "body": row.get::<_, String>("body")?,
        "metadata": serde_json::from_str::<Value>(&metadata).unwrap_or_else(|_| json!({})),
        "deleted": row.get::<_, i64>("deleted")? != 0,
        "version": row.get::<_, i64>("version")?,
        "sha256": row.get::<_, String>("content_hash")?,
        "device_id": row.get::<_, String>("device_id")?,
        "updated_at": row.get::<_, f64>("updated_at")?,
    }))
}

fn library_from_row(row: &Row) -> rusqlite::Result<Value> {
    let policy: String = row.get("policy")?;
    Ok(json!({
        "id": row.get::<_, String>("id")?,
        "name": row.get::<_, String>("name")?,
        "history": serde_json::from_str::<Value>(&policy).unwrap_or(Value::Null),
        "settings_version": row.get::<_, i64>("settings_version")?,
        "created_at": row.get::<_, f64>("created_at")?,
    }))
}

pub struct Store {
    connection: Mutex<Connection>,
    pub path: PathBuf,
    pub clock: Clock,
}

impl Store {
    pub fn open(directory: &Path, clock: Clock) -> Result<Self, StartError> {
        std::fs::create_dir_all(directory)?;
        let path = directory.join("library.sqlite");
        let connection = Connection::open(&path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
        connection.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if !(0..=2).contains(&version) {
            return Err(StartError("이 서버에서 지원하지 않는 데이터베이스 버전입니다.".into()));
        }
        connection.execute_batch(SCHEMA)?;
        Ok(Self { connection: Mutex::new(connection), path, clock })
    }

    fn transaction<T>(&self, behavior: TransactionBehavior, action: impl FnOnce(&Connection) -> ApiResult<T>) -> ApiResult<T> {
        let mut connection = self.connection.lock().unwrap_or_else(|error| error.into_inner());
        let transaction = connection.transaction_with_behavior(behavior)?;
        let result = action(&transaction)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn read<T>(&self, action: impl FnOnce(&Connection) -> ApiResult<T>) -> ApiResult<T> {
        self.transaction(TransactionBehavior::Deferred, action)
    }

    pub fn write<T>(&self, action: impl FnOnce(&Connection) -> ApiResult<T>) -> ApiResult<T> {
        self.transaction(TransactionBehavior::Immediate, action)
    }

    pub fn healthy(&self) -> bool {
        self.read(|db| Ok(db.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))? == 1)).unwrap_or(false)
    }

    pub fn require_library(db: &Connection, library_id: &str) -> ApiResult<Value> {
        db.query_row("SELECT * FROM libraries WHERE id=?1", [library_id], library_from_row)
            .optional()?
            .ok_or_else(|| not_found("library_not_found", "서재를 찾을 수 없습니다."))
    }

    pub fn document_row(db: &Connection, library_id: &str, document_id: &str) -> ApiResult<Option<Value>> {
        Ok(db
            .query_row("SELECT * FROM documents WHERE library_id=?1 AND id=?2", [library_id, document_id], document_from_row)
            .optional()?)
    }

    pub fn event(&self, db: &Connection, library_id: &str, kind: &str, payload: Value) -> ApiResult<()> {
        db.execute(
            "INSERT INTO changes(library_id,type,payload,created_at) VALUES(?1,?2,?3,?4)",
            params![library_id, kind, encode(&payload), self.clock.now()],
        )?;
        Ok(())
    }

    pub fn libraries(&self) -> ApiResult<Vec<Value>> {
        self.read(|db| {
            let mut statement = db.prepare("SELECT * FROM libraries ORDER BY id")?;
            let rows = statement.query_map([], library_from_row)?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    pub fn create_library(&self, request: &LibraryCreate) -> ApiResult<Value> {
        self.write(|db| {
            if db.query_row("SELECT 1 FROM libraries WHERE id=?1", [request.id.as_str()], |_| Ok(())).optional()?.is_some() {
                return Err(ApiError::new(409, "library_exists", "같은 ID의 서재가 이미 있습니다."));
            }
            let policy = encode(&serde_json::to_value(&request.history)?);
            db.execute(
                "INSERT INTO libraries VALUES(?1,?2,?3,?4,?5)",
                params![request.id.as_str(), request.name, policy, 1, self.clock.now()],
            )?;
            self.event(db, &request.id, "library.created", json!({"id": request.id}))?;
            Self::require_library(db, &request.id)
        })
    }

    pub fn settings(&self, library_id: &str) -> ApiResult<Value> {
        self.read(|db| Self::require_library(db, library_id))
    }

    pub fn update_settings(&self, library_id: &str, request: &PolicyUpdate) -> ApiResult<Value> {
        self.write(|db| {
            let current: (String, i64) = db
                .query_row("SELECT policy,settings_version FROM libraries WHERE id=?1", [library_id], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .optional()?
                .ok_or_else(|| not_found("library_not_found", "서재를 찾을 수 없습니다."))?;
            if current.1 != request.expected_version {
                return Err(ApiError::new(409, "settings_conflict", "다른 기기에서 설정을 바꿨습니다.")
                    .with("current_version", current.1));
            }
            let policy = encode(&serde_json::to_value(&request.history)?);
            if current.0 != policy {
                db.execute(
                    "UPDATE libraries SET policy=?1, settings_version=settings_version+1 WHERE id=?2",
                    params![policy, library_id],
                )?;
                self.event(db, library_id, "settings.updated", json!({"settings_version": current.1 + 1}))?;
            }
            Self::require_library(db, library_id)
        })
    }

    pub fn snapshot(&self, library_id: &str) -> ApiResult<Value> {
        // 목록과 커서를 한 읽기 트랜잭션에서 얻어 재연결 중 변경을 놓치지 않는다.
        self.read(|db| {
            let library = Self::require_library(db, library_id)?;
            let mut statement = db.prepare("SELECT * FROM documents WHERE library_id=?1 ORDER BY id")?;
            let documents = statement
                .query_map([library_id], |row| {
                    let mut document = document_from_row(row)?;
                    if let Some(object) = document.as_object_mut() {
                        object.remove("body");
                    }
                    Ok(document)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let mut statement = db.prepare(
                "SELECT sha256,media_type,length(data),created_at FROM assets WHERE library_id=?1 ORDER BY sha256",
            )?;
            let assets = statement
                .query_map([library_id], |row| {
                    Ok(json!({
                        "sha256": row.get::<_, String>(0)?, "media_type": row.get::<_, String>(1)?,
                        "size": row.get::<_, i64>(2)?, "created_at": row.get::<_, f64>(3)?,
                    }))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let cursor: i64 = db.query_row("SELECT coalesce(max(id),0) FROM changes", [], |row| row.get(0))?;
            Ok(json!({"library": library, "documents": documents, "assets": assets, "cursor": cursor}))
        })
    }

    pub fn read_document(&self, library_id: &str, document_id: &str) -> ApiResult<Value> {
        self.read(|db| {
            Self::require_library(db, library_id)?;
            Self::document_row(db, library_id, document_id)?
                .ok_or_else(|| not_found("document_not_found", "문서를 찾을 수 없습니다."))
        })
    }

    /// 같은 기기·요청 ID의 재전송에는 처음 처리한 결과를 그대로 돌려준다.
    #[allow(clippy::too_many_arguments)]
    pub fn once(
        &self,
        db: &Connection,
        library_id: &str,
        device_id: &Id,
        mutation_id: &Id,
        request: Value,
        operation: &str,
        action: impl FnOnce() -> ApiResult<(u16, Value)>,
    ) -> ApiResult<(u16, Value)> {
        let fingerprint = digest(&json!({"operation": operation, "request": request}));
        let previous: Option<(String, u16, String)> = db
            .query_row(
                "SELECT fingerprint,status,response FROM mutations WHERE library_id=?1 AND device_id=?2 AND mutation_id=?3",
                [library_id, device_id.as_str(), mutation_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((saved, status, response)) = previous {
            if saved != fingerprint {
                return Err(ApiError::new(409, "mutation_reused", "이미 사용한 mutation_id를 다른 요청에 사용할 수 없습니다."));
            }
            return Ok((status, serde_json::from_str(&response)?));
        }
        let (status, mut response) = action()?;
        if let Some(document) = response.get("document").cloned() {
            // 재전송 영수증에 원고 사본을 쌓지 않아 이력 생성·보관 규칙을 우회하지 않는다.
            let receipt: Map<String, Value> = ["id", "library_id", "version", "sha256", "deleted"]
                .iter()
                .map(|key| (key.to_string(), document[*key].clone()))
                .collect();
            response["document"] = Value::Object(receipt);
        }
        db.execute(
            "INSERT INTO mutations VALUES(?1,?2,?3,?4,?5,?6)",
            params![library_id, device_id.as_str(), mutation_id.as_str(), fingerprint, status, encode(&response)],
        )?;
        Ok((status, response))
    }

    pub fn apply_document(
        &self,
        db: &Connection,
        library_id: &str,
        document_id: &str,
        request: &DocumentMutation,
    ) -> ApiResult<(u16, Value)> {
        let current = Self::document_row(db, library_id, document_id)?;
        let mut content = request.content();
        if request.deleted {
            let Some(existing) = &current else {
                return Err(not_found("document_not_found", "삭제할 문서를 찾을 수 없습니다."));
            };
            content = content_of(existing);
            content["deleted"] = Value::Bool(true);
        }
        let content_hash = digest(&content);
        // 이미 같은 상태이면 오래된 응답의 재전송이어도 새 버전을 만들지 않는다.
        if let Some(existing) = &current {
            if existing["sha256"] == content_hash {
                return Ok((200, json!({"document": existing, "changed": false})));
            }
        }
        let version = current.as_ref().and_then(|existing| existing["version"].as_i64()).unwrap_or(0);
        let now = self.clock.now();
        if version != request.expected_version {
            if current.is_none() {
                return Err(ApiError::new(409, "document_not_found", "서버에 문서가 없습니다. 새 문서로 등록해 주세요."));
            }
            let conflict_id = new_id();
            db.execute(
                "INSERT INTO conflicts VALUES(?1,?2,?3,?4,?5,?6,?7,?8,NULL)",
                params![conflict_id, library_id, document_id, request.expected_version, version,
                        request.device_id.as_str(), encode(&request.content()), now],
            )?;
            self.event(db, library_id, "conflict.created", json!({"id": conflict_id, "document_id": document_id}))?;
            return Ok((409, json!({
                "error": "revision_conflict",
                "message": "서버의 다른 변경이 있습니다. 입력 내용은 충돌 사본으로 보존했습니다.",
                "current_version": version, "conflict_id": conflict_id,
            })));
        }
        db.execute(
            "INSERT INTO documents VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
             ON CONFLICT(library_id,id) DO UPDATE SET
             kind=excluded.kind,title=excluded.title,body=excluded.body,metadata=excluded.metadata,
             deleted=excluded.deleted,version=excluded.version,content_hash=excluded.content_hash,
             device_id=excluded.device_id,updated_at=excluded.updated_at",
            params![library_id, document_id, content["kind"].as_str().unwrap_or("episode"),
                    content["title"].as_str().unwrap_or(""), content["body"].as_str().unwrap_or(""),
                    encode(&content["metadata"]), content["deleted"].as_bool().unwrap_or(false),
                    version + 1, content_hash, request.device_id.as_str(), now],
        )?;
        let document = Self::document_row(db, library_id, document_id)?.ok_or_else(ApiError::internal)?;
        let deleted = document["deleted"].as_bool().unwrap_or(false);
        self.event(
            db,
            library_id,
            if deleted { "document.deleted" } else { "document.updated" },
            json!({"id": document_id, "version": document["version"], "sha256": content_hash}),
        )?;
        if current.is_none() {
            let (entry, _, _) = history::capture(db, &document, "initial", now)?;
            self.event(db, library_id, "history.created", json!({"id": entry["id"], "document_id": document_id}))?;
        }
        Ok((if current.is_none() { 201 } else { 200 }, json!({"document": document, "changed": true})))
    }

    pub fn mutate_document(&self, library_id: &str, document_id: &str, request: &DocumentMutation) -> ApiResult<(u16, Value)> {
        self.write(|db| {
            Self::require_library(db, library_id)?;
            self.once(
                db,
                library_id,
                &request.device_id,
                &request.mutation_id,
                serde_json::to_value(request)?,
                &format!("document:{document_id}"),
                || self.apply_document(db, library_id, document_id, request),
            )
        })
    }

    pub fn changes(&self, library_id: &str, after: i64, limit: i64) -> ApiResult<Value> {
        self.read(|db| {
            Self::require_library(db, library_id)?;
            let maximum: i64 = db.query_row("SELECT coalesce(max(id),0) FROM changes", [], |row| row.get(0))?;
            if after > maximum {
                return Err(ApiError::new(409, "cursor_ahead", "서버의 현재 위치보다 앞선 커서입니다. 서재 목록을 다시 받아 주세요."));
            }
            let mut statement =
                db.prepare("SELECT id,type,payload,created_at FROM changes WHERE library_id=?1 AND id>?2 ORDER BY id LIMIT ?3")?;
            let mut events = statement
                .query_map(params![library_id, after, limit + 1], |row| {
                    let payload: String = row.get(2)?;
                    Ok(json!({
                        "id": row.get::<_, i64>(0)?, "type": row.get::<_, String>(1)?,
                        "data": serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null),
                        "created_at": row.get::<_, f64>(3)?,
                    }))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let more = events.len() as i64 > limit;
            events.truncate(limit as usize);
            let cursor = if more { events.last().map(|event| event["id"].clone()).unwrap_or(json!(after)) } else { json!(maximum) };
            Ok(json!({"events": events, "cursor": cursor, "has_more": more}))
        })
    }

    pub fn conflicts(&self, library_id: &str, after: &str, limit: i64) -> ApiResult<Value> {
        self.read(|db| {
            Self::require_library(db, library_id)?;
            let mut statement = db.prepare(
                "SELECT id,document_id,base_version,server_version,device_id,created_at
                 FROM conflicts WHERE library_id=?1 AND resolved_at IS NULL AND id>?2 ORDER BY id LIMIT ?3",
            )?;
            let mut entries = statement
                .query_map(params![library_id, after, limit + 1], |row| {
                    Ok(json!({
                        "id": row.get::<_, String>(0)?, "document_id": row.get::<_, String>(1)?,
                        "base_version": row.get::<_, i64>(2)?, "server_version": row.get::<_, i64>(3)?,
                        "device_id": row.get::<_, String>(4)?, "created_at": row.get::<_, f64>(5)?,
                    }))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let more = entries.len() as i64 > limit;
            entries.truncate(limit as usize);
            Ok(json!({"entries": entries, "has_more": more}))
        })
    }

    fn conflict_row(db: &Connection, library_id: &str, conflict_id: &str) -> ApiResult<Option<Value>> {
        Ok(db
            .query_row("SELECT * FROM conflicts WHERE library_id=?1 AND id=?2", [library_id, conflict_id], |row| {
                let incoming: String = row.get("incoming")?;
                Ok(json!({
                    "id": row.get::<_, String>("id")?, "library_id": row.get::<_, String>("library_id")?,
                    "document_id": row.get::<_, String>("document_id")?,
                    "base_version": row.get::<_, i64>("base_version")?,
                    "server_version": row.get::<_, i64>("server_version")?,
                    "device_id": row.get::<_, String>("device_id")?,
                    "incoming": serde_json::from_str::<Value>(&incoming).unwrap_or(Value::Null),
                    "created_at": row.get::<_, f64>("created_at")?,
                    "resolved_at": row.get::<_, Option<f64>>("resolved_at")?,
                }))
            })
            .optional()?)
    }

    pub fn read_conflict(&self, library_id: &str, conflict_id: &str) -> ApiResult<Value> {
        self.read(|db| {
            Self::require_library(db, library_id)?;
            Self::conflict_row(db, library_id, conflict_id)?
                .ok_or_else(|| not_found("conflict_not_found", "충돌 사본을 찾을 수 없습니다."))
        })
    }

    pub fn resolve_conflict(&self, library_id: &str, conflict_id: &str, request: &Resolve) -> ApiResult<(u16, Value)> {
        self.write(|db| {
            Self::require_library(db, library_id)?;
            let action = || {
                let conflict = Self::conflict_row(db, library_id, conflict_id)?
                    .filter(|conflict| conflict["resolved_at"].is_null())
                    .ok_or_else(|| not_found("conflict_not_found", "해결할 충돌 사본이 없습니다."))?;
                let document_id = conflict["document_id"].as_str().unwrap_or_default();
                let current = Self::document_row(db, library_id, document_id)?
                    .ok_or_else(|| not_found("document_not_found", "문서를 찾을 수 없습니다."))?;
                if current["version"] != request.expected_version {
                    return Err(ApiError::new(409, "revision_conflict", "서버 원고가 다시 바뀌었습니다.")
                        .with("current_version", current["version"].clone()));
                }
                let mut result = json!({"document": current, "changed": false});
                if request.choice == "incoming" {
                    let mutation = DocumentMutation::from_content(
                        &conflict["incoming"], &request.device_id, &request.mutation_id, request.expected_version,
                    )?;
                    result = self.apply_document(db, library_id, document_id, &mutation)?.1;
                }
                db.execute("UPDATE conflicts SET resolved_at=?1 WHERE id=?2", params![self.clock.now(), conflict_id])?;
                self.event(db, library_id, "conflict.resolved", json!({"id": conflict_id}))?;
                Ok((200, result))
            };
            self.once(
                db,
                library_id,
                &request.device_id,
                &request.mutation_id,
                serde_json::to_value(request)?,
                &format!("resolve:{conflict_id}"),
                action,
            )
        })
    }

    pub fn put_asset(&self, library_id: &str, sha256: &str, media_type: &str, data: &[u8]) -> ApiResult<Value> {
        if hex::encode(Sha256::digest(data)) != sha256 {
            return Err(ApiError::new(422, "hash_mismatch", "첨부 자료의 SHA-256이 주소와 다릅니다."));
        }
        self.write(|db| {
            Self::require_library(db, library_id)?;
            let exists = db
                .query_row("SELECT 1 FROM assets WHERE library_id=?1 AND sha256=?2", [library_id, sha256], |_| Ok(()))
                .optional()?
                .is_some();
            if !exists {
                db.execute(
                    "INSERT INTO assets VALUES(?1,?2,?3,?4,?5)",
                    params![library_id, sha256, media_type, data, self.clock.now()],
                )?;
                self.event(db, library_id, "asset.created", json!({"sha256": sha256, "size": data.len()}))?;
            }
            Ok(json!({"sha256": sha256, "size": data.len(), "created": !exists}))
        })
    }

    pub fn read_asset(&self, library_id: &str, sha256: &str) -> ApiResult<(Vec<u8>, String)> {
        self.read(|db| {
            Self::require_library(db, library_id)?;
            db.query_row(
                "SELECT data,media_type FROM assets WHERE library_id=?1 AND sha256=?2",
                [library_id, sha256],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| not_found("asset_not_found", "첨부 자료를 찾을 수 없습니다."))
        })
    }

    /// 데이터베이스와 WAL 파일을 합친 저장 공간 사용량
    pub fn storage_bytes(&self) -> u64 {
        let Some(directory) = self.path.parent() else { return 0 };
        let Ok(entries) = std::fs::read_dir(directory) else { return 0 };
        entries
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("library.sqlite"))
            .filter_map(|entry| entry.metadata().ok())
            .filter(|metadata| metadata.is_file())
            .map(|metadata| metadata.len())
            .sum()
    }
}
