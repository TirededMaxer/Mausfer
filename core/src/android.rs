//! Android-specific file publication helpers.
//!
//! Android scoped storage forbids plain file writes into the public
//! `Download/` collection. The core therefore writes received files into the
//! app-private download staging directory (see [`AndroidPaths`]); the shell's
//! Kotlin side then inserts each finished file into
//! `MediaStore.Downloads` and removes the staging copy.
//!
//! This module only handles the physical move/rename of staging files into a
//! queue that Kotlin drains — everything Rust-side stays testable.

use std::fs;
use std::path::{Path, PathBuf};

/// The staging subdirectory inside the private download dir that holds files
/// successfully written by the core and waiting to be published.
pub const PUBLISH_QUEUE_DIR: &str = "publish-queue";

/// Move a completed file into the publication queue.
///
/// Returns the queued path. Callers (the Android shell) should register the
/// queued file with `MediaStore.Downloads` and then delete it.
pub fn queue_for_publication(private_download_dir: &Path, file: &Path) -> std::io::Result<PathBuf> {
    let queue = private_download_dir.join(PUBLISH_QUEUE_DIR);
    fs::create_dir_all(&queue)?;
    let name = file.file_name().unwrap_or_default().to_string_lossy();
    crate::transfer::publish_file(file, &queue, &name)
}

/// List files currently waiting in the publication queue.
pub fn queued_files(private_download_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let queue = private_download_dir.join(PUBLISH_QUEUE_DIR);
    if !queue.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&queue)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            out.push(entry.path());
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_moves_file_and_avoid_collision() {
        let dir = std::env::temp_dir().join(format!(
            "mausfer-test-android-queue-{}-{}",
            std::process::id(),
            crate::transfer::new_transfer_id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let file = dir.join("movie.mp4");
        fs::write(&file, b"data").unwrap();

        let q1 = queue_for_publication(&dir, &file).unwrap();
        assert!(q1.exists());
        assert!(!file.exists(), "source moved into queue");
        assert!(q1.ends_with("publish-queue/movie.mp4"));

        // Second file with the same name must not collide.
        let file2 = dir.join("movie.mp4");
        fs::write(&file2, b"more data").unwrap();
        let q2 = queue_for_publication(&dir, &file2).unwrap();
        assert_ne!(q1, q2);

        let queued = queued_files(&dir).unwrap();
        assert_eq!(queued.len(), 2);

        let _ = fs::remove_dir_all(&dir);
    }
}
