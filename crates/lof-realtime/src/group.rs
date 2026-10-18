//! 페어링 코드 한 번으로 서재 그룹에 가입하고, 서명한 구성원 목록을 그룹 안에서 공유한다.
//!
//! 그룹 비밀값은 가입 응답으로만 전달되며 페어링 코드에는 넣지 않는다. Gossip 주제는 비밀값에서
//! 만들어 구성원만 참여할 수 있게 하고, 구성원 기록은 현재 구성원이 서명한 것만 받아들인다.
use crate::{EndpointAddr, EndpointId, Node, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use iroh::{PublicKey, SecretKey, Signature};
use iroh_gossip::{
    api::{Event as GossipEvent, GossipSender},
    proto::TopicId,
};
use n0_future::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

pub const CODE_PREFIX: &str = "lof-group://";
const LEGACY_PREFIX: &str = "lof-peer://";
pub const MAX_ANNOUNCEMENT: usize = 65_536;
pub const MAX_MEMBERS: usize = 32;
pub const INVITE_SECONDS: u64 = 600;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub endpoint: String,
    pub vault: String,
    pub name: String,
    #[serde(default)]
    pub storage: bool,
    pub version: u64,
    #[serde(default)]
    pub removed: bool,
    /// 이 기록에 서명한 구성원의 공개키.
    pub by: String,
    pub sig: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    /// 32바이트 비밀값의 16진수 문자열. 저장할 때는 각 앱의 비공개 저장소 규칙을 따른다.
    pub secret: String,
    #[serde(default)]
    pub members: Vec<Member>,
}

pub struct Ticket {
    pub group: String,
    pub addr: EndpointAddr,
    pub token: String,
}

fn digest(label: &str, secret: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(label.as_bytes());
    hasher.update(secret);
    hasher.finalize().into()
}
fn payload(group: &str, member: &Member) -> Vec<u8> {
    serde_json::to_vec(&json!([
        "lof-group-member/1",
        group,
        member.endpoint,
        member.vault,
        member.name,
        member.storage,
        member.version,
        member.removed,
        member.by
    ]))
    .unwrap_or_default()
}
fn well_formed(member: &Member) -> bool {
    member.endpoint.parse::<PublicKey>().is_ok()
        && uuid::Uuid::parse_str(&member.vault).is_ok()
        && !member.name.trim().is_empty()
        && member.name.chars().count() <= 100
        && member.version > 0
        && member.sig.len() == 128
}
fn verify(group: &str, member: &Member) -> bool {
    let Ok(signer) = member.by.parse::<PublicKey>() else {
        return false;
    };
    let Some(bytes) = hex::decode(&member.sig)
        .ok()
        .and_then(|bytes| <[u8; 64]>::try_from(bytes).ok())
    else {
        return false;
    };
    signer
        .verify(&payload(group, member), &Signature::from_bytes(&bytes))
        .is_ok()
}

impl Group {
    pub fn create(key: &SecretKey, vault: &str, name: &str, storage: bool) -> Self {
        let mut group = Self {
            secret: hex::encode(SecretKey::generate().to_bytes()),
            members: Vec::new(),
        };
        group.update(key, &key.public().to_string(), vault, name, storage, false);
        group
    }
    pub fn from_secret(secret: &str) -> Result<Self> {
        if hex::decode(secret).map(|bytes| bytes.len()) != Ok(32) {
            return Err("그룹 정보 형식이 올바르지 않습니다.".into());
        }
        Ok(Self {
            secret: secret.to_ascii_lowercase(),
            members: Vec::new(),
        })
    }
    fn secret_bytes(&self) -> Vec<u8> {
        hex::decode(&self.secret).unwrap_or_default()
    }
    /// 화면과 페어링 코드에 표시하는 식별자. 비밀값을 알아도 이 값으로 되돌릴 수 없다.
    pub fn id(&self) -> String {
        hex::encode(&digest("lof-group-id/1", &self.secret_bytes())[..16])
    }
    pub fn topic(&self) -> TopicId {
        TopicId::from_bytes(digest("lof-group-topic/1", &self.secret_bytes()))
    }
    pub fn active(&self) -> impl Iterator<Item = &Member> {
        self.members.iter().filter(|member| !member.removed)
    }
    pub fn member(&self, endpoint: &str) -> Option<&Member> {
        self.active().find(|member| member.endpoint == endpoint)
    }
    /// 이 장치를 제외한 현재 구성원.
    pub fn others<'a>(&'a self, own: &'a str) -> impl Iterator<Item = &'a Member> {
        self.active().filter(move |member| member.endpoint != own)
    }
    pub fn endpoints(&self, own: &str) -> Vec<EndpointId> {
        self.others(own)
            .filter_map(|member| member.endpoint.parse().ok())
            .collect()
    }
    /// 구성원 기록을 새 버전으로 서명해 교체한다. 제거도 같은 방식으로 기록한다.
    pub fn update(
        &mut self,
        key: &SecretKey,
        endpoint: &str,
        vault: &str,
        name: &str,
        storage: bool,
        removed: bool,
    ) -> Member {
        let version = self
            .members
            .iter()
            .find(|member| member.endpoint == endpoint)
            .map_or(1, |member| member.version + 1);
        let name = name.trim().chars().take(100).collect::<String>();
        let mut member = Member {
            endpoint: endpoint.into(),
            vault: vault.into(),
            name: if name.is_empty() { "다른 장치".into() } else { name },
            storage,
            version,
            removed,
            by: key.public().to_string(),
            sig: String::new(),
        };
        member.sig = hex::encode(key.sign(&payload(&self.id(), &member)).to_bytes());
        self.replace(member.clone());
        member
    }
    fn replace(&mut self, member: Member) {
        self.members.retain(|m| m.endpoint != member.endpoint);
        self.members.push(member);
        self.members.sort_by(|a, b| a.endpoint.cmp(&b.endpoint));
    }
    /// 서명이 올바르고 현재 구성원이 서명한 기록만 반영한다. `trusted`는 가입 응답처럼
    /// 인증된 초대 장치가 보낸 첫 목록에만 사용한다. 반영한 기록이 있으면 true를 반환한다.
    pub fn merge(&mut self, incoming: &[Member], trusted: bool) -> bool {
        let id = self.id();
        let mut changed = false;
        loop {
            let mut progressed = false;
            for record in incoming.iter().take(MAX_MEMBERS * 4) {
                if !well_formed(record) || !verify(&id, record) {
                    continue;
                }
                if !trusted && self.member(&record.by).is_none() {
                    continue;
                }
                let existing = self.members.iter().find(|m| m.endpoint == record.endpoint);
                let newer = existing.is_none_or(|current| {
                    (record.version, record.removed, &record.sig)
                        > (current.version, current.removed, &current.sig)
                });
                if newer && (existing.is_some() || self.members.len() < MAX_MEMBERS) {
                    self.replace(record.clone());
                    progressed = true;
                    changed = true;
                }
            }
            if !progressed {
                break;
            }
        }
        changed
    }
    pub fn code(&self, addr: &EndpointAddr, token: &str) -> Result<String> {
        let ticket = json!({"version":2,"group":self.id(),"addr":addr,"token":token});
        Ok(format!(
            "{CODE_PREFIX}{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&ticket).map_err(|e| e.to_string())?)
        ))
    }
}

/// 페어링 코드는 그룹 식별자, 초대 장치의 연결 주소와 일회용 토큰만 담는다.
pub fn parse_code(code: &str) -> Result<Ticket> {
    let code = code.trim();
    if code.starts_with(LEGACY_PREFIX) {
        return Err("이전 방식의 연결 코드입니다. 두 장치를 최신 버전으로 업데이트한 뒤 새 페어링 코드를 만들어 주세요.".into());
    }
    let invalid = || {
        "페어링 코드 형식이 올바르지 않습니다. 다른 장치에서 복사한 코드 전체를 붙여 넣어 주세요."
            .to_string()
    };
    if code.len() > 16_384 {
        return Err(invalid());
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(code.strip_prefix(CODE_PREFIX).ok_or_else(invalid)?)
        .map_err(|_| invalid())?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    let group = value["group"].as_str().unwrap_or("");
    let token = value["token"].as_str().unwrap_or("");
    if value["version"] != 2
        || group.len() != 32
        || !group.bytes().all(|b| b.is_ascii_hexdigit())
        || token.is_empty()
        || token.len() > 256
    {
        return Err(invalid());
    }
    Ok(Ticket {
        group: group.to_ascii_lowercase(),
        addr: serde_json::from_value(value["addr"].clone()).map_err(|_| invalid())?,
        token: token.into(),
    })
}

/// Gossip으로 전달하는 그룹 알림. 원고 파일은 인증된 파일 요청으로만 주고받는다.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Announcement {
    Members { members: Vec<Member> },
    Changed { endpoint: String },
}
pub enum Event {
    Announcement(Announcement),
    Neighbor(String),
}

/// 그룹 주제 구독. 값을 버리면 주제에서 나간다.
pub struct Subscription {
    pub group: String,
    sender: GossipSender,
    task: tokio::task::JoinHandle<()>,
}
impl Subscription {
    pub async fn join(
        node: &Node,
        group: &Group,
        own: &str,
        events: impl Fn(Event) + Send + Sync + 'static,
    ) -> Result<Self> {
        let topic = node
            .gossip
            .subscribe(group.topic(), group.endpoints(own))
            .await
            .map_err(|e| e.to_string())?;
        let (sender, mut receiver) = topic.split();
        let task = tokio::spawn(async move {
            while let Some(Ok(event)) = receiver.next().await {
                match event {
                    GossipEvent::Received(message) => {
                        if let Ok(value) = serde_json::from_slice(&message.content) {
                            events(Event::Announcement(value));
                        }
                    }
                    GossipEvent::NeighborUp(id) => events(Event::Neighbor(id.to_string())),
                    GossipEvent::Lagged => break,
                    GossipEvent::NeighborDown(_) => {}
                }
            }
        });
        Ok(Self {
            group: group.id(),
            sender,
            task,
        })
    }
    pub fn finished(&self) -> bool {
        self.task.is_finished()
    }
    pub fn sender(&self) -> Announcer {
        Announcer(self.sender.clone())
    }
}
impl Drop for Subscription {
    fn drop(&mut self) {
        self.task.abort();
    }
}
#[derive(Clone)]
pub struct Announcer(GossipSender);
impl Announcer {
    pub async fn announce(&self, announcement: &Announcement) -> Result<()> {
        let bytes = serde_json::to_vec(announcement).map_err(|e| e.to_string())?;
        // gossip 메시지 머리 부분이 들어갈 여유를 남긴다.
        if bytes.len() > MAX_ANNOUNCEMENT - 1024 {
            return Err("그룹 알림이 너무 큽니다.".into());
        }
        self.0
            .broadcast(bytes.into())
            .await
            .map_err(|e| e.to_string())
    }
    pub async fn add_peers(&self, peers: Vec<EndpointId>) -> Result<()> {
        if peers.is_empty() {
            return Ok(());
        }
        self.0.join_peers(peers).await.map_err(|e| e.to_string())
    }
}
