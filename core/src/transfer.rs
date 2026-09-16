//! File transfer protocol for Mausfer (LAN TCP transport).
//!
//! Wire format: each frame is `[kind: u8][len: u32 BE][payload]`.
//!
//! - `HELLO`  JSON `FileMetadata`
//! - `ACCEPT` JSON `AcceptInfo { offset }` — resume offset from the receiver
//! - `CHUNK`  binary: `[offset: u64 BE][raw data]`
//! - `DONE`   JSON `DoneInfo { bytes_written, sha256, file_name }`
//! - `COMPLETE` JSON `DoneInfo` — receiver verified and committed the file
//! - `ERROR`  JSON `ErrorInfo { message }`
//! - `PROGRESS` negotiated checkpoint: empty request / confirmed u64 offset reply

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Shared admission control for LAN listeners. A permit releases on every
/// completion or error, keeping worker creation bounded.
#[derive(Clone)]
pub struct ReceiveLimiter {
    active: std::sync::Arc<std::sync::atomic::AtomicU32>,
    limit: u32,
}

pub struct ReceivePermit(std::sync::Arc<std::sync::atomic::AtomicU32>);
impl Drop for ReceivePermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}
impl ReceiveLimiter {
    pub fn new(limit: u32) -> Self {
        Self {
            active: Default::default(),
            limit: limit.clamp(1, 64),
        }
    }
    pub fn try_acquire(&self) -> Option<ReceivePermit> {
        self.active
            .fetch_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |n| (n < self.limit).then_some(n + 1),
            )
            .ok()
            .map(|_| ReceivePermit(self.active.clone()))
    }
}

/// Default chunk size for LAN transfers (256 KiB).
pub const DEFAULT_CHUNK_SIZE: u32 = 256 * 1024;

/// Maximum accepted frame payload (chunk data + header slack).
const MAX_FRAME_PAYLOAD: usize = DEFAULT_CHUNK_SIZE as usize + 64 * 1024;

/// Metadata for a single file transfer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileMetadata {
    pub transfer_id: String,
    pub file_name: String,
    pub file_size: u64,
    pub chunk_size: u32,
    pub sha256: String,
}

impl FileMetadata {
    /// Build metadata for a file on disk (computes size and whole-file hash).
    pub fn for_file(
        path: &Path,
        transfer_id: impl Into<String>,
        chunk_size: u32,
    ) -> io::Result<Self> {
        let file = File::open(path)?;
        let file_size = file.metadata()?.len();
        let sha256 = sha256_file(path)?;
        let file_name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "unnamed".to_string());
        Ok(Self {
            transfer_id: transfer_id.into(),
            file_name,
            file_size,
            chunk_size,
            sha256,
        })
    }
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    Hello = 0,
    Accept = 1,
    Chunk = 2,
    Done = 3,
    Error = 4,
    Complete = 5,
    Progress = 6,
}

impl FrameKind {
    fn from_u8(v: u8) -> Option<Self> {
        use FrameKind::*;
        Some(match v {
            0 => Hello,
            1 => Accept,
            2 => Chunk,
            3 => Done,
            4 => Error,
            5 => Complete,
            6 => Progress,
            _ => return None,
        })
    }
}

/// Write one frame: `[kind: u8][len: u32 BE][payload]`.
pub fn write_frame<W: Write>(w: &mut W, kind: FrameKind, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_FRAME_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame payload too large",
        ));
    }
    w.write_all(&[kind as u8])?;
    w.write_all(&(payload.len() as u32).to_be_bytes())?;
    w.write_all(payload)
}

/// Read one frame. Returns `None` on a clean EOF at a frame boundary.
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Option<(FrameKind, Vec<u8>)>> {
    let mut kind_buf = [0u8; 1];
    let n = r.read(&mut kind_buf)?;
    if n == 0 {
        return Ok(None);
    }
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame payload too large",
        ));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    let kind = FrameKind::from_u8(kind_buf[0])
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unknown frame kind"))?;
    Ok(Some((kind, payload)))
}

fn write_json<W: Write>(w: &mut W, kind: FrameKind, value: &impl Serialize) -> io::Result<()> {
    let bytes = serde_json::to_vec(value)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    write_frame(w, kind, &bytes)
}

fn read_json<T: for<'de> Deserialize<'de>>(payload: &[u8]) -> io::Result<T> {
    serde_json::from_slice(payload)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

// ---------------------------------------------------------------------------
// Protocol messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcceptInfo {
    /// Number of bytes already on disk; sender resumes from here.
    pub offset: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DoneInfo {
    pub bytes_written: u64,
    pub sha256: String,
    pub file_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorInfo {
    pub message: String,
}

/// Result of a completed receive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReceiveReport {
    pub transfer_id: String,
    pub file_name: String,
    pub saved_path: PathBuf,
    pub bytes_written: u64,
    pub sha256: String,
}

/// Result of a completed send.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SendReport {
    pub transfer_id: String,
    pub file_name: String,
    pub bytes_sent: u64,
    pub sha256: String,
}

/// Sidecar state for resume, stored next to the `.part` file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PartState {
    transfer_id: String,
    file_name: String,
    file_size: u64,
    chunk_size: u32,
    sha256: String,
}

// ---------------------------------------------------------------------------
// Hashing helpers
// ---------------------------------------------------------------------------

/// Hex-encoded SHA-256 of a file's contents.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn validate_metadata(meta: &FileMetadata) -> io::Result<()> {
    if meta.chunk_size == 0
        || meta.chunk_size as usize > MAX_FRAME_PAYLOAD - 8
        || meta.sha256.len() != 64
        || !meta.sha256.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid file metadata",
        ));
    }
    Ok(())
}

fn staging_paths(dir: &Path, name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let key = hex::encode(Sha256::digest(name.as_bytes()));
    let staging = dir.join(".mausfer-partials");
    (
        staging.join(format!("{key}.part")),
        staging.join(format!("{key}.json")),
        staging.join(format!("{key}.lock")),
    )
}

// ---------------------------------------------------------------------------
// Sender side
// ---------------------------------------------------------------------------

/// Send `path` over an already-connected stream using `metadata`.
///
/// The receiver replies with an `ACCEPT` frame carrying a resume offset; the
/// sender seeks to that offset and streams chunks until `DONE`.
/// Cumulative bytes confirmed by the receiver, including resumed bytes.
#[derive(Debug, Clone, Serialize)]
pub struct TransferProgress {
    pub transfer_id: String,
    pub completed: u64,
    pub total: u64,
    pub active: bool,
}

struct ProgressReporter<F: FnMut(TransferProgress)> {
    event: TransferProgress,
    callback: F,
}
impl<F: FnMut(TransferProgress)> ProgressReporter<F> {
    fn new(meta: &FileMetadata, offset: u64, callback: F) -> Self {
        let mut reporter = Self {
            event: TransferProgress {
                transfer_id: meta.transfer_id.clone(),
                completed: offset,
                total: meta.file_size,
                active: true,
            },
            callback,
        };
        reporter.update(offset);
        reporter
    }
    fn update(&mut self, bytes: u64) {
        self.event.completed = bytes;
        (self.callback)(self.event.clone());
    }
}
impl<F: FnMut(TransferProgress)> Drop for ProgressReporter<F> {
    fn drop(&mut self) {
        self.event.active = false;
        (self.callback)(self.event.clone());
    }
}

pub fn send_file<S: Read + Write>(
    stream: &mut S,
    path: &Path,
    metadata: &FileMetadata,
) -> io::Result<SendReport> {
    send_file_with_progress(stream, path, metadata, |_| {})
}

pub fn send_file_with_progress<S: Read + Write>(
    stream: &mut S,
    path: &Path,
    metadata: &FileMetadata,
    callback: impl FnMut(TransferProgress),
) -> io::Result<SendReport> {
    validate_metadata(metadata)?;
    let mut file = File::open(path)?;
    let actual = file.metadata()?.len();
    if actual != metadata.file_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file changed size since metadata was computed",
        ));
    }

    let mut hello = serde_json::to_value(metadata)?;
    hello["progress"] = true.into();
    write_json(stream, FrameKind::Hello, &hello)?;

    // Receiver's resume offset.
    let (kind, payload) = read_frame(stream)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "connection closed before accept",
        )
    })?;
    match kind {
        FrameKind::Accept => {}
        FrameKind::Error => {
            let err: ErrorInfo = read_json(&payload)?;
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                err.message,
            ));
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected ACCEPT frame",
            ))
        }
    }
    let confirmed_progress = read_json::<serde_json::Value>(&payload)?["progress"]
        .as_bool()
        .unwrap_or(false);
    let accept: AcceptInfo = read_json(&payload)?;
    if accept.offset > metadata.file_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "resume offset exceeds file size",
        ));
    }

    file.seek(SeekFrom::Start(accept.offset))?;
    let chunk_size = metadata.chunk_size.max(1) as usize;
    let mut buf = vec![0u8; chunk_size];
    let mut offset = accept.offset;
    let mut sent = 0u64;
    let mut reporter = ProgressReporter::new(metadata, offset, callback);
    let mut last_confirmed = offset;
    let mut checkpoint = std::time::Instant::now();

    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let payload_len = 8 + n;
        let mut chunk_payload = Vec::with_capacity(payload_len);
        chunk_payload.extend_from_slice(&offset.to_be_bytes());
        chunk_payload.extend_from_slice(&buf[..n]);
        write_frame(stream, FrameKind::Chunk, &chunk_payload)?;
        offset += n as u64;
        sent += n as u64;
        if confirmed_progress
            && (offset == metadata.file_size
                || offset - last_confirmed >= 4 * 1024 * 1024
                || checkpoint.elapsed() >= std::time::Duration::from_millis(150))
        {
            // A bounded checkpoint provides genuine receive progress without
            // requiring a round trip for every chunk on a high-latency link.
            write_frame(stream, FrameKind::Progress, &[])?;
            stream.flush()?;
            let (kind, payload) = read_frame(stream)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "closed before progress confirmation",
                )
            })?;
            if kind == FrameKind::Error {
                let error: ErrorInfo = read_json(&payload)?;
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    error.message,
                ));
            }
            if kind != FrameKind::Progress || payload.as_slice() != offset.to_be_bytes() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid progress confirmation",
                ));
            }
            reporter.update(offset);
            last_confirmed = offset;
            checkpoint = std::time::Instant::now();
        } else if !confirmed_progress {
            // Compatibility with an older receiver that did not negotiate ACKs.
            reporter.update(offset);
        }
    }

    let done = DoneInfo {
        bytes_written: sent,
        sha256: metadata.sha256.clone(),
        file_name: metadata.file_name.clone(),
    };
    write_json(stream, FrameKind::Done, &done)?;
    stream.flush()?;

    // A successful local write only means the bytes reached the transport.
    // Do not report success until the receiver has persisted, hashed, and
    // renamed the file. This also keeps WebRTC sessions alive while queued
    // DataChannel messages are still being delivered.
    let (kind, payload) = read_frame(stream)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "connection closed before completion confirmation",
        )
    })?;
    match kind {
        FrameKind::Complete => {
            let confirmed: DoneInfo = read_json(&payload)?;
            if confirmed.bytes_written != metadata.file_size
                || confirmed.sha256 != metadata.sha256
                || confirmed.file_name != metadata.file_name
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "receiver completion confirmation does not match the file",
                ));
            }
        }
        FrameKind::Error => {
            let err: ErrorInfo = read_json(&payload)?;
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                err.message,
            ));
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected COMPLETE frame",
            ))
        }
    }

    Ok(SendReport {
        transfer_id: metadata.transfer_id.clone(),
        file_name: metadata.file_name.clone(),
        bytes_sent: sent,
        sha256: metadata.sha256.clone(),
    })
}

// ---------------------------------------------------------------------------
// Receiver side
// ---------------------------------------------------------------------------

/// Receive one file into `download_dir` over an accepted stream.
///
/// Resumes automatically when a matching `.part` + sidecar state exist.
/// On success the `.part` file is renamed to a collision-free final name;
/// on failure the partial file is kept so a later attempt can resume.
pub fn receive_file<S: Read + Write>(
    stream: &mut S,
    download_dir: &Path,
) -> io::Result<ReceiveReport> {
    receive_file_with_progress(stream, download_dir, |_| {})
}

pub fn receive_file_with_progress<S: Read + Write>(
    stream: &mut S,
    download_dir: &Path,
    callback: impl FnMut(TransferProgress),
) -> io::Result<ReceiveReport> {
    fs::create_dir_all(download_dir)?;

    // -- HELLO ------------------------------------------------------------
    let (kind, payload) = read_frame(stream)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "connection closed before hello",
        )
    })?;
    if kind != FrameKind::Hello {
        let err = ErrorInfo {
            message: "expected HELLO".to_string(),
        };
        write_json(stream, FrameKind::Error, &err)?;
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "first frame was not HELLO",
        ));
    }
    let confirmed_progress = read_json::<serde_json::Value>(&payload)?["progress"]
        .as_bool()
        .unwrap_or(false);
    let meta: FileMetadata = read_json(&payload)?;
    validate_metadata(&meta)?;

    // -- Resume state -----------------------------------------------------
    let safe_name = sanitize_file_name(&meta.file_name);
    let (part_path, state_path, lock_path) = staging_paths(download_dir, &safe_name);
    fs::create_dir_all(part_path.parent().unwrap())?;
    // OS lock releases on errors and process exit. Keep the lock inode stable.
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    lock.try_lock()
        .map_err(|e| io::Error::new(io::ErrorKind::WouldBlock, e.to_string()))?;

    let mut offset = 0u64;
    let resume_ok = fs::read(&state_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<PartState>(&bytes).ok())
        .is_some_and(|state| {
            state.sha256 == meta.sha256
                && state.file_size == meta.file_size
                && state.file_name == safe_name
        })
        && part_path.is_file();

    if resume_ok {
        offset = fs::metadata(&part_path)?.len();
        if offset > meta.file_size {
            // Stale/corrupt partial; restart from scratch.
            let _ = fs::remove_file(&part_path);
            let _ = fs::remove_file(&state_path);
            offset = 0;
        }
    } else {
        let _ = fs::remove_file(&part_path);
        let _ = fs::remove_file(&state_path);
    }

    let mut part = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(!resume_ok || offset == 0)
        .open(&part_path)?;
    part.seek(SeekFrom::Start(offset))?;
    let state = PartState {
        transfer_id: meta.transfer_id.clone(),
        file_name: safe_name.clone(),
        file_size: meta.file_size,
        chunk_size: meta.chunk_size,
        sha256: meta.sha256.clone(),
    };
    fs::write(&state_path, serde_json::to_vec(&state)?)?;
    write_json(
        stream,
        FrameKind::Accept,
        &serde_json::json!({ "offset": offset, "progress": confirmed_progress }),
    )?;

    let mut written = offset;
    let mut reporter = ProgressReporter::new(&meta, offset, callback);

    loop {
        let Some((kind, payload)) = read_frame(stream)? else {
            // Clean EOF before DONE: keep partial for resume.
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before DONE",
            ));
        };
        match kind {
            FrameKind::Chunk => {
                if payload.len() < 8 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "short chunk"));
                }
                let mut off = [0u8; 8];
                off.copy_from_slice(&payload[..8]);
                let chunk_offset = u64::from_be_bytes(off);
                if chunk_offset != written {
                    // Out-of-order chunk; we can only write sequentially.
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "out-of-order chunk",
                    ));
                }
                let chunk_len = (payload.len() - 8) as u64;
                if chunk_len > meta.file_size - written {
                    let _ = fs::remove_file(&state_path);
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "received more bytes than expected",
                    ));
                }
                part.write_all(&payload[8..])?;
                written += chunk_len;
                if !confirmed_progress {
                    reporter.update(written);
                }
            }
            FrameKind::Progress => {
                if !confirmed_progress || !payload.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid progress checkpoint",
                    ));
                }
                reporter.update(written);
                write_frame(stream, FrameKind::Progress, &written.to_be_bytes())?;
                stream.flush()?;
            }
            FrameKind::Done => {
                let info: DoneInfo = read_json(&payload)?;
                part.sync_all()?;
                if info.sha256 != meta.sha256 || info.file_name != meta.file_name {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "DONE differs from HELLO",
                    ));
                }

                // Verify whole-file hash.
                let actual_sha = sha256_file(&part_path)?;
                if actual_sha != info.sha256 {
                    drop(part);
                    let _ = fs::remove_file(&part_path);
                    let _ = fs::remove_file(&state_path);
                    let err = ErrorInfo {
                        message: format!(
                            "sha256 mismatch: expected {}, got {}",
                            info.sha256, actual_sha
                        ),
                    };
                    let _ = write_json(stream, FrameKind::Error, &err);
                    let _ = stream.flush();
                    return Err(io::Error::new(io::ErrorKind::InvalidData, err.message));
                }

                if written != meta.file_size || info.bytes_written != meta.file_size - offset {
                    let err = ErrorInfo {
                        message: format!(
                            "size mismatch: expected {} bytes, received {}",
                            meta.file_size, written
                        ),
                    };
                    let _ = write_json(stream, FrameKind::Error, &err);
                    let _ = stream.flush();
                    return Err(io::Error::new(io::ErrorKind::InvalidData, err.message));
                }

                drop(part);
                let final_path = publish_file(&part_path, download_dir, &safe_name)?;
                let report = ReceiveReport {
                    transfer_id: meta.transfer_id,
                    file_name: info.file_name,
                    saved_path: final_path,
                    bytes_written: written,
                    sha256: actual_sha,
                };
                let confirmation = DoneInfo {
                    bytes_written: report.bytes_written,
                    sha256: report.sha256.clone(),
                    file_name: report.file_name.clone(),
                };
                write_json(stream, FrameKind::Complete, &confirmation)?;
                // The sender may receive COMPLETE and immediately close the
                // WebRTC channel while our local buffered-amount counter is
                // still settling. At that point the file is already safely
                // committed and the ACK was handed to SCTP; report local
                // receive success. Other flush failures remain real errors.
                if let Err(error) = stream.flush() {
                    if error.kind() != io::ErrorKind::ConnectionReset {
                        return Err(error);
                    }
                }
                let _ = fs::remove_file(&state_path);
                return Ok(report);
            }
            FrameKind::Error => {
                let err: ErrorInfo = read_json(&payload)?;
                let _ = fs::remove_file(&state_path);
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    err.message,
                ));
            }
            _ => {
                let _ = fs::remove_file(&state_path);
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected frame",
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Name helpers
// ---------------------------------------------------------------------------

/// Remove path separators and control characters from a file name.
pub fn sanitize_file_name(name: &str) -> String {
    let mut result = String::new();
    for ch in name.chars() {
        if matches!(ch, '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*') || ch.is_control() {
            result.push('_');
        } else {
            result.push(ch);
        }
    }
    let trimmed = result.trim().trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        "unnamed".to_string()
    } else {
        let stem = trimmed.split('.').next().unwrap_or("").to_ascii_uppercase();
        if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ((stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.len() == 4
                && matches!(stem.as_bytes()[3], b'1'..=b'9'))
        {
            format!("_{trimmed}")
        } else {
            trimmed.to_string()
        }
    }
}

/// Return a path that does not collide with an existing file.
///
/// If `file.ext` exists, this returns `file (1).ext`, then `file (2).ext`, etc.
pub fn unique_destination_path(dir: &Path, file_name: &str) -> PathBuf {
    let safe_name = sanitize_file_name(file_name);
    let candidate = dir.join(&safe_name);
    if candidate.symlink_metadata().is_err() {
        return candidate;
    }

    let stem = Path::new(&safe_name)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let ext = Path::new(&safe_name)
        .extension()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    for i in 1u64.. {
        let name = if ext.is_empty() {
            format!("{stem} ({i})")
        } else {
            format!("{stem} ({i}).{ext}")
        };
        let candidate = dir.join(name);
        if candidate.symlink_metadata().is_err() {
            return candidate;
        }
    }

    unreachable!("destination counter exhausted")
}

/// Publish without replacing a pre-existing destination. Hard links make
/// publication atomic on APFS/NTFS; exclusive creation also supports FAT/exFAT.
pub fn publish_file(source: &Path, dir: &Path, name: &str) -> io::Result<PathBuf> {
    loop {
        let destination = unique_destination_path(dir, name);
        match fs::hard_link(source, &destination) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => {
                let mut output = match OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&destination)
                {
                    Ok(file) => file,
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e),
                };
                let result = File::open(source)
                    .and_then(|mut input| io::copy(&mut input, &mut output))
                    .and_then(|_| output.sync_all());
                drop(output);
                if let Err(e) = result {
                    let _ = fs::remove_file(&destination);
                    return Err(e);
                }
            }
        }
        fs::remove_file(source)?;
        return Ok(destination);
    }
}

/// Generate a unique transfer id.
pub fn new_transfer_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}-{}", nanos, std::process::id())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    struct ConnectionResetOnFlush(TcpStream, Vec<u8>);

    impl Read for ConnectionResetOnFlush {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.read(buf)
        }
    }

    impl Write for ConnectionResetOnFlush {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let written = self.0.write(buf)?;
            self.1.extend_from_slice(&buf[..written]);
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            let mut frames = io::Cursor::new(std::mem::take(&mut self.1));
            let mut completed = false;
            while let Some((kind, _)) = read_frame(&mut frames)? {
                completed |= kind == FrameKind::Complete;
            }
            if !completed {
                return self.0.flush();
            }
            Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "peer closed after receiving completion",
            ))
        }
    }

    /// Spin up a receiver in a thread; the receiver connects back to the
    /// listener, and the caller accepts the other end. Returns
    /// `(JoinHandle<io::Result<ReceiveReport>>, TcpListener)`.
    fn spawn_receiver(
        download_dir: PathBuf,
    ) -> (thread::JoinHandle<io::Result<ReceiveReport>>, TcpListener) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let dir = download_dir.clone();
        let handle = thread::spawn(move || {
            let mut stream = TcpStream::connect(addr).unwrap();
            receive_file(&mut stream, &dir)
        });
        (handle, listener)
    }

    fn random_file(dir: &Path, name: &str, size: usize) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        let mut data = vec![0u8; size];
        // Deterministic pseudo-random content.
        let mut seed: u64 = 0x1234_5678_9abc_def0;
        for b in data.iter_mut() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            *b = (seed & 0xFF) as u8;
        }
        fs::write(&path, &data).unwrap();
        path
    }

    #[test]
    fn sender_and_receiver_report_the_same_confirmed_progress() {
        for size in [0, 10 * 1024 * 1024 + 123] {
            let dir = std::env::temp_dir().join(new_transfer_id());
            let src = random_file(&dir, "progress.bin", size);
            let meta = FileMetadata::for_file(&src, new_transfer_id(), DEFAULT_CHUNK_SIZE).unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let dest = dir.join("recv");
            let receiver = std::thread::spawn(move || {
                let mut stream = listener.accept().unwrap().0;
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                    .unwrap();
                let mut events = Vec::new();
                let report =
                    receive_file_with_progress(&mut stream, &dest, |p| events.push(p)).unwrap();
                (report, events)
            });
            let mut stream = TcpStream::connect(addr).unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            let mut sent = Vec::new();
            send_file_with_progress(&mut stream, &src, &meta, |p| sent.push(p)).unwrap();
            let (received, events) = receiver.join().unwrap();
            assert_eq!(sha256_file(&received.saved_path).unwrap(), meta.sha256);
            assert_eq!(
                sent.iter()
                    .map(|p| (p.completed, p.total, p.active))
                    .collect::<Vec<_>>(),
                events
                    .iter()
                    .map(|p| (p.completed, p.total, p.active))
                    .collect::<Vec<_>>()
            );
            assert!(!sent.last().unwrap().active);
            assert_eq!(sent.last().unwrap().completed, size as u64);
            assert!(sent.windows(2).all(|w| w[0].completed <= w[1].completed));
            if size > 0 {
                assert!(sent.len() >= 4);
            }
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn wrong_progress_ack_fails_and_hides_progress() {
        let dir = std::env::temp_dir().join(new_transfer_id());
        let src = random_file(&dir, "wrong-ack.bin", 10);
        let meta = FileMetadata::for_file(&src, new_transfer_id(), DEFAULT_CHUNK_SIZE).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let receiver = std::thread::spawn(move || {
            let mut stream = listener.accept().unwrap().0;
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            assert_eq!(
                read_frame(&mut stream).unwrap().unwrap().0,
                FrameKind::Hello
            );
            write_json(
                &mut stream,
                FrameKind::Accept,
                &serde_json::json!({"offset":0,"progress":true}),
            )
            .unwrap();
            loop {
                if read_frame(&mut stream).unwrap().unwrap().0 == FrameKind::Progress {
                    write_frame(&mut stream, FrameKind::Progress, &999u64.to_be_bytes()).unwrap();
                    break;
                }
            }
        });
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let mut events = Vec::new();
        let error =
            send_file_with_progress(&mut stream, &src, &meta, |p| events.push(p)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!events.last().unwrap().active);
        assert_eq!(events.last().unwrap().completed, 0);
        receiver.join().unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn receive_permits_bound_concurrency_and_release_on_drop() {
        let limiter = ReceiveLimiter::new(1);
        let permit = limiter.try_acquire().unwrap();
        assert!(limiter.try_acquire().is_none());
        drop(permit);
        assert!(limiter.try_acquire().is_some());
    }

    #[test]
    fn interrupted_receive_resumes_without_manufactured_state() {
        let dir = std::env::temp_dir().join(new_transfer_id());
        let src = random_file(&dir, "resume.bin", 100_000);
        let meta = FileMetadata::for_file(&src, new_transfer_id(), DEFAULT_CHUNK_SIZE).unwrap();
        let recv_dir = dir.join("recv");
        let (rx, listener) = spawn_receiver(recv_dir.clone());
        let mut stream = listener.accept().unwrap().0;
        write_json(&mut stream, FrameKind::Hello, &meta).unwrap();
        let (_, payload) = read_frame(&mut stream).unwrap().unwrap();
        assert_eq!(read_json::<AcceptInfo>(&payload).unwrap().offset, 0);
        let mut chunk = 0u64.to_be_bytes().to_vec();
        chunk.extend_from_slice(&fs::read(&src).unwrap()[..30_000]);
        write_frame(&mut stream, FrameKind::Chunk, &chunk).unwrap();
        drop(stream);
        assert_eq!(
            rx.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        let (rx, listener) = spawn_receiver(recv_dir.clone());
        let mut stream = listener.accept().unwrap().0;
        assert_eq!(
            send_file(&mut stream, &src, &meta).unwrap().bytes_sent,
            70_000
        );
        let report = rx.join().unwrap().unwrap();
        assert_eq!(sha256_file(&report.saved_path).unwrap(), meta.sha256);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupted_resume_state_restarts_and_preserves_user_part_file() {
        let dir = std::env::temp_dir().join(new_transfer_id());
        let src = random_file(&dir, "data.bin", 100);
        let meta = FileMetadata::for_file(&src, new_transfer_id(), DEFAULT_CHUNK_SIZE).unwrap();
        let recv_dir = dir.join("recv");
        let (partial, state, _) = staging_paths(&recv_dir, "data.bin");
        fs::create_dir_all(partial.parent().unwrap()).unwrap();
        fs::write(&partial, b"wrong").unwrap();
        fs::write(&state, b"invalid json").unwrap();
        fs::write(recv_dir.join("data.bin.part"), b"user file").unwrap();
        let (rx, listener) = spawn_receiver(recv_dir.clone());
        let mut stream = listener.accept().unwrap().0;
        assert_eq!(send_file(&mut stream, &src, &meta).unwrap().bytes_sent, 100);
        rx.join().unwrap().unwrap();
        assert_eq!(
            fs::read(recv_dir.join("data.bin.part")).unwrap(),
            b"user file"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn zero_byte_and_duplicate_names_are_committed_without_overwrite() {
        let dir = std::env::temp_dir().join(new_transfer_id());
        let src = random_file(&dir, "empty.txt", 0);
        let meta = FileMetadata::for_file(&src, new_transfer_id(), DEFAULT_CHUNK_SIZE).unwrap();
        let recv_dir = dir.join("recv");
        for expected in ["empty.txt", "empty (1).txt"] {
            let (rx, listener) = spawn_receiver(recv_dir.clone());
            let mut stream = listener.accept().unwrap().0;
            assert_eq!(send_file(&mut stream, &src, &meta).unwrap().bytes_sent, 0);
            assert!(rx.join().unwrap().unwrap().saved_path.ends_with(expected));
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_same_name_is_rejected_without_touching_active_partial() {
        let dir = std::env::temp_dir().join(new_transfer_id());
        let src = random_file(&dir, "same.bin", 100);
        let meta = FileMetadata::for_file(&src, new_transfer_id(), DEFAULT_CHUNK_SIZE).unwrap();
        let recv_dir = dir.join("recv");
        let (rx, listener) = spawn_receiver(recv_dir.clone());
        let mut stream = listener.accept().unwrap().0;
        write_json(&mut stream, FrameKind::Hello, &meta).unwrap();
        assert_eq!(
            read_frame(&mut stream).unwrap().unwrap().0,
            FrameKind::Accept
        );
        let (second, listener) = spawn_receiver(recv_dir.clone());
        let mut other = listener.accept().unwrap().0;
        write_json(&mut other, FrameKind::Hello, &meta).unwrap();
        assert_eq!(
            second.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        let (_, state, _) = staging_paths(&recv_dir, "same.bin");
        assert!(state.exists());
        drop(stream);
        rx.join().unwrap().unwrap_err();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn windows_reserved_names_are_safe_on_every_platform() {
        for (input, expected) in [
            ("..", "unnamed"),
            ("CON.txt", "_CON.txt"),
            ("a:b?", "a_b_"),
            ("name. ", "name"),
        ] {
            assert_eq!(sanitize_file_name(input), expected);
        }
    }

    #[test]
    fn sanitize_replaces_separators_and_control_chars() {
        assert_eq!(sanitize_file_name("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_file_name(""), "unnamed");
        assert_eq!(sanitize_file_name("  "), "unnamed");
    }

    #[test]
    fn unique_path_appends_counter() {
        let dir =
            std::env::temp_dir().join(format!("mausfer-test-transfer-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let first = unique_destination_path(&dir, "a.txt");
        fs::write(&first, "1").unwrap();
        let second = unique_destination_path(&dir, "a.txt");
        assert_eq!(second.file_name().unwrap().to_string_lossy(), "a (1).txt");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn frame_roundtrip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, FrameKind::Chunk, &[1, 2, 3]).unwrap();
        let mut cursor = io::Cursor::new(buf);
        let (kind, payload) = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(kind, FrameKind::Chunk);
        assert_eq!(payload, vec![1, 2, 3]);
    }

    #[test]
    fn send_receive_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "mausfer-test-roundtrip-{}-{}",
            std::process::id(),
            new_transfer_id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let src = random_file(&dir, "hello.bin", 1_000_000);
        let meta = FileMetadata::for_file(&src, new_transfer_id(), DEFAULT_CHUNK_SIZE).unwrap();

        let recv_dir = dir.join("downloads");
        let (rx, listener) = spawn_receiver(recv_dir.clone());

        let mut stream = listener.accept().unwrap().0;
        let report = send_file(&mut stream, &src, &meta).unwrap();
        assert_eq!(report.bytes_sent, meta.file_size);
        drop(stream);

        // Join receiver first so its error is visible in the panic message.
        let recv_report = rx.join().unwrap().expect("receiver must succeed");
        assert_eq!(recv_report.bytes_written, meta.file_size);
        assert_eq!(recv_report.sha256, meta.sha256);

        let saved = dir.join("downloads").join("hello.bin");
        assert!(saved.exists(), "file should be saved to download dir");
        assert_eq!(sha256_file(&saved).unwrap(), meta.sha256);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sender_requires_receiver_completion_confirmation() {
        let dir = std::env::temp_dir().join(format!(
            "mausfer-test-confirmation-{}-{}",
            std::process::id(),
            new_transfer_id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let src = random_file(&dir, "unconfirmed.bin", 32_000);
        let meta = FileMetadata::for_file(&src, new_transfer_id(), DEFAULT_CHUNK_SIZE).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let peer = thread::spawn(move || {
            let mut stream = TcpStream::connect(addr).unwrap();
            let (kind, _) = read_frame(&mut stream).unwrap().unwrap();
            assert_eq!(kind, FrameKind::Hello);
            write_json(&mut stream, FrameKind::Accept, &AcceptInfo { offset: 0 }).unwrap();

            loop {
                let Some((kind, _)) = read_frame(&mut stream).unwrap() else {
                    return;
                };
                if kind == FrameKind::Done {
                    // Simulate a peer that received the last byte but closed
                    // before confirming persistence and verification.
                    return;
                }
            }
        });

        let mut stream = listener.accept().unwrap().0;
        let err = send_file(&mut stream, &src, &meta)
            .expect_err("sender must not report success without COMPLETE");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        peer.join().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn receiver_stays_successful_when_peer_closes_after_complete() {
        let dir = std::env::temp_dir().join(format!(
            "mausfer-test-complete-close-{}-{}",
            std::process::id(),
            new_transfer_id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let src = random_file(&dir, "complete.bin", 32_000);
        let meta = FileMetadata::for_file(&src, new_transfer_id(), DEFAULT_CHUNK_SIZE).unwrap();
        let recv_dir = dir.join("downloads");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let receiver = thread::spawn(move || {
            let stream = TcpStream::connect(addr).unwrap();
            receive_file(&mut ConnectionResetOnFlush(stream, Vec::new()), &recv_dir)
        });

        let mut stream = listener.accept().unwrap().0;
        let sent = send_file(&mut stream, &src, &meta).unwrap();
        let received = receiver
            .join()
            .unwrap()
            .expect("committed receive must survive peer close after COMPLETE");
        assert_eq!(sent.sha256, received.sha256);
        assert_eq!(received.bytes_written, meta.file_size);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_after_partial() {
        let dir = std::env::temp_dir().join(format!(
            "mausfer-test-resume-{}-{}",
            std::process::id(),
            new_transfer_id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let src = random_file(&dir, "big.bin", 800_000);
        let meta = FileMetadata::for_file(&src, new_transfer_id(), DEFAULT_CHUNK_SIZE).unwrap();

        let recv_dir = dir.join("downloads");
        fs::create_dir_all(&recv_dir).unwrap();

        // Simulate an interrupted receive: write the first 300k bytes as .part
        // plus a matching sidecar state.
        let (part_path, state_path, _) = staging_paths(&recv_dir, "big.bin");
        fs::create_dir_all(part_path.parent().unwrap()).unwrap();
        let mut f = File::create(&part_path).unwrap();
        let mut src_file = File::open(&src).unwrap();
        let mut buf = [0u8; 4096];
        let mut left = 300_000usize;
        while left > 0 {
            let n = src_file.read(&mut buf).unwrap().min(left);
            f.write_all(&buf[..n]).unwrap();
            left -= n;
        }
        f.flush().unwrap();
        let state = PartState {
            transfer_id: meta.transfer_id.clone(),
            file_name: "big.bin".to_string(),
            file_size: meta.file_size,
            chunk_size: meta.chunk_size,
            sha256: meta.sha256.clone(),
        };
        fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();

        let (rx, listener) = spawn_receiver(recv_dir.clone());
        let mut stream = listener.accept().unwrap().0;
        let report = send_file(&mut stream, &src, &meta).unwrap();
        // Sender resumes at 300_000: only 500_000 bytes are sent.
        assert_eq!(report.bytes_sent, meta.file_size - 300_000);
        drop(stream);

        let recv_report = rx.join().unwrap().expect("receiver must succeed");
        assert_eq!(recv_report.bytes_written, meta.file_size);

        let saved = recv_dir.join("big.bin");
        assert!(saved.exists());
        assert_eq!(sha256_file(&saved).unwrap(), meta.sha256);
        assert!(!part_path.exists(), "part file should be cleaned up");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mismatch_fails_and_partial_is_kept_for_resume() {
        let dir = std::env::temp_dir().join(format!(
            "mausfer-test-mismatch-{}-{}",
            std::process::id(),
            new_transfer_id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let src = random_file(&dir, "m.bin", 100_000);
        let meta = FileMetadata::for_file(&src, new_transfer_id(), DEFAULT_CHUNK_SIZE).unwrap();

        let recv_dir = dir.join("downloads");
        let (rx, listener) = spawn_receiver(recv_dir.clone());

        let mut stream = listener.accept().unwrap().0;
        // Corrupt the metadata hash so verification must fail.
        let mut bad = meta.clone();
        bad.sha256 = "deadbeef".repeat(8);
        // The sender doesn't know the receiver rejects the hash at DONE time;
        // the receiver thread must report the error.
        let _ = send_file(&mut stream, &src, &bad);
        drop(stream);
        let result = rx.join().unwrap();
        assert!(result.is_err(), "receiver must reject a sha256 mismatch");
        let _ = fs::remove_dir_all(&dir);
    }
}
