use std::env;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::time;

/// How often the daemon checks whether its executable was replaced.
pub const SWAP_POLL: Duration = Duration::from_secs(2);

/// Returns the running executable's path with every symlink resolved.
pub fn canonical() -> io::Result<PathBuf> {
    let path = env::current_exe()?;
    fs::canonicalize(path)
}

/// The `(device, inode)` pair that identifies one file, whatever path it is reached by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    device: u64,
    inode: u64,
}

impl Identity {
    /// Reads the identity of the file at `path`.
    pub fn of(path: &Path) -> io::Result<Self> {
        let metadata = fs::metadata(path)?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

/// How the file at a watched path stopped being the original one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// A different file now sits at the path.
    Replaced,
    /// Nothing sits at the path.
    Removed,
}

/// Resolves once the file at `path` is no longer `original`, checking every `interval`.
pub async fn changed(path: PathBuf, original: Identity, interval: Duration) -> Change {
    loop {
        time::sleep(interval).await;
        match Identity::of(&path) {
            Ok(current) if current == original => {}
            Ok(_) => return Change::Replaced,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Change::Removed,
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "cannot check the executable");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTERVAL: Duration = Duration::from_millis(10);
    const QUIET: Duration = Duration::from_millis(100);

    fn watched() -> (tempfile::TempDir, PathBuf, Identity) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("eventkit-bridge");
        fs::write(&path, "old").unwrap();
        let identity = Identity::of(&path).unwrap();
        (dir, path, identity)
    }

    #[test]
    fn identity_is_stable_for_the_same_file() {
        let (_dir, path, identity) = watched();
        fs::write(&path, "rewritten in place").unwrap();
        assert_eq!(Identity::of(&path).unwrap(), identity);
    }

    #[test]
    fn identity_follows_symlinks() {
        let (dir, path, identity) = watched();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert_eq!(Identity::of(&link).unwrap(), identity);
    }

    #[test]
    fn identity_of_missing_file_fails() {
        let dir = tempfile::tempdir().unwrap();
        let err = Identity::of(&dir.path().join("missing")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn canonical_executable_exists() {
        let path = canonical().unwrap();
        assert!(path.is_absolute());
        assert_eq!(fs::canonicalize(&path).unwrap(), path);
    }

    #[tokio::test]
    async fn rename_over_is_replaced() {
        let (dir, path, identity) = watched();
        let watcher = tokio::spawn(changed(path.clone(), identity, INTERVAL));
        time::sleep(QUIET).await;
        assert!(!watcher.is_finished());
        let staged = dir.path().join("staged");
        fs::write(&staged, "new").unwrap();
        fs::rename(&staged, &path).unwrap();
        let change = time::timeout(Duration::from_secs(5), watcher)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(change, Change::Replaced);
    }

    #[tokio::test]
    async fn removal_is_removed() {
        let (_dir, path, identity) = watched();
        let watcher = tokio::spawn(changed(path.clone(), identity, INTERVAL));
        time::sleep(QUIET).await;
        assert!(!watcher.is_finished());
        fs::remove_file(&path).unwrap();
        let change = time::timeout(Duration::from_secs(5), watcher)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(change, Change::Removed);
    }

    #[tokio::test]
    async fn unchanged_file_never_resolves() {
        let (_dir, path, identity) = watched();
        fs::write(&path, "rewritten in place").unwrap();
        let result = time::timeout(QUIET, changed(path, identity, INTERVAL)).await;
        assert!(result.is_err());
    }
}
