use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

pub type ConfigResult<T> = Result<T, ConfigError>;

const LEGACY_STUN_URL: &str = "stun:stun.l.google.com:19302";
pub const DEFAULT_STUN_URL: &str = "stun:stun.cloudflare.com:3478, stun:stun.l.google.com:19302";

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "IO error: {e}"),
            ConfigError::Json(e) => write!(f, "JSON error: {e}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::Io(e)
    }
}

impl From<serde_json::Error> for ConfigError {
    fn from(e: serde_json::Error) -> Self {
        ConfigError::Json(e)
    }
}

/// The single JSON configuration file used by Mausfer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Empty string means "use the platform default Downloads directory".
    pub download_dir: String,
    #[serde(skip)]
    pub port: u16,
    #[serde(skip)]
    pub max_threads: u32,
    #[serde(skip)]
    pub auto_accept: bool,
    /// Self-hosted ws(s) address. Empty disables remote rendezvous.
    pub signaling_url: String,
    /// Public STUN is enabled by default; optional override.
    pub stun_url: String,
    /// Optional comma-separated TURN URLs. WebRTC tries direct candidates
    /// first and automatically relays through TURN when direct ICE fails.
    pub turn_url: String,
    pub turn_username: String,
    pub turn_password: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            download_dir: String::new(),
            port: 43110,
            max_threads: 4,
            auto_accept: true,
            signaling_url: String::new(),
            stun_url: DEFAULT_STUN_URL.to_string(),
            turn_url: String::new(),
            turn_username: String::new(),
            turn_password: String::new(),
        }
    }
}

impl Config {
    /// Load config from `path`, or create a default one if the file does not exist.
    pub fn load_or_create(path: &Path) -> ConfigResult<Self> {
        if path.exists() {
            let text = fs::read_to_string(path)?;
            let mut config: Self = serde_json::from_str(&text)?;
            // Upgrade the former single-provider default without overwriting
            // custom STUN lists or an intentionally empty setting.
            if config.stun_url.trim() == LEGACY_STUN_URL {
                config.stun_url = DEFAULT_STUN_URL.to_string();
            }
            if serde_json::from_str::<serde_json::Value>(&text)? != serde_json::to_value(&config)? {
                config.save(path)?;
            }
            Ok(config)
        } else {
            let config = Config::default();
            config.save(path)?;
            Ok(config)
        }
    }

    /// Save config to `path`, creating parent directories if needed.
    pub fn save(&self, path: &Path) -> ConfigResult<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        let temporary =
            path.with_extension(format!("json.{}.tmp", crate::transfer::new_transfer_id()));
        let result = (|| -> std::io::Result<()> {
            use std::io::Write;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
        Ok(())
    }

    /// Resolve the actual download directory.
    ///
    /// If `download_dir` is non-empty, use it. Otherwise use the platform
    /// default Downloads directory when available.
    pub fn resolved_download_dir(&self) -> Option<PathBuf> {
        let configured = self.download_dir.trim();
        if !configured.is_empty() {
            Some(PathBuf::from(configured))
        } else {
            dirs::download_dir()
        }
    }
}

/// Read the system name rather than persisting an application-specific name.
pub fn default_device_name() -> String {
    #[cfg(target_os = "android")]
    let name = std::env::var("MAUSFER_SYSTEM_DEVICE_NAME").ok();
    #[cfg(target_os = "windows")]
    let name = std::env::var("COMPUTERNAME").ok();
    #[cfg(target_os = "macos")]
    let name = std::process::Command::new("/usr/sbin/scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string());
    #[cfg(not(any(target_os = "android", target_os = "windows", target_os = "macos")))]
    let name = std::fs::read_to_string("/etc/hostname").ok();
    name.filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| std::env::consts::OS.to_string())
}

pub fn normalize_signaling_url(address: &str) -> Result<String, String> {
    let address = address.trim();
    if address.is_empty() {
        return Ok(String::new());
    }
    let url = url::Url::parse(address)
        .map_err(|_| "请输入完整的 ws:// 或 wss:// 服务器地址".to_string())?;
    if !matches!(url.scheme(), "ws" | "wss")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("请输入有效的 ws:// 或 wss:// 服务器地址".into());
    }
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_stun_default_upgrades_but_custom_and_disabled_settings_survive() {
        let dir =
            std::env::temp_dir().join(format!("mausfer-stun-migration-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        for (old, expected) in [
            (LEGACY_STUN_URL, DEFAULT_STUN_URL),
            ("stun:custom.example:3478", "stun:custom.example:3478"),
            ("", ""),
        ] {
            std::fs::write(&path, serde_json::json!({"stun_url":old}).to_string()).unwrap();
            let config = Config::load_or_create(&path).unwrap();
            assert_eq!(config.stun_url, expected);
            assert_eq!(Config::load_or_create(&path).unwrap().stun_url, expected);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn older_configuration_keeps_defaults_for_new_fields() {
        let c: Config = serde_json::from_str(r#"{"device_name":"Old device"}"#).unwrap();
        assert!(serde_json::to_value(&c)
            .unwrap()
            .get("device_name")
            .is_none());
        assert_eq!(c.port, 43110);
        assert_eq!(c.max_threads, 4);
    }

    #[test]
    fn default_config_has_sane_values() {
        let c = Config::default();
        assert!(c.max_threads >= 1);
        assert_eq!(c.port, 43110);
        assert!(c.auto_accept);
        assert!(c.download_dir.is_empty());
    }

    #[test]
    fn load_or_create_creates_file() {
        let dir = std::env::temp_dir().join(format!("mausfer-test-config-{}", std::process::id()));
        let path = dir.join("config.json");
        let _ = fs::remove_dir_all(&dir);
        let config = Config::load_or_create(&path).unwrap();
        assert!(path.exists());
        assert_eq!(config.port, 43110);

        let loaded = Config::load_or_create(&path).unwrap();
        assert_eq!(config, loaded);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_dir_override() {
        let config = Config {
            download_dir: "/tmp/custom-downloads".to_string(),
            ..Config::default()
        };
        assert_eq!(
            config.resolved_download_dir(),
            Some(PathBuf::from("/tmp/custom-downloads"))
        );
    }
}
