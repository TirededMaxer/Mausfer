use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// A very simple file logger.
///
/// The log file is truncated every time the application starts, as required.
pub struct Logger {
    file: File,
}

impl Logger {
    /// Create/open the log file with truncate mode.
    pub fn new(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?;
        Ok(Self { file })
    }

    pub fn log(&mut self, level: &str, message: &str) -> io::Result<()> {
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        writeln!(self.file, "[{now}] [{level}] {message}")?;
        self.file.flush()
    }

    pub fn info(&mut self, message: &str) -> io::Result<()> {
        self.log("INFO", message)
    }

    pub fn warn(&mut self, message: &str) -> io::Result<()> {
        self.log("WARN", message)
    }

    pub fn error(&mut self, message: &str) -> io::Result<()> {
        self.log("ERROR", message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn logger_truncates_on_startup() {
        let dir = std::env::temp_dir().join(format!("mausfer-test-log-{}", std::process::id()));
        let path = dir.join("mausfer.log");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        {
            let mut logger = Logger::new(&path).unwrap();
            logger.info("first run").unwrap();
        }
        {
            let mut logger = Logger::new(&path).unwrap();
            logger.info("second run").unwrap();
        }

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("second run"));
        assert!(!content.contains("first run"));
        let _ = fs::remove_dir_all(&dir);
    }
}
