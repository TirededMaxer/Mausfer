//! Mausfer core library.
//!
//! This crate contains all platform-independent logic:
//! configuration, logging, device discovery, file transfer metadata,
//! and future P2P/WebRTC session management.

pub mod android;
pub mod app;
pub mod config;
pub mod discovery;
pub mod identity;
pub mod logger;
pub mod p2p;
pub mod paths;
pub mod remote;
pub mod signaling;
pub mod transfer;

pub use app::App;
pub use config::{Config, ConfigError, ConfigResult};
pub use discovery::{
    DeviceInfo, DiscoveryConfig, DiscoveryMessage, DiscoveryService, MessageType,
    DEFAULT_DISCOVERY_PORT, MAUSFER_SERVICE,
};
pub use identity::device_id;
pub use logger::Logger;
pub use p2p::{DataChannelIo, P2pSession, P2pSignalEvent, CHANNEL_LABEL, CONNECT_TIMEOUT};
pub use paths::{AndroidPaths, DesktopPaths, PlatformPaths};
pub use remote::{remote_recv, remote_send, RemoteOptions, RemoteReport};
pub use signaling::{
    new_room_code, valid_room_code, SessionDescription, SignalClient, SignalMessage,
    SignalingServer,
};
pub use transfer::{
    new_transfer_id, read_frame, receive_file, sanitize_file_name, send_file, sha256_file,
    unique_destination_path, write_frame, AcceptInfo, DoneInfo, ErrorInfo, FileMetadata, FrameKind,
    ReceiveReport, SendReport, DEFAULT_CHUNK_SIZE,
};
