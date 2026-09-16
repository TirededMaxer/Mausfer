//! Mausfer reference CLI.
//!
//! Platform-independent command line used to exercise the core library
//! (the same code the Tauri shells will call later):
//!
//! ```text
//! mausfer serve                     # listen for transfers + discover on LAN
//! mausfer send <file> [--to ip:port]
//! mausfer peers                     # one-shot discovery
//! mausfer config                    # show resolved paths
//! ```

use mausfer_core::remote::{
    remote_recv_with_progress as remote_recv, remote_send_with_progress as remote_send,
};
use mausfer_core::{
    device_id, new_transfer_id, receive_file, send_file, App, DesktopPaths, DiscoveryConfig,
    DiscoveryService, FileMetadata, RemoteOptions, SendReport, SignalingServer, DEFAULT_CHUNK_SIZE,
    DEFAULT_DISCOVERY_PORT,
};
use std::io::Write;
use std::net::SocketAddr;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::exit;
use std::sync::Arc;
use std::time::{Duration, Instant};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let sub = args.get(1).map(String::as_str).unwrap_or("config");
    let rest = args.get(2..).unwrap_or_default();

    let result = match sub {
        "serve" => cmd_serve(rest),
        "send" => cmd_send(rest),
        "peers" => cmd_peers(rest),
        "config" => cmd_config(),
        "remote-send" => cmd_remote_send(rest),
        "remote-recv" => cmd_remote_recv(rest),
        "signal-server" => cmd_signal_server(rest),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        other => {
            eprintln!("unknown command: {other}");
            print_help();
            exit(2);
        }
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        exit(1);
    }
}

fn print_help() {
    println!(
        "Mausfer {VERSION}
usage:
  mausfer serve [--port <tcp>] [--download <dir>] [--discovery-port <port>]
  mausfer send <file> [--to ip:port] [--port <tcp>] [--timeout <secs>] [--discovery-port <port>]
  mausfer peers [--discovery-port <port>] [--wait <secs>]
  mausfer remote-send <file> --code <XXXX-XXXX-XXXX-XXXX> [--signal ws://host:port (可选,默认使用已保存的服务器)]
  mausfer remote-recv [--code <XXXX-XXXX-XXXX-XXXX>] [--signal ws://host:port (可选,默认使用已保存的服务器)] [--download <dir>]
  mausfer signal-server [--addr 0.0.0.0:9322]
  mausfer config"
    );
}

fn cmd_config() -> Result<(), Box<dyn std::error::Error>> {
    let app = App::init(&DesktopPaths)?;
    println!("config:  {}", app.config_path.display());
    println!("log:     {}", app.log_path.display());
    println!("name:    {}", app.device_info().name);
    println!("port:    {}", app.config().port);
    match app.resolved_download_dir() {
        Some(d) => println!("download: {}", d.display()),
        None => println!("download: <unavailable; set download_dir in config.json>"),
    }
    Ok(())
}

/// Append one line to the instance's log without truncating (thread-safe).
///
/// The main `App::init` truncates the log once at startup; concurrent worker
/// threads use this so they never clobber earlier entries.
fn append_log(app: &App, level: &str, message: &str) {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&app.log_path)
    {
        let _ = writeln!(f, "[{now}] [{level}] {message}");
    }
}

fn cmd_serve(rest: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let port = parse_opt::<u16>(rest, "--port", None);
    let discovery_port =
        parse_opt::<u16>(rest, "--discovery-port", None).unwrap_or(DEFAULT_DISCOVERY_PORT);
    let download_dir = parse_opt::<String>(rest, "--download", None).map(PathBuf::from);

    let app = Arc::new(App::init(&DesktopPaths)?);
    let bind_port = port.unwrap_or(app.config().port);
    // Prefer CLI override, fall back to config, then platform default.
    let recv_dir = download_dir
        .or_else(|| app.resolved_download_dir())
        .ok_or("no download directory; set download_dir in config.json")?;
    std::fs::create_dir_all(&recv_dir)?;

    append_log(
        &app,
        "INFO",
        &format!("Mausfer serve starting (port {bind_port})"),
    );
    append_log(
        &app,
        "INFO",
        &format!("download dir: {}", recv_dir.display()),
    );
    append_log(&app, "INFO", &format!("device id: {}", device_id()));

    // Discovery: announce ourselves to the LAN with the actual listen port,
    // so peers know where to connect even when --port overrides the config.
    let mut own_info = app.device_info();
    own_info.port = bind_port;
    let mut discovery = DiscoveryService::new(
        own_info,
        DiscoveryConfig {
            port: discovery_port,
            broadcast_addr: SocketAddr::from(([255, 255, 255, 255], discovery_port)),
            ..Default::default()
        },
    )?;
    discovery.start()?;
    append_log(&app, "INFO", "discovery started");

    let listener = TcpListener::bind(("0.0.0.0", bind_port))?;
    append_log(&app, "INFO", "listening for transfers");
    println!(
        "Mausfer listening on port {bind_port}, download dir: {}",
        recv_dir.display()
    );
    println!("Press Ctrl-C to stop.");
    let recv_dir = Arc::new(recv_dir);

    let limiter = mausfer_core::transfer::ReceiveLimiter::new(app.config().max_threads);
    for conn in listener.incoming() {
        if !app.config().auto_accept {
            continue;
        }
        let Some(permit) = limiter.try_acquire() else {
            continue;
        };
        let recv_dir = Arc::clone(&recv_dir);
        let app = Arc::clone(&app);
        match conn {
            Ok(stream) => {
                let peer = stream.peer_addr().ok();
                std::thread::spawn(move || {
                    let _permit = permit;
                    let mut stream = stream;
                    let timeout = Some(Duration::from_secs(60));
                    if stream
                        .set_read_timeout(timeout)
                        .and_then(|_| stream.set_write_timeout(timeout))
                        .is_err()
                    {
                        return;
                    }
                    let peer_str = peer
                        .map(|p| p.to_string())
                        .unwrap_or_else(|| "?".to_string());
                    append_log(&app, "INFO", &format!("connection from {peer_str}"));
                    match receive_file(&mut stream, &recv_dir) {
                        Ok(report) => {
                            append_log(
                                &app,
                                "INFO",
                                &format!(
                                    "received {} ({} bytes) -> {}",
                                    report.file_name,
                                    report.bytes_written,
                                    report.saved_path.display()
                                ),
                            );
                            println!(
                                "received: {} ({} bytes)",
                                report.file_name, report.bytes_written
                            );
                        }
                        Err(e) => {
                            append_log(&app, "ERROR", &format!("receive failed: {e}"));
                            eprintln!("receive failed: {e}");
                        }
                    }
                });
            }
            Err(e) => eprintln!("accept error: {e}"),
        }
    }
    Ok(())
}

fn cmd_send(rest: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let file = rest
        .first()
        .ok_or("usage: mausfer send <file> [--to ip:port]")?
        .clone();
    let path = PathBuf::from(&file);
    if !path.exists() {
        return Err(format!("file not found: {file}").into());
    }

    let to = parse_opt::<String>(rest, "--to", None);
    let port_override = parse_opt::<u16>(rest, "--port", None);
    let timeout = parse_opt::<u64>(rest, "--timeout", None).unwrap_or(5);
    let discovery_port =
        parse_opt::<u16>(rest, "--discovery-port", None).unwrap_or(DEFAULT_DISCOVERY_PORT);

    let app = App::init(&DesktopPaths)?;
    append_log(&app, "INFO", &format!("send: {file}"));

    let target: SocketAddr = if let Some(t) = to {
        t.parse()?
    } else {
        // Scanner mode: bind an ephemeral port, query the well-known
        // discovery port, and use unicast replies. This avoids port
        // collisions between scanners and servers on the same machine.
        let mut discovery = DiscoveryService::new(
            app.device_info(),
            DiscoveryConfig {
                port: 0,
                broadcast_addr: SocketAddr::from(([255, 255, 255, 255], discovery_port)),
                ..Default::default()
            },
        )?;
        discovery.start()?;
        let _ = discovery.send_query();
        println!("discovering on LAN (up to {timeout}s)...");
        let deadline = Instant::now() + Duration::from_secs(timeout);
        let mut peers = Vec::new();
        while Instant::now() < deadline {
            peers = discovery.peers();
            if !peers.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        if peers.is_empty() {
            return Err("no peers discovered on LAN; use --to ip:port".into());
        }
        let p = peers[0].clone();
        let ip = discovery
            .peer_addr(&p.id)
            .ok_or("peer address unavailable")?
            .ip();
        discovery.stop();
        println!("found peer: {} (id={}, port={})", p.name, p.id, p.port);
        SocketAddr::new(ip, port_override.unwrap_or(p.port))
    };

    let meta = FileMetadata::for_file(&path, new_transfer_id(), DEFAULT_CHUNK_SIZE)?;
    let mut stream = TcpStream::connect_timeout(&target, Duration::from_secs(timeout.max(1)))?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    stream.set_write_timeout(Some(Duration::from_secs(60)))?;
    println!(
        "sending {} ({} bytes) to {}",
        meta.file_name, meta.file_size, target
    );
    let report: SendReport = send_file(&mut stream, &path, &meta)?;
    append_log(
        &app,
        "INFO",
        &format!(
            "sent {} ({} bytes) to {}",
            report.file_name, report.bytes_sent, target
        ),
    );
    println!(
        "sent {} ({} bytes, sha256 {})",
        report.file_name,
        report.bytes_sent,
        &report.sha256[..16]
    );
    Ok(())
}

fn cmd_peers(rest: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let wait = parse_opt::<u64>(rest, "--wait", None).unwrap_or(3);
    let port = parse_opt::<u16>(rest, "--discovery-port", None).unwrap_or(DEFAULT_DISCOVERY_PORT);
    let app = App::init(&DesktopPaths)?;

    // Scanner: ephemeral bind + query to the well-known discovery port.
    let mut discovery = DiscoveryService::new(
        app.device_info(),
        DiscoveryConfig {
            port: 0,
            broadcast_addr: SocketAddr::from(([255, 255, 255, 255], port)),
            ..Default::default()
        },
    )?;
    discovery.start()?;
    let _ = discovery.send_query();
    println!("discovering for {wait}s...");
    let deadline = Instant::now() + Duration::from_secs(wait);
    let mut seen: Vec<String> = Vec::new();
    while Instant::now() < deadline {
        for p in discovery.peers() {
            let line = format!("  {}  id={}  port={}", p.name, p.id, p.port);
            if !seen.contains(&line) {
                seen.push(line);
            }
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    discovery.stop();
    if seen.is_empty() {
        println!("  (no peers found)");
    } else {
        for line in seen {
            println!("{line}");
        }
    }
    Ok(())
}

fn cmd_remote_send(rest: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let file = rest
        .first()
        .ok_or("usage: mausfer remote-send <file> --code CODE [--signal ws://host:port]")?
        .clone();
    let path = PathBuf::from(&file);
    if !path.exists() {
        return Err(format!("file not found: {file}").into());
    }
    let code = parse_opt::<String>(rest, "--code", None)
        .ok_or("remote-send needs --code (XXXX-XXXX-XXXX-XXXX from the receiver)")?;
    let signal_override = parse_opt::<String>(rest, "--signal", None);

    let app = App::init(&DesktopPaths)?;
    let device = app.device_info();
    let config = app.config();
    let signal = signal_override.unwrap_or_else(|| config.signaling_url.clone());

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?;
    println!("connecting via signaling...");
    let (report, used_code) = rt.block_on(async {
        remote_send(
            &config,
            RemoteOptions {
                room_code: code,
                signal_url: signal,
                device,
            },
            &path,
            |phase| println!("{phase}"),
        )
        .await
    })?;
    match report {
        mausfer_core::RemoteReport::Sent(s) => {
            println!(
                "sent {} ({} bytes, sha256 {}) via room {used_code}",
                s.file_name,
                s.bytes_sent,
                &s.sha256[..16]
            );
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn cmd_remote_recv(rest: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let code = parse_opt::<String>(rest, "--code", None)
        .ok_or("remote-recv needs --code (XXXX-XXXX-XXXX-XXXX from the sender)")?;
    let signal_override = parse_opt::<String>(rest, "--signal", None);
    let download_dir = parse_opt::<String>(rest, "--download", None).map(PathBuf::from);

    let app = App::init(&DesktopPaths)?;
    let device = app.device_info();
    let config = app.config();
    let signal = signal_override.unwrap_or_else(|| config.signaling_url.clone());
    let recv_dir = download_dir
        .or_else(|| app.resolved_download_dir())
        .ok_or("no download directory; set download_dir in config.json")?;
    std::fs::create_dir_all(&recv_dir)?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?;
    println!("waiting for sender (room {code})...");
    let report = rt.block_on(async {
        remote_recv(
            &config,
            RemoteOptions {
                room_code: code,
                signal_url: signal,
                device,
            },
            &recv_dir,
            |phase| println!("{phase}"),
        )
        .await
    })?;
    match report {
        mausfer_core::RemoteReport::Received(r) => {
            println!(
                "received {} ({} bytes) -> {}",
                r.file_name,
                r.bytes_written,
                r.saved_path.display()
            );
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn cmd_signal_server(rest: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let addr = parse_opt::<String>(rest, "--addr", None).unwrap_or_else(|| "0.0.0.0:9322".into());
    let addr: SocketAddr = addr.parse()?;
    let server = std::sync::Arc::new(SignalingServer::new());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?;
    let bound =
        rt.block_on(async { server.clone().serve(addr).await.map_err(|e| e.to_string()) })?;
    println!("Mausfer signaling server listening on {bound}");
    println!("Press Ctrl-C to stop.");
    // Keep the process alive.
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

fn parse_opt<T: std::str::FromStr>(args: &[String], name: &str, default: Option<T>) -> Option<T> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            return it.next().and_then(|v| v.parse().ok());
        }
    }
    default
}
