//! Library끼리도 같은 파일 요청으로 누락·동시 편집·삭제·이름 변경을 비교한다.
use crate::{request, storage, Connection, Message, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
};
#[derive(Clone, Deserialize)]
struct Info {
    etag: Option<String>,
}
#[derive(Default, Deserialize, Serialize)]
struct State {
    #[serde(default)]
    files: BTreeMap<String, String>,
    #[serde(default)]
    identities: BTreeMap<String, String>,
}
struct Pair {
    local: Option<String>,
    remote: Option<String>,
    local_id: Option<String>,
    remote_id: Option<String>,
}
struct Remote<'a> {
    connection: &'a Connection,
    local: &'a str,
    target: &'a str,
}
impl Remote<'_> {
    async fn call(&self, mut meta: Value, data: Vec<u8>) -> Result<Message> {
        meta["vault"] = json!(self.local);
        meta["target"] = json!(self.target);
        request(self.connection, Message { meta, data }).await
    }
    async fn read(&self, path: &str) -> Result<Vec<u8>> {
        Ok(self
            .call(json!({"op":"read","path":path}), vec![])
            .await?
            .data)
    }
    async fn apply(&self, path: &str, data: Option<Vec<u8>>, expected: Option<&str>) -> Result<()> {
        let op = if data.is_some() { "write" } else { "remove" };
        self.call(
            json!({"op":op,"path":path,"expected":expected}),
            data.unwrap_or_default(),
        )
        .await?;
        Ok(())
    }
}
fn administrative(root: &Path, relative: &Path) -> Result<PathBuf> {
    let mut target = root.to_path_buf();
    for part in std::iter::once(Path::new("")).chain(relative.iter().map(Path::new)) {
        target.push(part);
        match std::fs::symlink_metadata(&target) {
            Ok(meta) if storage::linked(&meta) => {
                return Err("관리 경로에 연결 폴더가 있습니다.".into())
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(target)
}
fn identity(path: &str, bytes: &[u8]) -> Option<String> {
    if path.ends_with("/project.json") {
        let value: Value = serde_json::from_slice(bytes).ok()?;
        return value["id"].as_str().map(|id| format!("project:{id}"));
    }
    if path.to_ascii_lowercase().ends_with(".md") {
        let raw = std::str::from_utf8(bytes).ok()?;
        let mut lines = raw.lines();
        if lines.next()? != "---" {
            return None;
        }
        let yaml = lines
            .take_while(|line| *line != "---")
            .collect::<Vec<_>>()
            .join("\n");
        let value: Value = serde_yaml::from_str(&yaml).ok()?;
        return value["id"].as_str().map(|id| format!("document:{id}"));
    }
    None
}
fn save(path: &Path, state: &State) -> Result<()> {
    storage::atomic(path, &serde_json::to_vec(state).map_err(|e| e.to_string())?)
}
fn archive(root: &Path, path: &str, bytes: &[u8]) -> Result<()> {
    if !storage::allowed(path) {
        return Err("충돌 사본 경로 오류".into());
    }
    let target = administrative(
        root,
        &Path::new(".lof/file-conflicts")
            .join(storage::hash(bytes))
            .join(path),
    )?;
    if !target.exists() {
        storage::atomic(&target, bytes)?;
    }
    Ok(())
}
async fn local_read(root: &Path, path: &str) -> Result<Option<Vec<u8>>> {
    let root = root.to_path_buf();
    let path = path.to_string();
    tokio::task::spawn_blocking(move || storage::read(&root, &path))
        .await
        .map_err(|_| "원본 파일 읽기 실패")?
}
async fn local_apply(
    root: &Path,
    path: &str,
    bytes: Option<Vec<u8>>,
    expected: Option<&str>,
) -> Result<()> {
    let root = root.to_path_buf();
    let path = path.to_string();
    let expected = expected.map(str::to_owned);
    tokio::task::spawn_blocking(move || {
        storage::apply(&root, &path, bytes.as_deref(), expected.as_deref())
    })
    .await
    .map_err(|_| "원본 파일 저장 실패")?
}
pub async fn synchronize(
    connection: &Connection,
    root: &Path,
    local: &str,
    target: &str,
    peer: &str,
    heartbeat: impl Fn() -> Result<()> + Send + Sync,
) -> Result<u64> {
    let remote = Remote {
        connection,
        local,
        target,
    };
    remote.call(json!({"op":"begin"}), vec![]).await?;
    let result = compare(&remote, root, peer, &heartbeat).await;
    let ended = remote
        .call(json!({"op":"end","success":result.is_ok()}), vec![])
        .await;
    let conflicts = result?;
    ended?;
    Ok(conflicts)
}
async fn compare(
    remote: &Remote<'_>,
    root: &Path,
    peer: &str,
    heartbeat: &(impl Fn() -> Result<()> + Send + Sync),
) -> Result<u64> {
    let root = root.to_path_buf();
    let checkpoint = administrative(
        &root,
        &Path::new(".lof/replicas").join(format!("{}.json", storage::hash(peer.as_bytes()))),
    )?;
    let mut state: State = match std::fs::read(&checkpoint) {
        Ok(bytes) if bytes.len() <= 16_000_000 => {
            serde_json::from_slice(&bytes).map_err(|_| "동기화 비교 정보를 읽지 못했습니다.")?
        }
        Ok(_) => return Err("동기화 비교 정보가 너무 큽니다.".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
        Err(e) => return Err(e.to_string()),
    };
    let remote_id = remote.read(".lof-sync-id").await?;
    if remote_id != remote.target.as_bytes() {
        return Err("상대 서재가 변경되었습니다. 다시 연결해 주세요.".into());
    }
    let mut other: BTreeMap<String, Info> =
        serde_json::from_value(remote.call(json!({"op":"inventory"}), vec![]).await?.meta)
            .map_err(|_| "파일 목록 형식 오류")?;
    other.remove(".lof-sync-id");
    let source = root.clone();
    let local: BTreeMap<String, Info> = serde_json::from_value(
        tokio::task::spawn_blocking(move || storage::inventory(&source))
            .await
            .map_err(|_| "파일 목록 읽기 실패")??,
    )
    .map_err(|_| "파일 목록 형식 오류")?;
    let names: BTreeSet<_> = local
        .keys()
        .chain(other.keys())
        .chain(state.files.keys())
        .cloned()
        .collect();
    if names.len() > 40_000 {
        return Err("동기화 파일은 4만 개 이하여야 합니다.".into());
    }
    let mut folded = HashSet::new();
    for path in &names {
        if !storage::allowed(path) || !folded.insert(path.to_lowercase()) {
            return Err("파일 경로 또는 대소문자 충돌을 확인해 주세요.".into());
        }
        for (index, _) in path.match_indices('/') {
            if names.contains(&path[..index]) {
                return Err("파일과 폴더 경로가 겹칩니다.".into());
            }
        }
    }
    let mut pairs = BTreeMap::new();
    let mut identities: HashMap<String, BTreeSet<String>> = HashMap::new();
    let (mut seen_local, mut seen_remote) = (HashSet::new(), HashSet::new());
    for path in &names {
        heartbeat()?;
        let local_hash = local.get(path).and_then(|i| i.etag.clone());
        let remote_hash = other.get(path).and_then(|i| i.etag.clone());
        let has_id = path.ends_with("/project.json") || path.to_ascii_lowercase().ends_with(".md");
        let local_id = if has_id && local_hash.is_some() {
            let bytes = local_read(&root, path)
                .await?
                .ok_or("원본이 변경되었습니다.")?;
            if Some(storage::hash(&bytes)) != local_hash {
                return Err("원본이 변경되어 다시 비교합니다.".into());
            }
            identity(path, &bytes)
        } else {
            None
        };
        let remote_id = if has_id && remote_hash.is_some() {
            if remote_hash == local_hash {
                local_id.clone()
            } else {
                let bytes = remote.read(path).await?;
                if Some(storage::hash(&bytes)) != remote_hash {
                    return Err("상대 파일이 변경되어 다시 비교합니다.".into());
                }
                identity(path, &bytes)
            }
        } else {
            None
        };
        for (id, seen) in [(&local_id, &mut seen_local), (&remote_id, &mut seen_remote)] {
            if let Some(id) = id {
                if !seen.insert(id.clone()) {
                    return Err("동일한 문서 ID가 여러 파일에 있습니다.".into());
                }
                identities
                    .entry(id.clone())
                    .or_default()
                    .insert(path.clone());
            }
        }
        pairs.insert(
            path.clone(),
            Pair {
                local: local_hash,
                remote: remote_hash,
                local_id,
                remote_id,
            },
        );
    }
    let mut losers = HashSet::new();
    for paths in identities.values().filter(|paths| paths.len() > 1) {
        let new: Vec<_> = paths
            .iter()
            .filter(|path| !state.files.contains_key(*path))
            .collect();
        let winner = if new.len() == 1 {
            new[0]
        } else {
            paths.last().unwrap()
        };
        for path in paths {
            if path != winner {
                losers.insert(path.clone());
            }
        }
    }
    let mut protected = HashSet::new();
    for (path, pair) in &pairs {
        if let Some(folder) = path.strip_suffix("/project.json") {
            if state.files.contains_key(path) && pair.local.is_some() != pair.remote.is_some() {
                let prefix = format!("{folder}/");
                let local_remains = pair.local.is_some();
                if pairs
                    .iter()
                    .filter(|(name, _)| name.starts_with(&prefix))
                    .any(|(name, p)| {
                        let current = if local_remains { &p.local } else { &p.remote };
                        current
                            .as_ref()
                            .is_some_and(|hash| state.files.get(name) != Some(hash))
                    })
                {
                    protected.insert(prefix);
                }
            }
        }
    }
    let mut conflicts = 0;
    for (path, pair) in pairs {
        heartbeat()?;
        remote.call(json!({"op":"touch"}), vec![]).await?;
        let before = state.files.get(&path).cloned();
        let l = pair.local.as_deref();
        let r = pair.remote.as_deref();
        if losers.contains(&path) {
            if let Some(expected) = l {
                let bytes = local_read(&root, &path)
                    .await?
                    .ok_or("파일이 변경되었습니다.")?;
                if storage::hash(&bytes) != expected {
                    return Err("파일이 변경되었습니다.".into());
                }
                if before.as_deref() != l {
                    archive(&root, &path, &bytes)?;
                    conflicts += 1;
                }
                local_apply(&root, &path, None, l).await?;
            }
            if let Some(expected) = r {
                let bytes = remote.read(&path).await?;
                if storage::hash(&bytes) != expected {
                    return Err("상대 파일이 변경되었습니다.".into());
                }
                if before.as_deref() != r {
                    archive(&root, &path, &bytes)?;
                    conflicts += 1;
                }
                remote.apply(&path, None, r).await?;
            }
            state.files.remove(&path);
            state.identities.remove(&path);
            save(&checkpoint, &state)?;
            continue;
        }
        if l == r {
            if let Some(hash) = l {
                state.files.insert(path.clone(), hash.into());
                if let Some(id) = pair.local_id {
                    state.identities.insert(path.clone(), id);
                }
            } else {
                state.files.remove(&path);
                state.identities.remove(&path);
            }
            continue;
        }
        let preserve = protected.iter().any(|prefix| path.starts_with(prefix));
        let take_remote = if preserve && (l.is_none() || r.is_none()) {
            r.is_some()
        } else if l == before.as_deref() {
            true
        } else if r == before.as_deref() {
            false
        } else if l.is_none() {
            true
        } else if r.is_none() {
            false
        } else {
            let remote_wins = r > l;
            let losing = if remote_wins {
                local_read(&root, &path)
                    .await?
                    .ok_or("파일이 변경되었습니다.")?
            } else {
                remote.read(&path).await?
            };
            let expected = if remote_wins { l } else { r };
            if Some(storage::hash(&losing)).as_deref() != expected {
                return Err("충돌 비교 중 파일이 변경되었습니다.".into());
            }
            archive(&root, &path, &losing)?;
            conflicts += 1;
            remote_wins
        };
        let winner = if take_remote { r } else { l };
        let bytes = if winner.is_none() {
            None
        } else if take_remote {
            Some(remote.read(&path).await?)
        } else {
            local_read(&root, &path).await?
        };
        if bytes.as_deref().map(storage::hash).as_deref() != winner {
            return Err("전송 직전 파일이 변경되었습니다.".into());
        }
        if take_remote {
            local_apply(&root, &path, bytes, l).await?;
        } else {
            remote.apply(&path, bytes, r).await?;
        }
        if let Some(hash) = winner {
            state.files.insert(path.clone(), hash.into());
            let id = if take_remote {
                pair.remote_id
            } else {
                pair.local_id
            };
            if let Some(id) = id {
                state.identities.insert(path.clone(), id);
            } else {
                state.identities.remove(&path);
            }
        } else {
            state.files.remove(&path);
            state.identities.remove(&path);
        }
        save(&checkpoint, &state)?;
    }
    save(&checkpoint, &state)?;
    Ok(conflicts)
}
