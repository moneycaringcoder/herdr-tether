use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

pub(crate) fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = usable_parent(path);
    ensure_private_directory(parent)?;

    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("storage path `{}` has no file name", path.display()))?;
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
                return Err(error)
                    .with_context(|| format!("create temporary file beside `{}`", path.display()));
            }
        }
    }

    let mut temp_file = temp_file.ok_or_else(|| {
        anyhow::anyhow!(
            "could not allocate a temporary file beside `{}` after repeated attempts",
            path.display()
        )
    })?;
    let mut cleanup = TempFileCleanup::new(temp_path.clone());

    temp_file
        .write_all(contents)
        .with_context(|| format!("write temporary file for `{}`", path.display()))?;
    temp_file
        .sync_all()
        .with_context(|| format!("sync temporary file for `{}`", path.display()))?;
    drop(temp_file);

    fs::rename(&temp_path, path)
        .with_context(|| format!("atomically replace `{}`", path.display()))?;
    cleanup.disarm();

    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("sync directory `{}`", parent.display()))?;
    Ok(())
}

fn usable_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)
        .with_context(|| format!("create storage directory `{}`", path.display()))?;

    #[cfg(unix)]
    if path != Path::new(".") {
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
