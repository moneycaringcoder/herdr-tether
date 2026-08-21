use std::{io, path::PathBuf, process::Output, sync::atomic::AtomicBool, time::Duration};

use anyhow::{Context, Result, bail};
use thiserror::Error;

use crate::{
    backend::{
        CommandSpec, CreateOutcomeUncertain, DurableBackend, LaunchSpec, ProcessBinaries,
        WorkloadState,
    },
    model::{ExternalSessionName, OwnershipProof, SessionId, TmuxSessionId},
    quote::posix_quote,
    sshcfg::{OpenSshTarget, openssh_connection_args, openssh_target},
    status::{BoundedOutput, run_bounded},
};

const LAUNCH_SCRIPT: &str = "directory=$1; case \"$directory\" in '~') directory=$HOME ;; '~/'*) directory=$HOME${directory#\\~} ;; esac; cd -- \"$directory\" && exec /bin/sh -c \"$2\"";
const SAME_DIRECTORY_SCRIPT: &str = "actual=$1; expected=$2; case \"$actual\" in '~') actual=$HOME ;; '~/'*) actual=$HOME${actual#\\~} ;; esac; case \"$expected\" in '~') expected=$HOME ;; '~/'*) expected=$HOME${expected#\\~} ;; esac; [ \"$actual\" -ef \"$expected\" ]";
/// Enters the workload's directory, then runs the health command.
///
/// The same directory handling as [`LAUNCH_SCRIPT`], so a probe sees what the
/// workload sees, but it runs the command rather than replacing the shell with
/// it: the exit status is the answer, so the shell has to survive to report it.
///
/// A directory it cannot enter exits `126`, which is the conventional "found but
/// could not execute". That keeps "I could not run the probe" apart from "the
/// probe says no", which a plain shell failure would have merged.
const HEALTH_SCRIPT: &str = "directory=$1; case \"$directory\" in '~') directory=$HOME ;; '~/'*) directory=$HOME${directory#\\~} ;; esac; cd -- \"$directory\" || exit 126; /bin/sh -c \"$2\"";
/// Exit statuses that mean the health command itself could not run.
pub(crate) const HEALTH_UNRUNNABLE: [i32; 2] = [126, 127];
const TMUX_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// Separates the two process samples in one invocation's output.
pub(crate) const PROCESS_SAMPLE_SEPARATOR: &str = "tether-sample";

/// How long to wait between the two processor-time samples.
///
/// Long enough that a busy process accumulates measurable time, short enough to
/// sit inside the phase budget with both samples.
pub(crate) const PROCESS_SAMPLE_SECONDS: u64 = 1;

const TMUX_CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const OWNERSHIP_GUARD_REJECTED: &str = "TETHER_OWNERSHIP_GUARD_REJECTED";

#[derive(Debug, Error)]
enum BoundedExecutionError {
    #[error(
        "start `{program}` ({kind:?}); install the tool or make it executable (Tether also searches /usr/bin, /bin, /opt/homebrew/bin, and /usr/local/bin)"
    )]
    Spawn {
        program: PathBuf,
        kind: io::ErrorKind,
    },
    #[error("process timed out after {} seconds", TMUX_COMMAND_TIMEOUT.as_secs())]
    TimedOut,
    #[error("process response exceeded the safe capture limit")]
    OutputLimit,
    #[error("process failed while reading its output")]
    Output,
}

impl BoundedExecutionError {
    fn outcome_uncertain(&self) -> bool {
        !matches!(self, Self::Spawn { .. })
    }
}

/// Plain terminal text captured from the single pane of an exact owned session.
///
/// Terminal control bytes are intentionally preserved as text so the renderer
/// can apply its own sanitization policy at the presentation boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedPaneCapture {
    text: String,
}

impl OwnedPaneCapture {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn into_text(self) -> String {
        self.text
    }
}

/// A bounded failure returned while capturing an exact owned tmux pane.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum OwnedCaptureError {
    #[error("exact owned tmux identity changed before capture")]
    IdentityChanged,
    #[error("tmux capture timed out")]
    TimedOut,
    #[error("tmux capture exceeded the safe output limit")]
    OutputLimit,
    #[error("tmux capture failed: {0}")]
    Transport(String),
}

#[derive(Clone, Debug)]
enum Location {
    Local,
    Remote(OpenSshTarget),
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
        let target = openssh_target(&target.into())?;
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
                let mut ssh_arguments = openssh_connection_args(interactive);
                if let Some(port) = target.port {
                    ssh_arguments.extend(["-p".to_owned(), port.to_string()]);
                }
                ssh_arguments.extend(["--".to_owned(), target.destination.clone(), remote_command]);
                Ok(CommandSpec::new(self.binaries.ssh(), ssh_arguments))
            }
        }
    }

    fn shell_spec(&self, arguments: Vec<String>) -> Result<CommandSpec> {
        match &self.location {
            Location::Local => Ok(CommandSpec::new("/bin/sh", arguments)),
            Location::Remote(target) => {
                let remote_command = remote_command("/bin/sh", &arguments)?;
                let mut ssh_arguments = openssh_connection_args(false);
                if let Some(port) = target.port {
                    ssh_arguments.extend(["-p".to_owned(), port.to_string()]);
                }
                ssh_arguments.extend(["--".to_owned(), target.destination.clone(), remote_command]);
                Ok(CommandSpec::new(self.binaries.ssh(), ssh_arguments))
            }
        }
    }

    /// A shell command that runs in a workload's directory on its host.
    ///
    /// Used for a configured health command, which asks whether the workload is
    /// serving. It deliberately does not enter the workload's own pane: a probe
    /// must not be able to disturb the work it is asking about, and there is no
    /// ownership-guarded way to run something in an existing pane.
    pub(crate) fn directory_shell_spec(
        &self,
        directory: &str,
        command: &str,
    ) -> Result<CommandSpec> {
        self.shell_spec(vec![
            "-lc".to_owned(),
            HEALTH_SCRIPT.to_owned(),
            "tether-health".to_owned(),
            directory.to_owned(),
            command.to_owned(),
        ])
    }

    fn bounded_output(&self, spec: &CommandSpec) -> Result<Output, BoundedExecutionError> {
        self.bounded_output_with_timeout(spec, TMUX_COMMAND_TIMEOUT)
    }

    fn bounded_output_with_timeout(
        &self,
        spec: &CommandSpec,
        timeout: Duration,
    ) -> Result<Output, BoundedExecutionError> {
        match run_bounded(spec, timeout, &AtomicBool::new(false)) {
            BoundedOutput::Completed {
                status,
                stdout,
                stdout_truncated: false,
                stderr,
                stderr_truncated: false,
            } => Ok(Output {
                status,
                stdout,
                stderr,
            }),
            BoundedOutput::Completed { .. } => Err(BoundedExecutionError::OutputLimit),
            BoundedOutput::TimedOut => Err(BoundedExecutionError::TimedOut),
            BoundedOutput::SpawnError(kind) => Err(BoundedExecutionError::Spawn {
                program: spec.program.clone(),
                kind,
            }),
            BoundedOutput::Error => Err(BoundedExecutionError::Output),
            BoundedOutput::Cancelled => unreachable!("direct tmux executions are not cancelled"),
        }
    }

    fn output(&self, spec: &CommandSpec) -> Result<Output> {
        self.bounded_output(spec).map_err(Into::into)
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

    pub(crate) fn close_exact_spec(
        &self,
        id: &SessionId,
        ownership_proof: &OwnershipProof,
        identity: TmuxSessionId,
    ) -> Result<CommandSpec> {
        self.guarded_spec(
            id,
            ownership_proof,
            identity,
            format!("kill-session -t {identity}"),
            false,
        )
    }

    pub(crate) fn capture_exact_spec(
        &self,
        id: &SessionId,
        ownership_proof: &OwnershipProof,
        identity: TmuxSessionId,
    ) -> Result<CommandSpec> {
        self.guarded_spec(
            id,
            ownership_proof,
            identity,
            format!("capture-pane -p -J -S -200 -t {identity}"),
            false,
        )
    }

    /// Captures recent joined text from the sole workload pane after checking
    /// the stored tmux identity, public session name, and ownership proof.
    pub fn capture_owned(
        &self,
        id: &SessionId,
        ownership_proof: &OwnershipProof,
        identity: TmuxSessionId,
    ) -> Result<OwnedPaneCapture, OwnedCaptureError> {
        let spec = self
            .capture_exact_spec(id, ownership_proof, identity)
            .map_err(|error| {
                OwnedCaptureError::Transport(sanitize_tmux_detail(
                    format!("{error:#}").as_bytes(),
                    Some(ownership_proof),
                ))
            })?;
        let output = self
            .bounded_output_with_timeout(&spec, TMUX_CAPTURE_TIMEOUT)
            .map_err(|error| match error {
                BoundedExecutionError::TimedOut => OwnedCaptureError::TimedOut,
                BoundedExecutionError::OutputLimit => OwnedCaptureError::OutputLimit,
                error => OwnedCaptureError::Transport(sanitize_tmux_detail(
                    format!("{error:#}").as_bytes(),
                    Some(ownership_proof),
                )),
            })?;
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        if !output.status.success()
            && text.trim_end_matches(['\r', '\n']) == OWNERSHIP_GUARD_REJECTED
        {
            return Err(OwnedCaptureError::IdentityChanged);
        }
        if !output.status.success() {
            let detail = sanitize_tmux_detail(&output.stderr, Some(ownership_proof));
            let message = if detail.is_empty() {
                format!("process exited with status {}", output.status)
            } else {
                format!("process exited with status {}: {detail}", output.status)
            };
            return Err(OwnedCaptureError::Transport(message));
        }
        Ok(OwnedPaneCapture { text })
    }

    fn guarded_spec(
        &self,
        id: &SessionId,
        ownership_proof: &OwnershipProof,
        identity: TmuxSessionId,
        guarded_command: String,
        interactive: bool,
    ) -> Result<CommandSpec> {
        let condition = format!(
            "#{{&&:#{{==:#{{session_id}},{identity}}},#{{&&:#{{==:#{{session_name}},{id}}},#{{==:#{{TETHER_OWNERSHIP_PROOF}},{ownership_proof}}}}}}}"
        );
        self.tmux_spec(
            vec![
                "if-shell".to_owned(),
                "-t".to_owned(),
                identity.to_string(),
                "-F".to_owned(),
                condition,
                guarded_command,
                format!("display-message -p {OWNERSHIP_GUARD_REJECTED} ; run-shell 'exit 75'"),
            ],
            interactive,
        )
    }
    pub(crate) fn classify_exact_inspect(
        &self,
        id: &SessionId,
        ownership_proof: &OwnershipProof,
        output: &Output,
        stdout_truncated: bool,
    ) -> WorkloadState {
        classify_exact_inspect(
            id,
            ownership_proof,
            output.status.code(),
            &output.stdout,
            stdout_truncated,
        )
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
                "#{session_name}:#{session_attached}".to_owned(),
            ],
            false,
        )
    }

    /// Asks which process each pane on this host was started for.
    ///
    /// A workload is a session, and Tether stores its session identity rather
    /// than a process id, so this is what connects an owned workload to the
    /// processes running under it. One call covers every session, which is why a
    /// host's cost does not grow with the number of workloads on it.
    pub(crate) fn pane_pids_spec(&self) -> Result<CommandSpec> {
        self.tmux_spec(
            vec![
                "list-panes".to_owned(),
                "-a".to_owned(),
                "-F".to_owned(),
                "#{session_name}:#{pane_pid}".to_owned(),
            ],
            false,
        )
    }

    /// Asks the host what its processes are using, twice.
    ///
    /// `ps` reports `%CPU` as an average over a process's whole life, which is
    /// the wrong question: a workload that has been up for days and starts
    /// eating a core would round to nothing, while one that finished a burst a
    /// minute ago would still look busy. So this takes two samples of cumulative
    /// processor time either side of a short wait and lets the caller difference
    /// them, which is what "using right now" means.
    ///
    /// One invocation, so the two samples and the pane mapping cannot drift far
    /// apart, and `pid` reuse between them stays a microsecond window rather than
    /// a round trip. `pid`, `ppid`, `time` and `rss` are all POSIX `-o` format
    /// names.
    pub(crate) fn process_samples_spec(&self) -> Result<CommandSpec> {
        self.shell_spec(vec![
            "-c".to_owned(),
            format!(
                "ps -Ao pid=,ppid=,time=,rss=; echo '{PROCESS_SAMPLE_SEPARATOR}'; sleep {}; ps -Ao pid=,time=",
                PROCESS_SAMPLE_SECONDS
            ),
        ])
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

        let detail = sanitize_tmux_detail(&output.stderr, None);
        if detail.is_empty() {
            bail!("tmux {operation} failed with status {}", output.status);
        }
        bail!(
            "tmux {operation} failed with status {}: {detail}",
            output.status
        )
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
                let detail = sanitize_tmux_detail(&output.stderr, None);
                bail!(
                    "compare created pane cwd with selected directory failed with status {}: {detail}",
                    output.status
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
            let detail = sanitize_tmux_detail(&output.stderr, None);
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
        ownership_proof: &OwnershipProof,
        internal_target: TmuxSessionId,
        cause: anyhow::Error,
    ) -> anyhow::Error {
        let cause = sanitize_tmux_error(cause, ownership_proof);
        let rollback = self.close(id, ownership_proof, internal_target);
        match rollback {
            Ok(()) => cause,
            Err(rollback) => {
                let rollback = sanitize_tmux_error(rollback, ownership_proof);
                CreateOutcomeUncertain::new(cause.context(format!(
                    "also failed to roll back exact owned session `{id}`: {rollback:#}"
                )))
                .into()
            }
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
        let output = match self.bounded_output(&spec) {
            Ok(output) => output,
            Err(error) if error.outcome_uncertain() => {
                return Err(CreateOutcomeUncertain::new(error.into()).into());
            }
            Err(error) => return Err(error.into()),
        };
        if !output.status.success() {
            let detail = sanitize_tmux_detail(&output.stderr, Some(&launch.ownership_proof));
            bail!("tmux create failed with status {}: {detail}", output.status);
        }
        let created = std::str::from_utf8(&output.stdout)
            .context("tmux returned non-UTF-8 created session identities")
            .map_err(CreateOutcomeUncertain::new)?
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
            // Creation is committed, but malformed output does not provide a
            // trustworthy internal target. Preserve the reservation so a
            // proof-based retry can reconcile it.
            Err(error) => return Err(CreateOutcomeUncertain::new(error).into()),
        };
        self.enable_remain_on_exit(&pane_target)
            .and_then(|()| self.verify_created_cwd(&pane_target, &launch.directory))
            .and_then(|()| self.enable_mouse_for_target(session_target.to_string()))
            .map_err(|error| {
                self.rollback_created(&launch.id, &launch.ownership_proof, session_target, error)
            })?;
        Ok(session_target)
    }

    fn inspect(&self, id: &SessionId, ownership_proof: &OwnershipProof) -> Result<WorkloadState> {
        let spec = self.inspect_exact_spec(id, ownership_proof)?;
        let output = self.output(&spec)?;
        Ok(self.classify_exact_inspect(id, ownership_proof, &output, false))
    }

    fn attach_command(
        &self,
        id: &SessionId,
        ownership_proof: &OwnershipProof,
        identity: TmuxSessionId,
    ) -> Result<CommandSpec> {
        self.guarded_spec(
            id,
            ownership_proof,
            identity,
            format!("set-option -t {identity} mouse on ; attach-session -t {identity}"),
            true,
        )
    }

    fn close(
        &self,
        id: &SessionId,
        ownership_proof: &OwnershipProof,
        identity: TmuxSessionId,
    ) -> Result<()> {
        let spec = self.close_exact_spec(id, ownership_proof, identity)?;
        let output = self.output(&spec)?;
        if output.status.success()
            && !String::from_utf8_lossy(&output.stdout).contains(OWNERSHIP_GUARD_REJECTED)
        {
            return Ok(());
        }
        bail!("exact owned tmux identity changed before close")
    }
}

fn sanitize_tmux_error(error: anyhow::Error, ownership_proof: &OwnershipProof) -> anyhow::Error {
    anyhow::anyhow!(sanitize_tmux_detail(
        format!("{error:#}").as_bytes(),
        Some(ownership_proof),
    ))
}

fn sanitize_tmux_detail(stderr: &[u8], ownership_proof: Option<&OwnershipProof>) -> String {
    const MAX_CHARS: usize = 240;
    let mut detail = String::from_utf8_lossy(stderr).into_owned();
    if let Some(ownership_proof) = ownership_proof {
        let ownership_proof = ownership_proof.to_string();
        detail = detail.replace(
            &format!("TETHER_OWNERSHIP_PROOF={ownership_proof}"),
            "[redacted ownership proof]",
        );
        detail = detail.replace(&ownership_proof, "[redacted ownership proof]");
    }
    detail = detail.replace("TETHER_OWNERSHIP_PROOF", "[redacted ownership proof]");

    let mut sanitized = String::with_capacity(detail.len().min(MAX_CHARS));
    let mut characters = detail.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            match characters.next() {
                Some('[') => {
                    for sequence_character in characters.by_ref() {
                        if ('@'..='~').contains(&sequence_character) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(sequence_character) = characters.next() {
                        if sequence_character == '\u{7}' {
                            break;
                        }
                        if sequence_character == '\u{1b}' {
                            let _ = characters.next_if_eq(&'\\');
                            break;
                        }
                    }
                }
                Some(_) | None => {}
            }
            continue;
        }
        sanitized.push(if character.is_control() {
            ' '
        } else {
            character
        });
    }

    let normalized = sanitized.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut characters = normalized.chars();
    let mut bounded = characters.by_ref().take(MAX_CHARS).collect::<String>();
    if characters.next().is_some() {
        bounded.push('…');
    }
    bounded
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
        let [
            name,
            identity,
            attached,
            pane_dead,
            exit_status,
            observed_proof,
        ] = fields.as_slice()
        else {
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
    use std::{env, fs, process::Command};

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
    fn a_health_probe_runs_in_the_workload_directory_without_entering_its_pane() {
        let home = env::var("HOME").unwrap();
        let expected = fs::canonicalize(&home).unwrap();

        // `~` resolves the same way a launch does, so a probe sees the directory
        // the workload sees.
        let output = Command::new("/bin/sh")
            .args(["-lc", HEALTH_SCRIPT, "tether-health", "~", "pwd -P"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let actual = String::from_utf8(output.stdout).unwrap();
        assert_eq!(fs::canonicalize(actual.trim()).unwrap(), expected);

        // The exit status is the answer, so the wrapper reports it rather than
        // replacing itself with the command and losing it.
        let refused = Command::new("/bin/sh")
            .args(["-lc", HEALTH_SCRIPT, "tether-health", "/", "exit 9"])
            .status()
            .unwrap();
        assert_eq!(refused.code(), Some(9));

        // A directory it cannot enter is reported as unrunnable, not as a
        // verdict about the workload.
        let unreachable = Command::new("/bin/sh")
            .args([
                "-lc",
                HEALTH_SCRIPT,
                "tether-health",
                "/definitely/not/a/directory",
                "exit 0",
            ])
            .status()
            .unwrap();
        assert!(
            unreachable
                .code()
                .is_some_and(|code| HEALTH_UNRUNNABLE.contains(&code)),
            "{unreachable:?}"
        );

        // A remote probe travels as one quoted remote command, and never as a
        // `tmux` pane operation.
        let backend = TmuxBackend::remote(
            "builder@example.test".to_owned(),
            ProcessBinaries::new("ssh", "tmux"),
        )
        .unwrap();
        let spec = backend
            .directory_shell_spec("/srv/app", "curl -fsS localhost:8080/healthz")
            .unwrap();
        assert!(
            spec.program.ends_with("ssh"),
            "a remote probe travels over ssh: {:?}",
            spec.program
        );
        let remote = spec.args.last().unwrap();
        assert!(remote.contains("/srv/app"), "{remote}");
        assert!(remote.contains("localhost:8080/healthz"), "{remote}");
        assert!(!remote.contains("tmux"), "{remote}");
    }

    #[cfg(unix)]
    #[test]
    fn the_process_sample_command_runs_on_this_host_and_finds_this_process() {
        // The command's exact text is the whole feature: a typo or a column name
        // one supported platform does not accept degrades silently to "usage
        // unknown", so it is run here rather than only matched by a fixture.
        let spec = TmuxBackend::local(ProcessBinaries::new("ssh", "tmux"))
            .process_samples_spec()
            .unwrap();
        let output = Command::new(&spec.program)
            .args(&spec.args)
            .output()
            .expect("the sampling command runs");
        assert!(
            output.status.success(),
            "ps rejected the requested columns: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        let (first, second) = text
            .split_once(PROCESS_SAMPLE_SEPARATOR)
            .expect("both samples are separated");
        let own = std::process::id();
        // This process is in both samples, with a parent and a resident size, so
        // every column the parser reads is present in the order it expects.
        let row = first
            .lines()
            .map(|line| line.split_whitespace().collect::<Vec<_>>())
            .find(|fields| fields.first() == Some(&own.to_string().as_str()))
            .expect("this process appears in the first sample");
        assert_eq!(row.len(), 4, "{row:?}");
        assert!(row[1].parse::<u32>().is_ok(), "a parent pid: {row:?}");
        assert!(
            row[3].parse::<u64>().is_ok_and(|rss| rss > 0),
            "a resident size in kibibytes: {row:?}"
        );
        assert!(
            second
                .lines()
                .any(|line| line.split_whitespace().next() == Some(own.to_string().as_str())),
            "this process appears in the second sample too"
        );
    }

    #[test]
    fn a_remote_sample_travels_as_one_quoted_command() {
        let spec = TmuxBackend::remote(
            "builder@example.test".to_owned(),
            ProcessBinaries::new("ssh", "tmux"),
        )
        .unwrap()
        .process_samples_spec()
        .unwrap();
        assert!(spec.program.ends_with("ssh"), "{:?}", spec.program);
        let remote = spec.args.last().unwrap();
        assert!(remote.contains("ps -Ao"), "{remote}");
        assert!(remote.contains(PROCESS_SAMPLE_SEPARATOR), "{remote}");
        assert!(!remote.contains("tmux"), "{remote}");
    }
    #[test]
    fn exact_inspection_distinguishes_running_and_dead_panes_with_identity() {
        let id = "tether-0197f198000070008000000000000001".parse().unwrap();
        let proof = "0197f198000070008000000000000002".parse().unwrap();

        assert_eq!(
            classify_exact_inspect(
                &id,
                &proof,
                Some(0),
                b"tether-0197f198000070008000000000000001:$7:2:0::0197f198000070008000000000000002\n",
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
                &proof,
                Some(0),
                b"tether-0197f198000070008000000000000001:$7:0:1:130:0197f198000070008000000000000002\n",
                false,
            ),
            WorkloadState::Ended {
                identity: "$7".parse().unwrap(),
                exit_status: Some(130),
            }
        );
    }
}
