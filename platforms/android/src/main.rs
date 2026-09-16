use mausfer_core::{App, PlatformPaths};
use std::path::PathBuf;

/// Placeholder Android path provider.
///
/// The real Android Tauri shell will provide the app sandbox directory.
struct AndroidPaths;

impl PlatformPaths for AndroidPaths {
    fn config_dir(&self) -> PathBuf {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("mausfer-data")
    }

    fn default_download_dir(&self) -> Option<PathBuf> {
        None
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut app = App::init(&AndroidPaths)?;
    app.logger()
        .info("Mausfer Android shell starting (placeholder)")?;

    println!("config: {}", app.config_path.display());
    println!("log:    {}", app.log_path.display());

    Ok(())
}
