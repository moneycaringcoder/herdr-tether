use std::process::{Command, Output};

use anyhow::{Context, Result, bail};

use crate::{
    backend::{CommandSpec, DurableBackend, LaunchSpec, ProcessBinaries, WorkloadState},
    model::{ExternalSessionName, SessionId},
    quote::posix_quote,
    sshcfg::validate_ssh_target,
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
        validate_ssh_target(&target)?;
        Ok(Self {
            location: Location::Remote(target),
            binaries,
        })
    }

    fn tmux_spec(&self, arguments: Vec<String>, interactive: bool) -> Result<CommandSpec> {
        match &self.location {
            Location::Local => Ok(CommandSpec::new(self.binaries.tmux(), arguments)),
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
                ssh_arguments.extend(["--".to_owned(), target.clone(), remote_command]);
                Ok(CommandSpec::new(self.binaries.ssh(), ssh_arguments))
            }
        }
    }

    fn shell_spec(&self, arguments: Vec<String>) -> Result<CommandSpec> {
        match &self.location {
            Location::Local => Ok(CommandSpec::new("/bin/sh", arguments)),
            Location::Remote(target) => {
                let remote_command = remote_command("/bin/sh", &arguments)?;
                Ok(CommandSpec::new(
                    self.binaries.ssh(),
                    vec![
                        "-o".to_owned(),
                        "BatchMode=yes".to_owned(),
                        "-o".to_owned(),
                        "ServerAliveInterval=15".to_owned(),
                        "-o".to_owned(),
                        "ServerAliveCountMax=3".to_owned(),
                        "--".to_owned(),
                        target.clone(),
                        remote_command,
                    ],
                ))
            }
        }
    }

    fn output(&self, spec: &CommandSpec) -> Result<Output> {
        Command::new(&spec.program)
            .args(&spec.args)
            .output()
            .with_context(|| {
                format!(
                    "start `{}`; install the tool or make it executable (Tether also searches /usr/bin, /bin, /opt/homebrew/bin, and /usr/local/bin)",
                    spec.program.display()
                )
            })
    }

    pub(crate) fn inspect_exact_spec(&self, id: &SessionId) -> Result<CommandSpec> {
        let id_text = id.to_string();
        self.tmux_spec(
            vec![
                "list-sessions".to_owned(),
                "-F".to_owned(),
                "#{session_name}:#{session_attached}".to_owned(),
                "-f".to_owned(),
                format!("#{{==:#{{session_name}},{id_text}}}"),
            ],
            false,
        )
    }

    pub(crate) fn close_exact_spec(&self, id: &SessionId) -> Result<CommandSpec> {
        self.tmux_spec(
            vec!["kill-session".to_owned(), "-t".to_owned(), exact_target(id)],
            false,
        )
    }

    pub(crate) fn classify_exact_inspect(
        &self,
        id: &SessionId,
        output: &Output,
        stdout_truncated: bool,
    ) -> WorkloadState {
        classify_exact_inspect(id, output.status.code(), &output.stdout, stdout_truncated)
    }

    pub(crate) fn classify_exact_inspect_parts(
        &self,
        id: &SessionId,
        exit_code: Option<i32>,
        stdout: &[u8],
        stdout_truncated: bool,
    ) -> WorkloadState {
        classify_exact_inspect(id, exit_code, stdout, stdout_truncated)
    }

    pub(crate) fn status_spec(&self) -> Result<CommandSpec> {
        self.tmux_spec(
            vec![
                "list-sessions".to_owned(),
                "-F".to_owned(),
                "#{session_name}:#{session_attached}".to_owned(),
            ],
            false,
        )
    }

    pub fn attach_external_command(&self, name: &ExternalSessionName) -> Result<CommandSpec> {
        self.tmux_spec(
            vec![
                "attach-session".to_owned(),
                "-t".to_owned(),
                format!("={}", name.as_str()),
            ],
            true,
        )
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

    fn enable_owned_mouse(&self, id: &SessionId) -> Result<()> {
        let target = self.owned_session_target(id)?;
        self.enable_mouse_for_target(target)
    }

    fn enable_mouse_for_target(&self, target: String) -> Result<()> {
        let spec = self.tmux_spec(
            vec![
                "set-option".to_owned(),
                "-t".to_owned(),
                target,
                "mouse".to_owned(),
                "on".to_owned(),
            ],
            false,
        )?;
        self.require_success("enable session mouse support", &spec)
    }

    fn owned_session_target(&self, id: &SessionId) -> Result<String> {
        let id_text = id.to_string();
        let spec = self.tmux_spec(
            vec![
                "list-sessions".to_owned(),
                "-F".to_owned(),
                "#{session_name}:#{session_id}".to_owned(),
                "-f".to_owned(),
                format!("#{{==:#{{session_name}},{id_text}}}"),
            ],
            false,
        )?;
        let output = self.output(&spec)?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr);
            bail!(
                "tmux resolve exact owned session ID failed with status {}: {}",
                output.status,
                detail.trim()
            );
        }
        let line = std::str::from_utf8(&output.stdout)
            .context("tmux returned a non-UTF-8 internal session ID")?
            .trim_end_matches(['\r', '\n']);
        let (reported_name, target) = line
            .split_once(':')
            .context("tmux did not return the exact owned session ID")?;
        if reported_name != id_text {
            bail!("tmux returned session `{reported_name}`, not exact owned session `{id}`");
        }
        validate_tmux_id(target, '$', "session")
    }

    fn paths_refer_to_same_directory(&self, actual: &str, expected: &str) -> Result<bool> {
        let spec = self.shell_spec(vec![
            "-c".to_owned(),
            "[ \"$1\" -ef \"$2\" ]".to_owned(),
            "tether-cwd-check".to_owned(),
            actual.to_owned(),
            expected.to_owned(),
        ])?;
        let output = self.output(&spec)?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => {
                let detail = String::from_utf8_lossy(&output.stderr);
                bail!(
                    "compare created pane cwd with selected directory failed with status {}: {}",
                    output.status,
                    detail.trim()
                )
            }
        }
    }

    fn verify_created_cwd(&self, pane_target: &str, expected: &str) -> Result<()> {
        let spec = self.tmux_spec(
            vec![
                "display-message".to_owned(),
                "-p".to_owned(),
                "-t".to_owned(),
                pane_target.to_owned(),
                "#{pane_current_path}".to_owned(),
            ],
            false,
        )?;
        let output = self.output(&spec)?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr);
            let detail = detail.trim();
            if detail.is_empty() {
                bail!(
                    "tmux verify created cwd failed with status {}",
                    output.status
                );
            }
            bail!(
                "tmux verify created cwd failed with status {}: {detail}",
                output.status
            );
        }
        let stdout = std::str::from_utf8(&output.stdout)
            .context("tmux returned a non-UTF-8 created pane cwd")?;
        let actual = stdout
            .strip_suffix('\n')
            .and_then(|value| value.strip_suffix('\r').or(Some(value)))
            .unwrap_or(stdout);
        if actual != expected && !self.paths_refer_to_same_directory(actual, expected)? {
            bail!("tmux created pane cwd mismatch: selected `{expected}`, got `{actual}`");
        }
        Ok(())
    }

    fn rollback_created(&self, id: &SessionId, cause: anyhow::Error) -> anyhow::Error {
        let rollback = self
            .close_exact_spec(id)
            .and_then(|spec| self.require_success("rollback created session", &spec));
        match rollback {
            Ok(()) => cause,
            Err(rollback) => cause.context(format!(
                "also failed to roll back exact owned session `{id}`: {rollback:#}"
            )),
        }
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
                "-P".to_owned(),
                "-F".to_owned(),
                "#{session_id}:#{pane_id}".to_owned(),
                "--".to_owned(),
                "/bin/sh".to_owned(),
                "-lc".to_owned(),
                "cd -- \"$1\" && exec /bin/sh -c \"$2\"".to_owned(),
                "tether-launch".to_owned(),
                launch.directory.clone(),
                launch.command.clone(),
            ],
            false,
        )?;
        let output = self.output(&spec)?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr);
            bail!(
                "tmux create failed with status {}: {}",
                output.status,
                detail.trim()
            );
        }
        let created = std::str::from_utf8(&output.stdout)
            .context("tmux returned non-UTF-8 created session identities")?
            .trim_end_matches(['\r', '\n']);
        let result = (|| {
            let (session_target, pane_target) = created
                .split_once(':')
                .context("tmux create did not return session and pane identities")?;
            let session_target = validate_tmux_id(session_target, '$', "session")?;
            let pane_target = validate_tmux_id(pane_target, '%', "pane")?;
            self.verify_created_cwd(&pane_target, &launch.directory)?;
            self.enable_mouse_for_target(session_target)
        })();
        if let Err(error) = result {
            return Err(self.rollback_created(&launch.id, error));
        }
        Ok(())
    }

    fn inspect(&self, id: &SessionId) -> Result<WorkloadState> {
        let spec = self.inspect_exact_spec(id)?;
        let output = self.output(&spec)?;
        Ok(self.classify_exact_inspect(id, &output, false))
    }

    fn attach_command(&self, id: &SessionId) -> Result<CommandSpec> {
        self.enable_owned_mouse(id)?;
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
        let spec = self.close_exact_spec(id)?;
        self.require_success("close", &spec)
    }
}

fn classify_exact_inspect(
    id: &SessionId,
    exit_code: Option<i32>,
    stdout: &[u8],
    stdout_truncated: bool,
) -> WorkloadState {
    if exit_code == Some(0) {
        if stdout_truncated {
            return WorkloadState::Unknown;
        }
        let Ok(stdout) = std::str::from_utf8(stdout) else {
            return WorkloadState::Unknown;
        };
        let line = stdout.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            return WorkloadState::Missing;
        }
        let Some((name, attached)) = line.split_once(':') else {
            return WorkloadState::Unknown;
        };
        if name != id.to_string() {
            return WorkloadState::Unknown;
        }
        return match attached.parse::<u32>() {
            Ok(attached) => WorkloadState::Running { attached },
            Err(_) => WorkloadState::Unknown,
        };
    }

    if exit_code == Some(1) {
        WorkloadState::Missing
    } else {
        WorkloadState::Unknown
    }
}

fn validate_tmux_id(value: &str, sigil: char, kind: &str) -> Result<String> {
    if value.len() < 2
        || !value.starts_with(sigil)
        || !value[1..]
            .chars()
            .all(|character| character.is_ascii_digit())
    {
        bail!("tmux returned invalid internal {kind} ID `{value}`");
    }
    Ok(value.to_owned())
}

fn exact_target(id: &SessionId) -> String {
    format!("={id}")
}

fn remote_tmux_command(arguments: &[String]) -> Result<String> {
    remote_command("tmux", arguments)
}

fn remote_command(program: &str, arguments: &[String]) -> Result<String> {
    let mut command = posix_quote(program)?;
    for argument in arguments {
        command.push(' ');
        command.push_str(&posix_quote(argument)?);
    }
    Ok(command)
}
