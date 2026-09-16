//! Mausfer Android (Tauri mobile) shell.
//!
//! Tauri mobile requires a library crate with `tauri::mobile_entry_point`;
//! the Kotlin `MainActivity` (see `gen/android/.../MainActivity.kt`) sets the
//! private-dir env vars and publishes completed files into
//! `MediaStore.Downloads` via the download queue.
//!
//! The shared Tauri glue (discovery, receive listener, commands) lives in
//! `mausfer-tauri-common`; this crate supplies the `AndroidPaths`
//! implementation and a post-receive hook that moves finished files into the
//! publication queue.

use mausfer_core::AndroidPaths;
use std::path::PathBuf;

/// Move a completed file into the private publication queue. Kotlin's
/// `publish_to_media_store()` drains this queue into `MediaStore.Downloads`.
#[allow(dead_code)]
fn queue_for_publication(saved_path: &std::path::Path) {
    let private_dl = std::env::var(AndroidPaths::DL_PRIVATE_ENV)
        .map(PathBuf::from)
        .unwrap_or_default();
    if let Err(e) = mausfer_core::android::queue_for_publication(&private_dl, saved_path) {
        eprintln!("android: queue_for_publication failed: {e}");
    }
}

/// Build the shared Tauri application for Android.
#[allow(dead_code)]
fn android_builder() -> tauri::Builder<tauri::Wry> {
    mausfer_tauri_common::builder_with_hook(mausfer_core::AndroidPaths, queue_for_publication)
        .invoke_handler(tauri::generate_handler![
            mausfer_tauri_common::commands::get_status,
            mausfer_tauri_common::commands::get_signaling_status,
            mausfer_tauri_common::commands::start_remote_receive,
            mausfer_tauri_common::commands::stop_remote_receive,
            mausfer_tauri_common::commands::list_peers,
            mausfer_tauri_common::commands::send_to_peer,
            mausfer_tauri_common::commands::remote_send_file,
            mausfer_tauri_common::commands::pick_file,
            mausfer_tauri_common::commands::set_download_dir,
            mausfer_tauri_common::commands::set_signaling_url,
            mausfer_tauri_common::commands::set_ice_settings,
            mausfer_tauri_common::commands::stat_file,
        ])
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[allow(dead_code)]
fn main() {
    // Desktop-target smoke run (e.g. `cargo run -p mausfer-android`).
    android_builder()
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Android/iOS entry point (called by the generated bootstrap).
///
/// The Kotlin shell must set the following env vars before calling `run()`:
///   MAUSFER_FILES_DIR        -> context.filesDir
///   MAUSFER_DL_PRIVATE_DIR   -> context.getDir("downloads", MODE_PRIVATE)
#[cfg(any(target_os = "android", target_os = "ios"))]
#[tauri::mobile_entry_point]
pub fn run() {
    android_builder()
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
