use std::collections::HashSet;

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::{json, Value};

use crate::error::{ApiError, ApiResult};
use crate::models::{Capture, DocumentMutation, HistoryPolicy, Restore};
use crate::store::{content_of, document_from_row, encode, new_id, not_found, Store};

const PROTECTED_REASONS: [&str; 3] = ["manual", "before_restore", "restore"];
/// 앞뒤 공통 부분을 뺀 비교 구간이 이보다 길면 정밀 비교 대신 전체 교체로 계산한다.
const DIFF_LIMIT: usize = 20_000;

fn entry(row: &Row, full: bool) -> rusqlite::Result<Value> {
    let mut result = json!({
        "id": row.get::<_, String>("id")?,
        "document_id": row.get::<_, String>("document_id")?,
        "document_version": row.get::<_, i64>("document_version")?,
        "reason": row.get::<_, String>("reason")?,
        "created_at": row.get::<_, f64>("created_at")?,
        "protected": row.get::<_, i64>("protected")? != 0,
        "cursor": row.get::<_, i64>("seq")?,
    });
    if full {
        let snapshot: String = row.get("snapshot")?;
        result["snapshot"] = serde_json::from_str(&snapshot).unwrap_or(Value::Null);
    }
    Ok(result)
}

/// 전송 버전과 별도로 보관할 복구 시점만 만든다. 같은 내용은 표지만 추가한다.
pub fn capture(db: &Connection, document: &Value, reason: &str, now: f64) -> ApiResult<(Value, bool, bool)> {
    let library_id = document["library_id"].as_str().unwrap_or_default();
    let document_id = document["id"].as_str().unwrap_or_default();
    let sha256 = document["sha256"].as_str().unwrap_or_default();
    let previous: Option<(String, String, bool, String)> = db
        .query_row(
            "SELECT id,content_hash,protected,reason FROM histories WHERE library_id=?1 AND document_id=?2 ORDER BY seq DESC LIMIT 1",
            [library_id, document_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)? != 0, row.get(3)?)),
        )
        .optional()?;
    let protected = PROTECTED_REASONS.contains(&reason);
    let created = previous.as_ref().is_none_or(|previous| previous.1 != sha256);
    let mut marked = false;
    let history_id = match previous {
        Some((id, _, was_protected, previous_reason)) if !created => {
            if protected && (!was_protected || (reason == "manual" && previous_reason != "manual")) {
                db.execute("UPDATE histories SET protected=1,reason=?1 WHERE id=?2", params![reason, id])?;
                marked = true;
            }
            id
        }
        _ => {
            let id = new_id();
            db.execute(
                "INSERT INTO histories(id,library_id,document_id,document_version,content_hash,snapshot,reason,protected,created_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![id, library_id, document_id, document["version"].as_i64(), sha256, encode(document),
                        reason, protected, now],
            )?;
            id
        }
    };
    let current = db.query_row("SELECT * FROM histories WHERE id=?1", [&history_id], |row| entry(row, true))?;
    db.execute(
        "INSERT INTO history_state VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
         ON CONFLICT(library_id,document_id) DO UPDATE SET snapshot=excluded.snapshot,
         content_hash=excluded.content_hash,recorded_at=excluded.recorded_at,
         checked_version=excluded.checked_version,checked_policy=excluded.checked_policy,changed_chars=0",
        params![library_id, document_id, encode(document), sha256, current["created_at"].as_f64(),
                document["version"].as_i64(), 0, 0],
    )?;
    Ok((current, created, marked))
}

fn common_prefix(left: &str, right: &str) -> usize {
    left.char_indices()
        .zip(right.chars())
        .find(|((_, a), b)| a != b)
        .map(|((index, _), _)| index)
        .unwrap_or_else(|| left.len().min(right.len()))
}

fn common_suffix(left: &str, right: &str) -> usize {
    left.chars()
        .rev()
        .zip(right.chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(char, _)| char.len_utf8())
        .sum()
}

/// 키 입력 횟수나 전체 길이 차이가 아니라 비교 결과의 추가·삭제 글자 수를 센다.
pub fn changed_chars(before: &Value, after: &Value) -> i64 {
    let text = |document: &Value| -> String {
        if document["deleted"].as_bool().unwrap_or(false) {
            String::new()
        } else {
            document["body"].as_str().unwrap_or_default().to_string()
        }
    };
    let (old, new) = (text(before), text(after));
    if old == new {
        return 0;
    }
    let prefix = common_prefix(&old, &new);
    let (old_rest, new_rest) = (&old[prefix..], &new[prefix..]);
    let suffix = common_suffix(old_rest, new_rest);
    let (old_middle, new_middle) = (&old_rest[..old_rest.len() - suffix], &new_rest[..new_rest.len() - suffix]);
    let total = old_middle.chars().count() + new_middle.chars().count();
    if total > DIFF_LIMIT {
        return total as i64;
    }
    dissimilar::diff(old_middle, new_middle)
        .iter()
        .map(|chunk| match chunk {
            dissimilar::Chunk::Equal(_) => 0,
            dissimilar::Chunk::Delete(text) | dissimilar::Chunk::Insert(text) => text.chars().count() as i64,
        })
        .sum()
}

fn parse_policy(raw: &str) -> HistoryPolicy {
    serde_json::from_str(raw).unwrap_or_default()
}

impl Store {
    fn read_history(db: &Connection, library_id: &str, document_id: &str, history_id: &str) -> ApiResult<Value> {
        db.query_row(
            "SELECT * FROM histories WHERE library_id=?1 AND document_id=?2 AND id=?3",
            [library_id, document_id, history_id],
            |row| entry(row, true),
        )
        .optional()?
        .ok_or_else(|| not_found("history_not_found", "이력을 찾을 수 없습니다."))
    }

    fn capture_event(&self, db: &Connection, document: &Value, reason: &str) -> ApiResult<Value> {
        let (result, created, marked) = capture(db, document, reason, self.clock.now())?;
        if created || marked {
            self.event(
                db,
                document["library_id"].as_str().unwrap_or_default(),
                if created { "history.created" } else { "history.marked" },
                json!({"id": result["id"], "document_id": document["id"], "reason": result["reason"]}),
            )?;
        }
        Ok(json!({"entry": result, "created": created, "marked": marked}))
    }

    pub fn history_list(&self, library_id: &str, document_id: &str, before: i64, limit: i64) -> ApiResult<Value> {
        self.read(|db| {
            Self::require_library(db, library_id)?;
            if Self::document_row(db, library_id, document_id)?.is_none() {
                return Err(not_found("document_not_found", "문서를 찾을 수 없습니다."));
            }
            let mut statement = db.prepare(
                "SELECT seq,id,document_id,document_version,reason,protected,created_at
                 FROM histories WHERE library_id=?1 AND document_id=?2 AND seq<?3 ORDER BY seq DESC LIMIT ?4",
            )?;
            let mut entries = statement
                .query_map(params![library_id, document_id, before, limit + 1], |row| entry(row, false))?
                .collect::<Result<Vec<_>, _>>()?;
            let more = entries.len() as i64 > limit;
            entries.truncate(limit as usize);
            Ok(json!({"entries": entries, "has_more": more}))
        })
    }

    pub fn history_read(&self, library_id: &str, document_id: &str, history_id: &str) -> ApiResult<Value> {
        self.read(|db| {
            Self::require_library(db, library_id)?;
            Self::read_history(db, library_id, document_id, history_id)
        })
    }

    pub fn history_manual(&self, library_id: &str, document_id: &str, request: &Capture) -> ApiResult<Value> {
        self.write(|db| {
            Self::require_library(db, library_id)?;
            let document = Self::document_row(db, library_id, document_id)?
                .ok_or_else(|| not_found("document_not_found", "문서를 찾을 수 없습니다."))?;
            if document["version"] != request.expected_version {
                return Err(ApiError::new(409, "revision_conflict", "수동 저장 전에 최신 원고를 확인해 주세요.")
                    .with("current_version", document["version"].clone()));
            }
            self.capture_event(db, &document, "manual")
        })
    }

    pub fn history_restore(&self, library_id: &str, document_id: &str, history_id: &str, request: &Restore) -> ApiResult<(u16, Value)> {
        self.write(|db| {
            Self::require_library(db, library_id)?;
            let action = || {
                let history = Self::read_history(db, library_id, document_id, history_id)?;
                let current = Self::document_row(db, library_id, document_id)?
                    .ok_or_else(|| not_found("document_not_found", "문서를 찾을 수 없습니다."))?;
                if current["version"] != request.expected_version {
                    return Err(ApiError::new(409, "revision_conflict", "복원 전에 최신 원고를 확인해 주세요.")
                        .with("current_version", current["version"].clone()));
                }
                self.capture_event(db, &current, "before_restore")?;
                let mutation = DocumentMutation::from_content(
                    &content_of(&history["snapshot"]), &request.device_id, &request.mutation_id, request.expected_version,
                )?;
                let (status, result) = self.apply_document(db, library_id, document_id, &mutation)?;
                if status == 409 {
                    tracing::error!("복원 중 잠긴 문서의 버전이 바뀌었습니다.");
                    return Err(ApiError::internal());
                }
                self.capture_event(db, &result["document"], "restore")?;
                Ok((200, result))
            };
            self.once(
                db,
                library_id,
                &request.device_id,
                &request.mutation_id,
                serde_json::to_value(request)?,
                &format!("restore:{document_id}:{history_id}"),
                action,
            )
        })
    }

    /// 시간과 변경 글자 수 조건을 모두 만족한 문서의 자동 이력을 만든다.
    pub fn history_tick(&self) -> ApiResult<usize> {
        let now = self.clock.now();
        self.write(|db| {
            // 같은 버전이 글자 수 조건을 만족하지 않았다면 다음 입력까지 다시 비교하지 않는다.
            let mut statement = db.prepare(
                "SELECT d.*,h.snapshot AS baseline,h.checked_version,h.checked_policy,h.changed_chars,
                        l.policy,l.settings_version AS policy_version
                 FROM documents d JOIN history_state h ON h.library_id=d.library_id AND h.document_id=d.id
                 JOIN libraries l ON l.id=d.library_id
                 WHERE d.content_hash<>h.content_hash
                   AND h.recorded_at<=?1 - json_extract(l.policy,'$.min_minutes')*60
                   AND (h.checked_version<>d.version OR h.checked_policy<>l.settings_version
                        OR h.changed_chars>=json_extract(l.policy,'$.min_changed_chars'))
                 ORDER BY h.recorded_at,d.library_id,d.id LIMIT 20",
            )?;
            let rows = statement
                .query_map([now], |row| {
                    Ok((
                        document_from_row(row)?,
                        row.get::<_, String>("baseline")?,
                        row.get::<_, i64>("checked_version")?,
                        row.get::<_, i64>("checked_policy")?,
                        row.get::<_, i64>("changed_chars")?,
                        row.get::<_, String>("policy")?,
                        row.get::<_, i64>("policy_version")?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let mut count = 0;
            for (document, baseline, checked_version, checked_policy, mut changed, policy, policy_version) in rows {
                let policy = parse_policy(&policy);
                let version = document["version"].as_i64().unwrap_or(0);
                if checked_version != version || checked_policy != policy_version {
                    changed = changed_chars(&serde_json::from_str(&baseline)?, &document);
                }
                db.execute(
                    "UPDATE history_state SET checked_version=?1,checked_policy=?2,changed_chars=?3
                     WHERE library_id=?4 AND document_id=?5",
                    params![version, policy_version, changed, document["library_id"].as_str(), document["id"].as_str()],
                )?;
                if changed >= policy.min_changed_chars {
                    self.capture_event(db, &document, "automatic")?;
                    count += 1;
                }
            }
            Ok(count)
        })
    }

    /// 보관 기간과 스마트 정리 규칙을 적용한다. 문서별 최신 이력은 항상 남긴다.
    pub fn history_maintain(&self) -> ApiResult<usize> {
        let now = self.clock.now();
        self.write(|db| {
            let mut statement = db.prepare("SELECT id,policy FROM libraries")?;
            let libraries = statement
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            let mut removed = 0;
            for (library_id, policy) in libraries {
                let policy = parse_policy(&policy);
                if policy.retention_days.is_none() && !policy.smart.enabled {
                    continue;
                }
                let mut statement = db.prepare(
                    "SELECT id,document_id,protected,created_at FROM histories WHERE library_id=?1 ORDER BY seq DESC",
                )?;
                let rows = statement
                    .query_map([&library_id], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)? != 0, row.get::<_, f64>(3)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                let mut latest = HashSet::new();
                let mut buckets = HashSet::new();
                let mut delete = Vec::new();
                for (history_id, document_id, protected, created_at) in rows {
                    let is_latest = latest.insert(document_id.clone());
                    let expired = policy.retention_days.is_some_and(|days| created_at < now - days as f64 * 86400.0);
                    if expired && !is_latest && (!protected || policy.expire_protected) {
                        delete.push(history_id);
                        continue;
                    }
                    if protected || !policy.smart.enabled || created_at >= now - policy.smart.keep_recent_minutes as f64 * 60.0 {
                        continue;
                    }
                    // 모든 기기에 동일한 UTC 고정 구간을 사용하고 구간의 최신 원본 이력을 남긴다.
                    let bucket = (document_id, (created_at / (policy.smart.bucket_minutes as f64 * 60.0)).floor() as i64);
                    if !buckets.insert(bucket) && !is_latest {
                        delete.push(history_id);
                    }
                }
                if !delete.is_empty() {
                    for history_id in &delete {
                        db.execute("DELETE FROM histories WHERE id=?1", [history_id])?;
                    }
                    self.event(db, &library_id, "history.pruned", json!({"removed": delete.len()}))?;
                    removed += delete.len();
                }
            }
            Ok(removed)
        })
    }
}
