use crate::config::Config;
use crate::logger::Logger;
use crate::paths::PlatformPaths;
use std::path::PathBuf;
use std::sync::Mutex;

/// Top-level application initializer used by every platform shell.
pub struct App {
    config: Mutex<Config>,
    pub config_path: PathBuf,
    pub log_path: PathBuf,
    logger: Logger,
    default_download_dir: Option<PathBuf>,
}

impl App {
    pub fn init<P: PlatformPaths>(paths: &P) -> Result<Self, Box<dyn std::error::Error>> {
        let config_dir = paths.config_dir();
        std::fs::create_dir_all(&config_dir)?;

        let config_path = paths.config_file_path();
        let config = Config::load_or_create(&config_path)?;

        let log_path = paths.log_file_path();
        let logger = Logger::new(&log_path)?;

        Ok(Self {
            config: Mutex::new(config),
            config_path,
            log_path,
            logger,
            default_download_dir: paths.default_download_dir(),
        })
    }

    pub fn config(&self) -> Config {
        self.config.lock().unwrap().clone()
    }

    pub fn set_signaling_url(&self, address: String) -> Result<String, String> {
        let address = crate::config::normalize_signaling_url(&address)?;
        let mut guard = self.config.lock().unwrap();
        let mut next = guard.clone();
        next.signaling_url = address.clone();
        next.save(&self.config_path).map_err(|e| e.to_string())?;
        *guard = next;
        Ok(address)
    }

    pub fn set_ice_settings(
        &self,
        stun: String,
        turn: String,
        username: String,
        password: String,
    ) -> Result<(), String> {
        for (value, prefixes) in [
            (&stun, &["stun:", "stuns:"][..]),
            (&turn, &["turn:", "turns:"][..]),
        ] {
            for address in value
                .split([',', ';'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                if !prefixes.iter().any(|prefix| address.starts_with(prefix))
                    || webrtc::ice::url::Url::parse_url(address).is_err()
                {
                    return Err("STUN / TURN 地址格式无效".into());
                }
            }
        }
        if !turn.trim().is_empty() && (username.is_empty() || password.is_empty()) {
            return Err("请填写 TURN 用户名和密码".into());
        }
        let mut guard = self.config.lock().unwrap();
        let mut next = guard.clone();
        next.stun_url = stun.trim().to_string();
        next.turn_url = turn.trim().to_string();
        next.turn_username = username;
        next.turn_password = password;
        next.save(&self.config_path).map_err(|e| e.to_string())?;
        *guard = next;
        Ok(())
    }

    pub fn logger(&mut self) -> &mut Logger {
        &mut self.logger
    }

    pub fn resolved_download_dir(&self) -> Option<PathBuf> {
        let guard = self.config.lock().unwrap();
        let configured = &guard.download_dir;
        if configured.is_empty() {
            self.default_download_dir.clone()
        } else {
            Some(PathBuf::from(configured))
        }
    }

    /// Persist first, then expose the new directory to concurrent callers.
    /// An empty override explicitly selects the platform default.
    pub fn set_download_dir(&self, path: String) -> Result<String, Box<dyn std::error::Error>> {
        let mut guard = self.config.lock().unwrap();
        let trimmed = path.trim();
        let resolved = if trimmed.is_empty() {
            self.default_download_dir
                .clone()
                .ok_or("无法解析下载目录")?
        } else {
            PathBuf::from(trimmed)
        };
        std::fs::create_dir_all(&resolved)?;
        let resolved = std::fs::canonicalize(resolved)?;
        let mut cfg = guard.clone();
        cfg.download_dir = if trimmed.is_empty() {
            String::new()
        } else {
            resolved.display().to_string()
        };
        cfg.save(&self.config_path)?;
        *guard = cfg;
        Ok(resolved.display().to_string())
    }

    /// Build this device's [`DeviceInfo`] from the loaded config.
    pub fn device_info(&self) -> crate::discovery::DeviceInfo {
        crate::discovery::DeviceInfo::new(
            crate::identity::device_id(),
            crate::config::default_device_name(),
            self.config().port,
            env!("CARGO_PKG_VERSION"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::PlatformPaths;
    use std::path::PathBuf;

    struct TestPaths(PathBuf);

    impl PlatformPaths for TestPaths {
        fn config_dir(&self) -> PathBuf {
            self.0.clone()
        }
        fn default_download_dir(&self) -> Option<PathBuf> {
            Some(self.0.join("Downloads"))
        }
    }

    #[test]
    fn directory_changes_reset_to_platform_default_and_survive_restart() {
        let dir = std::env::temp_dir().join(crate::new_transfer_id());
        let paths = TestPaths(dir.clone());
        let app = App::init(&paths).unwrap();
        assert_eq!(app.resolved_download_dir(), Some(dir.join("Downloads")));
        let custom = app
            .set_download_dir(dir.join("custom").display().to_string())
            .unwrap();
        assert_eq!(
            App::init(&paths).unwrap().resolved_download_dir(),
            Some(PathBuf::from(custom))
        );
        // Reset must ignore the old saved custom directory on a fresh instance.
        let restarted = App::init(&paths).unwrap();
        restarted.set_download_dir(String::new()).unwrap();
        assert!(Config::load_or_create(&restarted.config_path)
            .unwrap()
            .download_dir
            .is_empty());
        assert_eq!(
            App::init(&paths).unwrap().resolved_download_dir(),
            Some(dir.join("Downloads"))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn independent_settings_preserve_each_other_and_reject_invalid_updates() {
        let dir = std::env::temp_dir().join(crate::new_transfer_id());
        let paths = TestPaths(dir.clone());
        let app = App::init(&paths).unwrap();
        app.set_signaling_url("ws://localhost:38386".into())
            .unwrap();
        app.set_download_dir(dir.join("chosen").display().to_string())
            .unwrap();
        app.set_ice_settings(
            "stun:localhost:3478".into(),
            "turn:localhost:3478".into(),
            "user".into(),
            "secret".into(),
        )
        .unwrap();
        let saved = app.config();
        assert!(app.set_signaling_url("https://example.com".into()).is_err());
        assert!(app
            .set_ice_settings("stun:".into(), String::new(), String::new(), String::new())
            .is_err());
        assert_eq!(saved, App::init(&paths).unwrap().config());
        assert_eq!(saved.signaling_url, "ws://localhost:38386/");
        assert!(saved.download_dir.ends_with("chosen"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn init_creates_only_config_and_log() {
        let dir = std::env::temp_dir().join(format!("mausfer-test-app-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let paths = TestPaths(dir.clone());

        let mut app = App::init(&paths).unwrap();
        app.logger().info("started").unwrap();

        assert!(app.config_path.exists());
        assert!(app.log_path.exists());
        assert!(app.config_path.ends_with("config.json"));
        assert!(app.log_path.ends_with("mausfer.log"));

        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert!(entries.contains(&"config.json".to_string()));
        assert!(entries.contains(&"mausfer.log".to_string()));
        assert_eq!(entries.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
