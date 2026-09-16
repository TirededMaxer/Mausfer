//! Mausfer macOS (Tauri) shell.
//!
//! Thin entry point: the shared Tauri glue (discovery, receive listener,
//! commands) lives in `mausfer-tauri-common`.

fn main() {
    mausfer_tauri_common::builder(mausfer_core::DesktopPaths)
        .invoke_handler(tauri::generate_handler![
            mausfer_tauri_common::commands::get_status,
            mausfer_tauri_common::commands::get_signaling_status,
            mausfer_tauri_common::commands::start_remote_receive,
            mausfer_tauri_common::commands::stop_remote_receive,
            mausfer_tauri_common::commands::list_peers,
            mausfer_tauri_common::commands::send_to_peer,
            mausfer_tauri_common::commands::pick_file,
            mausfer_tauri_common::commands::stat_file,
            mausfer_tauri_common::commands::set_download_dir,
            mausfer_tauri_common::commands::set_signaling_url,
            mausfer_tauri_common::commands::set_ice_settings,
            mausfer_tauri_common::commands::pick_directory,
            mausfer_tauri_common::commands::open_config_dir,
            mausfer_tauri_common::commands::remote_send_file,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
