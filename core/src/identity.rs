//! Process-lifetime device identity for discovery and signaling.

use std::sync::OnceLock;

/// Return this running instance's identity.
///
/// Generate a fresh random ID once per process, then reuse it for every
/// discovery announcement and signaling session. Host/user environment
/// variables are often absent or shared across devices and are not unique.
/// Restarting the app creates a new presence; the old one expires by TTL.
/// `MAUSFER_DEVICE_ID` remains an explicit override for tests and deployments.
pub fn device_id() -> String {
    if let Ok(id) = std::env::var("MAUSFER_DEVICE_ID") {
        let id = id.trim();
        if !id.is_empty() {
            return id.to_string();
        }
    }
    static INSTANCE_ID: OnceLock<String> = OnceLock::new();
    INSTANCE_ID
        .get_or_init(|| format!("{}-{:032x}", std::env::consts::OS, rand::random::<u128>()))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_stable_within_the_process_and_nonempty() {
        let a = device_id();
        let b = device_id();
        assert_eq!(a, b);
        assert!(!a.is_empty());
        assert!(a.len() <= 64);
    }

    #[test]
    fn id_has_os_prefix_and_random_suffix() {
        let id = device_id();
        let prefix = format!("{}-", std::env::consts::OS);
        let random = id.strip_prefix(&prefix).unwrap();
        assert_eq!(random.len(), 32);
        assert!(random.bytes().all(|c| c.is_ascii_hexdigit()));
    }
}
