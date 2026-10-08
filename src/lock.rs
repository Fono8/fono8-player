//! Single-instance lock per data directory and the platform data directory.
//!
//! The lock file holds `pid\nhostname\napplication\n` and uses the OS advisory
//! lock from `std::fs::File::try_lock` instead of probing process ids, which
//! works the same way on Linux, macOS and Windows.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const INSTANCE_LOCK: &str = "fono8.lock";

pub struct InstanceLock {
    file: File,
    path: PathBuf,
}

impl InstanceLock {
    pub fn try_acquire(directory: &Path) -> Option<InstanceLock> {
        let path = directory.join(INSTANCE_LOCK);
        let mut file = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path).ok()?;
        if file.try_lock().is_err() {
            return None;
        }
        let _ = file.set_len(0);
        let _ = writeln!(file, "{}\n{}\nFono8", std::process::id(), hostname());
        let _ = file.flush();
        Some(InstanceLock { file, path })
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
        let _ = fs::remove_file(&self.path);
    }
}

fn hostname() -> String {
    fs::read_to_string("/etc/hostname")
        .ok()
        .map(|h| h.trim().to_string())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_default()
}

/// `${XDG_DATA_HOME}/fono8`, `~/Library/Application Support/fono8` or
/// `%LOCALAPPDATA%\fono8`.
pub fn default_data_directory() -> PathBuf {
    let base =
        if cfg!(target_os = "windows") { dirs::data_local_dir() } else { dirs::data_dir() }.unwrap_or_else(|| PathBuf::from("."));
    migrate(&base)
}

/// Move the library from the pre-rename `Sonora/Sonora` directory to `fono8` once.
/// When the move is not possible (e.g. `fono8` already exists), the old one stays in use.
fn migrate(base: &Path) -> PathBuf {
    let current = base.join("fono8");
    let legacy_parent = base.join("Sonora");
    let legacy = legacy_parent.join("Sonora");
    if !legacy.is_dir() {
        return current;
    }
    if current.exists() || fs::rename(&legacy, &current).is_err() {
        return if current.is_dir() { current } else { legacy };
    }
    // Only removes the parent when nothing else is left in it.
    let _ = fs::remove_dir(&legacy_parent);
    current
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_instance_is_refused_until_the_first_releases() {
        let dir = std::env::temp_dir().join(format!("fono8-lock-{}-{}", std::process::id(), rand::random::<u32>()));
        fs::create_dir_all(&dir).unwrap();
        let first = InstanceLock::try_acquire(&dir).expect("first lock");
        assert!(InstanceLock::try_acquire(&dir).is_none());
        // Through the locking handle: on Windows the lock is mandatory and blocks other readers.
        let mut content = String::new();
        let mut file = &first.file;
        std::io::Seek::rewind(&mut file).unwrap();
        std::io::Read::read_to_string(&mut file, &mut content).unwrap();
        assert!(content.starts_with(&std::process::id().to_string()));
        drop(first);
        assert!(InstanceLock::try_acquire(&dir).is_some());
    }

    #[test]
    fn moves_the_legacy_directory_once() {
        let base = std::env::temp_dir().join(format!("fono8-migrate-{}-{}", std::process::id(), rand::random::<u32>()));
        fs::create_dir_all(base.join("Sonora/Sonora/ytmusic-profile")).unwrap();
        fs::write(base.join("Sonora/Sonora/library.sqlite3"), b"db").unwrap();
        let dir = migrate(&base);
        assert_eq!(dir, base.join("fono8"));
        assert_eq!(fs::read(dir.join("library.sqlite3")).unwrap(), b"db");
        assert!(dir.join("ytmusic-profile").is_dir());
        assert!(!base.join("Sonora").exists());
        assert_eq!(migrate(&base), base.join("fono8"), "nothing left to move");
        // Both present: the new one wins and the old one is left alone.
        fs::create_dir_all(base.join("Sonora/Sonora")).unwrap();
        assert_eq!(migrate(&base), base.join("fono8"));
        assert!(base.join("Sonora/Sonora").is_dir());
        let _ = fs::remove_dir_all(&base);
    }
}
