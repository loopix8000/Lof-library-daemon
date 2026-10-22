//! Iroh로 원본 파일을 교환하는 상시 서재 피어.
//!
//! 서재마다 동기화 그룹 하나에 가입하고, 서명된 구성원 목록을 gossip과 연결 확인 요청으로
//! 받아 다른 구성원을 자동으로 찾는다. 파일 비교는 구성원마다 기존 1:1 비교를 사용한다.
use crate::store::Store;
use iroh::SecretKey;
use iroh::Watcher as _;
use lof_realtime::{
    group::{self, Announcement, Announcer, Event, Group, Member, Subscription},
    storage, Connection, Host, Message, Node,
};
use notify::Watcher;
use rusqlite::OptionalExtension;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};
use uuid::Uuid;
type Result<T> = std::result::Result<T, String>;

pub struct Realtime {
    store: Arc<Store>,
    base: PathBuf,
    key: SecretKey,
    state: Mutex<Value>,
    node: Mutex<Option<Node>>,
    error: Mutex<String>,
    invites: Mutex<HashMap<String, (String, Instant)>>,
    leases: Mutex<HashMap<String, (String, Instant)>>,
    connections: Mutex<HashMap<String, (String, Connection)>>,
    // 서재별 연결. 그룹에서 나갈 때 구성원에게 직접 알리는 데 사용한다.
    links: Mutex<HashMap<String, (String, Connection)>>,
    announcers: Mutex<HashMap<String, Announcer>>,
    dirty: Mutex<HashSet<String>>,
    statistics: Mutex<HashMap<String, Value>>,
    peer_status: Mutex<HashMap<String, Value>>,
    versions: Mutex<HashMap<String, u64>>,
    // 삭제는 파일 요청과 Library 간 비교가 끝난 뒤 독점 실행한다.
    file_activity: Mutex<HashMap<String, Arc<tokio::sync::RwLock<()>>>>,
    wake: tokio::sync::Notify,
}
fn read_key(store: &Store, key: &str) -> Result<Option<String>> {
    store
        .read(|db| {
            Ok(db
                .query_row("SELECT value FROM server WHERE key=?1", [key], |r| {
                    r.get::<_, String>(0)
                })
                .optional()?)
        })
        .map_err(|e| e.to_string())
}
fn group_of(library: &Value) -> Option<Group> {
    serde_json::from_value(library["group"].clone()).ok()
}
fn spawn(task: impl std::future::Future<Output = ()> + Send + 'static) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(task);
    }
}
impl Realtime {
    pub fn new(base: PathBuf, store: Arc<Store>) -> Result<Arc<Self>> {
        let state = match read_key(&store, "realtime-v2")? {
            Some(text) => {
                serde_json::from_str(&text).map_err(|_| "실시간 동기화 설정을 읽지 못했습니다.")?
            }
            None => Self::migrate(&base, &store)?,
        };
        let key = state["secret"]
            .as_str()
            .ok_or("장치 키가 없습니다.")?
            .parse::<SecretKey>()
            .map_err(|e| e.to_string())?;
        let file_activity = state["libraries"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|library| {
                Some((
                    library["id"].as_str()?.to_string(),
                    Arc::new(tokio::sync::RwLock::new(())),
                ))
            })
            .collect();
        let rt = Arc::new(Self {
            store,
            base,
            key,
            state: Mutex::new(state),
            node: Mutex::new(None),
            error: Mutex::new(String::new()),
            invites: Mutex::new(HashMap::new()),
            leases: Mutex::new(HashMap::new()),
            connections: Mutex::new(HashMap::new()),
            links: Mutex::new(HashMap::new()),
            announcers: Mutex::new(HashMap::new()),
            dirty: Mutex::new(HashSet::new()),
            statistics: Mutex::new(HashMap::new()),
            peer_status: Mutex::new(HashMap::new()),
            versions: Mutex::new(HashMap::new()),
            file_activity: Mutex::new(file_activity),
            wake: tokio::sync::Notify::new(),
        });
        rt.save(&rt.state.lock().unwrap())?;
        rt.store
            .write(|db| {
                db.execute("DELETE FROM server WHERE key='realtime-v1'", [])?;
                Ok(())
            })
            .map_err(|e| e.to_string())?;
        Ok(rt)
    }
    /// 그룹 방식으로 바꾸면서 이전 1:1 기기 연결과 비교 기록을 한 번 초기화한다. 서재와 원본 파일은 유지한다.
    fn migrate(base: &std::path::Path, store: &Store) -> Result<Value> {
        let Some(text) = read_key(store, "realtime-v1")? else {
            return Ok(
                json!({"secret":hex::encode(SecretKey::generate().to_bytes()),"libraries":[],"resetNotice":false}),
            );
        };
        let old: Value =
            serde_json::from_str(&text).map_err(|_| "실시간 동기화 설정을 읽지 못했습니다.")?;
        let mut reset = false;
        let libraries: Vec<Value> = old["libraries"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|library| {
                let id = library["id"]
                    .as_str()
                    .filter(|id| Uuid::parse_str(id).is_ok())?;
                reset |= library["peers"].as_array().is_some_and(|p| !p.is_empty());
                match std::fs::remove_dir_all(base.join(id).join(".lof").join("replicas")) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                        tracing::warn!("이전 비교 기록을 지우지 못했습니다: {e}")
                    }
                    _ => {}
                }
                Some(json!({"id":id,"name":library["name"],"group":null,"aliases":{}}))
            })
            .collect();
        Ok(json!({"secret":old["secret"],"libraries":libraries,"resetNotice":reset}))
    }
    fn save(&self, state: &Value) -> Result<()> {
        self.store.write(|db|{db.execute("INSERT INTO server(key,value) VALUES('realtime-v2',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[state.to_string()])?;Ok(())}).map_err(|e|e.to_string())
    }
    fn own(&self) -> String {
        self.key.public().to_string()
    }
    /// 서재의 그룹을 읽거나 고친다. 바뀐 경우에만 저장한다.
    fn edit_group<T>(
        &self,
        library: &str,
        edit: impl FnOnce(&mut Option<Group>, &Value) -> Result<T>,
    ) -> Result<T> {
        let mut state = self.state.lock().unwrap();
        let mut next = state.clone();
        let entry = next["libraries"]
            .as_array_mut()
            .ok_or("서재 설정 오류")?
            .iter_mut()
            .find(|l| l["id"].as_str() == Some(library))
            .ok_or("서재를 찾을 수 없습니다.")?;
        let previous = group_of(entry);
        let mut group = previous.clone();
        let result = edit(&mut group, entry)?;
        if group != previous {
            if group.as_ref().map(Group::id) != previous.as_ref().map(Group::id) {
                entry["aliases"] = json!({});
            }
            entry["group"] = json!(group);
            self.save(&next)?;
            *state = next;
        }
        Ok(result)
    }
    fn announce(&self, library: &str, announcement: Announcement) {
        if let Some(sender) = self.announcers.lock().unwrap().get(library).cloned() {
            spawn(async move {
                let _ = sender.announce(&announcement).await;
            });
        }
    }
    /// 다른 구성원이 보낸 목록을 반영한다. 이 서재가 제거된 기록이 오면 그룹에서 나간다.
    fn absorb(&self, library: &str, incoming: &[Member]) -> Result<bool> {
        if incoming.is_empty() {
            return Ok(false);
        }
        let own = self.own();
        let changed = self.edit_group(library, |group, _| {
            let Some(current) = group.as_mut() else {
                return Ok(false);
            };
            if !current.merge(incoming, false) {
                return Ok(false);
            }
            if current.member(&own).is_none() {
                *group = None;
            }
            Ok(true)
        })?;
        if changed {
            self.wake.notify_waiters();
        }
        Ok(changed)
    }
    pub fn info(&self) -> Value {
        let pending: Vec<_> = self
            .invites
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, (_, until))| *until > Instant::now())
            .map(|(id, _)| id.clone())
            .collect();
        let own = self.own();
        let statuses = self.peer_status.lock().unwrap().clone();
        let state = self.state.lock().unwrap();
        let connections = self.connections.lock().unwrap();
        let leases = self.leases.lock().unwrap();
        let statistics = self.statistics.lock().unwrap();
        let libraries:Vec<_>=state["libraries"].as_array().into_iter().flatten().map(|library|{
            let id=library["id"].as_str().unwrap_or("");
            let group=group_of(library);
            let peers:Vec<_>=group.iter().flat_map(|g|g.others(&own)).map(|peer|json!({"id":peer.endpoint,"remoteVault":peer.vault,
                "name":library["aliases"][&peer.endpoint].as_str().unwrap_or(&peer.name),"storage":peer.storage,
                "status":statuses.get(&format!("{id}:{}:{}",peer.endpoint,peer.vault)),
                "online":connections.values().any(|(endpoint,c)|*endpoint==peer.endpoint&&c.close_reason().is_none())})).collect();
            json!({"id":id,"name":library["name"],"path":self.base.join(id),"group":group.as_ref().map(|g|json!({"id":g.id(),"members":g.active().count()})),
                "peers":peers,"syncing":leases.get(id).is_some_and(|(_,until)|*until>Instant::now()),"statistics":statistics.get(id)})
        }).collect();
        let node = self.node.lock().unwrap();
        let relay = node
            .as_ref()
            .map(|n| {
                n.endpoint
                    .home_relay_status()
                    .get()
                    .iter()
                    .any(|r| r.is_connected())
            })
            .unwrap_or(false);
        json!({"pendingInvites":pending,"ready":node.is_some(),"endpoint":node.as_ref().map(|n|n.endpoint.id().to_string()),"relayConnected":relay,
            "libraries":libraries,"resetNotice":state["resetNotice"]==true,"error":*self.error.lock().unwrap()})
    }
    pub fn dismiss_reset(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let mut next = state.clone();
        next["resetNotice"] = json!(false);
        self.save(&next)?;
        *state = next;
        Ok(())
    }
    pub fn create_library(&self, name: &str) -> Result<Value> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 200 {
            return Err("서재 이름을 1~200자로 입력해 주세요.".into());
        }
        let id = Uuid::new_v4().to_string();
        let directory = self.base.join(&id);
        std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
        let mut state = self.state.lock().unwrap();
        let mut next = state.clone();
        next["libraries"]
            .as_array_mut()
            .ok_or("서재 설정 오류")?
            .push(json!({"id":id,"name":name,"group":null,"aliases":{}}));
        self.save(&next)?;
        *state = next;
        drop(state);
        self.file_activity
            .lock()
            .unwrap()
            .insert(id.clone(), Arc::new(tokio::sync::RwLock::new(())));
        self.dirty.lock().unwrap().insert(id.clone());
        self.wake.notify_waiters();
        Ok(json!({"id":id,"name":name,"path":directory}))
    }
    pub fn rename_library(&self, id: &str, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 200 {
            return Err("서재 이름을 1~200자로 입력해 주세요.".into());
        }
        let own = self.own();
        // 그룹 구성원에게 보이는 이름도 새로 서명해 알린다.
        let members = self.edit_group(id, |group, _| {
            Ok(group.as_mut().and_then(|current| {
                let me = current.member(&own).cloned()?;
                current.update(&self.key, &own, &me.vault, name, true, false);
                Some(current.members.clone())
            }))
        })?;
        let mut state = self.state.lock().unwrap();
        let mut next = state.clone();
        let library = next["libraries"]
            .as_array_mut()
            .ok_or("서재 설정 오류")?
            .iter_mut()
            .find(|l| l["id"].as_str() == Some(id))
            .ok_or("서재가 없습니다.")?;
        library["name"] = json!(name);
        self.save(&next)?;
        *state = next;
        drop(state);
        if let Some(members) = members {
            self.announce(id, Announcement::Members { members });
        }
        Ok(())
    }
    pub fn library_path(&self, id: &str) -> Result<PathBuf> {
        if Uuid::parse_str(id).is_err()
            || !self.state.lock().unwrap()["libraries"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|l| l["id"].as_str() == Some(id))
        {
            return Err("서재를 찾을 수 없습니다.".into());
        }
        let root = self.base.join(id);
        storage::path(&root, "경로확인")?;
        Ok(root)
    }
    fn activity(&self, id: &str) -> Result<Arc<tokio::sync::RwLock<()>>> {
        self.file_activity
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| "서재를 찾을 수 없습니다.".into())
    }
    /// 진행 중인 파일 작업을 마친 뒤 그룹에서 나가고 이 Library의 원본만 삭제한다.
    pub async fn delete_library(self: &Arc<Self>, id: &str) -> Result<()> {
        let activity = self.activity(id)?.write_owned().await;
        let rt = self.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let _activity = activity;
            let root = rt.library_path(&id)?;
            // 재귀 삭제 대상은 데이터 폴더 바로 아래의 실제 서재 폴더여야 한다.
            let base = rt.base.canonicalize().map_err(|e| e.to_string())?;
            match root.canonicalize() {
                Ok(path) if path.parent() == Some(base.as_path()) => {}
                Ok(_) => return Err("서재 폴더가 데이터 폴더 밖에 있습니다.".into()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.to_string()),
            }
            rt.leave(&id)?;
            let mut state = rt.state.lock().unwrap();
            let mut next = state.clone();
            next["libraries"]
                .as_array_mut()
                .ok_or("서재 설정 오류")?
                .retain(|library| library["id"].as_str() != Some(&id));
            // 실패하면 탈퇴한 서재 항목을 남겨 삭제를 다시 시도할 수 있게 한다.
            match std::fs::remove_dir_all(&root) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(format!(
                    "그룹에서 나갔지만 서재 폴더를 삭제하지 못했습니다. 다시 시도해 주세요: {e}"
                ))
                }
            }
            rt.save(&next)?;
            *state = next;
            drop(state);
            rt.invites.lock().unwrap().remove(&id);
            rt.leases.lock().unwrap().remove(&id);
            rt.dirty.lock().unwrap().remove(&id);
            rt.statistics.lock().unwrap().remove(&id);
            rt.versions.lock().unwrap().remove(&id);
            rt.file_activity.lock().unwrap().remove(&id);
            rt.peer_status
                .lock()
                .unwrap()
                .retain(|key, _| !key.starts_with(&format!("{id}:")));
            rt.wake.notify_waiters();
            Ok(())
        })
        .await
        .map_err(|_| "서재 삭제를 처리하지 못했습니다.".to_string())?
    }
    pub fn cancel_invite(&self, id: &str) {
        self.invites.lock().unwrap().remove(id);
    }
    pub fn rename_peer(&self, vault: &str, id: &str, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 100 {
            return Err("기기 이름을 1~100자로 입력해 주세요.".into());
        }
        let mut state = self.state.lock().unwrap();
        let mut next = state.clone();
        let library = next["libraries"]
            .as_array_mut()
            .ok_or("서재 설정 오류")?
            .iter_mut()
            .find(|l| l["id"].as_str() == Some(vault))
            .ok_or("서재가 없습니다.")?;
        if group_of(library)
            .and_then(|g| g.member(id).cloned())
            .is_none()
        {
            return Err("기기를 찾을 수 없습니다.".into());
        }
        // 기기 이름은 이 Library에서만 쓰는 별칭으로 저장한다.
        if !library["aliases"].is_object() {
            library["aliases"] = json!({});
        }
        library["aliases"][id] = json!(name);
        self.save(&next)?;
        *state = next;
        Ok(())
    }
    /// 다른 Writer 또는 Library의 페어링 코드로 서재를 그룹에 가입시킨다.
    pub async fn pair(&self, args: Value) -> Result<Value> {
        let node = self
            .node
            .lock()
            .unwrap()
            .clone()
            .ok_or("피어를 먼저 시작해 주세요.")?;
        let ticket = group::parse_code(args["code"].as_str().unwrap_or(""))?;
        if ticket.addr.id == node.endpoint.id() {
            return Err("같은 장치의 코드는 사용할 수 없습니다.".into());
        }
        let local = args["id"]
            .as_str()
            .ok_or("가입할 서재를 선택해 주세요.")?
            .to_string();
        let _activity = self.activity(&local)?.read_owned().await;
        self.library_path(&local)?;
        let own = self.own();
        self.check_join(&local, &ticket.group, &own)?;
        let name = self.library_name(&local);
        let connection = node.connect(ticket.addr.clone()).await?;
        let request = json!({"op":"join","group":ticket.group,"vault":local,"token":ticket.token,"name":name,"storage":true});
        let response = lof_realtime::request(&connection, Message::json(request)).await;
        let endpoint = connection.remote_id().to_string();
        connection.close(0u8.into(), b"joined");
        let response = response?;
        let rejected =
            || "페어링 코드와 상대 그룹 정보가 다릅니다. 새 코드를 받아 주세요.".to_string();
        let mut joined = Group::from_secret(response.meta["secret"].as_str().unwrap_or(""))
            .map_err(|_| rejected())?;
        if joined.id() != ticket.group
            || response.meta["group"].as_str() != Some(ticket.group.as_str())
        {
            return Err(rejected());
        }
        let members: Vec<Member> =
            serde_json::from_value(response.meta["members"].clone()).map_err(|_| rejected())?;
        joined.merge(&members, true);
        if joined.member(&endpoint).is_none()
            || joined.member(&own).is_none_or(|me| me.vault != local)
        {
            return Err(rejected());
        }
        self.check_join(&local, &ticket.group, &own)?;
        self.edit_group(&local, |group, _| {
            match group.as_mut() {
                Some(current) if current.id() == joined.id() => {
                    current.merge(&joined.members, true);
                }
                _ => *group = Some(joined.clone()),
            }
            Ok(())
        })?;
        self.dirty.lock().unwrap().insert(local);
        self.wake.notify_waiters();
        Ok(json!(true))
    }
    fn library_name(&self, id: &str) -> String {
        self.state.lock().unwrap()["libraries"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|l| l["id"].as_str() == Some(id))
            .and_then(|l| l["name"].as_str().map(str::to_owned))
            .unwrap_or_else(|| "Lof Library".into())
    }
    /// 이미 다른 장치가 있는 다른 그룹에 속했거나, 같은 그룹에 다른 서재가 가입한 경우 거부한다.
    fn check_join(&self, local: &str, target: &str, own: &str) -> Result<()> {
        let state = self.state.lock().unwrap();
        for library in state["libraries"].as_array().into_iter().flatten() {
            let Some(group) = group_of(library) else {
                continue;
            };
            if library["id"].as_str() == Some(local) {
                if group.id() != target && group.others(own).next().is_some() {
                    return Err("이 서재는 이미 다른 동기화 그룹에 속해 있습니다. 그룹에서 나간 뒤 다시 가입해 주세요.".into());
                }
            } else if group.id() == target {
                return Err("다른 서재가 이미 이 동기화 그룹에 가입해 있습니다.".into());
            }
        }
        Ok(())
    }
    fn refresh_statistics(&self, id: &str) {
        let Ok(activity) = self.activity(id) else {
            return;
        };
        let Ok(_activity) = activity.try_read() else {
            return;
        };
        if self.library_path(id).is_err() {
            return;
        }
        fn walk(
            root: &std::path::Path,
            folder: &std::path::Path,
            count: &mut u64,
            size: &mut u64,
        ) -> Result<()> {
            for item in std::fs::read_dir(folder).map_err(|e| e.to_string())? {
                let item = item.map_err(|e| e.to_string())?;
                let path = item.path();
                let rel = path
                    .strip_prefix(root)
                    .map_err(|_| "파일 경로 오류")?
                    .to_string_lossy()
                    .replace('\\', "/");
                if !storage::allowed(&rel) {
                    continue;
                }
                let safe = storage::path(root, &rel)?;
                let meta = std::fs::symlink_metadata(&safe).map_err(|e| e.to_string())?;
                if meta.is_dir() {
                    walk(root, &safe, count, size)?;
                } else if meta.is_file() {
                    *count += 1;
                    *size += meta.len();
                }
            }
            Ok(())
        }
        let (mut count, mut size) = (0, 0);
        let root = self.base.join(id);
        let result = walk(&root, &root, &mut count, &mut size);
        let mut statistics = self.statistics.lock().unwrap();
        let stats = statistics
            .entry(id.to_string())
            .or_insert_with(|| json!({}));
        stats["files"] = json!(count);
        stats["bytes"] = json!(size);
        stats["error"] = json!(result.err());
    }
    /// 서재의 그룹 초대 코드를 만든다. 그룹이 없으면 이 서재를 첫 구성원으로 새 그룹을 만든다.
    pub fn invite(&self, name: &str, existing: Option<&str>) -> Result<Value> {
        let node = self
            .node
            .lock()
            .unwrap()
            .clone()
            .ok_or("실시간 연결을 준비 중입니다.")?;
        let id = match existing {
            Some(id) => {
                self.library_path(id)?;
                id.to_string()
            }
            None => self.create_library(name)?["id"]
                .as_str()
                .unwrap_or("")
                .to_string(),
        };
        let activity = self.activity(&id)?;
        let _activity = activity.blocking_read();
        self.library_path(&id)?;
        std::fs::create_dir_all(self.base.join(&id)).map_err(|e| e.to_string())?;
        let own = self.own();
        let group = self.edit_group(&id, |group, library| {
            let title = library["name"].as_str().unwrap_or("Lof Library");
            let current = group.get_or_insert_with(|| Group::create(&self.key, &id, title, true));
            if current.member(&own).is_none() {
                current.update(&self.key, &own, &id, title, true, false);
            }
            Ok(current.clone())
        })?;
        self.wake.notify_waiters();
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        self.invites.lock().unwrap().insert(
            id.clone(),
            (
                token.clone(),
                Instant::now() + Duration::from_secs(group::INVITE_SECONDS),
            ),
        );
        let code = group.code(&node.endpoint.addr(), &token)?;
        Ok(json!({"id":id,"code":code,"group":group.id(),"expiresIn":group::INVITE_SECONDS}))
    }
    pub fn remove_peer(&self, vault: &str, peer: &str) -> Result<()> {
        let members = self.edit_group(vault, |group, _| {
            let Some(current) = group.as_mut() else {
                return Ok(None);
            };
            let Some(member) = current.member(peer).cloned() else {
                return Ok(None);
            };
            current.update(
                &self.key,
                &member.endpoint,
                &member.vault,
                &member.name,
                member.storage,
                true,
            );
            Ok(Some(current.members.clone()))
        })?;
        if let Some(members) = members {
            self.announce(vault, Announcement::Members { members });
            self.wake.notify_waiters();
        }
        Ok(())
    }
    /// 이 서재를 그룹에서 제거했다고 서명해 알리고 그룹 정보를 지운다. 원본 파일은 유지한다.
    pub fn leave(&self, library: &str) -> Result<()> {
        let own = self.own();
        let members = self.edit_group(library, |group, _| {
            let Some(current) = group.as_mut() else {
                return Ok(None);
            };
            if let Some(me) = current.member(&own).cloned() {
                current.update(&self.key, &own, &me.vault, &me.name, me.storage, true);
            }
            let members = current.members.clone();
            *group = None;
            Ok(Some(members))
        })?;
        let Some(members) = members else {
            return Ok(());
        };
        self.announce(
            library,
            Announcement::Members {
                members: members.clone(),
            },
        );
        for (key, (target, connection)) in self.links.lock().unwrap().iter() {
            if !key.starts_with(&format!("{library}:")) {
                continue;
            }
            let meta = json!({"op":"members","vault":library,"target":target,"members":members});
            let connection = connection.clone();
            spawn(async move {
                let _ = tokio::time::timeout(
                    Duration::from_secs(3),
                    lof_realtime::request(&connection, Message::json(meta)),
                )
                .await;
                connection.close(0u8.into(), b"left");
            });
        }
        self.invites.lock().unwrap().remove(library);
        self.wake.notify_waiters();
        Ok(())
    }
    pub async fn run(self: Arc<Self>, closing: tokio_util::sync::CancellationToken) {
        let result = self.clone().serve(closing).await;
        let node = self.node.lock().unwrap().take();
        if let Some(node) = node {
            node.close().await;
        }
        if let Err(error) = result {
            tracing::error!("실시간 피어 시작 실패: {error}");
            *self.error.lock().unwrap() = error;
        }
    }
    fn gossip_event(weak: &Weak<Self>, library: &str, event: Event) {
        let Some(rt) = weak.upgrade() else {
            return;
        };
        match event {
            Event::Announcement(Announcement::Members { members }) => {
                let library = library.to_string();
                tokio::task::spawn_blocking(move || {
                    let _ = rt.absorb(&library, &members);
                });
            }
            // 연결이 끊긴 구성원이 다시 보이면 재시도 대기를 건너뛴다.
            Event::Announcement(Announcement::Changed { .. }) | Event::Neighbor(_) => {
                rt.wake.notify_waiters();
            }
        }
    }
    async fn serve(self: Arc<Self>, closing: tokio_util::sync::CancellationToken) -> Result<()> {
        std::fs::create_dir_all(&self.base).map_err(|e| e.to_string())?;
        let node = Node::bind(self.key.clone(), self.clone()).await?;
        *self.node.lock().unwrap() = Some(node.clone());
        let weak = Arc::downgrade(&self);
        let base = self.base.clone();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if let Some(rt) = weak.upgrade() {
                    match event {
                        Ok(event) if !event.kind.is_access() => {
                            let mut dirty = rt.dirty.lock().unwrap();
                            for path in event.paths {
                                if let Ok(rel) = path.strip_prefix(&base) {
                                    let parts: Vec<_> = rel.components().collect();
                                    if let Some(first) = parts.first() {
                                        let id = first.as_os_str().to_string_lossy();
                                        let rest = parts
                                            .iter()
                                            .skip(1)
                                            .map(|p| p.as_os_str().to_string_lossy())
                                            .collect::<Vec<_>>()
                                            .join("/");
                                        if Uuid::parse_str(&id).is_ok()
                                            && (rest.is_empty() || storage::allowed(&rest))
                                        {
                                            dirty.insert(id.into_owned());
                                        }
                                    }
                                }
                            }
                        }
                        Err(_) => {
                            let ids: Vec<_> = rt.state.lock().unwrap()["libraries"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(|v| v["id"].as_str().map(str::to_owned))
                                .collect();
                            rt.dirty.lock().unwrap().extend(ids);
                        }
                        _ => {}
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        watcher
            .watch(&self.base, notify::RecursiveMode::Recursive)
            .map_err(|e| e.to_string())?;
        let ids: Vec<_> = self.state.lock().unwrap()["libraries"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v["id"].as_str().map(str::to_owned))
            .collect();
        self.dirty.lock().unwrap().extend(ids);
        let own = self.own();
        let mut links: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
        let mut subscriptions: HashMap<String, Subscription> = HashMap::new();
        let mut introduced: HashMap<String, Vec<String>> = HashMap::new();
        let mut announced: HashMap<String, u64> = HashMap::new();
        loop {
            let dirty = std::mem::take(&mut *self.dirty.lock().unwrap());
            for id in dirty {
                let rt = self.clone();
                let name = id.clone();
                let _ = tokio::task::spawn_blocking(move || rt.refresh_statistics(&name)).await;
                *self.versions.lock().unwrap().entry(id).or_default() += 1;
            }
            // 서재별 그룹 주제에 참여해 구성원 목록과 변경 알림을 받는다.
            let groups: Vec<(String, Group)> = self.state.lock().unwrap()["libraries"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|l| Some((l["id"].as_str()?.to_string(), group_of(l)?)))
                .collect();
            subscriptions.retain(|id, subscription| {
                groups
                    .iter()
                    .any(|(lib, g)| lib == id && g.id() == subscription.group)
                    && !subscription.finished()
            });
            introduced.retain(|id, _| subscriptions.contains_key(id));
            for (library, group) in &groups {
                if !subscriptions.contains_key(library) {
                    let weak = Arc::downgrade(&self);
                    let scope = library.clone();
                    match Subscription::join(&node, group, &own, move |event| {
                        Self::gossip_event(&weak, &scope, event)
                    })
                    .await
                    {
                        Ok(subscription) => {
                            subscriptions.insert(library.clone(), subscription);
                            introduced.insert(
                                library.clone(),
                                group.others(&own).map(|m| m.endpoint.clone()).collect(),
                            );
                        }
                        Err(error) => tracing::warn!("그룹 알림 참여 실패: {error}"),
                    }
                }
                let endpoints: Vec<String> =
                    group.others(&own).map(|m| m.endpoint.clone()).collect();
                if let Some(subscription) = subscriptions.get(library) {
                    if introduced.get(library) != Some(&endpoints) {
                        let _ = subscription.sender().add_peers(group.endpoints(&own)).await;
                        introduced.insert(library.clone(), endpoints);
                    }
                }
            }
            *self.announcers.lock().unwrap() = subscriptions
                .iter()
                .map(|(id, s)| (id.clone(), s.sender()))
                .collect();
            // 연결이 끊긴 구성원도 변경 알림을 받으면 재시도 대기 없이 다시 접속한다.
            let versions = self.versions.lock().unwrap().clone();
            for (library, _) in &groups {
                let version = versions.get(library).copied().unwrap_or(0);
                if announced.get(library) != Some(&version) {
                    self.announce(
                        library,
                        Announcement::Changed {
                            endpoint: own.clone(),
                        },
                    );
                    announced.insert(library.clone(), version);
                }
            }
            let wanted = self.targets();
            let keys: HashSet<_> = wanted
                .iter()
                .filter_map(|p| p["key"].as_str().map(str::to_owned))
                .collect();
            links.retain(|key, task| keys.contains(key) && !task.is_finished());
            for peer in wanted {
                let key = peer["key"].as_str().unwrap_or("").to_string();
                if let std::collections::hash_map::Entry::Vacant(entry) = links.entry(key) {
                    let rt = self.clone();
                    let node = node.clone();
                    let closing = closing.clone();
                    entry.insert(tokio::spawn(async move {
                        rt.link(node, peer, closing).await;
                    }));
                }
            }
            tokio::select! {_=closing.cancelled()=>break,_=tokio::time::sleep(Duration::from_secs(5))=>{},_=self.wake.notified()=>{}}
            self.wake.notify_waiters();
        }
        drop(subscriptions);
        self.announcers.lock().unwrap().clear();
        node.close().await;
        for (_, task) in links {
            let _ = task.await;
        }
        *self.node.lock().unwrap() = None;
        Ok(())
    }
    fn targets(&self) -> Vec<Value> {
        let own = self.own();
        self.state.lock().unwrap()["libraries"].as_array().into_iter().flatten().flat_map(|library|{
            let id=library["id"].as_str().unwrap_or("").to_string();
            group_of(library).map(|group|group.others(&own).map(|peer|json!({"key":format!("{id}:{}:{}",peer.endpoint,peer.vault),"local":id,"remote":peer.vault,"endpoint":peer.endpoint,"storage":peer.storage})).collect::<Vec<_>>()).unwrap_or_default()
        }).collect()
    }
    fn members(&self, library: &str) -> Vec<Member> {
        self.state.lock().unwrap()["libraries"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|l| l["id"].as_str() == Some(library))
            .and_then(group_of)
            .map(|g| g.members)
            .unwrap_or_default()
    }
    async fn link(
        self: Arc<Self>,
        node: Node,
        peer: Value,
        closing: tokio_util::sync::CancellationToken,
    ) {
        let key = peer["key"].as_str().unwrap_or("").to_string();
        let local = peer["local"].as_str().unwrap_or("").to_string();
        let remote = peer["remote"].as_str().unwrap_or("").to_string();
        let endpoint = peer["endpoint"].as_str().unwrap_or("").to_string();
        let mut connection: Option<Connection> = None;
        let mut seen = None;
        let mut delay = 5;
        loop {
            if closing.is_cancelled() || !self.targets().iter().any(|p| p["key"] == key) {
                break;
            }
            let version = *self.versions.lock().unwrap().get(&local).unwrap_or(&0);
            if connection
                .as_ref()
                .is_some_and(|c| c.close_reason().is_some())
            {
                connection = None;
                seen = None;
                self.links.lock().unwrap().remove(&key);
            }
            let result = async {
                if connection.is_none() {
                    self.peer_status
                        .lock()
                        .unwrap()
                        .insert(key.clone(), json!({"phase":"connecting"}));
                    let id = endpoint
                        .parse()
                        .map_err(|_| "기기 공개키 형식 오류".to_string())?;
                    let conn = node.connect(lof_realtime::EndpointAddr::new(id)).await?;
                    let reply = lof_realtime::request(
                        &conn,
                        Message::json(
                            json!({"op":"hello","vault":local,"target":remote,"storage":true,"members":self.members(&local)}),
                        ),
                    )
                    .await?;
                    let members: Vec<Member> =
                        serde_json::from_value(reply.meta["members"].clone()).unwrap_or_default();
                    let rt = self.clone();
                    let scope = local.clone();
                    let _ = tokio::task::spawn_blocking(move || rt.absorb(&scope, &members)).await;
                    if reply.meta["removed"] == true {
                        conn.close(0u8.into(), b"removed");
                        return Err("이 서재가 동기화 그룹에서 제거되었습니다.".to_string());
                    }
                    self.links
                        .lock()
                        .unwrap()
                        .insert(key.clone(), (remote.clone(), conn.clone()));
                    connection = Some(conn);
                    seen = None;
                }
                if seen == Some(version) {
                    return Ok::<(), String>(());
                }
                let conn = connection.as_ref().unwrap();
                if peer["storage"] == true && node.endpoint.id().to_string() < endpoint {
                    let _activity = self.activity(&local)?.read_owned().await;
                    if !self.targets().iter().any(|p| p["key"] == key) {
                        return Err("이 서재의 동기화 연결이 해제되었습니다.".into());
                    }
                    let owner = format!("outgoing:{}", Uuid::new_v4());
                    {
                        let mut leases = self.leases.lock().unwrap();
                        leases.retain(|_, (_, until)| *until > Instant::now());
                        if leases.contains_key(&local) {
                            return Err("다른 동기화가 진행 중입니다.".into());
                        }
                        leases.insert(
                            local.clone(),
                            (owner.clone(), Instant::now() + Duration::from_secs(180)),
                        );
                    }
                    let guard = OutgoingLease {
                        peer: self.clone(),
                        library: local.clone(),
                        owner: owner.clone(),
                    };
                    self.peer_status
                        .lock()
                        .unwrap()
                        .insert(key.clone(), json!({"phase":"syncing"}));
                    let rt = self.clone();
                    let scope = local.clone();
                    let conflicts = lof_realtime::replica::synchronize(
                        conn,
                        &self.base.join(&local),
                        &local,
                        &remote,
                        &key,
                        move || {
                            let mut leases = rt.leases.lock().unwrap();
                            let lease = leases
                                .get_mut(&scope)
                                .filter(|(id, _)| id == &owner)
                                .ok_or("동기화 세션이 변경되었습니다.")?;
                            lease.1 = Instant::now() + Duration::from_secs(180);
                            Ok(())
                        },
                    )
                    .await?;
                    drop(guard);
                    let time = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    self.peer_status.lock().unwrap().insert(
                        key.clone(),
                        json!({"phase":"connected","lastSync":time,"conflicts":conflicts}),
                    );
                    self.statistics
                        .lock()
                        .unwrap()
                        .entry(local.clone())
                        .or_insert_with(|| json!({}))["lastSync"] = json!(time);
                    let rt = self.clone();
                    let scope = local.clone();
                    let _ =
                        tokio::task::spawn_blocking(move || rt.refresh_statistics(&scope)).await;
                } else {
                    lof_realtime::request(
                        conn,
                        Message::json(json!({"op":"notify","vault":local,"target":remote})),
                    )
                    .await?;
                    self.peer_status
                        .lock()
                        .unwrap()
                        .insert(key.clone(), json!({"phase":"connected"}));
                }
                seen = Some(version);
                Ok(())
            };
            let outcome = tokio::select! {_=closing.cancelled()=>break,result=result=>result};
            let wait = if let Err(error) = outcome {
                self.peer_status
                    .lock()
                    .unwrap()
                    .insert(key.clone(), json!({"phase":"error","message":error}));
                if let Some(conn) = connection.take() {
                    conn.close(1u8.into(), b"retry");
                }
                self.links.lock().unwrap().remove(&key);
                seen = None;
                let wait = delay;
                delay = (delay * 2).min(60);
                wait
            } else {
                delay = 5;
                5
            };
            tokio::select! {_=closing.cancelled()=>break,_=tokio::time::sleep(Duration::from_secs(wait))=>{},_=self.wake.notified()=>{}}
        }
        self.links.lock().unwrap().remove(&key);
        if let Some(connection) = connection {
            connection.close(0u8.into(), b"stopped");
        }
    }
    /// 일회용 토큰을 확인한 뒤 가입한 장치를 서명해 구성원에 추가하고 그룹 정보를 돌려준다.
    fn join(&self, peer: &str, message: &Message) -> Result<Message> {
        let req = &message.meta;
        if !message.data.is_empty() {
            return Err("가입 요청 형식 오류".into());
        }
        let token = req["token"].as_str().unwrap_or("");
        let mut invites = self.invites.lock().unwrap();
        let id = invites
            .iter()
            .find(|(_, (saved, until))| {
                Instant::now() < *until
                    && saved.len() == token.len()
                    && bool::from(subtle::ConstantTimeEq::ct_eq(
                        saved.as_bytes(),
                        token.as_bytes(),
                    ))
            })
            .map(|(id, _)| id.clone())
            .ok_or("페어링 코드가 만료되었거나 올바르지 않습니다.")?;
        let activity = self.activity(&id)?;
        let _activity = activity
            .try_read()
            .map_err(|_| "서재 삭제 중입니다.".to_string())?;
        let remote = req["vault"]
            .as_str()
            .filter(|v| Uuid::parse_str(v).is_ok())
            .ok_or("서재 식별자 오류")?;
        let name = req["name"].as_str().unwrap_or("Lof Writer").to_string();
        let storage = req["storage"].as_bool().unwrap_or(false);
        let (group, title) = self.edit_group(&id, |group, library| {
            let current = group
                .as_mut()
                .filter(|g| req["group"].as_str() == Some(g.id().as_str()))
                .ok_or("코드를 만든 뒤 동기화 그룹이 변경되었습니다. 새 코드를 만들어 주세요.")?;
            current.update(&self.key, peer, remote, &name, storage, false);
            Ok((current.clone(), library["name"].clone()))
        })?;
        invites.remove(&id);
        drop(invites);
        self.announce(
            &id,
            Announcement::Members {
                members: group.members.clone(),
            },
        );
        self.dirty.lock().unwrap().insert(id.clone());
        self.wake.notify_waiters();
        Ok(Message::json(
            json!({"group":group.id(),"secret":group.secret,"members":group.members,"vault":id,"name":title,"storage":true}),
        ))
    }
}
impl Host for Realtime {
    fn connected(&self, peer: &str, session: &str, connection: Connection) {
        self.connections
            .lock()
            .unwrap()
            .insert(session.into(), (peer.into(), connection));
    }
    fn disconnected(&self, session: &str) {
        self.connections.lock().unwrap().remove(session);
        self.leases
            .lock()
            .unwrap()
            .retain(|_, (owner, _)| owner != session);
    }
    fn handle(&self, peer: &str, session: &str, message: Message) -> Result<Message> {
        let req = &message.meta;
        let op = req["op"].as_str().unwrap_or("");
        if op == "join" {
            return self.join(peer, &message);
        }
        let target = req["target"].as_str().ok_or("대상 서재가 없습니다.")?;
        if Uuid::parse_str(target).is_err() {
            return Err("서재 식별자 오류".into());
        }
        // 삭제 대기 중에는 새 요청을 거부한다. 비교 작업이 보낸 요청을 기다리는
        // 동안 여기서 잠금을 기다리면 양쪽 피어의 삭제와 비교가 서로 막힐 수 있다.
        let activity = self.activity(target)?;
        let _activity = activity
            .try_read()
            .map_err(|_| "서재 삭제 중입니다. 동기화를 다시 연결해 주세요.".to_string())?;
        if op == "hello" || op == "members" {
            // 처음 보는 장치도 현재 구성원이 서명한 가입 기록을 가져오면 인증할 수 있다.
            let incoming: Vec<Member> =
                serde_json::from_value(req["members"].clone()).unwrap_or_default();
            self.absorb(target, &incoming)?;
        }
        {
            let state = self.state.lock().unwrap();
            let library = state["libraries"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|v| v["id"].as_str() == Some(target))
                .ok_or("서재를 찾을 수 없습니다.")?;
            let group = group_of(library);
            let allowed = group
                .as_ref()
                .and_then(|g| g.member(peer))
                .is_some_and(|m| req["vault"].as_str() == Some(m.vault.as_str()));
            if !allowed {
                // 제거된 장치에는 제거 기록을 돌려주어 스스로 그룹에서 나가게 한다.
                if let Some(group) = group.filter(|g| {
                    op == "hello" && g.members.iter().any(|m| m.endpoint == peer && m.removed)
                }) {
                    return Ok(Message::json(
                        json!({"removed":true,"members":group.members}),
                    ));
                }
                return Err("이 서재의 동기화 그룹에 없는 장치입니다.".into());
            }
            if op == "hello" {
                return Ok(Message::json(
                    json!({"storage":true,"members":group.map(|g|g.members).unwrap_or_default()}),
                ));
            }
        }
        if op == "members" {
            return Ok(Message::json(json!(true)));
        }
        if op == "notify" {
            *self
                .versions
                .lock()
                .unwrap()
                .entry(target.into())
                .or_default() += 1;
            self.wake.notify_waiters();
            return Ok(Message::json(json!(true)));
        }
        let mut leases = self.leases.lock().unwrap();
        leases.retain(|_, (_, until)| Instant::now() < *until);
        if op == "begin" {
            if leases.contains_key(target) {
                return Err("다른 기기가 동기화 중입니다.".into());
            }
            leases.insert(
                target.into(),
                (session.into(), Instant::now() + Duration::from_secs(180)),
            );
            return Ok(Message::json(json!(true)));
        }
        if !leases
            .get(target)
            .is_some_and(|(owner, _)| owner == session)
        {
            return Err("동기화 세션을 다시 연결해 주세요.".into());
        }
        leases.get_mut(target).unwrap().1 = Instant::now() + Duration::from_secs(180);
        if op == "end" {
            if req["success"] != false {
                self.statistics
                    .lock()
                    .unwrap()
                    .entry(target.into())
                    .or_insert_with(|| json!({}))["lastSync"] = json!(std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs());
            }
            leases.remove(target);
            return Ok(Message::json(json!(true)));
        }
        if op == "touch" {
            return Ok(Message::json(json!(true)));
        }
        let root = self.base.join(target);
        if op == "inventory" {
            let mut inventory = storage::inventory(&root)?;
            inventory[".lof-sync-id"] = json!({"size":36,"modified":null,"etag":null});
            return Ok(Message::json(inventory));
        }
        let path = req["path"].as_str().ok_or("파일 경로가 없습니다.")?;
        if op == "read" && path == ".lof-sync-id" {
            return Ok(Message {
                meta: json!(null),
                data: target.as_bytes().to_vec(),
            });
        }
        match op {
            "read" => Ok(Message {
                meta: json!(null),
                data: storage::read(&root, path)?.ok_or("파일이 변경되었습니다.")?,
            }),
            "exists" => Ok(Message::json(json!(storage::path(&root, path)?
                .try_exists()
                .map_err(|e| e.to_string())?))),
            "write" | "remove" => {
                storage::apply(
                    &root,
                    path,
                    if op == "write" {
                        Some(&message.data)
                    } else {
                        None
                    },
                    req["expected"].as_str(),
                )?;
                self.dirty.lock().unwrap().insert(target.into());
                Ok(Message::json(json!(true)))
            }
            _ => Err("지원하지 않는 실시간 요청입니다.".into()),
        }
    }
}

struct OutgoingLease {
    peer: Arc<Realtime>,
    library: String,
    owner: String,
}
impl Drop for OutgoingLease {
    fn drop(&mut self) {
        let mut leases = self.peer.leases.lock().unwrap();
        if leases
            .get(&self.library)
            .is_some_and(|(owner, _)| owner == &self.owner)
        {
            leases.remove(&self.library);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        rt: Arc<Realtime>,
        _directory: TestDirectory,
    }
    struct TestDirectory(PathBuf);
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    impl Fixture {
        fn new() -> Self {
            let directory =
                std::env::temp_dir().join(format!("lof-delete-test-{}", Uuid::new_v4()));
            let store = Arc::new(Store::open(&directory).unwrap());
            let rt = Realtime::new(directory.join("realtime"), store).unwrap();
            Self {
                rt,
                _directory: TestDirectory(directory),
            }
        }
        fn library(&self) -> String {
            let library = self.rt.create_library("테스트 서재").unwrap();
            let id = library["id"].as_str().unwrap().to_string();
            std::fs::write(self.rt.base.join(&id).join("원고.md"), "보존할 원고").unwrap();
            id
        }
        fn group(&self, id: &str) -> (String, String) {
            let key = SecretKey::generate();
            let peer = key.public().to_string();
            let vault = Uuid::new_v4().to_string();
            self.rt
                .edit_group(id, |group, _| {
                    let mut current = Group::create(&self.rt.key, id, "Library", true);
                    current.update(&self.rt.key, &peer, &vault, "Writer", false, false);
                    *group = Some(current);
                    Ok(())
                })
                .unwrap();
            (peer, vault)
        }
        fn persisted(&self) -> Value {
            serde_json::from_str(&read_key(&self.rt.store, "realtime-v2").unwrap().unwrap())
                .unwrap()
        }
    }
    #[tokio::test]
    async fn deletes_only_selected_library_and_persists_removal() {
        let fixture = Fixture::new();
        let id = fixture.library();
        let other = fixture.library();
        fixture.rt.invites.lock().unwrap().insert(
            id.clone(),
            ("token".into(), Instant::now() + Duration::from_secs(60)),
        );
        fixture.rt.leases.lock().unwrap().insert(
            id.clone(),
            ("session".into(), Instant::now() + Duration::from_secs(60)),
        );
        assert_eq!(
            crate::control::action(fixture.rt.clone(), "delete", json!({"id":id}))
                .await
                .unwrap(),
            true
        );
        assert!(!fixture.rt.base.join(&id).exists());
        assert_eq!(
            std::fs::read_to_string(fixture.rt.base.join(&other).join("원고.md")).unwrap(),
            "보존할 원고"
        );
        assert_eq!(
            fixture.persisted()["libraries"].as_array().unwrap().len(),
            1
        );
        assert!(fixture.rt.library_path(&id).is_err());
        assert!(!fixture.rt.invites.lock().unwrap().contains_key(&id));
        assert!(!fixture.rt.leases.lock().unwrap().contains_key(&id));
    }

    #[tokio::test]
    async fn grouped_delete_rejects_existing_peer_session() {
        let fixture = Fixture::new();
        let id = fixture.library();
        let (peer, vault) = fixture.group(&id);
        fixture
            .rt
            .handle(
                &peer,
                "session",
                Message::json(json!({"op":"begin","vault":vault,"target":id})),
            )
            .unwrap();
        fixture.rt.delete_library(&id).await.unwrap();
        for op in ["inventory", "read", "write", "remove", "begin"] {
            assert!(fixture
                .rt
                .handle(
                    &peer,
                    "session",
                    Message::json(json!({"op":op,"vault":vault,"target":id,"path":"원고.md"}))
                )
                .is_err());
        }
        assert!(!fixture.rt.base.join(&id).exists());
        assert!(fixture.persisted()["libraries"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn waits_for_file_activity_then_rejects_new_requests() {
        let fixture = Fixture::new();
        let id = fixture.library();
        let (peer, vault) = fixture.group(&id);
        let other = fixture.library();
        let (other_peer, other_vault) = fixture.group(&other);
        let activity = fixture.rt.activity(&id).unwrap().read_owned().await;
        let deletion = fixture.rt.delete_library(&id);
        tokio::pin!(deletion);
        // poll 한 뒤에도 파일 작업이 진행 중이면 삭제가 시작되지 않아야 한다.
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut deletion)
                .await
                .is_err()
        );
        assert!(fixture.rt.base.join(&id).join("원고.md").exists());
        assert!(fixture
            .rt
            .handle(
                &peer,
                "session",
                Message::json(json!({"op":"begin","vault":vault,"target":id}))
            )
            .is_err());
        assert!(fixture
            .rt
            .handle(
                &other_peer,
                "other-session",
                Message::json(json!({"op":"begin","vault":other_vault,"target":other}))
            )
            .is_ok());
        drop(activity);
        deletion.await.unwrap();
        assert!(!fixture.rt.base.join(&id).exists());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn failed_folder_removal_keeps_detached_library_for_retry() {
        use std::os::windows::fs::OpenOptionsExt;
        let fixture = Fixture::new();
        let id = fixture.library();
        fixture.group(&id);
        let root = fixture.rt.base.join(&id);
        let locked = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(root.join("원고.md"))
            .unwrap();
        assert!(fixture.rt.delete_library(&id).await.is_err());
        assert!(root.join("원고.md").is_file());
        assert_eq!(fixture.persisted()["libraries"][0]["group"], Value::Null);
        assert!(fixture.rt.targets().is_empty());
        drop(locked);
        fixture.rt.delete_library(&id).await.unwrap();
        assert!(fixture.persisted()["libraries"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn rejects_unknown_and_traversal_ids_and_handles_missing_folder() {
        let fixture = Fixture::new();
        let id = fixture.library();
        for invalid in ["", "..", "../realtime", &Uuid::new_v4().to_string()] {
            assert!(fixture.rt.delete_library(invalid).await.is_err());
        }
        assert!(fixture.rt.base.join(&id).join("원고.md").exists());
        std::fs::remove_file(fixture.rt.base.join(&id).join("원고.md")).unwrap();
        std::fs::remove_dir(fixture.rt.base.join(&id)).unwrap();
        fixture.rt.delete_library(&id).await.unwrap();
        assert!(fixture.persisted()["libraries"]
            .as_array()
            .unwrap()
            .is_empty());
    }
}
