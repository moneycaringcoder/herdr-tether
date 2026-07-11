use std::process::{Command, Output};

use anyhow::{Context, Result, bail};

use crate::{
    backend::{CommandSpec, DurableBackend, LaunchSpec, ProcessBinaries, WorkloadState},
    model::{ExternalSessionName, OwnershipProof, SessionId, TmuxSessionId},
    quote::posix_quote,
    sshcfg::validate_ssh_target,
};

const LAUNCH_SCRIPT: &str = "directory=$1; case \"$directory\" in '~') directory=$HOME ;; '~/'*) directory=$HOME${directory#\\~} ;; esac; cd -- \"$directory\" && exec /bin/sh -c \"$2\"";
const SAME_DIRECTORY_SCRIPT: &str = "actual=$1; expected=$2; case \"$actual\" in '~') actual=$HOME ;; '~/'*) actual=$HOME${actual#\\~} ;; esac; case \"$expected\" in '~') expected=$HOME ;; '~/'*) expected=$HOME${expected#\\~} ;; esac; [ \"$actual\" -ef \"$expected\" ]";

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

    pub(crate) fn inspect_exact_spec(
        &self,
        id: &SessionId,
        ownership_proof: &OwnershipProof,
    ) -> Result<CommandSpec> {
        let id_text = id.to_string();
        self.tmux_spec(
            vec![
                "list-sessions".to_owned(),
                "-F".to_owned(),
                "#{session_name}:#{session_id}:#{session_attached}:#{pane_dead}:#{pane_dead_status}:#{TETHER_OWNERSHIP_PROOF}"
                    .to_owned(),
                "-f".to_owned(),
                format!(
                    "#{{&&:#{{==:#{{session_name}},{id_text}}},#{{==:#{{TETHER_OWNERSHIP_PROOF}},{ownership_proof}}}}}"
                ),
            ],
            false,
        )
    }

    pub(crate) fn close_exact_spec(&self, identity: TmuxSessionId) -> Result<CommandSpec> {
        self.tmux_spec(
            vec![
                "kill-session".to_owned(),
                "-t".to_owned(),
                identity.to_string(),
            ],
            false,
        )
    }

    pub(crate) fn classify_exact_inspect(
        &self,
        id: &SessionId,
        ownership_proof: &OwnershipProof,
        output: &Output,
        stdout_truncated: bool,
    ) -> WorkloadState {
        classify_exact_inspect(id, ownership_proof, output.status.code(), &output.stdout, stdout_truncated)
    }

    pub(crate) fn classify_exact_inspect_parts(
        &self,
        id: &SessionId,
        ownership_proof: &OwnershipProof,
        exit_code: Option<i32>,
        stdout: &[u8],
        stdout_truncated: bool,
    ) -> WorkloadState {
        classify_exact_inspect(id, ownership_proof, exit_code, stdout, stdout_truncated)
    }

    pub(crate) fn status_spec(&self) -> Result<CommandSpec> {
        self.tmux_spec(
            vec![
                "list-sessions".to_owned(),
                "-F".to_owned(),
                "#{session_name}:#{session_id}:#{session_attached}:#{pane_dead}:#{pane_dead_status}"
                    .to_owned(),
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

    fn enable_owned_mouse(&self, identity: TmuxSessionId) -> Result<()> {
        self.enable_mouse_for_target(identity.to_string())
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

    fn enable_remain_on_exit(&self, pane_target: &str) -> Result<()> {
        let spec = self.tmux_spec(
            vec![
                "set-option".to_owned(),
                "-p".to_owned(),
                "-t".to_owned(),
                pane_target.to_owned(),
                "remain-on-exit".to_owned(),
                "on".to_owned(),
            ],
            false,
        )?;
        self.require_success("retain ended pane status", &spec)
    }

    fn paths_refer_to_same_directory(&self, actual: &str, expected: &str) -> Result<bool> {
        let spec = self.shell_spec(vec![
            "-c".to_owned(),
            SAME_DIRECTORY_SCRIPT.to_owned(),
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

    fn rollback_created(
        &self,
        id: &SessionId,
        internal_target: TmuxSessionId,
        cause: anyhow::Error,
    ) -> anyhow::Error {
        let rollback = self
            .close_exact_spec(internal_target)
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
    fn create(&self, launch: &LaunchSpec) -> Result<TmuxSessionId> {
        let spec = self.tmux_spec(
            vec![
                "new-session".to_owned(),
                "-d".to_owned(),
                "-s".to_owned(),
                launch.id.to_string(),
                "-c".to_owned(),
                launch.directory.clone(),
                "-e".to_owned(),
                format!("TETHER_OWNERSHIP_PROOF={}", launch.ownership_proof),
                "-P".to_owned(),
                "-F".to_owned(),
                "#{session_id}:#{pane_id}".to_owned(),
                "--".to_owned(),
                "/bin/sh".to_owned(),
                "-lc".to_owned(),
                LAUNCH_SCRIPT.to_owned(),
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
        let identities = (|| {
            let (session_target, pane_target) = created
                .split_once(':')
                .context("tmux create did not return session and pane identities")?;
            let session_target = session_target.parse::<TmuxSessionId>()?;
            let pane_target = validate_tmux_id(pane_target, '%', "pane")?;
            Ok::<_, anyhow::Error>((session_target, pane_target))
        })();
        let (session_target, pane_target) = match identities {
            Ok(identities) => identities,
            // Creation may already be committed, but without a trustworthy
            // internal identity destructive compensation would be unsafe.
            Err(error) => return Err(error),
        };
        self.enable_remain_on_exit(&pane_target)
            .and_then(|()| self.verify_created_cwd(&pane_target, &launch.directory))
            .and_then(|()| self.enable_mouse_for_target(session_target.to_string()))
            .map_err(|error| self.rollback_created(&launch.id, session_target, error))?;
        Ok(session_target)
    }

    fn inspect(
        &self,
        id: &SessionId,
        ownership_proof: &OwnershipProof,
    ) -> Result<WorkloadState> {
        let spec = self.inspect_exact_spec(id, ownership_proof)?;
        let output = self.output(&spec)?;
        Ok(self.classify_exact_inspect(id, ownership_proof, &output, false))
    }

    fn attach_command(&self, identity: TmuxSessionId) -> Result<CommandSpec> {
        self.enable_owned_mouse(identity)?;
        self.tmux_spec(
            vec![
                "attach-session".to_owned(),
                "-t".to_owned(),
                identity.to_string(),
            ],
            true,
        )
    }

    fn close(&self, identity: TmuxSessionId) -> Result<()> {
        let spec = self.close_exact_spec(identity)?;
        self.require_success("close", &spec)
    }
}

fn classify_exact_inspect(
    id: &SessionId,
    ownership_proof: &OwnershipProof,
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
        let fields = line.split(':').collect::<Vec<_>>();
        let [name, identity, attached, pane_dead, exit_status, observed_proof] = fields.as_slice() else {
            return WorkloadState::Unknown;
        };
        if *name != id.to_string() {
            return WorkloadState::Unknown;
        }
        if observed_proof.parse::<OwnershipProof>().ok() != Some(*ownership_proof) {
            return WorkloadState::Unknown;
        }
        let Ok(identity) = identity.parse::<TmuxSessionId>() else {
            return WorkloadState::Unknown;
        };
        let Ok(attached) = attached.parse::<u32>() else {
            return WorkloadState::Unknown;
        };
        match *pane_dead {
            "0" => WorkloadState::Running { attached, identity },
            "1" => {
                let exit_status = if exit_status.is_empty() {
                    None
                } else {
                    match exit_status.parse::<i32>() {
                        Ok(status) => Some(status),
                        Err(_) => return WorkloadState::Unknown,
                    }
                };
                WorkloadState::Ended {
                    identity,
                    exit_status,
                }
            }
            _ => WorkloadState::Unknown,
        }
    } else if exit_code == Some(1) {
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

#[cfg(test)]
mod tests {
    use std::{env, fs};

    use super::*;

    #[test]
    fn shell_scripts_expand_remote_home_shorthand() {
        let home = env::var("HOME").unwrap();
        let expected = fs::canonicalize(&home).unwrap();

        for selected in ["~", "~/"] {
            let output = Command::new("/bin/sh")
                .args(["-lc", LAUNCH_SCRIPT, "tether-launch", selected, "pwd -P"])
                .output()
                .unwrap();
            assert!(output.status.success(), "{selected}: {output:?}");
            let actual = String::from_utf8(output.stdout).unwrap();
            assert_eq!(fs::canonicalize(actual.trim()).unwrap(), expected);
        }

        let comparison = Command::new("/bin/sh")
            .args(["-c", SAME_DIRECTORY_SCRIPT, "tether-cwd-check", &home, "~/"])
            .status()
            .unwrap();
        assert!(comparison.success());
    }
    #[test]
    fn exact_inspection_distinguishes_running_and_dead_panes_with_identity() {
        let id = "tether-0197f198000070008000000000000001"
            .parse()
            .unwrap();

        assert_eq!(
            classify_exact_inspect(
                &id,
                Some(0),
                b"tether-0197f198000070008000000000000001:$7:2:0:\n",
                false,
            ),
            WorkloadState::Running {
                attached: 2,
                identity: "$7".parse().unwrap(),
            }
        );
        assert_eq!(
            classify_exact_inspect(
                &id,
                Some(0),
                b"tether-0197f198000070008000000000000001:$7:0:1:130\n",
                false,
            ),
            WorkloadState::Ended {
                identity: "$7".parse().unwrap(),
                exit_status: Some(130),
            }
        );
    }
}
