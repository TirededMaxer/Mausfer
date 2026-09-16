//! LAN device discovery for Mausfer.
//!
//! Uses UDP broadcast (plus optional subnet-directed broadcast) to announce
//! devices, answer discovery queries, and maintain a live peer table.
//! This works on Windows, macOS and Android without any external daemon.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// mDNS/DNS-SD service type used by Mausfer on local networks.
pub const MAUSFER_SERVICE: &str = "_mausfer._tcp.local.";

/// UDP discovery port (separate from the transfer port).
pub const DEFAULT_DISCOVERY_PORT: u16 = 43111;

/// Maximum accepted datagram size.
const MAX_DATAGRAM: usize = 4096;

/// Default peer expiry: a peer that stops announcing for this long is dropped.
const DEFAULT_PEER_TTL: Duration = Duration::from_secs(15);

/// Default announce interval.
const DEFAULT_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(3);

/// Information about a Mausfer device.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Unique running-instance ID, stable until the app restarts.
    pub id: String,
    /// Human-readable system device name.
    pub name: String,
    /// The TCP port the device listens on for file transfers.
    pub port: u16,
    /// Protocol / app version.
    pub version: String,
}

impl DeviceInfo {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        port: u16,
        version: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            port,
            version: version.into(),
        }
    }
}

/// Discovery protocol message. UDP datagram payloads are the JSON encoding
/// of this struct.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscoveryMessage {
    #[serde(rename = "t")]
    pub msg_type: MessageType,
    #[serde(rename = "d")]
    pub device: DeviceInfo,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MessageType {
    /// I am here; refresh my peer entry (unicast or broadcast).
    Announce,
    /// Wake up: please announce yourself so I can find you.
    Query,
    /// I am going away.
    Bye,
}

impl DiscoveryMessage {
    pub fn encode(&self) -> Vec<u8> {
        // Cannot fail for this struct; panic here would hide a bug.
        serde_json::to_vec(self).expect("discovery message serialization failed")
    }

    pub fn decode(data: &[u8]) -> Option<Self> {
        serde_json::from_slice(data).ok()
    }
}

/// Configuration for a [`DiscoveryService`].
#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    /// Local UDP port to bind on every interface (`0.0.0.0`). Use `0` for an
    /// ephemeral port (mainly useful in tests).
    pub port: u16,
    /// Broadcast destination used for announces and queries.
    pub broadcast_addr: SocketAddr,
    /// How often to announce ourselves.
    pub announce_interval: Duration,
    /// How long an unresponsive peer is kept before being dropped.
    pub peer_ttl: Duration,
    /// Whether to answer incoming queries with an announce.
    pub answer_queries: bool,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_DISCOVERY_PORT,
            broadcast_addr: default_broadcast_addr(DEFAULT_DISCOVERY_PORT),
            announce_interval: DEFAULT_ANNOUNCE_INTERVAL,
            peer_ttl: DEFAULT_PEER_TTL,
            answer_queries: true,
        }
    }
}

fn default_broadcast_addr(port: u16) -> SocketAddr {
    // 255.255.255.255 requires SO_BROADCAST; subnet-directed broadcasts
    // (e.g. 192.168.1.255) also pass through here if configured.
    SocketAddr::from(([255, 255, 255, 255], port))
}

pub(crate) struct PeerEntry {
    pub info: DeviceInfo,
    pub addr: SocketAddr,
    pub last_seen: Instant,
}

/// UDP broadcast discovery service.
///
/// Start it with [`DiscoveryService::start`]; query peers with
/// [`DiscoveryService::peers`]; stop with [`DiscoveryService::stop`]
/// (it sends a `bye` and joins the listener thread).
pub struct DiscoveryService {
    socket: Arc<UdpSocket>,
    device: DeviceInfo,
    config: DiscoveryConfig,
    peers: Arc<Mutex<HashMap<String, PeerEntry>>>,
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl DiscoveryService {
    /// Create a service bound to `0.0.0.0:config.port`.
    ///
    /// Sets `SO_REUSEADDR`/`SO_REUSEPORT` so multiple Mausfer instances on the
    /// same machine (or a brief restart) can share the discovery port.
    pub fn new(device: DeviceInfo, config: DiscoveryConfig) -> io::Result<Self> {
        use socket2::{Domain, Protocol, Socket, Type};

        let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        s.set_reuse_address(true)?;
        // SO_REUSEPORT is unavailable on Windows (it's the default there) and
        // on some socket2 targets; not supported everywhere, ignore failure.
        #[cfg(all(unix, not(target_os = "android")))]
        let _ = s.set_reuse_port(true);
        let addr: SocketAddr = SocketAddr::from(([0, 0, 0, 0], config.port));
        s.bind(&addr.into())?;
        let socket: UdpSocket = s.into();
        socket.set_broadcast(true)?;
        socket.set_read_timeout(Some(Duration::from_millis(500)))?;
        Ok(Self {
            socket: Arc::new(socket),
            device,
            config,
            peers: Arc::new(Mutex::new(HashMap::new())),
            running: Arc::new(AtomicBool::new(false)),
            thread: None,
        })
    }

    /// The socket's local address (after binding).
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Start the background announce/listen thread.
    pub fn start(&mut self) -> io::Result<()> {
        if self.running.swap(true, Ordering::SeqCst) {
            return Ok(()); // already running
        }
        let socket = self.socket.clone();
        let device = self.device.clone();
        let config = self.config.clone();
        let peers = self.peers.clone();
        let running = self.running.clone();

        self.thread = Some(thread::spawn(move || {
            run_loop(&socket, &device, &config, &peers, &running);
        }));
        // Announce immediately so peers learn about us fast. Do not fail
        // startup when the broadcast itself is blocked (e.g. some networks
        // drop all-broadcast); the periodic loop keeps retrying.
        let _ = self.send_announce();
        Ok(())
    }

    /// Send an announce datagram to the configured broadcast address.
    pub fn send_announce(&self) -> io::Result<()> {
        self.send_to(self.config.broadcast_addr, MessageType::Announce)
    }

    /// Send an announce to a specific address (used in tests).
    pub fn send_announce_to(&self, addr: SocketAddr) -> io::Result<()> {
        self.send_to(addr, MessageType::Announce)
    }

    /// Send a query to the configured broadcast address.
    pub fn send_query(&self) -> io::Result<()> {
        self.send_to(self.config.broadcast_addr, MessageType::Query)
    }

    /// Send a query to a specific address (used in tests).
    pub fn send_query_to(&self, addr: SocketAddr) -> io::Result<()> {
        self.send_to(addr, MessageType::Query)
    }

    /// Send a bye to the broadcast address.
    pub fn send_bye(&self) -> io::Result<()> {
        self.send_to(self.config.broadcast_addr, MessageType::Bye)
    }

    fn send_to(&self, addr: SocketAddr, msg_type: MessageType) -> io::Result<()> {
        let msg = DiscoveryMessage {
            msg_type,
            device: self.device.clone(),
        };
        self.socket.send_to(&msg.encode(), addr)?;
        Ok(())
    }

    /// Currently known peers (excluding self and expired entries).
    pub fn peers(&self) -> Vec<DeviceInfo> {
        let now = Instant::now();
        let mut peers = self.peers.lock().unwrap();
        let ttl = self.config.peer_ttl;
        peers.retain(|_, e| now.duration_since(e.last_seen) <= ttl);
        let mut out: Vec<DeviceInfo> = peers.values().map(|e| e.info.clone()).collect();
        out.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        out
    }

    /// The last-seen source address of a peer, if known.
    ///
    /// Combined with [`DeviceInfo::port`] this is where the peer's transfer
    /// server listens.
    pub fn peer_addr(&self, id: &str) -> Option<SocketAddr> {
        self.peers.lock().unwrap().get(id).map(|e| e.addr)
    }

    /// Stop the service: send `bye` and join the background thread.
    pub fn stop(&mut self) {
        if !self.running.swap(false, Ordering::SeqCst) {
            return;
        }
        let _ = self.send_bye();
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }

    /// Process one raw datagram (used by tests and available for manual
    /// injection).
    #[cfg(test)]
    fn handle_datagram(&self, data: &[u8], from: SocketAddr) {
        if let Some(msg) = DiscoveryMessage::decode(data) {
            let _ = apply_message(&self.device, &self.peers, &msg, from);
        }
    }

    /// Directly record a peer (used by tests to exercise TTL removal).
    #[cfg(test)]
    fn insert_peer(&self, info: DeviceInfo, addr: SocketAddr) {
        let mut peers = self.peers.lock().unwrap();
        peers.insert(
            info.id.clone(),
            PeerEntry {
                info,
                addr,
                last_seen: Instant::now(),
            },
        );
    }
}

/// Apply one decoded discovery message to the peer table.
///
/// Returns `true` if the message was a query that should be answered with an
/// unicast announce.
fn apply_message(
    device: &DeviceInfo,
    peers: &Arc<Mutex<HashMap<String, PeerEntry>>>,
    msg: &DiscoveryMessage,
    from: SocketAddr,
) -> bool {
    // Never record ourselves.
    if msg.device.id == device.id {
        return false;
    }
    let now = Instant::now();
    let mut table = peers.lock().unwrap();
    match msg.msg_type {
        MessageType::Announce | MessageType::Query => {
            table.insert(
                msg.device.id.clone(),
                PeerEntry {
                    info: msg.device.clone(),
                    addr: from,
                    last_seen: now,
                },
            );
            matches!(msg.msg_type, MessageType::Query)
        }
        MessageType::Bye => {
            table.remove(&msg.device.id);
            false
        }
    }
}

impl Drop for DiscoveryService {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_loop(
    socket: &UdpSocket,
    device: &DeviceInfo,
    config: &DiscoveryConfig,
    peers: &Arc<Mutex<HashMap<String, PeerEntry>>>,
    running: &Arc<AtomicBool>,
) {
    let mut buf = [0u8; MAX_DATAGRAM];
    let mut last_announce = Instant::now() - config.announce_interval;
    while running.load(Ordering::SeqCst) {
        // Periodically announce ourselves so peers keep us in their table.
        if last_announce.elapsed() >= config.announce_interval {
            last_announce = Instant::now();
            let msg = DiscoveryMessage {
                msg_type: MessageType::Announce,
                device: device.clone(),
            };
            let _ = socket.send_to(&msg.encode(), config.broadcast_addr);
        }
        match socket.recv_from(&mut buf) {
            Ok((len, from)) => {
                if running.load(Ordering::SeqCst) {
                    if let Some(msg) = DiscoveryMessage::decode(&buf[..len]) {
                        let answer = apply_message(device, peers, &msg, from);
                        // Answer queries with a unicast announce so the asker
                        // learns about us without waiting for our next broadcast.
                        if answer && config.answer_queries {
                            let reply = DiscoveryMessage {
                                msg_type: MessageType::Announce,
                                device: device.clone(),
                            };
                            let _ = socket.send_to(&reply.encode(), from);
                        }
                    }
                }
            }
            Err(ref e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
            }
            Err(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn wait_until<F: Fn() -> bool>(timeout: Duration, check: F) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if check() {
                return true;
            }
            thread::sleep(Duration::from_millis(30));
        }
        false
    }

    fn test_config(port: u16) -> DiscoveryConfig {
        DiscoveryConfig {
            port,
            broadcast_addr: SocketAddr::from(([127, 0, 0, 1], port)),
            announce_interval: Duration::from_millis(200),
            peer_ttl: Duration::from_secs(5),
            answer_queries: true,
        }
    }

    /// The loopback address for a service bound to `0.0.0.0`.
    /// (`local_addr()` reports `0.0.0.0:port`, which cannot be sent to.)
    fn loopback(service: &DiscoveryService) -> SocketAddr {
        let port = service.local_addr().unwrap().port();
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    /// Create a service bound to an ephemeral port and point its broadcast
    /// target at its own loopback address (so periodic announces are valid
    /// and harmless in tests).
    fn test_service(device: DeviceInfo) -> DiscoveryService {
        let mut svc = DiscoveryService::new(device, test_config(0)).unwrap();
        let target = loopback(&svc);
        svc.config.broadcast_addr = target;
        svc
    }

    #[test]
    fn device_info_serializes_to_json() {
        let device = DeviceInfo::new("dev-1", "MacBook", 43110, "0.1.0");
        let json = serde_json::to_string(&device).unwrap();
        let back: DeviceInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(device, back);
    }

    #[test]
    fn service_name_is_stable() {
        assert_eq!(MAUSFER_SERVICE, "_mausfer._tcp.local.");
    }

    #[test]
    fn discovery_message_roundtrip() {
        let msg = DiscoveryMessage {
            msg_type: MessageType::Query,
            device: DeviceInfo::new("dev-1", "MacBook", 43110, "0.1.0"),
        };
        let data = msg.encode();
        let back = DiscoveryMessage::decode(&data).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn announce_registers_peer() {
        let a = DeviceInfo::new("dev-a", "Device A", 1001, "0.1.0");
        let b = DeviceInfo::new("dev-b", "Device B", 1002, "0.1.0");
        let mut sa = test_service(a);
        let mut sb = test_service(b);
        sa.start().unwrap();
        sb.start().unwrap();

        // A announces to B's socket (unicast, loopback).
        sa.send_announce_to(loopback(&sb)).unwrap();

        let found = wait_until(Duration::from_secs(2), || {
            sb.peers().iter().any(|p| p.id == "dev-a")
        });
        assert!(found, "B should have discovered A");
        sa.stop();
        sb.stop();
    }

    #[test]
    fn query_is_answered_with_announce() {
        let a = DeviceInfo::new("dev-a", "Device A", 1001, "0.1.0");
        let b = DeviceInfo::new("dev-b", "Device B", 1002, "0.1.0");
        let mut sa = test_service(a);
        let mut sb = test_service(b);
        sa.start().unwrap();
        sb.start().unwrap();

        // A queries B; B should answer with an announce that A records.
        sa.send_query_to(loopback(&sb)).unwrap();

        let found_a = wait_until(Duration::from_secs(2), || {
            sa.peers().iter().any(|p| p.id == "dev-b")
        });
        let found_b = wait_until(Duration::from_secs(2), || {
            sb.peers().iter().any(|p| p.id == "dev-a")
        });
        assert!(found_a, "A should have discovered B via query answer");
        assert!(found_b, "B should have recorded the querier A");
        sa.stop();
        sb.stop();
    }

    #[test]
    fn bye_removes_peer_and_self_is_ignored() {
        let a = DeviceInfo::new("dev-a", "Device A", 1001, "0.1.0");
        let sa = DiscoveryService::new(a, test_config(0)).unwrap();

        // Manually insert a peer, then check self is filtered and bye works.
        let addr_b: SocketAddr = "127.0.0.1:99".parse().unwrap();
        sa.insert_peer(DeviceInfo::new("dev-b", "Device B", 1002, "0.1.0"), addr_b);
        assert_eq!(sa.peers().len(), 1);
        // Self id must never appear.
        let own_id = sa.device.id.clone();
        assert!(!sa.peers().iter().any(|p| p.id == own_id));

        // Send a bye and ensure removal.
        let bye = DiscoveryMessage {
            msg_type: MessageType::Bye,
            device: DeviceInfo::new("dev-b", "Device B", 1002, "0.1.0"),
        };
        sa.handle_datagram(&bye.encode(), addr_b);
        assert!(sa.peers().is_empty());
    }

    #[test]
    fn stale_peer_expires_after_ttl() {
        let a = DeviceInfo::new("dev-a", "Device A", 1001, "0.1.0");
        let mut config = test_config(0);
        config.peer_ttl = Duration::from_millis(150);
        let sa = DiscoveryService::new(a, config).unwrap();
        sa.insert_peer(
            DeviceInfo::new("dev-b", "Device B", 1002, "0.1.0"),
            "127.0.0.1:99".parse().unwrap(),
        );
        assert_eq!(sa.peers().len(), 1);
        thread::sleep(Duration::from_millis(250));
        assert!(sa.peers().is_empty(), "expired peer should be pruned");
    }
}
