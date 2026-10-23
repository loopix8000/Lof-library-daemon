//! 파일 목록은 이전 목록과의 차이만 교환하고 요약값으로 복원 결과를 확인한다.
use crate::{request, storage, Connection, Message, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Mutex,
};

pub const FORMAT: &str = "inventory-delta-v1";
const MAX_CACHED_SCOPES: usize = 64;

// Writer와 같은 필드 순서와 기본값으로 JSON을 직렬화한다.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct Info {
    size: u64,
    modified: Option<i64>,
    etag: Option<String>,
    #[serde(default)]
    unavailable: bool,
}
type Files = BTreeMap<String, Info>;
#[derive(Hash, PartialEq, Eq)]
struct Scope {
    peer: String,
    local: String,
    remote: String,
}
struct Snapshot {
    digest: String,
    files: Files,
}
impl Snapshot {
    fn new(files: Files) -> Result<Self> {
        let digest = storage::hash(&serde_json::to_vec(&files).map_err(|e| e.to_string())?);
        Ok(Self { digest, files })
    }
}

#[derive(Default)]
pub struct Cache {
    sent: Mutex<HashMap<Scope, Snapshot>>,
    received: Mutex<HashMap<Scope, Snapshot>>,
}
fn remember(cache: &mut HashMap<Scope, Snapshot>, scope: Scope, snapshot: Snapshot) {
    if !cache.contains_key(&scope) && cache.len() >= MAX_CACHED_SCOPES {
        cache.clear();
    }
    cache.insert(scope, snapshot);
}
impl Cache {
    pub fn forget_library(&self, library: &str) {
        self.sent
            .lock()
            .unwrap()
            .retain(|scope, _| scope.local != library);
        self.received
            .lock()
            .unwrap()
            .retain(|scope, _| scope.local != library);
    }
    pub fn reply(
        &self,
        peer: &str,
        local: &str,
        remote: &str,
        req: &Value,
        files: Value,
    ) -> Result<Value> {
        if req["delta"] != true {
            return Ok(files);
        }
        let current = Snapshot::new(serde_json::from_value(files).map_err(|e| e.to_string())?)?;
        let scope = Scope {
            peer: peer.into(),
            local: local.into(),
            remote: remote.into(),
        };
        let mut sent = self.sent.lock().unwrap();
        let reply = match sent.get(&scope) {
            Some(previous) if req["known"].as_str() == Some(previous.digest.as_str()) => {
                let changed: BTreeMap<_, _> = current
                    .files
                    .iter()
                    .filter(|(path, info)| previous.files.get(*path) != Some(*info))
                    .collect();
                let removed: Vec<_> = previous
                    .files
                    .keys()
                    .filter(|path| !current.files.contains_key(*path))
                    .collect();
                json!({"format":FORMAT,"digest":current.digest,"base":previous.digest,"changed":changed,"removed":removed})
            }
            _ => json!({"format":FORMAT,"digest":current.digest,"files":current.files}),
        };
        remember(&mut sent, scope, current);
        Ok(reply)
    }
    pub async fn fetch(&self, connection: &Connection, local: &str, target: &str) -> Result<Value> {
        let scope = Scope {
            peer: connection.remote_id().to_string(),
            local: local.into(),
            remote: target.into(),
        };
        let mut cached = self.received.lock().unwrap().remove(&scope);
        // 변경 목록을 복원할 수 없으면 전체 목록을 한 번 다시 요청한다.
        for _ in 0..2 {
            let known = cached.as_ref().map(|snapshot| snapshot.digest.as_str());
            let mut reply = request(connection, Message::json(json!({"op":"inventory","vault":local,"target":target,"delta":true,"known":known}))).await?.meta;
            if reply["format"].as_str() != Some(FORMAT) {
                // 이전 피어는 추가 요청 필드를 무시하고 전체 목록을 반환한다.
                return Ok(reply);
            }
            let digest = reply["digest"]
                .as_str()
                .ok_or("파일 목록 요약값이 없습니다.")?
                .to_string();
            let files: Files = if reply["files"].is_object() {
                serde_json::from_value(reply["files"].take()).map_err(|e| e.to_string())?
            } else {
                let Some(previous) = cached.take() else {
                    continue;
                };
                if reply["base"].as_str() != Some(previous.digest.as_str()) {
                    continue;
                }
                let mut files = previous.files;
                let changed: Files =
                    serde_json::from_value(reply["changed"].take()).map_err(|e| e.to_string())?;
                let removed: Vec<String> =
                    serde_json::from_value(reply["removed"].take()).map_err(|e| e.to_string())?;
                files.extend(changed);
                for path in removed {
                    files.remove(&path);
                }
                files
            };
            let snapshot = Snapshot::new(files)?;
            if snapshot.digest == digest {
                let result = serde_json::to_value(&snapshot.files).map_err(|e| e.to_string())?;
                remember(&mut self.received.lock().unwrap(), scope, snapshot);
                return Ok(result);
            }
            cached = None;
        }
        Err("파일 목록이 변경되어 다시 비교합니다.".into())
    }
}
