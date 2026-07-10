use std::process::{Command, Output};

use anyhow::{Context, Result, bail};

use crate::{
    backend::{CommandSpec, DurableBackend, LaunchSpec, ProcessBinaries, WorkloadState},
    model::SessionId,
    quote::posix_quote,
};

#[derive(Clone, Debug)]
enum Location {
    Local,
    Remote(String),
}

/// Durable local or OpenSSH-backed tmux sessions.
#[derive(Clone, Debug)]
pub struct TmuxBackend {
    location: Location,
    binaries: ProcessBinaries,
}

impl TmuxBackend {
    pub fn local(binaries: ProcessBinaries) -> Self {
        Self {
            location: Location::Local,
            binaries,
        }
    }

    pub fn remote(target: impl Into<String>, binaries: ProcessBinaries) -> Result<Self> {
        let target = target.into();
        validate_target(&target)?;
        Ok(Self {
            location: Location::Remote(target),
            binaries,
        })
    }

    fn tmux_spec(&self, arguments: Vec<String>, interactive: bool) -> Result<CommandSpec> {
        match &self.location {
            Location::Local => Ok(CommandSpec::new(self.binaries.tmux.clone(), arguments)),
            Location::Remote(target) => {
                let remote_command = remote_tmux_command(&arguments)?;
                let mut ssh_arguments = if interactive {
                    vec![
                        "-o".to_owned(),
                        "BatchMode=yes".to_owned(),
                        "-t".to_owned(),
                        "-o".to_owned(),
                        "ServerAliveInterval=15".to_owned(),
                        "-o".to_owned(),
                        "ServerAliveCountMax=3".to_owned(),
                    ]
                } else {
                    vec![
                        "-o".to_owned(),
                        "BatchMode=yes".to_owned(),
                        "-o".to_owned(),
                        "ServerAliveInterval=15".to_owned(),
                        "-o".to_owned(),
                        "ServerAliveCountMax=3".to_owned(),
                    ]
                };
                ssh_arguments.extend([
                    "--".to_owned(),
                    target.clone(),
                    remote_command,
                ]);
                Ok(CommandSpec::new(self.binaries.ssh.clone(), ssh_arguments))
            }
        }
    }

    fn output(&self, spec: &CommandSpec) -> Result<Output> {
        Command::new(&spec.program)
            .args(&spec.args)
            .output()
            .with_context(|| format!("run `{}`", spec.program.display()))
    }

    fn require_success(&self, operation: &str, spec: &CommandSpec) -> Result<()> {
        let output = self.output(spec)?;
        if output.status.success() {
            return Ok(());
        }

        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.trim();
        if detail.is_empty() {
            bail!("tmux {operation} failed with status {}", output.status);
        }
        bail!(
            "tmux {operation} failed with status {}: {detail}",
            output.status
        )
    }
}

impl DurableBackend for TmuxBackend {
    fn create(&self, launch: &LaunchSpec) -> Result<()> {
        let spec = self.tmux_spec(
            vec![
                "new-session".to_owned(),
                "-d".to_owned(),
                "-s".to_owned(),
                launch.id.to_string(),
                "-c".to_owned(),
                launch.directory.clone(),
                "--".to_owned(),
                "/bin/sh".to_owned(),
                "-lc".to_owned(),
                launch.command.clone(),
            ],
            false,
        )?;
        self.require_success("create", &spec)
    }

    fn inspect(&self, id: &SessionId) -> Result<WorkloadState> {
        let spec = self.tmux_spec(
            vec![
                "display-message".to_owned(),
                "-p".to_owned(),
                "-t".to_owned(),
                exact_target(id),
                "#{session_attached}".to_owned(),
            ],
            false,
        )?;
        let output = self.output(&spec)?;
        if output.status.success() {
            return Ok(match String::from_utf8(output.stdout)
                .ok()
                .and_then(|value| value.trim().parse::<u32>().ok())
            {
                Some(attached) => WorkloadState::Running { attached },
                None => WorkloadState::Unknown,
            });
        }

        Ok(if output.status.code() == Some(1) {
            WorkloadState::Missing
        } else {
            WorkloadState::Unknown
        })
    }

    fn attach_command(&self, id: &SessionId) -> Result<CommandSpec> {
        self.tmux_spec(
            vec![
                "attach-session".to_owned(),
                "-t".to_owned(),
                exact_target(id),
            ],
            true,
        )
    }

    fn close(&self, id: &SessionId) -> Result<()> {
        let spec = self.tmux_spec(
            vec![
                "kill-session".to_owned(),
                "-t".to_owned(),
                exact_target(id),
            ],
            false,
        )?;
        self.require_success("close", &spec)
    }
}

fn exact_target(id: &SessionId) -> String {
    format!("={id}")
}

fn remote_tmux_command(arguments: &[String]) -> Result<String> {
    let mut command = posix_quote("tmux")?;
    for argument in arguments {
        command.push(' ');
        command.push_str(&posix_quote(argument)?);
    }
    Ok(command)
}

fn validate_target(target: &str) -> Result<()> {
    if target.is_empty() || target.starts_with('-') || target.bytes().any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control()) {
        bail!("invalid explicit SSH target `{target}`");
    }

    if let Some(authority) = target.strip_prefix("ssh://") {
        validate_ssh_authority(authority)
    } else {
        validate_user_host(target)
    }
}

fn validate_ssh_authority(authority: &str) -> Result<()> {
    if authority.is_empty()
        || authority.contains('/')
        || authority.contains('?')
        || authority.contains('#')
    {
        bail!("invalid explicit SSH target `ssh://{authority}`");
    }

    let host_port = match authority.split_once('@') {
        Some((user, host_port)) if valid_component(user) && !host_port.contains('@') => host_port,
        Some(_) => bail!("invalid explicit SSH target `ssh://{authority}`"),
        None => authority,
    };

    let (host, port, bracketed_host) = if let Some(bracketed) = host_port.strip_prefix('[') {
        let Some(close) = bracketed.find(']') else {
            bail!("invalid explicit SSH target `ssh://{authority}`");
        };
        let host = &bracketed[..close];
        let suffix = &bracketed[close + 1..];
        let port = if suffix.is_empty() {
            None
        } else {
            Some(suffix.strip_prefix(':').ok_or_else(|| {
                anyhow::anyhow!("invalid explicit SSH target `ssh://{authority}`")
            })?)
        };
        if host.is_empty()
            || !host
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || matches!(byte, b':' | b'.' | b'%'))
        {
            bail!("invalid explicit SSH target `ssh://{authority}`");
        }
        (host, port, true)
    } else if let Some((host, port)) = host_port.rsplit_once(':') {
        if host.contains(':') {
            bail!("invalid explicit SSH target `ssh://{authority}`");
        }
        (host, Some(port), false)
    } else {
        (host_port, None, false)
    };

    if host.is_empty() || host.starts_with('-') || (!bracketed_host && !valid_host(host)) {
        bail!("invalid explicit SSH target `ssh://{authority}`");
    }
    if let Some(port) = port {
        let valid_port = !port.is_empty()
            && port.bytes().all(|byte| byte.is_ascii_digit())
            && port.parse::<u16>().is_ok_and(|port| port != 0);
        if !valid_port {
            bail!("invalid explicit SSH target `ssh://{authority}`");
        }
    }
    Ok(())
}

fn validate_user_host(target: &str) -> Result<()> {
    let host = match target.split_once('@') {
        Some((user, host)) if valid_component(user) && !host.contains('@') => host,
        Some(_) => bail!("invalid explicit SSH target `{target}`"),
        None => target,
    };
    if host.is_empty() || host.starts_with('-') || !valid_host(host) {
        bail!("invalid explicit SSH target `{target}`");
    }
    Ok(())
}

fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'%')
        })
}

fn valid_host(value: &str) -> bool {
    value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
    })
}
