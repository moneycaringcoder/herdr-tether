use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Output,
    sync::atomic::AtomicBool,
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::{
    backend::CommandSpec,
    model::Placement,
    quote::posix_quote,
    status::{BoundedOutput, run_bounded},
};

pub const PLUGIN_ID: &str = "moneycaringcoder.tether";
const HERDR_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// Herdr process and placement context supplied to a plugin action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HerdrContext {
    pub binary: PathBuf,
    pub pane_id: String,
    pub workspace_id: String,
}

impl HerdrContext {
    /// Loads the subset of Herdr's plugin environment needed to place panes.
    pub fn from_env() -> Result<Self> {
        let pane_id = required_string_env("HERDR_PANE_ID", Some("PANE_ID"))?;
        let plugin_context = match env::var("HERDR_PLUGIN_CONTEXT_JSON") {
            Ok(value) => Some(value),
            Err(env::VarError::NotPresent) => None,
            Err(env::VarError::NotUnicode(_)) => {
                bail!("Herdr provided a non-UTF-8 HERDR_PLUGIN_CONTEXT_JSON")
            }
        };
        Ok(Self {
            binary: required_os_env("HERDR_BIN_PATH")?.into(),
            pane_id: placement_pane_id(pane_id, plugin_context.as_deref())?,
            workspace_id: required_string_env("HERDR_WORKSPACE_ID", Some("WORKSPACE_ID"))?,
        })
    }
}

fn placement_pane_id(pane_id: String, plugin_context: Option<&str>) -> Result<String> {
    let Some(plugin_context) = plugin_context else {
        return Ok(pane_id);
    };
    let context: Value =
        serde_json::from_str(plugin_context).context("decode HERDR_PLUGIN_CONTEXT_JSON")?;
    let Some(invoking_pane_id) = context.get("focused_pane_id").and_then(Value::as_str) else {
        bail!("Herdr plugin context did not report the invoking pane")
    };
    require_nonempty_env(
        "HERDR_PLUGIN_CONTEXT_JSON focused_pane_id",
        invoking_pane_id.to_owned(),
    )
}

fn required_os_env(name: &str) -> Result<OsString> {
    let value = env::var_os(name).ok_or_else(|| anyhow::anyhow!("Herdr did not provide {name}"))?;
    if value.to_string_lossy().trim().is_empty() {
        bail!("Herdr provided an empty {name}");
    }
    Ok(value)
}

fn required_string_env(name: &str, fallback: Option<&str>) -> Result<String> {
    match env::var(name) {
        Ok(value) => require_nonempty_env(name, value),
        Err(env::VarError::NotPresent) => {
            let fallback =
                fallback.ok_or_else(|| anyhow::anyhow!("Herdr did not provide {name}"))?;
            let value = env::var(fallback)
                .with_context(|| format!("Herdr did not provide a valid UTF-8 {name}"))?;
            require_nonempty_env(fallback, value)
        }
        Err(env::VarError::NotUnicode(_)) => {
            bail!("Herdr did not provide a valid UTF-8 {name}")
        }
    }
}

fn require_nonempty_env(name: &str, value: String) -> Result<String> {
    if value.trim().is_empty() {
        bail!("Herdr provided an empty {name}");
    }
    Ok(value)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlacedPane {
    pub pane_id: String,
    pub warning: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForegroundProcess {
    pub pid: u64,
    pub name: String,
    argv: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplacementInspection {
    pub pane_id: String,
    pub foreground_processes: Vec<ForegroundProcess>,
}

impl ReplacementInspection {
    pub fn requires_confirmation(&self) -> bool {
        !self.foreground_processes.is_empty()
    }

    pub fn safe_summary(&self) -> String {
        self.foreground_processes
            .iter()
            .take(3)
            .map(|process| {
                let name = process
                    .name
                    .chars()
                    .map(|character| {
                        if character.is_control() {
                            ' '
                        } else {
                            character
                        }
                    })
                    .collect::<String>();
                format!(
                    "{} (PID {})",
                    name.split_whitespace().collect::<Vec<_>>().join(" "),
                    process.pid
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Clone, Debug)]
pub struct HerdrClient {
    context: HerdrContext,
}

impl HerdrClient {
    pub fn new(context: HerdrContext) -> Self {
        Self { context }
    }

    /// Creates the requested transient pane and runs `command` in that exact pane.
    pub fn place(&self, command: &CommandSpec, placement: Placement) -> Result<PlacedPane> {
        let pane_id = match placement {
            Placement::SplitRight => self.split("right")?,
            Placement::SplitDown => self.split("down")?,
            Placement::NewTab => self.create_tab()?,
            Placement::ReplaceCurrentPane => {
                bail!("replace-current placement requires explicit replacement confirmation")
            }
        };
        // Presentation metadata must never block a working attach.
        let _ = self.label_pane(&pane_id);
        match self.run_in_pane(command, pane_id.clone()) {
            Ok(placed) => Ok(placed),
            Err(error) => {
                let detail = format!("{error:#}");
                match self.close_pane(&pane_id) {
                    Ok(()) => Err(error).context(format!(
                        "placement failed ({detail}); empty destination `{pane_id}` was removed"
                    )),
                    Err(cleanup_error) => Err(error).context(format!(
                        "placement failed ({detail}) and empty destination `{pane_id}` could not be removed: {cleanup_error:#}"
                    )),
                }
            }
        }
    }

    /// Reports foreground processes that replacement would terminate.
    pub fn inspect_replacement_source(&self) -> Result<ReplacementInspection> {
        self.process_info(&self.context.pane_id)
            .context("inspect the pane that Replace current pane would close")
    }

    /// Creates and verifies a destination before closing the exact invoking pane.
    pub fn replace_current(&self, command: &CommandSpec) -> Result<PlacedPane> {
        const VERIFY_ATTEMPTS: usize = 20;
        const VERIFY_INTERVAL: Duration = Duration::from_millis(50);

        let source_pane_id = self.context.pane_id.clone();
        let source_identity = self
            .process_info(&source_pane_id)
            .context("capture the exact source pane occupant before replacement")?;
        let destination_pane_id = self.split("right")?;
        // Presentation metadata must never block a working replacement.
        let _ = self.label_pane(&destination_pane_id);
        let placed = match self.run_in_pane(command, destination_pane_id.clone()) {
            Ok(placed) => placed,
            Err(error) => {
                let cleanup = self.close_pane(&destination_pane_id);
                return Err(error).context(replacement_failure_context(
                    &source_pane_id,
                    &destination_pane_id,
                    cleanup.as_ref().err(),
                ));
            }
        };

        let mut last_verification_error = None;
        let mut destination_ready = false;
        for attempt in 0..VERIFY_ATTEMPTS {
            match self.process_info(&destination_pane_id) {
                Ok(destination)
                    if destination
                        .foreground_processes
                        .iter()
                        .any(|process| Self::is_attach_process(process, command)) =>
                {
                    destination_ready = true;
                    break;
                }
                Ok(_) => {
                    last_verification_error =
                        Some("no verified tmux attach process was reported".to_owned());
                }
                Err(error) => {
                    last_verification_error = Some(format!("{error:#}"));
                }
            }
            if attempt + 1 < VERIFY_ATTEMPTS {
                thread::sleep(VERIFY_INTERVAL);
            }
        }
        if !destination_ready {
            let cleanup = self.close_pane(&destination_pane_id);
            let verification = last_verification_error
                .unwrap_or_else(|| "destination verification did not complete".to_owned());
            bail!(
                "{}; verification failed: {verification}",
                replacement_failure_context(
                    &source_pane_id,
                    &destination_pane_id,
                    cleanup.as_ref().err()
                )
            );
        }

        let warning = match self.process_info(&source_pane_id) {
            Ok(current) if current == source_identity => {
                self.close_pane(&source_pane_id).err().map(|error| {
                    format!(
                        "replacement destination `{destination_pane_id}` is running, but source pane `{source_pane_id}` could not be closed: {error:#}"
                    )
                })
            }
            Ok(_) => Some(format!(
                "replacement destination `{destination_pane_id}` is running, but source pane `{source_pane_id}` changed during replacement and was preserved"
            )),
            Err(error) => Some(format!(
                "replacement destination `{destination_pane_id}` is running, but source pane `{source_pane_id}` could not be reverified and was preserved: {error:#}"
            )),
        };
        Ok(PlacedPane {
            pane_id: placed.pane_id,
            warning,
        })
    }

    fn run_in_pane(&self, command: &CommandSpec, pane_id: String) -> Result<PlacedPane> {
        let command_line = placed_command(command)?.posix_command_line()?;
        let arguments = [
            "pane".to_owned(),
            "run".to_owned(),
            pane_id.clone(),
            command_line,
        ];
        let output = self.execute("run command in pane", &arguments)?;
        if output.stdout.is_empty() {
            require_empty_success(&output, "run command in pane")?;
        } else {
            let response = decode_response(output, "run command in pane")?;
            require_result_type(&response, "pane_ran")?;
            let ran_pane = result_string(&response, &["pane_id"], "pane run response pane_id")?;
            if ran_pane != pane_id {
                bail!(
                    "Herdr ran the command in pane `{ran_pane}`, not newly created pane `{pane_id}`"
                );
            }
        }
        Ok(PlacedPane {
            pane_id,
            warning: None,
        })
    }

    /// Opens one of this plugin's manifest-declared managed overlay panes.
    pub fn open_plugin_pane(&self, entrypoint: &str) -> Result<()> {
        if entrypoint.trim().is_empty() {
            bail!("plugin pane entrypoint must not be empty");
        }
        let response = self.invoke(
            "open plugin pane",
            &[
                "plugin".to_owned(),
                "pane".to_owned(),
                "open".to_owned(),
                "--plugin".to_owned(),
                PLUGIN_ID.to_owned(),
                "--entrypoint".to_owned(),
                entrypoint.to_owned(),
                "--placement".to_owned(),
                "overlay".to_owned(),
            ],
        )?;
        require_result_type(&response, "plugin_pane_opened")
    }

    fn split(&self, direction: &str) -> Result<String> {
        let response = self.invoke(
            "split pane",
            &[
                "pane".to_owned(),
                "split".to_owned(),
                "--pane".to_owned(),
                self.context.pane_id.clone(),
                "--direction".to_owned(),
                direction.to_owned(),
                "--focus".to_owned(),
            ],
        )?;
        require_result_type(&response, "pane_info")?;
        result_string(&response, &["pane", "pane_id"], "split response pane_id")
    }

    fn process_info(&self, pane_id: &str) -> Result<ReplacementInspection> {
        let response = self.invoke(
            "inspect pane process",
            &[
                "pane".to_owned(),
                "process-info".to_owned(),
                "--pane".to_owned(),
                pane_id.to_owned(),
            ],
        )?;
        require_result_type(&response, "pane_process_info")?;
        let process_info = response
            .get("result")
            .and_then(|result| result.get("process_info"))
            .ok_or_else(|| {
                anyhow::anyhow!("Herdr process response did not contain process_info")
            })?;
        let reported_pane_id = process_info
            .get("pane_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Herdr process response did not contain pane_id"))?;
        if reported_pane_id != pane_id {
            bail!("Herdr inspected pane `{reported_pane_id}`, not requested pane `{pane_id}`");
        }
        let foreground_processes = process_info
            .get("foreground_processes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|process| {
                let pid = process.get("pid").and_then(Value::as_u64).ok_or_else(|| {
                    anyhow::anyhow!("Herdr foreground process did not contain pid")
                })?;
                let name = process
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!("Herdr foreground process did not contain name")
                    })?;
                let argv = process
                    .get("argv")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                Ok(ForegroundProcess {
                    pid,
                    name: name.to_owned(),
                    argv,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(ReplacementInspection {
            pane_id: pane_id.to_owned(),
            foreground_processes,
        })
    }

    fn is_attach_process(process: &ForegroundProcess, expected: &CommandSpec) -> bool {
        let Some(program) = process
            .argv
            .first()
            .and_then(|value| Path::new(value).file_name())
        else {
            return false;
        };
        let expected_program = expected.program.file_name();
        if Some(program) == expected_program && process.argv[1..] == expected.args {
            return true;
        }

        let target = match expected.args.as_slice() {
            [session, attach, target_flag, _, separator, name]
                if session == "session"
                    && attach == "attach-external"
                    && target_flag == "--target"
                    && separator == "--" =>
            {
                format!("={name}")
            }
            _ => return false,
        };
        if program == "tmux" {
            return process.argv[1..] == ["attach-session", "-t", target.as_str()];
        }
        if program != "ssh" {
            return false;
        }
        let Ok(target) = posix_quote(&target) else {
            return false;
        };
        process
            .argv
            .last()
            .is_some_and(|argument| argument == &format!("'tmux' 'attach-session' '-t' {target}"))
    }

    fn label_pane(&self, pane_id: &str) -> Result<()> {
        let response = self.invoke(
            "label Tether pane",
            &[
                "pane".to_owned(),
                "rename".to_owned(),
                pane_id.to_owned(),
                "Tether session".to_owned(),
            ],
        )?;
        require_result_type(&response, "pane_info")
    }

    fn close_pane(&self, pane_id: &str) -> Result<()> {
        let response = self.invoke(
            "close replaced pane",
            &["pane".to_owned(), "close".to_owned(), pane_id.to_owned()],
        )?;
        require_result_type(&response, "ok")
    }

    fn create_tab(&self) -> Result<String> {
        let response = self.invoke(
            "create tab",
            &[
                "tab".to_owned(),
                "create".to_owned(),
                "--workspace".to_owned(),
                self.context.workspace_id.clone(),
                "--focus".to_owned(),
            ],
        )?;
        require_result_type(&response, "tab_created")?;
        result_string(
            &response,
            &["root_pane", "pane_id"],
            "tab response root pane_id",
        )
    }

    fn invoke(&self, operation: &str, arguments: &[String]) -> Result<Value> {
        decode_response(self.execute(operation, arguments)?, operation)
    }

    fn execute(&self, operation: &str, arguments: &[String]) -> Result<Output> {
        let spec = CommandSpec::new(&self.context.binary, arguments.to_vec());
        match run_bounded(&spec, HERDR_COMMAND_TIMEOUT, &AtomicBool::new(false)) {
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
            BoundedOutput::Completed { .. } => {
                bail!("Herdr {operation} response exceeded the safe capture limit")
            }
            BoundedOutput::TimedOut => bail!(
                "Herdr {operation} timed out after {} seconds",
                HERDR_COMMAND_TIMEOUT.as_secs()
            ),
            BoundedOutput::SpawnError(kind) => bail!(
                "run Herdr {operation} via `{}` ({kind:?})",
                self.context.binary.display()
            ),
            BoundedOutput::Error => bail!("read Herdr {operation} process output"),
            BoundedOutput::Cancelled => unreachable!("direct Herdr executions are not cancelled"),
        }
    }
}

fn replacement_failure_context(
    source_pane_id: &str,
    destination_pane_id: &str,
    cleanup_error: Option<&anyhow::Error>,
) -> String {
    match cleanup_error {
        Some(error) => format!(
            "replacement destination `{destination_pane_id}` failed and cleanup also failed ({error:#}); source pane `{source_pane_id}` was preserved"
        ),
        None => format!(
            "replacement destination `{destination_pane_id}` failed and was removed; source pane `{source_pane_id}` was preserved"
        ),
    }
}

fn decode_response(output: std::process::Output, operation: &str) -> Result<Value> {
    if output.stdout.is_empty() && !output.status.success() {
        require_empty_success(&output, operation)?;
        unreachable!("nonzero Herdr command accepted as success");
    }
    let envelope: Value = serde_json::from_slice(&output.stdout).with_context(|| {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if stderr.is_empty() {
            format!("decode Herdr {operation} response as JSON")
        } else {
            format!("decode Herdr {operation} response as JSON: {stderr}")
        }
    })?;

    if let Some(message) = envelope
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
    {
        bail!("Herdr {operation} failed: {message}");
    }
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        if detail.is_empty() {
            bail!("Herdr {operation} failed with status {}", output.status);
        }
        bail!(
            "Herdr {operation} failed with status {}: {detail}",
            output.status
        );
    }
    if envelope.get("result").is_none() {
        bail!("Herdr {operation} response did not contain a result");
    }
    Ok(envelope)
}

fn require_empty_success(output: &std::process::Output, operation: &str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    if let Ok(envelope) = serde_json::from_slice::<Value>(&output.stderr)
        && let Some(message) = envelope
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
    {
        bail!("Herdr {operation} failed: {message}");
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    if detail.is_empty() {
        bail!("Herdr {operation} failed with status {}", output.status);
    }
    bail!(
        "Herdr {operation} failed with status {}: {detail}",
        output.status
    )
}

fn placed_command(command: &CommandSpec) -> Result<CommandSpec> {
    let mut plugin_paths = Vec::new();
    for name in ["HERDR_PLUGIN_CONFIG_DIR", "HERDR_PLUGIN_STATE_DIR"] {
        match env::var(name) {
            Ok(value) => plugin_paths.push((name, value)),
            Err(env::VarError::NotPresent) => {}
            Err(env::VarError::NotUnicode(_)) => {
                bail!("Herdr provided a non-UTF-8 {name}")
            }
        }
    }
    placed_command_with_paths(command, &plugin_paths)
}

fn placed_command_with_paths(
    command: &CommandSpec,
    plugin_paths: &[(&str, String)],
) -> Result<CommandSpec> {
    let program = command.program.to_str().with_context(|| {
        format!(
            "placed command program path `{}` is not valid UTF-8",
            command.program.display()
        )
    })?;
    let mut arguments = vec![
        "-u".to_owned(),
        "HERDR_BIN_PATH".to_owned(),
        "-u".to_owned(),
        "TMUX".to_owned(),
    ];
    arguments.extend(
        plugin_paths
            .iter()
            .map(|(name, value)| format!("{name}={value}")),
    );
    arguments.push(program.to_owned());
    arguments.extend(command.args.iter().cloned());
    Ok(CommandSpec::new("env", arguments))
}

fn require_result_type(envelope: &Value, expected: &str) -> Result<()> {
    let actual = envelope
        .get("result")
        .and_then(|result| result.get("type"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("Herdr response result did not contain a type"))?;
    if actual != expected {
        bail!("unexpected Herdr response type `{actual}`; expected `{expected}`");
    }
    Ok(())
}

fn result_string(envelope: &Value, path: &[&str], description: &str) -> Result<String> {
    let mut value = envelope
        .get("result")
        .ok_or_else(|| anyhow::anyhow!("Herdr response did not contain a result"))?;
    for component in path {
        value = value
            .get(*component)
            .ok_or_else(|| anyhow::anyhow!("Herdr response did not contain {description}"))?;
    }
    let value = value
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Herdr {description} was not a string"))?;
    if value.is_empty() {
        bail!("Herdr {description} was empty");
    }
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_pane_context_targets_the_invoking_pane() {
        let context = r#"{"focused_pane_id":"w1:p1","invocation_source":"api"}"#;

        assert_eq!(
            placement_pane_id("w1:pF".to_owned(), Some(context)).unwrap(),
            "w1:p1"
        );
        assert_eq!(
            placement_pane_id("w1:pF".to_owned(), Some("{}"))
                .unwrap_err()
                .to_string(),
            "Herdr plugin context did not report the invoking pane"
        );
        assert_eq!(
            placement_pane_id("w1:pF".to_owned(), Some(r#"{"focused_pane_id":null}"#))
                .unwrap_err()
                .to_string(),
            "Herdr plugin context did not report the invoking pane"
        );
        assert!(
            placement_pane_id("w1:pF".to_owned(), Some("{"))
                .unwrap_err()
                .to_string()
                .starts_with("decode HERDR_PLUGIN_CONTEXT_JSON")
        );
        assert_eq!(
            placement_pane_id("w1:pF".to_owned(), Some(r#"{"focused_pane_id":""}"#))
                .unwrap_err()
                .to_string(),
            "Herdr provided an empty HERDR_PLUGIN_CONTEXT_JSON focused_pane_id"
        );
        assert_eq!(
            placement_pane_id("w1:p2".to_owned(), None).unwrap(),
            "w1:p2"
        );
    }

    #[test]
    fn placed_command_forwards_exact_plugin_paths_without_recursive_placement() {
        let command = CommandSpec::new(
            "/plugin root/herdr-tether",
            vec![
                "session".to_owned(),
                "resume".to_owned(),
                "tether-id".to_owned(),
            ],
        );

        assert_eq!(
            placed_command_with_paths(
                &command,
                &[
                    (
                        "HERDR_PLUGIN_CONFIG_DIR",
                        "/config path/$literal".to_owned()
                    ),
                    (
                        "HERDR_PLUGIN_STATE_DIR",
                        "/state path/$(literal)".to_owned()
                    ),
                ],
            )
            .unwrap(),
            CommandSpec::new(
                "env",
                vec![
                    "-u".to_owned(),
                    "HERDR_BIN_PATH".to_owned(),
                    "-u".to_owned(),
                    "TMUX".to_owned(),
                    "HERDR_PLUGIN_CONFIG_DIR=/config path/$literal".to_owned(),
                    "HERDR_PLUGIN_STATE_DIR=/state path/$(literal)".to_owned(),
                    "/plugin root/herdr-tether".to_owned(),
                    "session".to_owned(),
                    "resume".to_owned(),
                    "tether-id".to_owned(),
                ],
            )
        );
    }

    #[test]
    fn replacement_readiness_requires_the_expected_attach_target() {
        let process = |name: &str, argv: &[&str]| ForegroundProcess {
            pid: 42,
            name: name.to_owned(),
            argv: argv.iter().map(|argument| (*argument).to_owned()).collect(),
        };
        let owned = CommandSpec::new(
            "/usr/bin/tmux",
            vec![
                "if-shell".to_owned(),
                "-t".to_owned(),
                "$7".to_owned(),
                "-F".to_owned(),
                "#{&&:proof-and-identity}".to_owned(),
                "set-option -t $7 mouse on ; attach-session -t $7".to_owned(),
                "display-message -p TETHER_OWNERSHIP_GUARD_REJECTED ; run-shell 'exit 75'"
                    .to_owned(),
            ],
        );
        let external = CommandSpec::new(
            "/plugin/herdr-tether",
            vec![
                "session".to_owned(),
                "attach-external".to_owned(),
                "--target".to_owned(),
                "builder".to_owned(),
                "--".to_owned(),
                "work box".to_owned(),
            ],
        );

        assert!(HerdrClient::is_attach_process(
            &process(
                "tmux: client",
                &[
                    "/usr/bin/tmux",
                    "if-shell",
                    "-t",
                    "$7",
                    "-F",
                    "#{&&:proof-and-identity}",
                    "set-option -t $7 mouse on ; attach-session -t $7",
                    "display-message -p TETHER_OWNERSHIP_GUARD_REJECTED ; run-shell 'exit 75'",
                ],
            ),
            &owned,
        ));
        assert!(HerdrClient::is_attach_process(
            &process(
                "ssh",
                &[
                    "/usr/bin/ssh",
                    "--",
                    "builder",
                    "'tmux' 'attach-session' '-t' '=work box'"
                ]
            ),
            &external
        ));
        assert!(!HerdrClient::is_attach_process(
            &process("sh", &["sh", "-c", "echo attach-session"]),
            &owned
        ));
        assert!(!HerdrClient::is_attach_process(
            &process(
                "tmux",
                &[
                    "tmux",
                    "if-shell",
                    "-t",
                    "$8",
                    "-F",
                    "#{&&:proof-and-identity}",
                    "set-option -t $8 mouse on ; attach-session -t $8",
                    "display-message -p TETHER_OWNERSHIP_GUARD_REJECTED ; run-shell 'exit 75'",
                ],
            ),
            &owned,
        ));
        assert!(!HerdrClient::is_attach_process(
            &process("tmux", &["tmux"]),
            &owned
        ));
    }
}
