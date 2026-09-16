//! Remote P2P file transfer over WebRTC DataChannel.
//!
//! Each transfer is a single bidirectional DataChannel. The existing framed
//! transfer protocol ([`crate::transfer`]) runs unchanged on top of the
//! channel; the DataChannel is ordered + reliable by default, which matches
//! the TCP framing assumptions.
//!
//! WebRTC peer setup is driven by relayed SDP/ICE messages over the
//! [`crate::signaling`] WebSocket channel. STUN/TURN servers come from the
//! config fields (`stun_url`, `turn_url`, `turn_username`, `turn_password`).

use crate::config::Config;
use crate::signaling::SessionDescription;
use bytes::Bytes;
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

/// Label used for the Mausfer data channel.
pub const CHANNEL_LABEL: &str = "mausfer";

/// Default timeout for the ICE/connection establishment phase.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

// Bound queued incoming messages before disk consumption. Our sender emits
// at most 4 KiB per message, so normal traffic queues at most 512 KiB here.
const INBOUND_CAPACITY: usize = 128;

/// Events a peer connection produces that the caller relays over signaling.
#[derive(Debug, Clone, PartialEq)]
pub enum P2pSignalEvent {
    /// Outbound session description to send to the other side.
    LocalDescription(SessionDescription),
    /// Local ICE candidate to send to the other side.
    LocalIce {
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    },
}

fn ice_servers(config: &Config) -> Vec<RTCIceServer> {
    let mut servers = Vec::new();
    let stun_urls = split_ice_urls(&config.stun_url);
    if !stun_urls.is_empty() {
        servers.push(RTCIceServer {
            urls: stun_urls,
            ..Default::default()
        });
    }
    let turn_urls = split_ice_urls(&config.turn_url);
    if !turn_urls.is_empty() {
        servers.push(RTCIceServer {
            urls: turn_urls,
            username: config.turn_username.clone(),
            credential: config.turn_password.clone(),
        });
    }
    servers
}

fn split_ice_urls(value: &str) -> Vec<String> {
    value
        .split([',', ';'])
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(str::to_string)
        .collect()
}

/// SDP kind carried alongside the description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdpKind {
    Offer,
    Answer,
}

/// Detect whether an SDP blob is an answer vs an offer from `a=type:`.
fn looks_like_answer(sdp: &str) -> bool {
    sdp.lines()
        .find_map(|l| l.strip_prefix("a=type:"))
        .map(|t| t.trim() == "answer")
        .unwrap_or(false)
}

/// Wait for ICE gathering to complete (non-trickle mode).
async fn wait_gathering_complete(pc: &Arc<RTCPeerConnection>) -> Result<(), String> {
    let mut rx = pc.gathering_complete_promise().await;
    tokio::time::timeout(Duration::from_secs(15), rx.recv())
        .await
        .map_err(|_| "ICE gathering timeout".to_string())
        .map_err(|e| e.to_string())?;
    Ok(())
}

async fn new_pc(config: &Config) -> Result<Arc<RTCPeerConnection>, String> {
    let mut media_engine = MediaEngine::default();
    media_engine
        .register_default_codecs()
        .map_err(|e| format!("codec registration failed: {e}"))?;
    let mut registry = webrtc::interceptor::registry::Registry::new();
    registry = register_default_interceptors(registry, &mut media_engine)
        .map_err(|e| format!("interceptor registration failed: {e}"))?;
    let builder = APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry);
    // Unit tests run both peers locally. Keep those tests on loopback rather
    // than depending on VPN/VM interfaces and their changing ICE routes.
    // Normal builds retain all interfaces; subprocess/device tests cover them.
    #[cfg(test)]
    let builder = {
        let mut settings = webrtc::api::setting_engine::SettingEngine::default();
        settings.set_include_loopback_candidate(true);
        settings.set_ip_filter(Box::new(|ip| ip.is_ipv4() && ip.is_loopback()));
        builder.with_setting_engine(settings)
    };
    let api = builder.build();

    api.new_peer_connection(RTCConfiguration {
        ice_servers: ice_servers(config),
        ..Default::default()
    })
    .await
    .map(Arc::new)
    .map_err(|e| format!("peer connection failed: {e}"))
}

/// Forward ICE candidates from `pc` into `tx`.
pub fn attach_signal_forwarding(
    pc: &Arc<RTCPeerConnection>,
    tx: mpsc::UnboundedSender<P2pSignalEvent>,
) -> Result<(), String> {
    pc.on_ice_candidate(Box::new(
        move |cand: Option<webrtc::ice_transport::ice_candidate::RTCIceCandidate>| {
            let tx = tx.clone();
            Box::pin(async move {
                if let Some(cand) = cand {
                    if let Ok(init) = cand.to_json() {
                        let _ = tx.send(P2pSignalEvent::LocalIce {
                            candidate: init.candidate,
                            sdp_mid: init.sdp_mid,
                            sdp_mline_index: init.sdp_mline_index,
                        });
                    }
                }
            })
        },
    ));
    Ok(())
}

/// One side of an in-progress WebRTC connection.
pub struct P2pSession {
    pc: Arc<RTCPeerConnection>,
    _cleanup: ConnectionCleanup,
    /// Locally created data channel (host side); None on the answer side.
    local_dc: Option<Arc<RTCDataChannel>>,
    /// Answer side: fires when the remote's data channel arrives. Registered
    /// as soon as the session is created so no event is missed.
    pending_channel: std::sync::Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
    channel_notify: std::sync::Arc<tokio::sync::Notify>,
    /// Inbound message queue shared with [`DataChannelIo`]. The sender is
    /// installed once at session construction (nothing is ever dropped by a
    /// late registration); the receiver is handed out to the first
    /// [`wrap_channel`]/`accept_channel` consumer.
    inbound: std::sync::Arc<tokio::sync::Mutex<Option<mpsc::Receiver<Bytes>>>>,
    inbound_tx: mpsc::Sender<Bytes>,
}

// Acquire immediately after creating the peer connection, before any fallible
// negotiation or await. This covers cancellation during session construction,
// as well as dropping a completed session from a blocking thread.
struct ConnectionCleanup {
    pc: Arc<RTCPeerConnection>,
    runtime: tokio::runtime::Handle,
}

impl ConnectionCleanup {
    fn new(pc: &Arc<RTCPeerConnection>) -> Self {
        Self {
            pc: pc.clone(),
            runtime: tokio::runtime::Handle::current(),
        }
    }
}

impl Drop for ConnectionCleanup {
    fn drop(&mut self) {
        let pc = self.pc.clone();
        self.runtime.spawn(async move {
            let _ = pc.close().await;
        });
    }
}

impl P2pSession {
    /// Install the inbound capture on a data channel (idempotent per channel).
    fn capture_inbound(&self, dc: &Arc<RTCDataChannel>) {
        let tx = self.inbound_tx.clone();
        dc.on_message(Box::new(move |msg: DataChannelMessage| {
            let tx = tx.clone();
            Box::pin(async move {
                let _ = tx.send(msg.data).await;
            })
        }));
    }

    /// Take the pre-created inbound receiver (once).
    async fn take_inbound_receiver(&self) -> Result<mpsc::Receiver<Bytes>, String> {
        self.inbound
            .lock()
            .await
            .take()
            .ok_or_else(|| "data channel already wrapped".to_string())
    }

    /// Host side: create the DataChannel, produce an offer. Relay
    /// [`P2pSignalEvent::LocalDescription`] to the remote side.
    pub async fn create_offer_session(
        config: &Config,
        signal_tx: mpsc::UnboundedSender<P2pSignalEvent>,
    ) -> Result<Self, String> {
        let pc = new_pc(config).await?;
        let cleanup = ConnectionCleanup::new(&pc);
        attach_signal_forwarding(&pc, signal_tx.clone())?;

        let dc = pc
            .create_data_channel(CHANNEL_LABEL, None)
            .await
            .map_err(|e| format!("create data channel failed: {e}"))?;

        let offer = pc
            .create_offer(None)
            .await
            .map_err(|e| format!("create offer failed: {e}"))?;
        pc.set_local_description(offer.clone())
            .await
            .map_err(|e| format!("set local description failed: {e}"))?;
        // Non-trickle: wait for ICE gathering so all candidates are embedded
        // in the SDP. Makes the simple relayed-signaling path work without a
        // candidate channel.
        wait_gathering_complete(&pc).await?;
        let offer = pc
            .local_description()
            .await
            .ok_or_else(|| "local description unavailable".to_string())?;
        let _ = signal_tx.send(P2pSignalEvent::LocalDescription(SessionDescription {
            kind: "offer".to_string(),
            sdp: offer.sdp.clone(),
        }));

        let (inbound_tx, inbound_rx) = mpsc::channel::<Bytes>(INBOUND_CAPACITY);
        let session = Self {
            pc,
            _cleanup: cleanup,
            local_dc: Some(dc.clone()),
            pending_channel: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            channel_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
            inbound: std::sync::Arc::new(tokio::sync::Mutex::new(Some(inbound_rx))),
            inbound_tx,
        };
        // Capture inbound messages from the moment the channel exists.
        session.capture_inbound(&dc);
        Ok(session)
    }

    /// Answer side: apply the remote offer and produce an answer. Relay
    /// [`P2pSignalEvent::LocalDescription`] to the remote side.
    pub async fn accept_offer_session(
        config: &Config,
        remote_offer: &SessionDescription,
        signal_tx: mpsc::UnboundedSender<P2pSignalEvent>,
    ) -> Result<Self, String> {
        let pc = new_pc(config).await?;
        let cleanup = ConnectionCleanup::new(&pc);
        attach_signal_forwarding(&pc, signal_tx.clone())?;

        let desc = RTCSessionDescription::offer(remote_offer.sdp.clone())
            .map_err(|e| format!("bad offer: {e}"))?;
        pc.set_remote_description(desc)
            .await
            .map_err(|e| format!("set remote description failed: {e}"))?;
        let answer = pc
            .create_answer(None)
            .await
            .map_err(|e| format!("create answer failed: {e}"))?;
        pc.set_local_description(answer.clone())
            .await
            .map_err(|e| format!("set local description failed: {e}"))?;
        wait_gathering_complete(&pc).await?;
        let answer = pc
            .local_description()
            .await
            .ok_or_else(|| "local description unavailable".to_string())?;
        let _ = signal_tx.send(P2pSignalEvent::LocalDescription(SessionDescription {
            kind: "answer".to_string(),
            sdp: answer.sdp.clone(),
        }));

        // Register the incoming-channel receiver NOW so the event can never be
        // missed, even if the channel arrives before the caller awaits. The
        // inbound message pair is created eagerly so messages are queued from
        // the instant the channel exists.
        let pending_channel = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let channel_notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let (inbound_tx, inbound_rx) = mpsc::channel::<Bytes>(INBOUND_CAPACITY);
        {
            let pending = std::sync::Arc::clone(&pending_channel);
            let notify = std::sync::Arc::clone(&channel_notify);
            let tx = inbound_tx.clone();
            let pc = pc.clone();
            pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
                let pending = std::sync::Arc::clone(&pending);
                let notify = std::sync::Arc::clone(&notify);
                let tx = tx.clone();
                Box::pin(async move {
                    if dc.label() == CHANNEL_LABEL {
                        *pending.lock().await = Some(dc.clone());
                        // Capture inbound from the moment the channel exists.
                        dc.on_message(Box::new(move |msg: DataChannelMessage| {
                            let tx = tx.clone();
                            Box::pin(async move {
                                let _ = tx.send(msg.data).await;
                            })
                        }));
                        notify.notify_one();
                    }
                })
            }));
        }

        Ok(Self {
            pc,
            _cleanup: cleanup,
            local_dc: None,
            pending_channel,
            channel_notify,
            inbound: std::sync::Arc::new(tokio::sync::Mutex::new(Some(inbound_rx))),
            inbound_tx,
        })
    }

    /// Apply the remote session description. The `kind` field of the
    /// description decides offer vs answer.
    pub async fn set_remote_description(&self, sdp: &SessionDescription) -> Result<(), String> {
        let is_answer = sdp.kind.eq_ignore_ascii_case("answer") || looks_like_answer(&sdp.sdp);
        let desc = if is_answer {
            RTCSessionDescription::answer(sdp.sdp.clone())
        } else {
            RTCSessionDescription::offer(sdp.sdp.clone())
        }
        .map_err(|e| format!("bad session description: {e}"))?;
        self.pc
            .set_remote_description(desc)
            .await
            .map_err(|e| format!("set remote description failed: {e}"))
    }

    /// Apply a remote ICE candidate.
    pub async fn add_remote_candidate(
        &self,
        candidate: &str,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    ) -> Result<(), String> {
        let candidate = candidate.strip_prefix("candidate:").unwrap_or(candidate);
        let init = webrtc::ice_transport::ice_candidate::RTCIceCandidateInit {
            candidate: format!("candidate:{candidate}"),
            sdp_mid,
            sdp_mline_index,
            username_fragment: None,
        };
        self.pc
            .add_ice_candidate(init)
            .await
            .map_err(|e| format!("add candidate failed: {e}"))
    }

    /// Wait until the peer connection is connected (or fails / times out).
    ///
    /// Observe current transport state without replacing the peer's single
    /// state-change callback. Multiple callers may wait concurrently, and
    /// ICE/DataChannel readiness need not emit an aggregate state transition.
    ///
    /// Some webrtc-rs versions leave the answerer's aggregate state at `New`
    /// even once the transport is usable, so we also accept the ICE
    /// connection state or an open local DataChannel as "connected".
    pub async fn wait_connected(&self, timeout: Duration) -> Result<(), String> {
        let is_connected = |pc: &RTCPeerConnection| -> bool {
            use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
            match pc.connection_state() {
                RTCPeerConnectionState::Connected => {
                    return true;
                }
                RTCPeerConnectionState::Failed => return false,
                _ => {}
            }
            match pc.ice_connection_state() {
                RTCIceConnectionState::Connected | RTCIceConnectionState::Completed => true,
                RTCIceConnectionState::Failed => false,
                _ => {
                    // Local data channel open implies the SCTP/DTLS transport
                    // is up even if the aggregate state lags behind.
                    if let Some(dc) = self.local_channel() {
                        use webrtc::data_channel::data_channel_state::RTCDataChannelState;
                        dc.ready_state() == RTCDataChannelState::Open
                    } else {
                        false
                    }
                }
            }
        };

        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match self.pc.connection_state() {
                RTCPeerConnectionState::Failed => return Err("peer connection failed".to_string()),
                RTCPeerConnectionState::Closed => return Err("peer connection closed".to_string()),
                _ => {}
            }
            if is_connected(&self.pc) {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(format!(
                    "connect timeout (peer={}, ice={})",
                    self.pc.connection_state(),
                    self.pc.ice_connection_state()
                ));
            }
            tokio::time::sleep(remaining.min(Duration::from_millis(50))).await;
        }
    }

    /// The locally created data channel (host side).
    pub fn local_channel(&self) -> Option<Arc<RTCDataChannel>> {
        self.local_dc.clone()
    }

    /// Wait for the incoming data channel with our label (answer side).
    ///
    /// The receiver is registered at session creation, so the caller may
    /// await this at any time without missing the event.
    pub async fn accept_channel(&self) -> Result<Arc<RTCDataChannel>, String> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(dc) = self.pending_channel.lock().await.as_ref() {
                return Ok(dc.clone());
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err("timed out waiting for data channel".to_string());
            }
            let _ = tokio::time::timeout(remaining, self.channel_notify.notified()).await;
        }
    }

    /// Wait for the channel to open and wrap it with an I/O bridge wired to
    /// the session's inbound capture (no message can be lost).
    pub async fn wrap_channel(&self, dc: Arc<RTCDataChannel>) -> Result<DataChannelIo, String> {
        use webrtc::data_channel::data_channel_state::RTCDataChannelState;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            match dc.ready_state() {
                RTCDataChannelState::Open => break,
                RTCDataChannelState::Closed => {
                    return Err("data channel closed before open".to_string())
                }
                _ => {}
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err("data channel open timeout".to_string());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // Consume the session's pre-created inbound receiver (already
        // capturing from channel creation onward).
        let rx = self.take_inbound_receiver().await?;
        Ok(DataChannelIo::new_with_inbound(dc, rx))
    }

    /// Close the peer connection.
    pub async fn close(&self) {
        let _ = self.pc.close().await;
    }
}

/// ICE candidate relay loop.
///
/// Runs until the sender of `signal_tx` is dropped or the signal channel
/// yields `PeerLeft`/`Error`. Forwards local ICE candidates to the signaling
/// channel and applies remote ICE candidates to the session.
pub async fn run_ice_loop(
    session: &P2pSession,
    signal: &mut crate::signaling::SignalClient,
    signal_rx: &mut mpsc::UnboundedReceiver<P2pSignalEvent>,
) -> Result<(), String> {
    loop {
        tokio::select! {
            ev = signal_rx.recv() => match ev {
                Some(P2pSignalEvent::LocalIce { candidate, sdp_mid, sdp_mline_index }) => {
                    signal.send(&crate::signaling::SignalMessage::Ice {
                        candidate,
                        sdp_mid,
                        sdp_mline_index,
                    })?;
                }
                Some(P2pSignalEvent::LocalDescription(_)) => { /* handled before */ }
                None => break,
            },
            msg = signal.recv() => match msg? {
                crate::signaling::SignalMessage::Ice { candidate, sdp_mid, sdp_mline_index } => {
                    session.add_remote_candidate(&candidate, sdp_mid, sdp_mline_index).await?;
                }
                crate::signaling::SignalMessage::PeerLeft => break,
                crate::signaling::SignalMessage::Error { message } => return Err(message),
                _ => {}
            },
        }
    }
    Ok(())
}

/// Bridge a WebRTC DataChannel to the synchronous `std::io::Read + Write`
/// interface used by the transfer protocol.
///
/// - Inbound: `on_message` waits for space in a bounded channel; [`Read`]
///   consumes them.
/// - Outbound: [`Write`] enqueues messages; a worker task sends them via the
///   channel's async `send`.
///
/// The outbound worker runs on the tokio runtime that existed when
/// [`DataChannelIo::new`] was called.
pub struct DataChannelIo {
    dc: Arc<RTCDataChannel>,
    inbound: mpsc::Receiver<Bytes>,
    runtime: tokio::runtime::Handle,
    read_buf: Vec<u8>,
    write_tx: mpsc::Sender<WriteCommand>,
}

enum WriteCommand {
    Data(Bytes),
    /// Ordered barrier: acknowledged only after every earlier message has
    /// been handed to SCTP and the DataChannel buffer has drained.
    Flush(std::sync::mpsc::SyncSender<Result<(), String>>),
}

impl DataChannelIo {
    /// Create the bridge for a data channel. Spawns the outbound worker and
    /// bridges the inbound channel (provided by the session, which captures
    /// messages from channel creation onward) to the blocking read buffer.
    /// Must be called on a tokio runtime.
    pub fn new_with_inbound(dc: Arc<RTCDataChannel>, inbound_rx: mpsc::Receiver<Bytes>) -> Self {
        // Outbound: mpsc -> DataChannel::send. The worker waits for the
        // channel to open before sending so early writes are not dropped.
        let (write_tx, mut write_rx) = mpsc::channel::<WriteCommand>(32);
        let dc_write = dc.clone();
        tokio::spawn(async move {
            use webrtc::data_channel::data_channel_state::RTCDataChannelState;
            let opened = tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    let st = dc_write.ready_state();
                    if st == RTCDataChannelState::Open {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await;
            if opened.is_err() {
                return;
            }
            // Backpressure: keep the SCTP buffer under a bounded size so the
            // channel does not stall/close under sustained load.
            const HIGH_WATER: usize = 256 * 1024;
            while let Some(command) = write_rx.recv().await {
                match command {
                    WriteCommand::Data(data) => {
                        let result = tokio::time::timeout(Duration::from_secs(30), async {
                            while dc_write.buffered_amount().await > HIGH_WATER {
                                if dc_write.ready_state() != RTCDataChannelState::Open {
                                    return Err("data channel closed".to_string());
                                }
                                tokio::time::sleep(Duration::from_millis(10)).await;
                            }
                            dc_write.send(&data).await.map_err(|e| e.to_string())
                        })
                        .await;
                        if !matches!(result, Ok(Ok(_))) {
                            return;
                        }
                    }
                    WriteCommand::Flush(reply) => {
                        let result = tokio::time::timeout(Duration::from_secs(30), async {
                            while dc_write.buffered_amount().await > 0 {
                                if dc_write.ready_state() != RTCDataChannelState::Open {
                                    return Err("data channel closed while flushing".to_string());
                                }
                                tokio::time::sleep(Duration::from_millis(10)).await;
                            }
                            Ok(())
                        })
                        .await
                        .unwrap_or_else(|_| Err("data channel flush timeout".to_string()));
                        let _ = reply.send(result);
                    }
                }
            }
        });

        Self {
            dc,
            inbound: inbound_rx,
            runtime: tokio::runtime::Handle::current(),
            read_buf: Vec::new(),
            write_tx,
        }
    }

    /// Create the bridge for a data channel. Installs the inbound handler at
    /// wrap time (messages sent before this may be dropped; prefer
    /// [`P2pSession::wrap_channel`]).
    pub fn new(dc: Arc<RTCDataChannel>) -> Self {
        let (tx, rx) = mpsc::channel::<Bytes>(INBOUND_CAPACITY);
        let tx2 = tx.clone();
        dc.on_message(Box::new(move |msg: DataChannelMessage| {
            let tx = tx2.clone();
            Box::pin(async move {
                let _ = tx.send(msg.data).await;
            })
        }));
        Self::new_with_inbound(dc, rx)
    }

    /// Attach an inbound forwarder (kept for callers that manage the channel
    /// themselves via [`P2pSession`]).
    pub fn attach_inbound(dc: &Arc<RTCDataChannel>, tx: mpsc::Sender<Bytes>) {
        let tx2 = tx.clone();
        dc.on_message(Box::new(move |msg: DataChannelMessage| {
            let tx = tx2.clone();
            Box::pin(async move {
                let _ = tx.send(msg.data).await;
            })
        }));
    }

    /// Wait for the channel to open, then wrap it.
    ///
    /// Polls `ready_state` (robust against late registrations) while also
    /// installing the `on_open` handler so the wait completes promptly either
    /// way.
    pub async fn open_and_wrap(dc: &Arc<RTCDataChannel>) -> Result<Self, String> {
        use webrtc::data_channel::data_channel_state::RTCDataChannelState;
        if dc.ready_state() == RTCDataChannelState::Open {
            return Ok(Self::new(dc.clone()));
        }
        let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
        let tx2 = tx;
        dc.on_open(Box::new(move || {
            let tx = tx2;
            Box::pin(async move {
                let _ = tx.send(());
            })
        }));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            match dc.ready_state() {
                RTCDataChannelState::Open => break,
                RTCDataChannelState::Closed => {
                    return Err("data channel closed before open".to_string())
                }
                _ => {}
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err("data channel open timeout".to_string());
            }
            tokio::select! {
                _ = &mut rx => break,
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        }
        Ok(Self::new(dc.clone()))
    }
    /// Number of bytes buffered locally (diagnostics).
    pub fn buffered(&self) -> usize {
        self.read_buf.len()
    }

    /// Close the channel.
    pub async fn close(&self) {
        let _ = self.dc.close().await;
    }

    /// Underlying channel (for lifecycle management).
    pub fn channel(&self) -> &Arc<RTCDataChannel> {
        &self.dc
    }
}

impl Read for DataChannelIo {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        // Serve from the buffer first.
        if !self.read_buf.is_empty() {
            let n = out.len().min(self.read_buf.len());
            out[..n].copy_from_slice(&self.read_buf[..n]);
            self.read_buf.drain(..n);
            return Ok(n);
        }
        // This synchronous adapter is used on a blocking thread. Waiting on
        // the bounded receiver directly propagates disk backpressure to SCTP
        // without another forwarding task or unbounded queue.
        let inbound = &mut self.inbound;
        let dc = &self.dc;
        let chunk = self.runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(60), async {
                loop {
                    tokio::select! {
                        biased;
                        chunk = inbound.recv() => match chunk {
                            Some(chunk) if !chunk.is_empty() => return Ok(chunk),
                            Some(_) => continue, // Empty messages are not stream EOF.
                            None => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "data channel closed")),
                        },
                        _ = tokio::time::sleep(Duration::from_millis(100)) => {
                            if matches!(dc.ready_state(),
                                webrtc::data_channel::data_channel_state::RTCDataChannelState::Closing
                                | webrtc::data_channel::data_channel_state::RTCDataChannelState::Closed) {
                                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "data channel closed"));
                            }
                        }
                    }
                }
            }).await.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "data channel read timeout"))?
        })?;
        let n = out.len().min(chunk.len());
        out[..n].copy_from_slice(&chunk[..n]);
        // Keep the remainder for the next read.
        if n < chunk.len() {
            self.read_buf.extend_from_slice(&chunk[n..]);
        }
        Ok(n)
    }
}

impl Write for DataChannelIo {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        // SCTP datachannel messages are limited to DEFAULT_MAX_MESSAGE_SIZE
        // (64 KiB); WebRTC best practice is 16 KiB. Split large writes into
        // multiple channel messages so the underlying transport always fits.
        const MAX_WRITE: usize = 4 * 1024;
        let mut written = 0usize;
        while written < data.len() {
            let end = (written + MAX_WRITE).min(data.len());
            self.write_tx
                .blocking_send(WriteCommand::Data(Bytes::copy_from_slice(
                    &data[written..end],
                )))
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "data channel closed"))?;
            written = end;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        self.write_tx
            .blocking_send(WriteCommand::Flush(reply_tx))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "data channel closed"))?;
        reply_rx
            .recv_timeout(std::time::Duration::from_secs(35))
            .map_err(|e| match e {
                std::sync::mpsc::RecvTimeoutError::Timeout => {
                    io::Error::new(io::ErrorKind::TimedOut, "data channel flush timeout")
                }
                std::sync::mpsc::RecvTimeoutError::Disconnected => {
                    io::Error::new(io::ErrorKind::BrokenPipe, "data channel closed")
                }
            })?
            .map_err(|message| {
                let kind = if message == "data channel closed while flushing" {
                    io::ErrorKind::ConnectionReset
                } else {
                    io::ErrorKind::BrokenPipe
                };
                io::Error::new(kind, message)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::transfer;
    use std::time::Duration;

    #[test]
    fn ice_server_lists_support_direct_and_turn_fallbacks() {
        let config = Config {
            stun_url: "stun:a.example:3478, stun:b.example:3478".into(),
            turn_url: "turn:a.example:3478?transport=udp; turns:a.example:443?transport=tcp".into(),
            turn_username: "user".into(),
            turn_password: "secret".into(),
            ..Config::default()
        };

        let servers = ice_servers(&config);
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].urls.len(), 2);
        assert_eq!(servers[1].username, "user");
        assert_eq!(servers[1].credential, "secret");
    }

    /// Creates two peer connections wired together peer-to-peer on loopback by
    /// relaying the SDP exchange directly (no signaling server needed).
    async fn establish_pair() -> (P2pSession, P2pSession) {
        let config = Config {
            stun_url: String::new(),
            ..Config::default()
        };
        // Loopback test: a remote STUN server is irrelevant (and injects an
        // unusable srflx candidate + retry delays on offline CI hosts).
        let (tx_a, mut rx_a) = mpsc::unbounded_channel::<P2pSignalEvent>();
        let (tx_b, mut rx_b) = mpsc::unbounded_channel::<P2pSignalEvent>();

        let session_a = P2pSession::create_offer_session(&config, tx_a)
            .await
            .unwrap();
        let offer_sdp = loop {
            if let P2pSignalEvent::LocalDescription(sdp) = rx_a.recv().await.unwrap() {
                break sdp;
            }
        };
        let session_b = P2pSession::accept_offer_session(&config, &offer_sdp, tx_b)
            .await
            .unwrap();
        let answer_sdp = loop {
            if let P2pSignalEvent::LocalDescription(sdp) = rx_b.recv().await.unwrap() {
                break sdp;
            }
        };
        session_a.set_remote_description(&answer_sdp).await.unwrap();

        session_a
            .wait_connected(Duration::from_secs(15))
            .await
            .unwrap();

        (session_a, session_b)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_connection_waiters_observe_the_same_connection() {
        let config = Config {
            stun_url: String::new(),
            ..Config::default()
        };
        let (tx_a, mut rx_a) = mpsc::unbounded_channel();
        let (tx_b, mut rx_b) = mpsc::unbounded_channel();
        let sender = P2pSession::create_offer_session(&config, tx_a)
            .await
            .unwrap();
        let offer = loop {
            if let P2pSignalEvent::LocalDescription(sdp) = rx_a.recv().await.unwrap() {
                break sdp;
            }
        };
        let receiver = P2pSession::accept_offer_session(&config, &offer, tx_b)
            .await
            .unwrap();
        let answer = loop {
            if let P2pSignalEvent::LocalDescription(sdp) = rx_b.recv().await.unwrap() {
                break sdp;
            }
        };
        // Poll both before completing negotiation: neither can use the
        // already-connected fast path, and both must observe the transition.
        let mut first = std::pin::pin!(sender.wait_connected(Duration::from_secs(5)));
        let mut second = std::pin::pin!(sender.wait_connected(Duration::from_secs(5)));
        assert!(futures_util::poll!(first.as_mut()).is_pending());
        assert!(futures_util::poll!(second.as_mut()).is_pending());
        sender.set_remote_description(&answer).await.unwrap();
        let (first, second) = tokio::join!(first, second);
        assert!(first.is_ok(), "first waiter: {first:?}");
        assert!(second.is_ok(), "second waiter: {second:?}");
        receiver
            .wait_connected(Duration::from_secs(5))
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn slow_receiver_queue_is_bounded_and_transfer_recovers() {
        let (sender, receiver) = establish_pair().await;
        let mut output = sender
            .wrap_channel(sender.local_channel().unwrap())
            .await
            .unwrap();
        let channel = receiver.accept_channel().await.unwrap();
        let expected = vec![0x5a; 2 * 1024 * 1024];
        let data = expected.clone();
        let writer = tokio::task::spawn_blocking(move || {
            output.write_all(&data)?;
            output.flush()
        });
        let saturated = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let length = receiver.inbound.lock().await.as_ref().unwrap().len();
                assert!(length <= INBOUND_CAPACITY);
                if length == INBOUND_CAPACITY {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .is_ok();
        // Always drain before asserting, so even a failed observation cannot
        // leave a blocking writer behind during runtime shutdown.
        let mut input = receiver.wrap_channel(channel).await.unwrap();
        let reader = tokio::task::spawn_blocking(move || {
            let mut actual = vec![0; expected.len()];
            input.read_exact(&mut actual).unwrap();
            assert_eq!(actual, expected);
        });
        writer.await.unwrap().unwrap();
        reader.await.unwrap();
        assert!(saturated, "slow receiver should exercise the queue limit");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn channel_wrap_is_single_use_and_close_wakes_reader() {
        let (_sender, receiver) = establish_pair().await;
        let channel = receiver.accept_channel().await.unwrap();
        let mut input = receiver.wrap_channel(channel.clone()).await.unwrap();
        let repeated = receiver.wrap_channel(channel).await;
        assert_eq!(
            repeated.err().as_deref(),
            Some("data channel already wrapped")
        );
        let reader = tokio::task::spawn_blocking(move || input.read(&mut [0; 1]));
        receiver.close().await;
        let result = tokio::time::timeout(Duration::from_secs(3), reader)
            .await
            .expect("closing a session must wake its blocking reader promptly")
            .unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn data_channel_transfers_file_end_to_end() {
        let dir = std::env::temp_dir().join(format!(
            "mausfer-test-webrtc-{}-{}",
            std::process::id(),
            transfer::new_transfer_id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let src = dir.join("payload.bin");
        let mut data = vec![0u8; 300_000];
        let mut seed: u64 = 0xABCD_EF01_2345_6789;
        for b in data.iter_mut() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            *b = (seed & 0xFF) as u8;
        }
        std::fs::write(&src, &data).unwrap();

        let (session_a, session_b) = establish_pair().await;
        let dc_a = session_a.local_channel().unwrap();
        let dc_b = session_b.accept_channel().await.unwrap();
        let io_a = session_a.wrap_channel(dc_a).await.unwrap();
        let io_b = session_b.wrap_channel(dc_b).await.unwrap();
        let meta = transfer::FileMetadata::for_file(
            &src,
            transfer::new_transfer_id(),
            transfer::DEFAULT_CHUNK_SIZE,
        )
        .unwrap();

        let recv_dir = dir.join("recv");
        std::fs::create_dir_all(&recv_dir).unwrap();

        let send_meta = meta.clone();
        let recv_dir2 = recv_dir.clone();
        let send_task = tokio::spawn(async move {
            let io = io_a;
            tokio::task::spawn_blocking(move || {
                let mut io = io;
                transfer::send_file(&mut io, &src, &send_meta).unwrap()
            })
            .await
            .unwrap()
        });
        let recv_task = tokio::spawn(async move {
            let io = io_b;
            tokio::task::spawn_blocking(move || {
                let mut io = io;
                transfer::receive_file(&mut io, &recv_dir2).unwrap()
            })
            .await
            .unwrap()
        });

        let sent = send_task.await.unwrap();
        let received = recv_task.await.unwrap();
        assert_eq!(sent.bytes_sent, meta.file_size);
        assert_eq!(received.bytes_written, meta.file_size);
        assert_eq!(sent.sha256, meta.sha256);
        assert_eq!(received.sha256, meta.sha256);
        assert!(received.saved_path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
