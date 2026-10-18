use rusqlite::{params, Connection, Row};
use serde_json::{json, Value};

use crate::error::{ApiError, ApiResult};
use crate::models::{HistoryPolicy, Id, LibraryCreate, PolicyUpdate, Validate};
use crate::store::{digest, library_from_row, new_id, Store};
use crate::AppState;

const SUMMARY: &str = "SELECT l.*,
    (SELECT count(*) FROM documents WHERE library_id=l.id AND deleted=0) AS documents_count,
    (SELECT count(*) FROM documents WHERE library_id=l.id AND deleted<>0) AS deleted_count,
    (SELECT count(*) FROM assets WHERE library_id=l.id) AS assets_count,
    (SELECT coalesce(sum(length(data)),0) FROM assets WHERE library_id=l.id) AS assets_bytes,
    (SELECT count(*) FROM histories WHERE library_id=l.id) AS histories_count,
    (SELECT count(*) FROM conflicts WHERE library_id=l.id AND resolved_at IS NULL) AS conflicts_count,
    (SELECT count(*) FROM conflicts WHERE library_id=l.id) AS conflicts_total,
    (SELECT coalesce(max(id),0) FROM changes WHERE library_id=l.id) AS cursor
    FROM libraries l";

fn summary(row: &Row<'_>) -> rusqlite::Result<Value> {
    let mut library = library_from_row(row)?;
    library["counts"] = json!({
        "documents": row.get::<_, i64>("documents_count")?,
        "deleted_documents": row.get::<_, i64>("deleted_count")?,
        "assets": row.get::<_, i64>("assets_count")?,
        "assets_bytes": row.get::<_, i64>("assets_bytes")?,
        "histories": row.get::<_, i64>("histories_count")?,
        "conflicts": row.get::<_, i64>("conflicts_count")?,
        "conflicts_total": row.get::<_, i64>("conflicts_total")?,
    });
    library["cursor"] = json!(row.get::<_, i64>("cursor")?);
    library["revision"] = json!(digest(&library));
    Ok(library)
}

fn detail(db: &Connection, id: &str) -> ApiResult<Value> {
    Store::require_library(db, id)?;
    Ok(db.query_row(&format!("{SUMMARY} WHERE l.id=?1"), [id], summary)?)
}

impl AppState {
    /// 설치형 앱이 호스트에서 직접 호출한다. 일반 기기에 관리 API를 열지 않는다.
    pub async fn managed_libraries(&self) -> ApiResult<Value> {
        self.blocking(|inner| {
            inner.store.read(|db| {
                let mut statement = db.prepare(&format!("{SUMMARY} ORDER BY l.created_at,l.id"))?;
                let libraries = statement
                    .query_map([], summary)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(json!({ "libraries": libraries }))
            })
        })
        .await
    }

    pub async fn managed_library_create(&self, name: String) -> ApiResult<Value> {
        let request = LibraryCreate {
            id: Id::parse(&new_id())?,
            name: name.trim().to_string(),
            history: HistoryPolicy::default(),
        };
        request.validate().map_err(ApiError::invalid)?;
        self.write(move |inner| inner.store.create_library(&request))
            .await
    }

    pub async fn managed_library_rename(
        &self,
        id: String,
        name: String,
        expected_version: i64,
    ) -> ApiResult<Value> {
        let id = Id::parse(&id)?;
        let name = name.trim().to_string();
        if name.is_empty() || name.chars().count() > 200 {
            return Err(ApiError::invalid("서재 이름은 1~200자로 입력해 주세요."));
        }
        self.write(move |inner| inner.store.write(|db| {
            let current = Store::require_library(db, &id)?;
            if current["settings_version"].as_i64() != Some(expected_version) {
                return Err(ApiError::new(409, "settings_conflict", "서재 설정이 바뀌었습니다. 목록을 새로 확인한 뒤 다시 시도해 주세요."));
            }
            if current["name"].as_str() != Some(name.as_str()) {
                db.execute("UPDATE libraries SET name=?1,settings_version=settings_version+1 WHERE id=?2", params![name, id.as_str()])?;
                inner.store.event(db, &id, "library.updated", json!({ "name": name }))?;
            }
            Store::require_library(db, &id)
        })).await
    }

    pub async fn managed_library_policy(
        &self,
        id: String,
        request: PolicyUpdate,
    ) -> ApiResult<Value> {
        let id = Id::parse(&id)?;
        request.validate().map_err(ApiError::invalid)?;
        self.write(move |inner| inner.store.update_settings(&id, &request))
            .await
    }

    pub async fn managed_library_delete(
        &self,
        id: String,
        name: String,
        expected_revision: String,
    ) -> ApiResult<()> {
        let id = Id::parse(&id)?;
        self.write(move |inner| {
            inner.store.write(|db| {
                let current = detail(db, &id)?;
                if current["name"].as_str() != Some(name.as_str()) {
                    return Err(ApiError::invalid(
                        "삭제할 서재 이름을 정확히 입력해 주세요.",
                    ));
                }
                if current["revision"].as_str() != Some(expected_revision.as_str()) {
                    return Err(ApiError::new(
                        409,
                        "library_changed",
                        "서재 내용이 바뀌었습니다. 삭제 범위를 다시 확인해 주세요.",
                    ));
                }
                // 확인한 서재 ID에 속한 자료만 자식 테이블부터 한 트랜잭션에서 삭제한다.
                for table in [
                    "history_state",
                    "histories",
                    "conflicts",
                    "mutations",
                    "assets",
                    "changes",
                    "documents",
                ] {
                    db.execute(
                        &format!("DELETE FROM {table} WHERE library_id=?1"),
                        [id.as_str()],
                    )?;
                }
                db.execute("DELETE FROM libraries WHERE id=?1", [id.as_str()])?;
                Ok(())
            })
        })
        .await
    }
}
