use std::path::PathBuf;

/// Platform path provider abstraction.
///
/// The core does not know whether it is running on Windows, macOS, or
/// Android. Each platform shell supplies the correct directories through
/// this trait.
pub trait PlatformPaths: Send + Sync {
    /// Directory where `config.json` and `mausfer.log` are stored.
    fn config_dir(&self) -> PathBuf;
    /// The platform default Downloads directory, if available.
    fn default_download_dir(&self) -> Option<PathBuf>;

    fn config_file_path(&self) -> PathBuf {
        self.config_dir().join("config.json")
    }

    fn log_file_path(&self) -> PathBuf {
        self.config_dir().join("mausfer.log")
    }
}

/// Default path provider for Windows and macOS desktops.
pub struct DesktopPaths;

impl DesktopPaths {
    /// Env var override for the data directory (useful for portability /
    /// testing: `MAUSFER_CONFIG_DIR=/path/to/dir`).
    pub const CONFIG_DIR_ENV: &'static str = "MAUSFER_CONFIG_DIR";

    fn env_config_dir() -> Option<PathBuf> {
        std::env::var(Self::CONFIG_DIR_ENV).ok().map(PathBuf::from)
    }
}

impl PlatformPaths for DesktopPaths {
    fn config_dir(&self) -> PathBuf {
        Self::env_config_dir().unwrap_or_else(|| {
            dirs::config_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("Mausfer")
        })
    }

    fn default_download_dir(&self) -> Option<PathBuf> {
        dirs::download_dir()
    }
}

/// Android path provider.
///
/// Android has scoped storage: writing to the public `Download/` directory
/// must go through MediaStore, which a raw path cannot express. The design:
///
/// 1. The Rust core writes received files into the app's **private** cache
///    download area (`default_download_dir`).
/// 2. The platform shell (Tauri Android, Kotlin side) publishes each
///    completed file to the public `MediaStore.Downloads` collection, then
///    removes the private copy.
///
/// Configuration directory lives in the app-private `filesDir`, obtained from
/// the shell via env vars (`MAUSFER_FILES_DIR`, `MAUSFER_DL_PRIVATE_DIR`).
pub struct AndroidPaths;

impl AndroidPaths {
    /// Env var: app-private data directory (`filesDir`).
    pub const FILES_DIR_ENV: &'static str = "MAUSFER_FILES_DIR";
    /// Env var: private download staging directory.
    pub const DL_PRIVATE_ENV: &'static str = "MAUSFER_DL_PRIVATE_DIR";

    fn files_dir() -> PathBuf {
        std::env::var(Self::FILES_DIR_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
    }

    fn private_download_dir() -> PathBuf {
        std::env::var(Self::DL_PRIVATE_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|_| Self::files_dir().join("downloads"))
    }
}

impl PlatformPaths for AndroidPaths {
    fn config_dir(&self) -> PathBuf {
        Self::files_dir().join("mausfer")
    }

    fn default_download_dir(&self) -> Option<PathBuf> {
        Some(Self::private_download_dir())
    }
}
