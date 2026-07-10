use std::path::{Path, PathBuf};

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
    pub ssh: PathBuf,
    pub tmux: PathBuf,
}

impl ProcessBinaries {
    pub fn new(ssh: impl Into<PathBuf>, tmux: impl Into<PathBuf>) -> Self {
        Self {
            ssh: ssh.into(),
            tmux: tmux.into(),
        }
    }

    pub fn ssh(&self) -> &Path {
        &self.ssh
    }

    pub fn tmux(&self) -> &Path {
        &self.tmux
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
