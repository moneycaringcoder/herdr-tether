use std::{
    env,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::{model::SessionId, quote::posix_quote};

/// An executable and its already-separated argument vector.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl CommandSpec {
    pub fn new(program: impl Into<PathBuf>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
        }
    }

    /// Produces a shell-safe command line containing exactly this argv.
    pub fn posix_command_line(&self) -> Result<String> {
        let program = self.program.to_str().with_context(|| {
            format!(
                "command program path `{}` is not valid UTF-8",
                self.program.display()
            )
        })?;
        let mut command = posix_quote(program)?;
        for argument in &self.args {
            command.push(' ');
            command.push_str(&posix_quote(argument)?);
        }
        Ok(command)
    }
}

/// Process locations used to invoke OpenSSH and local tmux.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessBinaries {
    ssh: PathBuf,
    tmux: PathBuf,
}

impl ProcessBinaries {
    pub fn new(ssh: impl Into<PathBuf>, tmux: impl Into<PathBuf>) -> Self {
        Self {
            ssh: resolve_executable(ssh.into()),
            tmux: resolve_executable(tmux.into()),
        }
    }

    pub fn ssh(&self) -> &Path {
        &self.ssh
    }

    pub fn tmux(&self) -> &Path {
        &self.tmux
    }
}

fn resolve_executable(program: PathBuf) -> PathBuf {
    if program.components().count() != 1
        || !matches!(program.components().next(), Some(Component::Normal(_)))
    {
        return program;
    }

    let search_name = program.as_os_str();
    let path_directories = env::var_os("PATH")
        .into_iter()
        .flat_map(|path| env::split_paths(&path).collect::<Vec<_>>())
        .filter(|directory| directory.is_absolute());
    let standard_directories = [
        Path::new("/usr/bin"),
        Path::new("/bin"),
        Path::new("/opt/homebrew/bin"),
        Path::new("/usr/local/bin"),
    ];
    path_directories
        .chain(standard_directories.into_iter().map(Path::to_path_buf))
        .map(|directory| directory.join(search_name))
        .find(|candidate| is_executable(candidate))
        .unwrap_or(program)
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchSpec {
    pub id: SessionId,
    pub directory: String,
    pub command: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkloadState {
    Missing,
    Running { attached: u32 },
    Unknown,
}

/// Operations required from a durable session backend.
pub trait DurableBackend {
    fn create(&self, launch: &LaunchSpec) -> Result<()>;
    fn inspect(&self, id: &SessionId) -> Result<WorkloadState>;
    fn attach_command(&self, id: &SessionId) -> Result<CommandSpec>;
    fn close(&self, id: &SessionId) -> Result<()>;
}
