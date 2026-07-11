use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use fs2::FileExt;
use uuid::Uuid;
use thiserror::Error;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

pub(crate) fn with_advisory_lock<T>(
    path: &Path,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    with_advisory_lock_mode(path, operation, true)
}

pub(crate) fn with_advisory_lock_preserving_parent<T>(
    path: &Path,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    with_advisory_lock_mode(path, operation, false)
}

fn with_advisory_lock_mode<T>(
    path: &Path,
    operation: impl FnOnce() -> Result<T>,
    private_parent: bool,
) -> Result<T> {
    let parent = usable_parent(path);
    ensure_directory(parent, private_parent)?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("storage path `{}` has no file name", path.display()))?;
    let lock_path = parent.join(format!(".{}.lock", file_name.to_string_lossy()));
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    options.mode(0o600);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let lock = options
        .open(&lock_path)
        .with_context(|| format!("open storage lock `{}`", lock_path.display()))?;
    let metadata = lock
        .metadata()
        .with_context(|| format!("inspect storage lock `{}`", lock_path.display()))?;
    if !metadata.is_file() {
        anyhow::bail!("storage lock `{}` is not a regular file", lock_path.display());
    }
    #[cfg(unix)]
    if metadata.nlink() != 1 {
        anyhow::bail!(
            "storage lock `{}` has unexpected hard links",
            lock_path.display()
        );
    }
    #[cfg(unix)]
    lock.set_permissions(fs::Permissions::from_mode(0o600))
        .with_context(|| format!("set private permissions on `{}`", lock_path.display()))?;
    lock.lock_exclusive()
        .with_context(|| format!("lock storage `{}`", path.display()))?;

    let result = operation();
    let unlock =
        FileExt::unlock(&lock).with_context(|| format!("unlock storage `{}`", path.display()));
    match (result, unlock) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

#[derive(Debug, Error)]
pub(crate) enum AtomicWriteError {
    #[error("atomic write failed before rename commit")]
    PreCommit(#[source] anyhow::Error),
    #[error("atomic rename committed, but directory durability is uncertain")]
    PostCommitDurability(#[source] io::Error),
}

impl AtomicWriteError {
    pub(crate) fn committed(&self) -> bool {
        matches!(self, Self::PostCommitDurability(_))
    }
}

pub(crate) fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), AtomicWriteError> {
    reconcile_committed_write(path, contents, atomic_write_mode(path, contents, true))
}

pub(crate) fn atomic_write_preserving_parent(
    path: &Path,
    contents: &[u8],
) -> Result<(), AtomicWriteError> {
    reconcile_committed_write(path, contents, atomic_write_mode(path, contents, false))
}

fn reconcile_committed_write(
    path: &Path,
    contents: &[u8],
    result: Result<(), AtomicWriteError>,
) -> Result<(), AtomicWriteError> {
    match result {
        Err(error) if error.committed() => match fs::read(path) {
            Ok(actual) if actual == contents => Ok(()),
            _ => Err(error),
        },
        result => result,
    }
}

fn atomic_write_mode(
    path: &Path,
    contents: &[u8],
    private_parent: bool,
) -> Result<(), AtomicWriteError> {
    atomic_write_mode_with(path, contents, private_parent, || Ok(()), |parent| {
        File::open(parent).and_then(|directory| directory.sync_all())
    })
}

fn atomic_write_mode_with(
    path: &Path,
    contents: &[u8],
    private_parent: bool,
    before_rename: impl FnOnce() -> io::Result<()>,
    sync_parent: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<(), AtomicWriteError> {
    let precommit = || -> Result<()> {
        ensure_directory(usable_parent(path), private_parent)?;
        Ok(())
    };
    precommit().map_err(AtomicWriteError::PreCommit)?;
    let parent = usable_parent(path);
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("storage path `{}` has no file name", path.display()))
        .map_err(AtomicWriteError::PreCommit)?;
    let mut temp_path = PathBuf::new();
    let mut temp_file = None;

    for _ in 0..16 {
        let candidate = parent.join(format!(
            ".{}.{}.tmp",
            file_name.to_string_lossy(),
            Uuid::now_v7().simple()
        ));
        match private_new_file(&candidate) {
            Ok(file) => {
                temp_path = candidate;
                temp_file = Some(file);
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(AtomicWriteError::PreCommit(anyhow::Error::new(error).context(
                    format!("create temporary file beside `{}`", path.display()),
                )));
            }
        }
    }

    let mut temp_file = temp_file
        .ok_or_else(|| {
            anyhow::anyhow!(
                "could not allocate a temporary file beside `{}` after repeated attempts",
                path.display()
            )
        })
        .map_err(AtomicWriteError::PreCommit)?;
    let mut cleanup = TempFileCleanup::new(temp_path.clone());

    temp_file
        .write_all(contents)
        .with_context(|| format!("write temporary file for `{}`", path.display()))
        .map_err(AtomicWriteError::PreCommit)?;
    temp_file
        .sync_all()
        .with_context(|| format!("sync temporary file for `{}`", path.display()))
        .map_err(AtomicWriteError::PreCommit)?;
    drop(temp_file);
    before_rename()
        .with_context(|| format!("prepare rename for `{}`", path.display()))
        .map_err(AtomicWriteError::PreCommit)?;

    fs::rename(&temp_path, path)
        .with_context(|| format!("atomically replace `{}`", path.display()))
        .map_err(AtomicWriteError::PreCommit)?;
    cleanup.disarm();

    sync_parent(parent).map_err(AtomicWriteError::PostCommitDurability)
}

fn usable_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn ensure_directory(path: &Path, enforce_private: bool) -> Result<()> {
    let existed = path.exists();
    fs::create_dir_all(path)
        .with_context(|| format!("create storage directory `{}`", path.display()))?;

    #[cfg(unix)]
    if path != Path::new(".") && (enforce_private || !existed) {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("set private permissions on `{}`", path.display()))?;
    }

    Ok(())
}

fn private_new_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);

    let file = options.open(path)?;
    #[cfg(unix)]
    if let Err(error) = file.set_permissions(fs::Permissions::from_mode(0o600)) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(file)
}

struct TempFileCleanup {
    path: PathBuf,
    armed: bool,
}

impl TempFileCleanup {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempFileCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injected_precommit_and_postcommit_failures_have_distinct_safe_results() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        fs::write(&path, b"old").unwrap();

        let precommit = atomic_write_mode_with(
            &path,
            b"new",
            true,
            || Err(io::Error::other("injected before rename")),
            |_| Ok(()),
        )
        .unwrap_err();
        assert!(!precommit.committed());
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert_eq!(
            fs::read_dir(temp.path())
                .unwrap()
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path() != path)
                .count(),
            0,
            "precommit failure must clean its temporary file"
        );

        let postcommit = atomic_write_mode_with(
            &path,
            b"new",
            true,
            || Ok(()),
            |_| Err(io::Error::other("injected directory sync failure")),
        )
        .unwrap_err();
        assert!(postcommit.committed());
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert!(
            reconcile_committed_write(&path, b"new", Err(postcommit)).is_ok(),
            "a caller must re-read and accept the complete committed document"
        );
    }
}
