//! WebSocket signaling for remote P2P connections.
//!
//! The signaling channel only exchanges connection metadata (room join,
//! SDP offer/answer, ICE candidates) — never file data. A room is keyed by a
//! 16-character code (`XXXX-XXXX-XXXX-XXXX`, uppercase letters + digits) that
//! identifies a room on a self-hosted server: the first joiner waits, the second
//! joiner triggers pairing; any further joiner is rejected.

use crate::discovery::DeviceInfo;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

/// WebRTC session description (SDP) with explicit kind.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionDescription {
    /// `offer` or `answer` (lowercase), matching `RTCSdpType` serialization.
    pub kind: String,
    pub sdp: String,
}

/// Signaling protocol message (JSON over WebSocket text frames).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SignalMessage {
    /// Application heartbeat: verifies that this is a compatible Mausfer server.
    Ping,
    Pong,
    /// Client -> Server: join a room.
    Join {
        room: String,
        device: DeviceInfo,
    },
    /// Server -> Client: a peer is present.
    PeerJoined {
        peer: DeviceInfo,
    },
    /// Server -> Client: the other peer has left.
    PeerLeft,
    /// P2P handshake relay.
    Offer {
        sdp: SessionDescription,
    },
    Answer {
        sdp: SessionDescription,
    },
    Ice {
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    },
    /// Server -> Client: fatal room error.
    Error {
        message: String,
    },
}

/// Alphabet for room codes: uppercase A-Z + digits 2-9 (no 0/O/1/I to avoid
/// transcription mistakes).
const ROOM_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
/// Characters in one code (before formatting).
pub const ROOM_CODE_LEN: usize = 16;
/// Total formatted length including dashes (`XXXX-XXXX-XXXX-XXXX`).
pub const ROOM_CODE_FORMATTED_LEN: usize = 19;

pub fn encode(msg: &SignalMessage) -> String {
    serde_json::to_string(msg).expect("signal message serialization failed")
}

pub fn decode(text: &str) -> Option<SignalMessage> {
    serde_json::from_str(text).ok()
}

/// Generate a random 16-character room code (uppercase letters + digits,
/// formatted `XXXX-XXXX-XXXX-XXXX`).
pub fn new_room_code() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let mut code = String::with_capacity(ROOM_CODE_FORMATTED_LEN);
    for i in 0..ROOM_CODE_LEN {
        if i > 0 && i % 4 == 0 {
            code.push('-');
        }
        code.push(ROOM_ALPHABET[rng.gen_range(0..ROOM_ALPHABET.len())] as char);
    }
    code
}

/// Normalize a room code: strip separators/whitespace and uppercase.
pub fn normalize_room_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// Validate a room code (16 chars from the alphabet).
pub fn valid_room_code(code: &str) -> bool {
    let code = normalize_room_code(code);
    code.len() == ROOM_CODE_LEN && code.bytes().all(|b| ROOM_ALPHABET.contains(&b))
}

/// WebSocket client for a self-hosted signaling server.
pub struct SignalClient {
    sender: mpsc::UnboundedSender<Message>,
    messages: mpsc::UnboundedReceiver<SignalMessage>,
    _reader: tokio::task::JoinHandle<()>,
    _writer: tokio::task::JoinHandle<()>,
}

impl Drop for SignalClient {
    fn drop(&mut self) {
        self._reader.abort();
        self._writer.abort();
    }
}

impl SignalClient {
    /// Connect to the signaling server at `ws://host:port`.
    pub async fn connect(addr: &str) -> Result<Self, String> {
        if addr.trim().is_empty() {
            return Err("请先在界面设置自建信令服务器地址".into());
        }
        let (ws, _) = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio_tungstenite::connect_async(addr),
        )
        .await
        .map_err(|_| "信令服务器连接超时，请检查地址、端口和服务器防火墙".to_string())?
        .map_err(|e| format!("信令服务器连接失败: {e}"))?;
        let (mut sink, mut stream) = ws.split();

        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        let (msg_tx, msg_rx) = mpsc::unbounded_channel::<SignalMessage>();

        // Writer task: forwards outbound messages to the socket.
        let writer = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if sink.send(msg).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        // Reader task: parses JSON text frames into SignalMessage.
        let reader = tokio::spawn(async move {
            while let Some(ok) = stream.next().await {
                let text = match ok {
                    Ok(Message::Text(text)) => text,
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(_) => continue,
                };
                if let Some(sm) = decode(&text) {
                    if msg_tx.send(sm).is_err() {
                        break;
                    }
                }
            }
            let _ = msg_tx.send(SignalMessage::Error {
                message: "signaling connection closed".into(),
            });
        });

        Ok(Self {
            sender: tx,
            messages: msg_rx,
            _reader: reader,
            _writer: writer,
        })
    }

    /// Send a signal message.
    pub fn send(&self, msg: &SignalMessage) -> Result<(), String> {
        self.sender
            .send(Message::Text(encode(msg)))
            .map_err(|_| "signal channel closed".to_string())
    }

    /// Receive the next signal message (blocks until one arrives).
    pub async fn recv(&mut self) -> Result<SignalMessage, String> {
        self.inner_recv().await
    }

    async fn inner_recv(&mut self) -> Result<SignalMessage, String> {
        let msg = self
            .messages
            .recv()
            .await
            .ok_or_else(|| "signal channel closed".to_string())?;
        // Surface transport-level closure as an error unless the server
        // explicitly closed a connected session.
        match msg {
            SignalMessage::Error { message } if message.contains("connection closed") => {
                Err(message)
            }
            other => Ok(other),
        }
    }
}

/// Maintain a real, independently monitored connection to the configured server.
/// A successful TCP/WebSocket handshake alone does not imply protocol compatibility.
pub async fn monitor_connection(address: impl Fn() -> String, publish: impl Fn(&str, &str, &str)) {
    use std::time::Duration;
    let mut active: Option<(String, SignalClient)> = None;
    loop {
        let url = address();
        if active.as_ref().is_some_and(|(old, _)| old != &url) {
            active = None;
        }
        if url.is_empty() {
            publish(&url, "unconfigured", "");
        } else {
            if active.is_none() {
                publish(&url, "connecting", "");
                match SignalClient::connect(&url).await {
                    Ok(client) => active = Some((url.clone(), client)),
                    Err(error) => publish(&url, "failed", &error),
                }
            }
            if let Some((_, client)) = active.as_mut() {
                let result = tokio::time::timeout(Duration::from_secs(5), async {
                    client.send(&SignalMessage::Ping)?;
                    match client.recv().await? {
                        SignalMessage::Pong => Ok(()),
                        SignalMessage::Error { message } => Err(message),
                        _ => Err("服务器协议不兼容，请更新服务端 JAR".to_string()),
                    }
                })
                .await
                .unwrap_or_else(|_| Err("信令服务器无响应，请检查网络或更新服务端 JAR".into()));
                match result {
                    Ok(()) => publish(&url, "connected", ""),
                    Err(error) => {
                        publish(&url, "failed", &error);
                        active = None;
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

struct PeerConn {
    /// Writer task receiver; sends are forwarded to this peer's socket.
    tx: mpsc::UnboundedSender<Message>,
    device: DeviceInfo,
}

#[derive(Default)]
struct Room {
    peers: Vec<PeerConn>,
}

/// In-memory signaling server.
#[derive(Default)]
pub struct SignalingServer {
    rooms: std::sync::Mutex<HashMap<String, Room>>,
}

impl SignalingServer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind and serve on `addr`. Returns the actual bound address so tests
    /// can use port 0.
    pub async fn serve(self: Arc<Self>, addr: SocketAddr) -> Result<SocketAddr, String> {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| format!("signal server bind failed: {e}"))?;
        let local = listener
            .local_addr()
            .map_err(|e| format!("signal server local addr failed: {e}"))?;
        tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => break,
                };
                let server = self.clone();
                tokio::spawn(async move {
                    let ws = match tokio_tungstenite::accept_async(stream).await {
                        Ok(ws) => ws,
                        Err(_) => return,
                    };
                    let _ = server.handle_peer(ws).await;
                });
            }
        });
        Ok(local)
    }

    async fn handle_peer(&self, ws: WebSocketStream<TcpStream>) -> Result<(), String> {
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

        // Writer task owned by this peer.
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if sink.send(msg).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        let mut joined_room: Option<String> = None;
        let mut own_device = DeviceInfo::new("unknown", "unknown", 0, "0.0.0");

        while let Some(ok) = stream.next().await {
            let text = match ok {
                Ok(Message::Text(text)) => text,
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(_) => continue,
            };
            let Some(sm) = decode(&text) else { continue };

            match sm {
                SignalMessage::Ping => {
                    let _ = tx_local(&tx, &SignalMessage::Pong);
                }
                SignalMessage::Join { room, device } => {
                    if joined_room.is_some() {
                        let _ = tx_local(
                            &tx,
                            &SignalMessage::Error {
                                message: "already joined a room".into(),
                            },
                        );
                        continue;
                    }
                    if !valid_room_code(&room) {
                        let _ = tx_local(
                            &tx,
                            &SignalMessage::Error {
                                message: "invalid room code (XXXX-XXXX-XXXX-XXXX)".into(),
                            },
                        );
                        continue;
                    }
                    let room = normalize_room_code(&room);
                    let mut rooms = self.rooms.lock().unwrap();
                    let entry = rooms.entry(room.clone()).or_default();
                    if entry.peers.len() >= 2
                        || entry.peers.iter().any(|p| p.device.id == device.id)
                    {
                        drop(rooms);
                        let _ = tx_local(
                            &tx,
                            &SignalMessage::Error {
                                message: "room is full".into(),
                            },
                        );
                        continue;
                    }
                    let first: Option<(DeviceInfo, mpsc::UnboundedSender<Message>)> = entry
                        .peers
                        .first()
                        .map(|p| (p.device.clone(), p.tx.clone()));
                    own_device = device.clone();
                    entry.peers.push(PeerConn {
                        tx: tx.clone(),
                        device,
                    });
                    drop(rooms);

                    if let Some((first_device, first_tx)) = first {
                        // Second joiner learns about the first.
                        let _ = tx_local(&tx, &SignalMessage::PeerJoined { peer: first_device });
                        // First joiner learns about the second.
                        let _ = tx_local(
                            &first_tx,
                            &SignalMessage::PeerJoined {
                                peer: own_device.clone(),
                            },
                        );
                    }
                    joined_room = Some(room);
                }
                SignalMessage::Offer { .. }
                | SignalMessage::Answer { .. }
                | SignalMessage::Ice { .. } => {
                    self.relay(&joined_room, &own_device, &sm);
                }
                _ => {}
            }
        }

        // Remove from its room on disconnect and notify the remaining peer.
        if let Some(room) = joined_room {
            let mut rooms = self.rooms.lock().unwrap();
            if let Some(r) = rooms.get_mut(&room) {
                r.peers.retain(|p| p.device.id != own_device.id);
                if let Some(first) = r.peers.first() {
                    let _ = tx_local(&first.tx, &SignalMessage::PeerLeft);
                }
                if r.peers.is_empty() {
                    rooms.remove(&room);
                }
            }
        }
        Ok(())
    }

    fn relay(&self, joined_room: &Option<String>, own_device: &DeviceInfo, msg: &SignalMessage) {
        let Some(room) = joined_room else { return };
        let mut rooms = self.rooms.lock().unwrap();
        if let Some(r) = rooms.get_mut(room) {
            if let Some(target) = r.peers.iter().find(|p| p.device.id != own_device.id) {
                let _ = tx_local(&target.tx, msg);
            }
        }
    }
}

fn tx_local(tx: &mpsc::UnboundedSender<Message>, msg: &SignalMessage) -> Result<(), String> {
    tx.send(Message::Text(encode(msg)))
        .map_err(|_| "peer writer closed".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn heartbeat_replies_without_joining_room() {
        let server = Arc::new(SignalingServer::new());
        let bound = server.serve("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let mut client = SignalClient::connect(&format!("ws://{bound}"))
            .await
            .unwrap();
        client.send(&SignalMessage::Ping).unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(2), client.recv())
                .await
                .unwrap()
                .unwrap(),
            SignalMessage::Pong
        );
    }

    #[test]
    fn message_roundtrip() {
        let msg = SignalMessage::Join {
            room: "AABBCCDDEEFFGGHH".into(),
            device: DeviceInfo::new("dev-1", "MacBook", 43110, "0.1.0"),
        };
        let text = encode(&msg);
        let back = decode(&text).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn room_code_generation() {
        let a = new_room_code();
        let b = new_room_code();
        assert!(valid_room_code(&a));
        assert!(valid_room_code(&b));
        assert!(a != b, "codes must differ");
    }

    #[tokio::test]
    async fn two_clients_pair_and_relay() {
        let server = Arc::new(SignalingServer::new());
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let bound = server.clone().serve(addr).await.unwrap();
        let url = format!("ws://{bound}");

        let mut a = SignalClient::connect(&url).await.unwrap();
        let mut b = SignalClient::connect(&url).await.unwrap();

        let dev_a = DeviceInfo::new("dev-a", "A", 1001, "0.1.0");
        let dev_b = DeviceInfo::new("dev-b", "B", 1002, "0.1.0");

        a.send(&SignalMessage::Join {
            room: "AABBCCDDEEFFGGHH".into(),
            device: dev_a,
        })
        .unwrap();
        b.send(&SignalMessage::Join {
            room: "aabb-ccdd-eeff-gghh".into(),
            device: dev_b,
        })
        .unwrap();

        // First joiner may receive PeerJoined once the second joins; then the
        // second joiner also gets PeerJoined. Order is not deterministic, so
        // collect until both have seen it.
        let a_peer = tokio::time::timeout(std::time::Duration::from_secs(2), a.recv())
            .await
            .expect("timed out waiting for A's PeerJoined")
            .expect("A recv failed");
        let b_peer = tokio::time::timeout(std::time::Duration::from_secs(2), b.recv())
            .await
            .expect("timed out waiting for B's PeerJoined")
            .expect("B recv failed");
        match a_peer {
            SignalMessage::PeerJoined { peer } => assert_eq!(peer.id, "dev-b"),
            other => panic!("A received unexpected: {other:?}"),
        }
        match b_peer {
            SignalMessage::PeerJoined { peer } => assert_eq!(peer.id, "dev-a"),
            other => panic!("B received unexpected: {other:?}"),
        }

        a.send(&SignalMessage::Join {
            room: "BBBBCCCCDDDDEEEE".into(),
            device: DeviceInfo::new("replacement", "other", 0, "0.1.0"),
        })
        .unwrap();
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(2), a.recv())
                .await
                .unwrap()
                .unwrap(),
            SignalMessage::Error { .. }
        ));

        // Relay an offer from A to B.
        let sdp = SessionDescription {
            kind: "offer".into(),
            sdp: "v=0\r\nfake".into(),
        };
        a.send(&SignalMessage::Offer { sdp: sdp.clone() }).unwrap();
        let recv = tokio::time::timeout(std::time::Duration::from_secs(2), b.recv())
            .await
            .expect("timed out waiting for relayed offer")
            .expect("B recv failed");
        assert_eq!(recv, SignalMessage::Offer { sdp });

        // Third joiner must be rejected.
        let mut c = SignalClient::connect(&url).await.unwrap();
        c.send(&SignalMessage::Join {
            room: "AABBCCDDEEFFGGHH".into(),
            device: DeviceInfo::new("dev-c", "C", 1003, "0.1.0"),
        })
        .unwrap();
        let recv = tokio::time::timeout(std::time::Duration::from_secs(2), c.recv())
            .await
            .expect("timed out waiting for room-full error")
            .expect("C recv failed");
        match recv {
            SignalMessage::Error { message } => assert!(message.contains("full")),
            other => panic!("C received unexpected: {other:?}"),
        }
    }
}
