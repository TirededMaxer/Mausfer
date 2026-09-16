//! Shared Tauri glue for desktop platform shells (Windows / macOS).
//!
//! Each platform crate is a thin `main()` that calls [`builder`] with its
//! [`PlatformPaths`] and runs `tauri::generate_context!()`.

use mausfer_core::{
    new_transfer_id, App, DeviceInfo, DiscoveryConfig, DiscoveryService, FileMetadata,
    PlatformPaths, SendReport, DEFAULT_CHUNK_SIZE, DEFAULT_DISCOVERY_PORT,
};
use serde::Serialize;
use std::io::Write as _;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tauri::{Emitter, Manager};

#[cfg(target_os = "android")]
struct AndroidFileMetadata(tauri::plugin::PluginHandle<tauri::Wry>);

pub struct AppState {
    core: App,
    signaling_status: Mutex<SignalingStatus>,
    discovery: Mutex<Option<DiscoveryService>>,
    /// Active connection-code receive task. Starting a new session or pressing
    /// Stop aborts the previous task, which drops its signaling/WebRTC state.
    remote_rx: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    /// App handle for emitting transfer lifecycle events.
    app_handle: Option<tauri::AppHandle<tauri::Wry>>,
    /// Optional post-receive hook (used by Android MediaStore publication).
    pub on_file_received: Option<std::sync::Arc<dyn OnFileReceived>>,
}

#[derive(Serialize, Clone)]
pub struct SignalingStatus {
    pub address: String,
    pub state: String,
    pub message: String,
}

#[derive(Serialize, Clone)]
pub struct Status {
    pub device_name: String,
    pub device_id: String,
    pub port: u16,
    pub download_dir: String,
    pub config_path: String,
    pub log_path: String,
    pub version: String,
    pub desktop: bool,
    pub signaling_url: String,
    pub stun_url: String,
    pub turn_url: String,
    pub turn_username: String,
    pub turn_password: String,
}

#[derive(Serialize, Clone)]
pub struct PeerView {
    pub id: String,
    pub name: String,
    pub port: u16,
    pub addr: Option<String>,
}

#[derive(Serialize, Clone)]
pub struct RemoteSendOutcome {
    pub file_name: String,
    pub bytes: u64,
    pub sha256: String,
}

fn transfer_progress_handler(
    handle: Option<tauri::AppHandle<tauri::Wry>>,
    role: &'static str,
) -> impl FnMut(mausfer_core::transfer::TransferProgress) + Send + 'static {
    move |progress| {
        if let Some(handle) = &handle {
            let _ = handle.emit(
                "transfer-progress",
                serde_json::json!({
                    "transfer_id": progress.transfer_id, "role": role,
                    "completed": progress.completed, "total": progress.total,
                    "active": progress.active,
                }),
            );
        }
    }
}

fn append_log(log_path: &Path, level: &str, message: &str) {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        let _ = writeln!(f, "[{now}] [{level}] {message}");
    }
}

fn status_of(core: &App) -> Status {
    Status {
        device_name: core.device_info().name,
        device_id: mausfer_core::device_id(),
        port: core.config().port,
        download_dir: core
            .resolved_download_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        config_path: core.config_path.display().to_string(),
        log_path: core.log_path.display().to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        desktop: !cfg!(target_os = "android"),
        signaling_url: core.config().signaling_url,
        stun_url: core.config().stun_url,
        turn_url: core.config().turn_url,
        turn_username: core.config().turn_username,
        turn_password: core.config().turn_password,
    }
}

/// Tauri commands for the shared desktop shell.
///
/// These live in a submodule: `#[tauri::command]` on `pub` functions in a
/// lib crate root triggers a macro-namespace collision (`#[macro_export]`
/// + `pub use` reimport), which the submodule avoids.
pub mod commands {
    use super::*;

    #[tauri::command]
    pub fn get_status(state: tauri::State<AppState>) -> Status {
        status_of(&state.core)
    }

    #[tauri::command]
    pub fn get_signaling_status(state: tauri::State<AppState>) -> SignalingStatus {
        state.signaling_status.lock().unwrap().clone()
    }

    #[tauri::command]
    pub fn set_signaling_url(
        state: tauri::State<AppState>,
        address: String,
    ) -> Result<String, String> {
        let address = state.core.set_signaling_url(address)?;
        *state.signaling_status.lock().unwrap() = SignalingStatus {
            address: address.clone(),
            state: if address.is_empty() {
                "unconfigured"
            } else {
                "connecting"
            }
            .into(),
            message: String::new(),
        };
        Ok(address)
    }

    #[tauri::command]
    pub fn set_ice_settings(
        state: tauri::State<AppState>,
        stun: String,
        turn: String,
        username: String,
        password: String,
    ) -> Result<(), String> {
        state.core.set_ice_settings(stun, turn, username, password)
    }

    /// Set a custom download directory (persisted to config.json; empty string
    /// resets to the platform default Downloads folder).
    #[tauri::command]
    pub fn set_download_dir(state: tauri::State<AppState>, path: String) -> Result<String, String> {
        let resolved = state
            .core
            .set_download_dir(path)
            .map_err(|e| e.to_string())?;
        Ok(resolved)
    }

    /// Open a native directory picker (desktop); returns the chosen path.
    #[cfg(not(target_os = "android"))]
    #[tauri::command]
    pub async fn pick_directory(
        app: tauri::AppHandle<tauri::Wry>,
    ) -> Result<Option<String>, String> {
        use tauri_plugin_dialog::DialogExt;
        let picked = tauri::async_runtime::spawn_blocking(move || {
            let dialog = app.dialog().file();
            let dialog = match app.get_webview_window("main") {
                Some(window) => dialog.set_parent(&window),
                None => dialog,
            };
            dialog.blocking_pick_folder()
        })
        .await
        .map_err(|e| e.to_string())?;
        Ok(picked
            .and_then(|p| p.into_path().ok())
            .map(|p| p.display().to_string()))
    }

    /// Open the config directory in the system file manager (Finder/Explorer).
    #[cfg(not(target_os = "android"))]
    #[tauri::command]
    pub fn open_config_dir(state: tauri::State<AppState>) -> Result<(), String> {
        let dir = state
            .core
            .config_path
            .parent()
            .ok_or("config path has no parent")?
            .to_path_buf();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        #[cfg(target_os = "macos")]
        let status = std::process::Command::new("open")
            .arg(&dir)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string());
        #[cfg(target_os = "windows")]
        let status = std::process::Command::new("explorer")
            .arg(&dir)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string());
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let status: Result<(), String> = Ok(());
        status
    }

    /// Open a native file picker and return the selected path (None if the
    /// user cancels). Required because Tauri 2 no longer exposes `File.path`
    /// to webview JS.
    ///
    /// On Android the SAF picker returns a content:// URI; the fs plugin's
    /// content-resolver access is used to copy the bytes into the private
    /// download dir so the rest of the core can read a plain path.
    #[tauri::command]
    pub async fn pick_file(app: tauri::AppHandle<tauri::Wry>) -> Result<Option<String>, String> {
        use tauri_plugin_dialog::DialogExt;
        // The blocking dialog must not run on the async runtime thread;
        // spawn_blocking keeps the event loop responsive.
        let dialog_app = app.clone();
        let picked = tauri::async_runtime::spawn_blocking(move || {
            let dialog = dialog_app.dialog().file();
            #[cfg(not(target_os = "android"))]
            let dialog = match dialog_app.get_webview_window("main") {
                Some(window) => dialog.set_parent(&window),
                None => dialog,
            };
            dialog.blocking_pick_file()
        })
        .await
        .map_err(|e| e.to_string())?;
        let Some(p) = picked else { return Ok(None) };

        #[cfg(target_os = "android")]
        {
            let uri_str = match &p {
                tauri_plugin_dialog::FilePath::Url(u) => u.to_string(),
                tauri_plugin_dialog::FilePath::Path(pb) => {
                    return Ok(Some(pb.display().to_string()))
                }
            };
            if !(uri_str.starts_with("content://") || uri_str.starts_with("file://")) {
                return Ok(Some(uri_str));
            }
            // Read via the fs plugin (content resolver) and stage privately.
            let mut opts = tauri_plugin_fs::OpenOptions::default();
            opts.read(true);
            let file_path: tauri_plugin_fs::FilePath = uri_str.parse().unwrap();
            let mut file = tauri_plugin_fs::FsExt::fs(&app)
                .open(file_path, opts)
                .map_err(|e| format!("打开 {uri_str} 失败: {e}"))?;
            #[derive(serde::Deserialize)]
            struct DisplayName {
                name: String,
            }
            let name: DisplayName = app
                .state::<AndroidFileMetadata>()
                .0
                .run_mobile_plugin("displayName", serde_json::json!({ "uri": uri_str }))
                .map_err(|e| e.to_string())?;
            let staging = app
                .path()
                .app_cache_dir()
                .map_err(|e| e.to_string())?
                .join("picked-files")
                .join(new_transfer_id());
            std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
            let dest = staging.join(mausfer_core::sanitize_file_name(&name.name));
            let result = tauri::async_runtime::spawn_blocking(move || {
                let mut out = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&dest)
                    .map_err(|e| e.to_string())?;
                if let Err(e) = std::io::copy(&mut file, &mut out) {
                    drop(out);
                    let _ = std::fs::remove_file(&dest);
                    return Err(e.to_string());
                }
                Ok::<_, String>(dest)
            })
            .await
            .map_err(|e| e.to_string())??;
            let dest = result;
            Ok(Some(dest.display().to_string()))
        }
        #[cfg(not(target_os = "android"))]
        {
            Ok(p.into_path().ok().map(|p| p.display().to_string()))
        }
    }

    /// Return name + size for a file path (for the UI picker display).
    #[tauri::command]
    pub fn stat_file(path: String) -> Result<(String, u64), String> {
        let p = PathBuf::from(&path);
        let meta = std::fs::metadata(&p).map_err(|e| e.to_string())?;
        if !meta.is_file() {
            return Err("不是一个文件".into());
        }
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.clone());
        Ok((name, meta.len()))
    }

    /// Start (or restart) the unified connection-code receive flow.
    ///
    /// The command returns immediately with a one-time room code while the
    /// actual signaling, ICE (direct/TURN), transfer, verification, and
    /// final ACK run in the background. Completion/failure is delivered as a
    /// Tauri event so the UI remains responsive and Stop can cancel the task.
    #[tauri::command]
    pub fn start_remote_receive(state: tauri::State<AppState>) -> Result<String, String> {
        let mut active = state.remote_rx.lock().unwrap();
        if let Some(previous) = active.take() {
            previous.abort();
        }

        let room_code = mausfer_core::new_room_code();
        let task_code = room_code.clone();
        let recv_dir = state
            .core
            .resolved_download_dir()
            .ok_or("无法解析下载目录")?;
        let config = state.core.config();
        let signal_url = config.signaling_url.clone();
        if signal_url.is_empty() {
            return Err("请先设置自建信令服务器地址".into());
        }
        let device = state.core.device_info();
        let handle = state.app_handle.clone();
        let hook = state.on_file_received.clone();
        let log_path = state.core.log_path.clone();

        let task = tauri::async_runtime::spawn(async move {
            let result = mausfer_core::remote::remote_recv_observed(
                &config,
                mausfer_core::RemoteOptions {
                    room_code: task_code.clone(),
                    signal_url,
                    device,
                },
                &recv_dir,
                |phase| {
                    append_log(&log_path, "INFO", phase);
                    if let Some(h) = &handle { let _ = h.emit("remote-phase", serde_json::json!({ "role": "receive", "room_code": task_code, "message": phase })); }
                },
                transfer_progress_handler(handle.clone(), "receive"),
            )
            .await;

            match result {
                Ok(mausfer_core::RemoteReport::Received(report)) => {
                    append_log(
                        &log_path,
                        "INFO",
                        &format!(
                            "received {} ({} bytes) via room {} -> {}",
                            report.file_name,
                            report.bytes_written,
                            task_code,
                            report.saved_path.display()
                        ),
                    );
                    if let Some(h) = &handle {
                        let _ = h.emit(
                            "remote-receive-completed",
                            serde_json::json!({
                                "room_code": task_code,
                                "file_name": report.file_name,
                                "bytes_written": report.bytes_written,
                                "saved_path": report.saved_path.display().to_string(),
                            }),
                        );
                        #[cfg(not(target_os = "android"))]
                        super::desktop::notify_received(h, &report.file_name, report.bytes_written);
                    }
                    if let Some(hook) = hook {
                        hook(&report.saved_path);
                    }
                }
                Ok(_) => {}
                Err(message) => {
                    append_log(
                        &log_path,
                        "ERROR",
                        &format!("remote receive failed in room {task_code}: {message}"),
                    );
                    if let Some(h) = &handle {
                        let _ = h.emit(
                            "remote-receive-failed",
                            serde_json::json!({ "room_code": task_code, "message": message }),
                        );
                    }
                }
            }
        });
        *active = Some(task);
        Ok(room_code)
    }

    /// Stop the active connection-code receive task.
    #[tauri::command]
    pub fn stop_remote_receive(state: tauri::State<AppState>) -> Result<(), String> {
        if let Some(task) = state.remote_rx.lock().unwrap().take() {
            task.abort();
        }
        Ok(())
    }

    #[tauri::command]
    pub fn list_peers(state: tauri::State<AppState>) -> Vec<PeerView> {
        let guard = state.discovery.lock().unwrap();
        match guard.as_ref() {
            Some(d) => d
                .peers()
                .into_iter()
                .map(|p: DeviceInfo| PeerView {
                    addr: d.peer_addr(&p.id).map(|a| a.to_string()),
                    id: p.id,
                    name: p.name,
                    port: p.port,
                })
                .collect(),
            None => Vec::new(),
        }
    }

    #[tauri::command]
    pub async fn send_to_peer(
        state: tauri::State<'_, AppState>,
        path: String,
        peer_id: String,
    ) -> Result<SendReport, String> {
        let path = PathBuf::from(&path);
        if !path.exists() {
            return Err(format!("文件不存在: {}", path.display()));
        }
        let (ip, port) = {
            let guard = state.discovery.lock().unwrap();
            let discovery = guard.as_ref().ok_or("discovery not started")?;
            let peer = discovery
                .peers()
                .into_iter()
                .find(|p| p.id == peer_id)
                .ok_or_else(|| format!("找不到设备: {peer_id}"))?;
            let addr = discovery.peer_addr(&peer_id).ok_or("设备地址不可用")?;
            (addr.ip(), peer.port)
        };

        let log_path = state.core.log_path.clone();
        let progress_handle = state.app_handle.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let meta = FileMetadata::for_file(&path, new_transfer_id(), DEFAULT_CHUNK_SIZE)
                .map_err(|e| e.to_string())?;
            let target = SocketAddr::new(ip, port);
            let mut stream =
                TcpStream::connect_timeout(&target, std::time::Duration::from_secs(10))
                    .map_err(|e| format!("连接失败: {e}"))?;
            let timeout = Some(std::time::Duration::from_secs(60));
            stream
                .set_read_timeout(timeout)
                .map_err(|e| e.to_string())?;
            stream
                .set_write_timeout(timeout)
                .map_err(|e| e.to_string())?;
            let report = mausfer_core::transfer::send_file_with_progress(
                &mut stream,
                &path,
                &meta,
                transfer_progress_handler(progress_handle, "send"),
            )
            .map_err(|e| e.to_string())?;
            append_log(
                &log_path,
                "INFO",
                &format!(
                    "sent {} ({} bytes) to {}",
                    report.file_name, report.bytes_sent, target
                ),
            );
            Ok(report)
        })
        .await
        .map_err(|e| e.to_string())?
    }

    /// Send through the unified connection-code path. ICE automatically tries
    /// local and configured TURN relay candidates.
    #[tauri::command]
    pub async fn remote_send_file(
        state: tauri::State<'_, AppState>,
        path: String,
        room_code: String,
    ) -> Result<RemoteSendOutcome, String> {
        let path = PathBuf::from(&path);
        if !path.exists() {
            return Err(format!("文件不存在: {}", path.display()));
        }
        let config = state.core.config();
        let signal_url = config.signaling_url.clone();
        if signal_url.is_empty() {
            return Err("请先设置自建信令服务器地址".into());
        }
        let device = state.core.device_info();
        let report = mausfer_core::remote::remote_send_observed(
            &config,
            mausfer_core::RemoteOptions {
                room_code,
                signal_url,
                device,
            },
            &path,
            |phase| {
                append_log(&state.core.log_path, "INFO", phase);
                if let Some(h) = &state.app_handle {
                    let _ = h.emit(
                        "remote-phase",
                        serde_json::json!({ "role": "send", "message": phase }),
                    );
                }
            },
            transfer_progress_handler(state.app_handle.clone(), "send"),
        )
        .await
        .inspect_err(|message| {
            append_log(&state.core.log_path, "ERROR", message);
        })?;
        let (r, _code) = report;
        match r {
            mausfer_core::RemoteReport::Sent(s) => Ok(RemoteSendOutcome {
                file_name: s.file_name,
                bytes: s.bytes_sent,
                sha256: s.sha256,
            }),
            _ => Err("unexpected remote report".into()),
        }
    }
}

/// Called after a file is successfully received. The default is a no-op;
/// platform shells (e.g. Android) use it to publish the file elsewhere.
pub trait OnFileReceived: Fn(&std::path::Path) + Send + Sync + 'static {}
impl<T: Fn(&std::path::Path) + Send + Sync + 'static> OnFileReceived for T {}

/// Desktop-only integration: system tray icon + menu, and OS notifications.
#[cfg(not(target_os = "android"))]
mod desktop {
    use super::*;
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::TrayIconBuilder;

    pub fn setup(app: &tauri::App<tauri::Wry>) -> tauri::Result<()> {
        use tauri_plugin_notification::NotificationExt;

        let show = MenuItem::with_id(app, "show-window", "显示主窗口", true, None::<&str>)?;
        let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
        let menu = Menu::with_items(app, &[&show, &quit])?;

        let mut builder = TrayIconBuilder::with_id("main-tray")
            .tooltip("Mausfer 文件传输")
            .menu(&menu)
            .show_menu_on_left_click(true)
            .on_menu_event(|app, event| match event.id.as_ref() {
                "show-window" => {
                    if let Some(w) = app.get_webview_window("main") {
                        let _ = w.show();
                        let _ = w.unminimize();
                        let _ = w.set_focus();
                    }
                }
                "quit" => app.exit(0),
                _ => {}
            });
        if let Some(icon) = app.default_window_icon() {
            builder = builder.icon(icon.clone());
        }
        builder.build(app)?;

        // Request notification permission; no-op on macOS/Windows if denied.
        let _ = app.notification().request_permission();
        Ok(())
    }

    /// Fire an OS notification for a completed transfer.
    pub fn notify_received(handle: &tauri::AppHandle<tauri::Wry>, file_name: &str, bytes: u64) {
        use tauri_plugin_notification::NotificationExt;
        let _ = handle
            .notification()
            .builder()
            .title("Mausfer 收到文件")
            .body(format!("{file_name}（{bytes} 字节）"))
            .show();
    }
}

/// Build the shared Tauri application (setup + receive listener + discovery)
/// for a desktop shell. The shell crate is responsible for applying
/// `invoke_handler` with the path-qualified command names.
pub fn builder<P: PlatformPaths + Send + Sync + 'static>(paths: P) -> tauri::Builder<tauri::Wry> {
    builder_with_hook(paths, |_| {})
}

/// Like [`builder`], but with a callback invoked for every received file.
///
/// The callback receives the saved path (already in the download directory).
pub fn builder_with_hook<P: PlatformPaths + Send + Sync + 'static>(
    paths: P,
    on_file_received: impl OnFileReceived,
) -> tauri::Builder<tauri::Wry> {
    #[allow(unused_mut)]
    let mut builder = tauri::Builder::<tauri::Wry>::default();
    // The notification plugin needs Kotlin-side registration on Android
    // (generated by `tauri android init`); only enable it on desktop shells.
    #[cfg(not(target_os = "android"))]
    {
        builder = builder
            .plugin(tauri_plugin_notification::init())
            .plugin(tauri_plugin_dialog::init());
    }
    // The fs plugin provides content:// URI resolution on Android (SAF
    // selections); its Kotlin module is wired in gen/android.
    #[cfg(target_os = "android")]
    {
        builder = builder
            .plugin(tauri_plugin_dialog::init())
            .plugin(tauri_plugin_fs::init())
            .plugin(
                tauri::plugin::Builder::<tauri::Wry>::new("file-metadata")
                    .setup(|app, api| {
                        let handle =
                            api.register_android_plugin("com.mausfer.app", "FileMetadataPlugin")?;
                        app.manage(AndroidFileMetadata(handle));
                        Ok(())
                    })
                    .build(),
            );
    }
    // Close-to-tray on desktop: keep the app (and the receive listener)
    // running in the tray; quit via the tray menu.
    #[cfg(not(target_os = "android"))]
    let builder = builder.on_window_event(|window, event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            let _ = window.hide();
        }
    });
    builder.setup(move |app| {
        let core = App::init(&paths)?;
        let receive_dir = core.resolved_download_dir().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "download directory unavailable",
            )
        })?;
        std::fs::create_dir_all(&receive_dir)?;

        let on_file_received: std::sync::Arc<dyn OnFileReceived> =
            std::sync::Arc::new(on_file_received);
        let state = AppState {
            signaling_status: Mutex::new(SignalingStatus {
                address: core.config().signaling_url,
                state: "connecting".into(),
                message: String::new(),
            }),
            core,
            discovery: Mutex::new(None),
            remote_rx: Mutex::new(None),
            app_handle: Some(app.handle().clone()),
            on_file_received: Some(on_file_received.clone()),
        };
        app.manage(state);
        let monitor_handle = app.handle().clone();
        tauri::async_runtime::spawn(async move {
            mausfer_core::signaling::monitor_connection(
                || {
                    monitor_handle
                        .state::<AppState>()
                        .core
                        .config()
                        .signaling_url
                },
                |address, status, message| {
                    let state = monitor_handle.state::<AppState>();
                    if state.core.config().signaling_url != address {
                        return;
                    }
                    let mut current = state.signaling_status.lock().unwrap();
                    if current.state != status || current.message != message {
                        append_log(
                            &state.core.log_path,
                            "INFO",
                            &format!("signaling {status}: {message}"),
                        );
                    }
                    *current = SignalingStatus {
                        address: address.into(),
                        state: status.into(),
                        message: message.into(),
                    };
                },
            )
            .await;
        });
        let state = app.state::<AppState>();
        let handle = app.handle().clone();
        let log_path = state.core.log_path.clone();
        let bind_port = state.core.config().port;

        #[cfg(not(target_os = "android"))]
        desktop::setup(app)?;

        // LAN discovery: announce + listen on the discovery port.
        let mut info = state.core.device_info();
        info.port = bind_port;
        let mut discovery = DiscoveryService::new(
            info,
            DiscoveryConfig {
                port: DEFAULT_DISCOVERY_PORT,
                ..Default::default()
            },
        )?;
        discovery.start()?;
        append_log(&log_path, "INFO", "discovery started");
        *state.discovery.lock().unwrap() = Some(discovery);

        // Receive listener: accept transfers and save into the download dir.
        let listener = TcpListener::bind(("0.0.0.0", bind_port))?;
        append_log(&log_path, "INFO", &format!("listening on port {bind_port}"));

        let hook: std::sync::Arc<dyn OnFileReceived> = on_file_received;
        let limiter = mausfer_core::transfer::ReceiveLimiter::new(state.core.config().max_threads);
        let auto_accept = state.core.config().auto_accept;
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if !auto_accept {
                    continue;
                }
                let Some(permit) = limiter.try_acquire() else {
                    continue;
                };
                let log_path = log_path.clone();
                let Some(recv_dir) = handle.state::<AppState>().core.resolved_download_dir() else {
                    append_log(&log_path, "ERROR", "download directory unavailable");
                    continue;
                };
                let handle = handle.clone();
                let hook = hook.clone();
                std::thread::spawn(move || {
                    let _permit = permit;
                    match conn {
                        Ok(stream) => {
                            let mut stream = stream;
                            let timeout = Some(std::time::Duration::from_secs(60));
                            if stream
                                .set_read_timeout(timeout)
                                .and_then(|_| stream.set_write_timeout(timeout))
                                .is_err()
                            {
                                return;
                            }
                            match mausfer_core::transfer::receive_file_with_progress(
                                &mut stream,
                                &recv_dir,
                                transfer_progress_handler(Some(handle.clone()), "receive"),
                            ) {
                                Ok(report) => {
                                    append_log(
                                        &log_path,
                                        "INFO",
                                        &format!(
                                            "received {} ({} bytes) -> {}",
                                            report.file_name,
                                            report.bytes_written,
                                            report.saved_path.display()
                                        ),
                                    );
                                    let _ = handle.emit(
                                        "transfer-completed",
                                        serde_json::json!({
                                            "file_name": report.file_name,
                                            "bytes_written": report.bytes_written,
                                            "saved_path": report.saved_path.display().to_string(),
                                        }),
                                    );
                                    #[cfg(not(target_os = "android"))]
                                    desktop::notify_received(
                                        &handle,
                                        &report.file_name,
                                        report.bytes_written,
                                    );
                                    // Post-receive hook (Android publication etc.)
                                    hook(&report.saved_path);
                                }
                                Err(e) => {
                                    append_log(&log_path, "ERROR", &format!("receive failed: {e}"));
                                }
                            }
                        }
                        Err(e) => append_log(&log_path, "ERROR", &format!("accept failed: {e}")),
                    }
                });
            }
        });

        Ok(())
    })
}
