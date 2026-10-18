//! Writer와 상시 보관 피어가 함께 사용하는 인증된 원본 파일 전송 계층.
use iroh::{endpoint::presets, Endpoint, SecretKey};
pub use iroh::{endpoint::Connection, EndpointAddr, EndpointId};
use iroh_gossip::{net::GOSSIP_ALPN, Gossip};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;
pub mod group;
pub mod replica;
pub mod storage;

pub const ALPN: &[u8] = b"lof/realtime-files/1";
pub const MAX_FILE: usize = 50_000_000;
const MAX_HEADER: usize = 8_000_000;
pub type Result<T> = std::result::Result<T, String>;

#[derive(Default)]
pub struct Message {
    pub meta: Value,
    pub data: Vec<u8>,
}
impl Message {
    pub fn json(meta: Value) -> Self {
        Self {
            meta,
            data: Vec::new(),
        }
    }
}
pub trait Host: Send + Sync + 'static {
    /// peer는 QUIC에서 검증한 공개키이며 요청 본문의 값으로 대체하지 않는다.
    fn handle(&self, peer: &str, session: &str, message: Message) -> Result<Message>;
    fn disconnected(&self, _session: &str) {}
    fn connected(&self, _peer: &str, _session: &str, _connection: Connection) {}
}

#[derive(Clone)]
pub struct Node {
    pub endpoint: Endpoint,
    /// 그룹 구성원 목록과 변경 알림을 퍼뜨린다. 원고 파일은 전송하지 않는다.
    pub gossip: Gossip,
    host: Arc<dyn Host>,
    slots: Arc<Semaphore>,
}
impl Node {
    pub async fn bind(secret: SecretKey, host: Arc<dyn Host>) -> Result<Self> {
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(secret)
            .alpns(vec![ALPN.to_vec(), GOSSIP_ALPN.to_vec()])
            .bind()
            .await
            .map_err(|e| e.to_string())?;
        let gossip = Gossip::builder()
            .max_message_size(group::MAX_ANNOUNCEMENT)
            .spawn(endpoint.clone());
        let node = Self {
            endpoint,
            gossip,
            host,
            slots: Arc::new(Semaphore::new(24)),
        };
        let acceptor = node.clone();
        tokio::spawn(async move {
            while let Some(incoming) = acceptor.endpoint.accept().await {
                let node = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(Ok(connection)) =
                        tokio::time::timeout(Duration::from_secs(20), incoming).await
                    else {
                        return;
                    };
                    if connection.alpn() == GOSSIP_ALPN {
                        let _ = node.gossip.handle_connection(connection).await;
                        return;
                    }
                    let Ok(_permit) = node.slots.clone().try_acquire_owned() else {
                        connection.close(2u8.into(), b"busy");
                        return;
                    };
                    node.serve(connection).await;
                });
            }
        });
        Ok(node)
    }
    pub async fn close(&self) {
        let _ = self.gossip.shutdown().await;
        self.endpoint.close().await;
    }
    pub async fn connect(&self, addr: EndpointAddr) -> Result<Connection> {
        let connection =
            tokio::time::timeout(Duration::from_secs(25), self.endpoint.connect(addr, ALPN))
                .await
                .map_err(|_| "피어 연결 시간이 초과되었습니다.".to_string())?
                .map_err(|e| e.to_string())?;
        let node = self.clone();
        let serving = connection.clone();
        tokio::spawn(async move {
            node.serve(serving).await;
        });
        Ok(connection)
    }
    async fn serve(&self, connection: Connection) {
        let peer = connection.remote_id().to_string();
        let session = uuid::Uuid::new_v4().to_string();
        self.host.connected(&peer, &session, connection.clone());
        while let Ok((mut send, mut recv)) = connection.accept_bi().await {
            let result = tokio::time::timeout(Duration::from_secs(90), async {
                let request = decode(
                    recv.read_to_end(MAX_FILE + MAX_HEADER + 4)
                        .await
                        .map_err(|e| e.to_string())?,
                )?;
                let host = self.host.clone();
                let peer = peer.clone();
                let session = session.clone();
                let answer =
                    tokio::task::spawn_blocking(move || host.handle(&peer, &session, request))
                        .await
                        .map_err(|_| "피어 요청 처리에 실패했습니다.".to_string())?;
                let message = answer.unwrap_or_else(|error| Message::json(json!({"error":error})));
                send.write_all(&encode(message)?)
                    .await
                    .map_err(|e| e.to_string())?;
                send.finish().map_err(|e| e.to_string())?;
                Ok::<_, String>(())
            })
            .await;
            if !matches!(result, Ok(Ok(()))) {
                break;
            }
        }
        self.host.disconnected(&session);
        connection.close(0u8.into(), b"closed");
    }
}
pub async fn request(connection: &Connection, message: Message) -> Result<Message> {
    tokio::time::timeout(Duration::from_secs(90), async {
        let (mut send, mut recv) = connection.open_bi().await.map_err(|e| e.to_string())?;
        send.write_all(&encode(message)?)
            .await
            .map_err(|e| e.to_string())?;
        send.finish().map_err(|e| e.to_string())?;
        let answer = decode(
            recv.read_to_end(MAX_FILE + MAX_HEADER + 4)
                .await
                .map_err(|e| e.to_string())?,
        )?;
        if let Some(error) = answer.meta["error"].as_str() {
            return Err(error.to_string());
        }
        Ok(answer)
    })
    .await
    .map_err(|_| "피어 응답 시간이 초과되었습니다.".to_string())?
}
fn encode(message: Message) -> Result<Vec<u8>> {
    if message.data.len() > MAX_FILE {
        return Err("전송 파일은 50MB 이하여야 합니다.".into());
    }
    let header = serde_json::to_vec(&json!({"meta":message.meta,"length":message.data.len()}))
        .map_err(|e| e.to_string())?;
    if header.len() > MAX_HEADER {
        return Err("파일 목록이 너무 큽니다.".into());
    }
    let mut bytes = Vec::with_capacity(4 + header.len() + message.data.len());
    bytes.extend_from_slice(&(header.len() as u32).to_be_bytes());
    bytes.extend(header);
    bytes.extend(message.data);
    Ok(bytes)
}
fn decode(bytes: Vec<u8>) -> Result<Message> {
    let size = u32::from_be_bytes(
        bytes
            .get(..4)
            .ok_or("전송 헤더 오류")?
            .try_into()
            .map_err(|_| "전송 헤더 오류")?,
    ) as usize;
    if size > MAX_HEADER || size + 4 > bytes.len() {
        return Err("전송 헤더 크기 오류".into());
    }
    let header: Value =
        serde_json::from_slice(&bytes[4..size + 4]).map_err(|_| "전송 헤더 형식 오류")?;
    let data = &bytes[size + 4..];
    if data.len() > MAX_FILE || header["length"].as_u64() != Some(data.len() as u64) {
        return Err("전송 파일 크기 오류".into());
    }
    Ok(Message {
        meta: header["meta"].clone(),
        data: data.to_vec(),
    })
}
