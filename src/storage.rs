use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use fs2::FileExt;
use thiserror::Error;
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

pub(crate) fn with_advisory_lock<T>(
    path: &Path,
    operation: impl FnOnce(&Path) -> Result<T>,
) -> Result<T> {
    with_advisory_lock_mode(path, operation, true)
}

pub(crate) fn with_advisory_lock_preserving_parent<T>(
    path: &Path,
    operation: impl FnOnce(&Path) -> Result<T>,
) -> Result<T> {
    with_advisory_lock_mode(path, operation, false)
}

fn with_advisory_lock_mode<T>(
    path: &Path,
    operation: impl FnOnce(&Path) -> Result<T>,
    private_parent: bool,
) -> Result<T> {
    // Resolution necessarily precedes the lock, so a link replaced in between
    // is locked under its previous identity. Storage paths live in the caller's
    // own private tree, where that window is a same-user ordering concern
    // rather than a trust boundary.
    let path = prepare_storage_path(path, private_parent)
        .with_context(|| format!("prepare storage path `{}`", path.display()))?;
    let path = path.as_path();
    let parent = usable_parent(path);
    #[cfg(unix)]
    let parent_lock = {
        let directory = File::open(parent)
            .with_context(|| format!("open storage directory `{}`", parent.display()))?;
        let metadata = directory
            .metadata()
            .with_context(|| format!("inspect storage directory `{}`", parent.display()))?;
        if !metadata.is_dir() {
            anyhow::bail!("storage parent `{}` is not a directory", parent.display());
        }
        directory
            .lock_exclusive()
            .with_context(|| format!("lock storage directory `{}`", parent.display()))?;
        directory
    };
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
        anyhow::bail!(
            "storage lock `{}` is not a regular file",
            lock_path.display()
        );
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

    let result = operation(path);
    let unlock =
        FileExt::unlock(&lock).with_context(|| format!("unlock storage `{}`", path.display()));
    #[cfg(unix)]
    let parent_unlock = FileExt::unlock(&parent_lock)
        .with_context(|| format!("unlock storage directory `{}`", parent.display()));
    #[cfg(not(unix))]
    let parent_unlock: Result<()> = Ok(());
    match (result, unlock, parent_unlock) {
        (Ok(value), Ok(()), Ok(())) => Ok(value),
        (Err(error), _, _) => Err(error),
        (Ok(_), Err(error), _) | (Ok(_), Ok(()), Err(error)) => Err(error),
    }
}

#[derive(Debug, Error)]
pub(crate) enum AtomicWriteError {
    #[error("atomic write failed before rename commit")]
    PreCommit(#[source] anyhow::Error),
    #[error("atomic rename committed, but directory durability is uncertain")]
    PostCommitDurability(#[source] io::Error),
}

#[cfg(test)]
impl AtomicWriteError {
    fn committed(&self) -> bool {
        matches!(self, Self::PostCommitDurability(_))
    }
}

#[cfg(test)]
pub(crate) fn atomic_write_preserving_parent(
    path: &Path,
    contents: &[u8],
) -> Result<(), AtomicWriteError> {
    atomic_write_mode(path, contents, false)
}

pub(crate) fn atomic_write_resolved(path: &Path, contents: &[u8]) -> Result<(), AtomicWriteError> {
    atomic_write_prepared_mode_with(
        path,
        contents,
        || Ok(()),
        |parent| File::open(parent).and_then(|directory| directory.sync_all()),
    )
}

#[cfg(test)]
fn atomic_write_mode(
    path: &Path,
    contents: &[u8],
    private_parent: bool,
) -> Result<(), AtomicWriteError> {
    atomic_write_mode_with(
        path,
        contents,
        private_parent,
        || Ok(()),
        |parent| File::open(parent).and_then(|directory| directory.sync_all()),
    )
}

#[cfg(test)]
fn atomic_write_mode_with(
    path: &Path,
    contents: &[u8],
    private_parent: bool,
    before_rename: impl FnOnce() -> io::Result<()>,
    sync_parent: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<(), AtomicWriteError> {
    let path = prepare_storage_path(path, private_parent)
        .with_context(|| format!("prepare atomic write destination `{}`", path.display()))
        .map_err(AtomicWriteError::PreCommit)?;
    atomic_write_prepared_mode_with(&path, contents, before_rename, sync_parent)
}

fn atomic_write_prepared_mode_with(
    path: &Path,
    contents: &[u8],
    before_rename: impl FnOnce() -> io::Result<()>,
    sync_parent: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<(), AtomicWriteError> {
    #[cfg(unix)]
    let existing_mode = destination_mode(path)
        .with_context(|| format!("inspect atomic write destination `{}`", path.display()))
        .map_err(AtomicWriteError::PreCommit)?;
    #[cfg(not(unix))]
    validate_destination(path)
        .with_context(|| format!("inspect atomic write destination `{}`", path.display()))
        .map_err(AtomicWriteError::PreCommit)?;
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
                return Err(AtomicWriteError::PreCommit(
                    anyhow::Error::new(error)
                        .context(format!("create temporary file beside `{}`", path.display())),
                ));
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
    #[cfg(unix)]
    if let Some(mode) = existing_mode {
        temp_file
            .set_permissions(fs::Permissions::from_mode(mode))
            .with_context(|| format!("preserve permissions for `{}`", path.display()))
            .map_err(AtomicWriteError::PreCommit)?;
    }
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

#[cfg(unix)]
fn destination_mode(path: &Path) -> io::Result<Option<u32>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(Some(metadata.permissions().mode() & 0o7777)),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "destination is not a regular file",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(not(unix))]
fn validate_destination(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "destination is not a regular file",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn prepare_storage_path(path: &Path, private_parent: bool) -> Result<PathBuf> {
    let parent = usable_parent(path);
    // A linked parent belongs to the user's own layout, so its existing
    // permissions are preserved. A directory Tether creates itself is still
    // privatized, linked ancestors included.
    let enforce_private = private_parent && !contains_linked_component(parent)?;
    ensure_directory(parent, enforce_private)?;
    resolve_storage_path(path).map_err(Into::into)
}

fn contains_linked_component(path: &Path) -> io::Result<bool> {
    let mut current = if path.is_relative() {
        PathBuf::from(".")
    } else {
        PathBuf::new()
    };
    #[cfg(unix)]
    let owner = unsafe { libc::geteuid() };
    #[cfg(unix)]
    let mut user_tree = path.is_relative()
        && fs::symlink_metadata(".").is_ok_and(|metadata| metadata.uid() == owner);

    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                #[cfg(unix)]
                {
                    if metadata.file_type().is_symlink() && (user_tree || metadata.uid() == owner) {
                        return Ok(true);
                    }
                    user_tree |= metadata.uid() == owner;
                }
                #[cfg(not(unix))]
                if metadata.file_type().is_symlink() {
                    return Ok(true);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

fn resolve_storage_path(path: &Path) -> io::Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            let resolved = fs::canonicalize(path)?;
            if fs::metadata(&resolved)?.is_file() {
                Ok(resolved)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "storage path is not a regular file",
                ))
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let file_name = path.file_name().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "storage path has no file name")
            })?;
            Ok(fs::canonicalize(usable_parent(path))?.join(file_name))
        }
        Err(error) => Err(error),
    }
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

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_existing_regular_file_modes() {
        for expected_mode in [0o640, 0o600] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("state.json");
            fs::write(&path, b"old").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(expected_mode)).unwrap();

            atomic_write_preserving_parent(&path, b"new").unwrap();

            assert_eq!(fs::read(&path).unwrap(), b"new");
            assert_eq!(mode(&path), expected_mode);
        }
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_creates_private_file_without_changing_parent_mode() {
        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o750)).unwrap();
        let path = temp.path().join("state.json");

        atomic_write_preserving_parent(&path, b"new").unwrap();

        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(temp.path()), 0o750);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_follows_symlink_and_rejects_non_regular_destinations() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target.json");
        let link = temp.path().join("state.json");
        fs::write(&target, b"target").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        symlink(&target, &link).unwrap();

        atomic_write_preserving_parent(&link, b"new").unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert_eq!(mode(&target), 0o640);

        let missing = temp.path().join("missing.json");
        let dangling = temp.path().join("dangling.json");
        symlink(&missing, &dangling).unwrap();
        let error = atomic_write_preserving_parent(&dangling, b"new").unwrap_err();
        assert!(!error.committed());
        assert!(fs::symlink_metadata(&dangling).unwrap().is_symlink());
        assert!(!missing.exists());

        let directory = temp.path().join("directory");
        fs::create_dir(&directory).unwrap();
        let error = atomic_write_preserving_parent(&directory, b"new").unwrap_err();
        assert!(!error.committed());
        assert!(directory.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn advisory_lock_rejects_non_regular_symlink_targets() {
        use std::{
            ffi::CString,
            os::unix::{ffi::OsStrExt, fs::symlink},
        };

        let temp = tempfile::tempdir().unwrap();
        let fifo = temp.path().join("config.fifo");
        let link = temp.path().join("config.toml");
        let fifo_path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
        symlink(&fifo, &link).unwrap();

        let error = with_advisory_lock(&link, |_| Ok(())).unwrap_err();

        let error = format!("{error:#}");
        assert!(error.contains("regular file"), "unexpected error: {error}");
    }

    #[cfg(unix)]
    #[test]
    fn advisory_lock_uses_resolved_symlink_target_identity() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let logical_parent = temp.path().join("config");
        let target_parent = temp.path().join("stow");
        fs::create_dir_all(&logical_parent).unwrap();
        fs::create_dir_all(&target_parent).unwrap();
        let target = target_parent.join("config.toml");
        let link = logical_parent.join("config.toml");
        fs::write(&target, b"version = 1\n").unwrap();
        symlink("../stow/config.toml", &link).unwrap();
        let resolved = fs::canonicalize(&target).unwrap();

        with_advisory_lock(&link, |locked_path| {
            assert_eq!(locked_path, resolved);
            assert!(target_parent.join(".config.toml.lock").is_file());
            assert!(!logical_parent.join(".config.toml.lock").exists());
            Ok(())
        })
        .unwrap();

        with_advisory_lock(&target, |locked_path| {
            assert_eq!(locked_path, resolved);
            Ok(())
        })
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn linked_ancestors_keep_created_directories_private_without_reclaiming_existing_ones() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let stow = temp.path().join("dotfiles");
        fs::create_dir_all(&stow).unwrap();
        let link = temp.path().join("config");
        symlink(&stow, &link).unwrap();

        // Tether creates this directory itself, so it stays private even though
        // an ancestor is a link.
        let created = stow.join("herdr-tether");
        with_advisory_lock(&link.join("herdr-tether/state.json"), |_| Ok(())).unwrap();
        assert_eq!(mode(&created), 0o700);

        // A directory the user already manages keeps its own permissions.
        let adopted = stow.join("adopted");
        fs::create_dir_all(&adopted).unwrap();
        fs::set_permissions(&adopted, fs::Permissions::from_mode(0o755)).unwrap();
        with_advisory_lock(&link.join("adopted/state.json"), |_| Ok(())).unwrap();
        assert_eq!(mode(&adopted), 0o755);
    }

    #[cfg(unix)]
    #[test]
    fn precommit_failure_preserves_original_mode_and_content() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        fs::write(&path, b"old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

        let error = atomic_write_mode_with(
            &path,
            b"new",
            false,
            || Err(io::Error::other("injected before rename")),
            |_| Ok(()),
        )
        .unwrap_err();

        assert!(!error.committed());
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert_eq!(mode(&path), 0o640);
        assert_eq!(
            fs::read_dir(temp.path())
                .unwrap()
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path() != path)
                .count(),
            0,
            "precommit failure must clean its temporary file"
        );
    }

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
        atomic_write_mode_with(&path, b"new", true, || Ok(()), |_| Ok(())).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
    }
}
