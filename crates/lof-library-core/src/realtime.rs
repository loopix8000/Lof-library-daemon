//! Iroh로 원본 파일을 교환하는 상시 서재 피어.
use crate::store::Store;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use iroh::SecretKey;
use iroh::Watcher as _;
use lof_realtime::{storage, Connection, Host, Message, Node};
use notify::Watcher;
use rusqlite::OptionalExtension;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use uuid::Uuid;
type Result<T> = std::result::Result<T, String>;

pub struct Realtime {
    store: Arc<Store>,
    base: PathBuf,
    state: Mutex<Value>,
    node: Mutex<Option<Node>>,
    error: Mutex<String>,
    invites: Mutex<HashMap<String, (String, Instant)>>,
    leases: Mutex<HashMap<String, (String, Instant)>>,
    connections: Mutex<HashMap<String, (String, Connection)>>,
    dirty: Mutex<HashSet<String>>,
    statistics: Mutex<HashMap<String, Value>>,
    peer_status: Mutex<HashMap<String, Value>>,
    versions: Mutex<HashMap<String, u64>>,
    wake: tokio::sync::Notify,
}
impl Realtime {
    pub fn new(base: PathBuf, store: Arc<Store>) -> Result<Arc<Self>> {
        let saved = store
            .read(|db| {
                Ok(db
                    .query_row(
                        "SELECT value FROM server WHERE key='realtime-v1'",
                        [],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?)
            })
            .map_err(|e| e.to_string())?;
        let state = match saved {
            Some(text) => {
                serde_json::from_str(&text).map_err(|_| "실시간 동기화 설정을 읽지 못했습니다.")?
            }
            None => json!({"secret":hex::encode(SecretKey::generate().to_bytes()),"libraries":[]}),
        };
        let rt = Arc::new(Self {
            store,
            base,
            state: Mutex::new(state),
            node: Mutex::new(None),
            error: Mutex::new(String::new()),
            invites: Mutex::new(HashMap::new()),
            leases: Mutex::new(HashMap::new()),
            connections: Mutex::new(HashMap::new()),
            dirty: Mutex::new(HashSet::new()),
            statistics: Mutex::new(HashMap::new()),
            peer_status: Mutex::new(HashMap::new()),
            versions: Mutex::new(HashMap::new()),
            wake: tokio::sync::Notify::new(),
        });
        rt.save(&rt.state.lock().unwrap())?;
        Ok(rt)
    }
    fn save(&self, state: &Value) -> Result<()> {
        self.store.write(|db|{db.execute("INSERT INTO server(key,value) VALUES('realtime-v1',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[state.to_string()])?;Ok(())}).map_err(|e|e.to_string())
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
        let statuses = self.peer_status.lock().unwrap().clone();
        let state = self.state.lock().unwrap();
        let connections = self.connections.lock().unwrap();
        let leases = self.leases.lock().unwrap();
        let statistics = self.statistics.lock().unwrap();
        let libraries:Vec<_>=state["libraries"].as_array().into_iter().flatten().map(|library|{
            let peers:Vec<_>=library["peers"].as_array().into_iter().flatten().map(|peer|json!({"id":peer["endpoint"],"remoteVault":peer["vault"],"name":peer["name"],"storage":peer["storage"],"status":statuses.get(&format!("{}:{}:{}",library["id"].as_str().unwrap_or(""),peer["endpoint"].as_str().unwrap_or(""),peer["vault"].as_str().unwrap_or(""))),"online":connections.values().any(|(id,c)|Some(id.as_str())==peer["endpoint"].as_str()&&c.close_reason().is_none())})).collect();
            let id=library["id"].as_str().unwrap_or("");
            json!({"id":id,"name":library["name"],"path":self.base.join(id),"peers":peers,"syncing":leases.get(id).is_some_and(|(_,until)|*until>Instant::now()),"statistics":statistics.get(id)})
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
        json!({"pendingInvites":pending,"ready":node.is_some(),"endpoint":node.as_ref().map(|n|n.endpoint.id().to_string()),"relayConnected":relay,"libraries":libraries,"error":*self.error.lock().unwrap()})
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
            .push(json!({"id":id,"name":name,"peers":[]}));
        self.save(&next)?;
        *state = next;
        drop(state);
        self.dirty.lock().unwrap().insert(id.clone());
        self.wake.notify_waiters();
        Ok(json!({"id":id,"name":name,"path":directory}))
    }
    pub fn rename_library(&self, id: &str, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 200 {
            return Err("서재 이름을 1~200자로 입력해 주세요.".into());
        }
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
        let peer = library["peers"]
            .as_array_mut()
            .ok_or("기기 설정 오류")?
            .iter_mut()
            .find(|p| p["endpoint"].as_str() == Some(id))
            .ok_or("기기를 찾을 수 없습니다.")?;
        peer["name"] = json!(name);
        self.save(&next)?;
        *state = next;
        Ok(())
    }
    pub async fn pair(&self, args: Value) -> Result<Value> {
        let node = self
            .node
            .lock()
            .unwrap()
            .clone()
            .ok_or("피어를 먼저 시작해 주세요.")?;
        let code = args["code"].as_str().unwrap_or("").trim();
        if code.len() > 16_384 {
            return Err("연결 코드가 너무 깁니다.".into());
        }
        let ticket: Value = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(
                    code.strip_prefix("lof-peer://")
                        .ok_or("Iroh 연결 코드를 입력해 주세요.")?,
                )
                .map_err(|_| "연결 코드 형식 오류")?,
        )
        .map_err(|_| "연결 코드 형식 오류")?;
        if ticket["version"] != 1
            || Uuid::parse_str(ticket["vault"].as_str().unwrap_or("")).is_err()
        {
            return Err("지원하지 않는 연결 코드입니다.".into());
        }
        let addr: lof_realtime::EndpointAddr =
            serde_json::from_value(ticket["addr"].clone()).map_err(|_| "장치 주소 형식 오류")?;
        if addr.id == node.endpoint.id() {
            return Err("같은 장치는 연결할 수 없습니다.".into());
        }
        let local = args["id"]
            .as_str()
            .ok_or("연결할 서재를 선택해 주세요.")?
            .to_string();
        self.library_path(&local)?;
        let connection = node.connect(addr).await?;
        let response=lof_realtime::request(&connection,Message::json(json!({"op":"pair","vault":local,"token":ticket["token"],"name":"Lof Library","storage":true}))).await?;
        if response.meta["vault"] != ticket["vault"] {
            connection.close(1u8.into(), b"vault mismatch");
            return Err("연결 대상 서재가 코드와 다릅니다.".into());
        }
        let endpoint = connection.remote_id().to_string();
        let mut state = self.state.lock().unwrap();
        let mut next = state.clone();
        let library = next["libraries"]
            .as_array_mut()
            .ok_or("서재 설정 오류")?
            .iter_mut()
            .find(|l| l["id"] == local)
            .ok_or("서재가 변경되었습니다.")?;
        let peers = library["peers"].as_array_mut().ok_or("기기 설정 오류")?;
        peers.retain(|p| p["endpoint"] != endpoint || p["vault"] != ticket["vault"]);
        peers.push(json!({"endpoint":endpoint,"vault":ticket["vault"],"name":response.meta["name"].as_str().unwrap_or("다른 기기"),"storage":response.meta["storage"].as_bool().unwrap_or(false)}));
        self.save(&next)?;
        *state = next;
        drop(state);
        self.dirty.lock().unwrap().insert(local);
        self.wake.notify_waiters();
        connection.close(0u8.into(), b"paired");
        Ok(json!(true))
    }
    fn refresh_statistics(&self, id: &str) {
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
    pub fn invite(&self, name: &str, existing: Option<&str>) -> Result<Value> {
        let node = self
            .node
            .lock()
            .unwrap()
            .clone()
            .ok_or("실시간 연결을 준비 중입니다.")?;
        let mut state = self.state.lock().unwrap();
        let mut next = state.clone();
        let id = if let Some(id) = existing {
            if !next["libraries"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|v| v["id"].as_str() == Some(id))
            {
                return Err("서재를 찾을 수 없습니다.".into());
            }
            id.to_string()
        } else {
            let name = name.trim();
            if name.is_empty() || name.chars().count() > 200 {
                return Err("서재 이름을 1~200자로 입력해 주세요.".into());
            }
            let id = Uuid::new_v4().to_string();
            next["libraries"]
                .as_array_mut()
                .ok_or("서재 설정 오류")?
                .push(json!({"id":id,"name":name,"peers":[]}));
            id
        };
        let directory = self.base.join(&id);
        std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
        self.save(&next)?;
        *state = next;
        drop(state);
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        self.invites.lock().unwrap().insert(
            id.clone(),
            (token.clone(), Instant::now() + Duration::from_secs(600)),
        );
        let ticket = json!({"version":1,"addr":node.endpoint.addr(),"vault":id,"token":token});
        let code = format!(
            "lof-peer://{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&ticket).map_err(|e| e.to_string())?)
        );
        Ok(json!({"id":id,"code":code,"expiresIn":600}))
    }
    pub fn remove_peer(&self, vault: &str, peer: &str) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let mut next = state.clone();
        let library = next["libraries"]
            .as_array_mut()
            .ok_or("서재 설정 오류")?
            .iter_mut()
            .find(|l| l["id"].as_str() == Some(vault))
            .ok_or("서재가 없습니다.")?;
        library["peers"]
            .as_array_mut()
            .ok_or("기기 설정 오류")?
            .retain(|p| p["endpoint"].as_str() != Some(peer));
        self.save(&next)?;
        *state = next;
        Ok(())
    }
    pub async fn run(self: Arc<Self>, closing: tokio_util::sync::CancellationToken) {
        let result = self.clone().serve(closing).await;
        let node = self.node.lock().unwrap().take();
        if let Some(node) = node {
            node.endpoint.close().await;
        }
        if let Err(error) = result {
            tracing::error!("실시간 피어 시작 실패: {error}");
            *self.error.lock().unwrap() = error;
        }
    }
    async fn serve(self: Arc<Self>, closing: tokio_util::sync::CancellationToken) -> Result<()> {
        std::fs::create_dir_all(&self.base).map_err(|e| e.to_string())?;
        let secret = self.state.lock().unwrap()["secret"]
            .as_str()
            .ok_or("장치 키가 없습니다.")?
            .parse::<SecretKey>()
            .map_err(|e| e.to_string())?;
        let node = Node::bind(secret, self.clone()).await?;
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
        let mut links: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
        loop {
            let dirty = std::mem::take(&mut *self.dirty.lock().unwrap());
            for id in dirty {
                let rt = self.clone();
                let name = id.clone();
                let _ = tokio::task::spawn_blocking(move || rt.refresh_statistics(&name)).await;
                *self.versions.lock().unwrap().entry(id).or_default() += 1;
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
        node.endpoint.close().await;
        for (_, task) in links {
            let _ = task.await;
        }
        *self.node.lock().unwrap() = None;
        Ok(())
    }
    fn targets(&self) -> Vec<Value> {
        self.state.lock().unwrap()["libraries"].as_array().into_iter().flatten().flat_map(|library|library["peers"].as_array().into_iter().flatten().map(move|peer|json!({"key":format!("{}:{}:{}",library["id"].as_str().unwrap_or(""),peer["endpoint"].as_str().unwrap_or(""),peer["vault"].as_str().unwrap_or("")),"local":library["id"],"remote":peer["vault"],"endpoint":peer["endpoint"],"storage":peer["storage"]}))).collect()
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
                    lof_realtime::request(
                        &conn,
                        Message::json(
                            json!({"op":"hello","vault":local,"target":remote,"storage":true}),
                        ),
                    )
                    .await?;
                    connection = Some(conn);
                    seen = None;
                }
                if seen == Some(version) {
                    return Ok::<(), String>(());
                }
                let conn = connection.as_ref().unwrap();
                if peer["storage"] == true && node.endpoint.id().to_string() < endpoint {
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
        if let Some(connection) = connection {
            connection.close(0u8.into(), b"stopped");
        }
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
        if op == "pair" {
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
                .ok_or("연결 코드가 만료되었거나 올바르지 않습니다.")?;
            let remote = req["vault"]
                .as_str()
                .filter(|v| Uuid::parse_str(v).is_ok())
                .ok_or("서재 식별자 오류")?;
            let mut state = self.state.lock().unwrap();
            let mut next = state.clone();
            let library = next["libraries"]
                .as_array_mut()
                .ok_or("서재 설정 오류")?
                .iter_mut()
                .find(|v| v["id"] == id)
                .ok_or("서재가 없습니다.")?;
            let name = req["name"]
                .as_str()
                .unwrap_or("Lof Writer")
                .chars()
                .take(100)
                .collect::<String>();
            let peers = library["peers"].as_array_mut().ok_or("기기 설정 오류")?;
            peers.retain(|p| {
                p["endpoint"].as_str() != Some(peer) || p["vault"].as_str() != Some(remote)
            });
            peers.push(json!({"endpoint":peer,"vault":remote,"name":name,"storage":req["storage"].as_bool().unwrap_or(false)}));
            let title = library["name"].clone();
            self.save(&next)?;
            *state = next;
            invites.remove(&id);
            self.wake.notify_waiters();
            return Ok(Message::json(
                json!({"vault":id,"name":title,"storage":true}),
            ));
        }
        let target = req["target"].as_str().ok_or("대상 서재가 없습니다.")?;
        if Uuid::parse_str(target).is_err() {
            return Err("서재 식별자 오류".into());
        }
        {
            let state = self.state.lock().unwrap();
            let library = state["libraries"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|v| v["id"].as_str() == Some(target))
                .ok_or("서재를 찾을 수 없습니다.")?;
            if !library["peers"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|p| p["endpoint"].as_str() == Some(peer) && p["vault"] == req["vault"])
            {
                return Err("이 서재에 등록된 장치가 아닙니다.".into());
            }
        }
        if op == "hello" {
            return Ok(Message::json(json!({"storage":true})));
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
