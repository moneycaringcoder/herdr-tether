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
    agent_view::{AGENT_VIEW_SOURCE, GROUP_TOKEN},
    backend::CommandSpec,
    model::{OrchestrationGroupId, Placement},
    quote::posix_quote,
    status::{BoundedOutput, run_bounded},
};

pub const PLUGIN_ID: &str = "moneycaringcoder.tether";
const HERDR_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_INVOCATION_DIRECTORY_BYTES: usize = 4096;

/// A validated invocation directory supplied by Herdr's documented plugin context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvocationLocation {
    directory: PathBuf,
}

impl InvocationLocation {
    /// Reads an optional picker preference from Herdr's plugin action context.
    ///
    /// Invalid or unavailable context is deliberately ignored. Placement context
    /// is parsed separately and retains its stricter error behavior.
    pub fn from_plugin_env() -> Option<Self> {
        let context = env::var("HERDR_PLUGIN_CONTEXT_JSON").ok()?;
        Self::from_plugin_context_json(Some(&context))
    }

    /// Parses only Herdr's documented invocation CWD fields.
    ///
    /// Pane context takes precedence over workspace context. If the pane field is
    /// present but unusable, the workspace field is not used as a fallback.
    pub fn from_plugin_context_json(plugin_context: Option<&str>) -> Option<Self> {
        let context: Value = serde_json::from_str(plugin_context?).ok()?;
        let context = context.as_object()?;
        let directory = match context.get("focused_pane_cwd") {
            Some(value) => value.as_str()?,
            None => context.get("workspace_cwd")?.as_str()?,
        };
        if directory.is_empty()
            || directory.len() > MAX_INVOCATION_DIRECTORY_BYTES
            || directory.chars().any(char::is_control)
        {
            return None;
        }
        let directory = PathBuf::from(directory);
        if !directory.is_absolute() {
            return None;
        }
        Some(Self { directory })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }
}

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
        let plugin_context = plugin_context_env()?;
        Self::from_runtime_values(
            required_os_env("HERDR_BIN_PATH")?.into(),
            optional_string_env("HERDR_PANE_ID", Some("PANE_ID"))?,
            optional_string_env("HERDR_WORKSPACE_ID", Some("WORKSPACE_ID"))?,
            plugin_context.as_deref(),
        )
    }

    fn from_runtime_values(
        binary: PathBuf,
        pane_id: Option<String>,
        workspace_id: Option<String>,
        plugin_context: Option<&str>,
    ) -> Result<Self> {
        let pane_id = match plugin_context {
            Some(context) => placement_pane_id(String::new(), Some(context))?,
            None => {
                pane_id.ok_or_else(|| anyhow::anyhow!("Herdr did not provide HERDR_PANE_ID"))?
            }
        };
        let workspace_id = placement_workspace_id(workspace_id, plugin_context)?;
        Ok(Self {
            binary,
            pane_id,
            workspace_id,
        })
    }

    /// Loads authoritative placement context for a managed plugin UI action.
    ///
    /// Unlike [`Self::from_env`], this never falls back to the managed pane ID.
    pub fn from_plugin_env() -> Result<Self> {
        let plugin_context = plugin_context_env()?;
        Self::from_plugin_values(
            required_os_env("HERDR_BIN_PATH")?.into(),
            optional_string_env("HERDR_WORKSPACE_ID", Some("WORKSPACE_ID"))?,
            plugin_context.as_deref(),
        )
    }

    fn from_plugin_values(
        binary: PathBuf,
        workspace_id: Option<String>,
        plugin_context: Option<&str>,
    ) -> Result<Self> {
        Ok(Self {
            binary,
            pane_id: plugin_placement_pane_id(plugin_context)?,
            workspace_id: placement_workspace_id(workspace_id, plugin_context)?,
        })
    }
}

fn plugin_context_env() -> Result<Option<String>> {
    match env::var("HERDR_PLUGIN_CONTEXT_JSON") {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => {
            bail!("Herdr provided a non-UTF-8 HERDR_PLUGIN_CONTEXT_JSON")
        }
    }
}

fn placement_workspace_id(
    workspace_id: Option<String>,
    plugin_context: Option<&str>,
) -> Result<String> {
    if let Some(plugin_context) = plugin_context {
        let context: Value =
            serde_json::from_str(plugin_context).context("decode HERDR_PLUGIN_CONTEXT_JSON")?;
        if let Some(context_workspace_id) = context.get("workspace_id").and_then(Value::as_str) {
            return require_nonempty_env(
                "HERDR_PLUGIN_CONTEXT_JSON workspace_id",
                context_workspace_id.to_owned(),
            );
        }
    }
    workspace_id.ok_or_else(|| anyhow::anyhow!("Herdr did not provide HERDR_WORKSPACE_ID"))
}

fn plugin_placement_pane_id(plugin_context: Option<&str>) -> Result<String> {
    let plugin_context = plugin_context.ok_or_else(|| {
        anyhow::anyhow!(
            "Herdr did not provide HERDR_PLUGIN_CONTEXT_JSON for authoritative plugin placement"
        )
    })?;
    placement_pane_id(String::new(), Some(plugin_context))
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

fn optional_string_env(name: &str, fallback: Option<&str>) -> Result<Option<String>> {
    match env::var(name) {
        Ok(value) => require_nonempty_env(name, value).map(Some),
        Err(env::VarError::NotPresent) => {
            let Some(fallback) = fallback else {
                return Ok(None);
            };
            match env::var(fallback) {
                Ok(value) => require_nonempty_env(fallback, value).map(Some),
                Err(env::VarError::NotPresent) => Ok(None),
                Err(env::VarError::NotUnicode(_)) => {
                    bail!("Herdr did not provide a valid UTF-8 {name}")
                }
            }
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

/// Safe, bounded presentation metadata for a placed Tether pane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaneTitle(String);

impl PaneTitle {
    /// Builds a title for an owned workload from persisted presentation fields.
    pub fn owned(host: &str, directory: &str, preset: Option<&str>, command: Option<&str>) -> Self {
        let workload = preset.and_then(sanitize_title_component).or_else(|| {
            command
                .and_then(command_basename)
                .and_then(sanitize_title_component)
        });
        Self::from_components([
            (host != "local").then_some(host),
            directory_leaf(directory),
            workload.as_deref(),
        ])
    }

    /// Builds a title for an externally owned tmux session.
    pub fn external(host: &str, session: &str) -> Self {
        Self::from_components([(host != "local").then_some(host), Some(session), None])
    }

    /// Builds a bounded title for an orchestration Observer pane.
    pub fn observer(group_title: &str) -> Self {
        Self::from_components([Some("Observer"), Some(group_title)])
    }

    /// Supplies the deterministic title used when no safe context is available.
    pub fn fallback() -> Self {
        Self("Tether session".to_owned())
    }

    fn from_components<'a>(components: impl IntoIterator<Item = Option<&'a str>>) -> Self {
        let mut title = String::new();
        for raw in components.into_iter().flatten() {
            let Some(component) = sanitize_title_component(raw) else {
                continue;
            };
            if title
                .split(" · ")
                .any(|existing| existing == component.as_str())
            {
                continue;
            }
            if !title.is_empty() {
                title.push_str(" · ");
            }
            title.push_str(&component);
        }
        if title.is_empty() {
            return Self::fallback();
        }
        Self(truncate_title(title))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
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
    pub fn place(
        &self,
        command: &CommandSpec,
        title: &PaneTitle,
        placement: Placement,
    ) -> Result<PlacedPane> {
        self.place_with_destination(title, placement, |_| Ok(command.clone()))
    }

    /// Creates a destination and builds its command from the exact resulting Herdr context.
    ///
    /// This is intended for commands whose runtime must know the pane it occupies. The builder
    /// runs only after Herdr returns the new pane ID. A builder or launch failure removes the
    /// otherwise empty destination.
    pub fn place_with_destination<F>(
        &self,
        title: &PaneTitle,
        placement: Placement,
        build: F,
    ) -> Result<PlacedPane>
    where
        F: FnOnce(&HerdrContext) -> Result<CommandSpec>,
    {
        let pane_id = match placement {
            Placement::SplitRight => self.split("right")?,
            Placement::SplitDown => self.split("down")?,
            Placement::NewTab => self.create_tab()?,
            Placement::ReplaceCurrentPane => {
                bail!("replace-current placement requires explicit replacement confirmation")
            }
        };
        let destination = HerdrContext {
            binary: self.context.binary.clone(),
            pane_id: pane_id.clone(),
            workspace_id: self.context.workspace_id.clone(),
        };
        let command = match build(&destination) {
            Ok(command) => command,
            Err(error) => {
                let detail = format!("{error:#}");
                return match self.close_pane(&pane_id) {
                    Ok(()) => Err(error).context(format!(
                        "build destination command failed ({detail}); empty destination `{pane_id}` was removed"
                    )),
                    Err(cleanup_error) => Err(error).context(format!(
                        "build destination command failed ({detail}) and empty destination `{pane_id}` could not be removed: {cleanup_error:#}"
                    )),
                };
            }
        };
        // Presentation metadata must never block a working attach.
        let _ = self.label_pane(&pane_id, title);
        match self.run_in_pane(&command, pane_id.clone()) {
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
    pub fn replace_current(&self, command: &CommandSpec, title: &PaneTitle) -> Result<PlacedPane> {
        self.replace_current_with_destination(title, |_| Ok(command.clone()))
    }

    /// Replaces the invoking pane with a command built from the exact destination context.
    pub fn replace_current_with_destination<F>(
        &self,
        title: &PaneTitle,
        build: F,
    ) -> Result<PlacedPane>
    where
        F: FnOnce(&HerdrContext) -> Result<CommandSpec>,
    {
        const VERIFY_ATTEMPTS: usize = 20;
        const VERIFY_INTERVAL: Duration = Duration::from_millis(50);

        let source_pane_id = self.context.pane_id.clone();
        let source_identity = self
            .process_info(&source_pane_id)
            .context("capture the exact source pane occupant before replacement")?;
        let destination_pane_id = self.split("right")?;
        let destination = HerdrContext {
            binary: self.context.binary.clone(),
            pane_id: destination_pane_id.clone(),
            workspace_id: self.context.workspace_id.clone(),
        };
        let command = match build(&destination) {
            Ok(command) => command,
            Err(error) => {
                let cleanup = self.close_pane(&destination_pane_id);
                return Err(error).context(replacement_failure_context(
                    &source_pane_id,
                    &destination_pane_id,
                    cleanup.as_ref().err(),
                ));
            }
        };
        // Presentation metadata must never block a working replacement.
        let _ = self.label_pane(&destination_pane_id, title);
        let placed = match self.run_in_pane(&command, destination_pane_id.clone()) {
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
                        .any(|process| Self::is_attach_process(process, &command)) =>
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

    /// Opens one of this plugin's manifest-declared managed panes.
    ///
    /// Herdr 0.7.4 and newer use a session-modal popup. Older compatible
    /// releases retain the overlay behavior.
    pub fn open_plugin_pane(&self, entrypoint: &str) -> Result<()> {
        if entrypoint.trim().is_empty() {
            bail!("plugin pane entrypoint must not be empty");
        }
        let popup = self.supports_popup_plugin_panes()?;
        let placement = if popup { "popup" } else { "overlay" };
        let mut arguments = vec![
            "plugin".to_owned(),
            "pane".to_owned(),
            "open".to_owned(),
            "--plugin".to_owned(),
            PLUGIN_ID.to_owned(),
            "--entrypoint".to_owned(),
            entrypoint.to_owned(),
            "--placement".to_owned(),
            placement.to_owned(),
        ];
        if popup {
            arguments.extend([
                "--width".to_owned(),
                "80%".to_owned(),
                "--height".to_owned(),
                "80%".to_owned(),
            ]);
        }
        let response = self.invoke("open plugin pane", &arguments)?;
        if popup {
            require_result_type_in(&response, &["ok", "plugin_pane_opened"])
        } else {
            require_result_type(&response, "plugin_pane_opened")
        }
    }

    /// Labels a pane for an opted-in Tether orchestration view.
    ///
    /// Herdr releases before 0.7.5 have no metadata-token API; keeping this a
    /// no-op preserves Tether's 0.7.3 compatibility outside the optional view.
    pub fn report_agent_view_group(
        &self,
        pane_id: &str,
        group_id: &OrchestrationGroupId,
    ) -> Result<()> {
        let output = self.execute("read version", &["--version".to_owned()])?;
        if parse_herdr_version(&output.stdout)? < (0, 7, 5) {
            return Ok(());
        }
        let response = self.invoke(
            "label pane for Agent view",
            &[
                "pane".to_owned(),
                "report-metadata".to_owned(),
                pane_id.to_owned(),
                "--source".to_owned(),
                AGENT_VIEW_SOURCE.to_owned(),
                "--token".to_owned(),
                format!("{GROUP_TOKEN}={group_id}"),
            ],
        )?;
        require_result_type(&response, "ok")
    }

    fn supports_popup_plugin_panes(&self) -> Result<bool> {
        let output = self.execute("read version", &["--version".to_owned()])?;
        parse_herdr_version(&output.stdout).map(|version| version >= (0, 7, 4))
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
        let (expected_program, expected_args) = if expected.program.file_name()
            == Some(std::ffi::OsStr::new("env"))
            && expected
                .args
                .first()
                .is_some_and(|argument| argument.starts_with("HERDR_AGENT="))
        {
            let Some(wrapped_program) = expected.args.get(1) else {
                return false;
            };
            (Path::new(wrapped_program).file_name(), &expected.args[2..])
        } else {
            (expected.program.file_name(), expected.args.as_slice())
        };
        if Some(program) == expected_program && process.argv[1..] == *expected_args {
            return true;
        }

        let target = match expected_args {
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

    fn label_pane(&self, pane_id: &str, title: &PaneTitle) -> Result<()> {
        let response = self.invoke(
            "label Tether pane",
            &[
                "pane".to_owned(),
                "rename".to_owned(),
                pane_id.to_owned(),
                title.as_str().to_owned(),
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

const MAX_PANE_TITLE_CHARS: usize = 48;

fn directory_leaf(value: &str) -> Option<&str> {
    let value = value.trim();
    if value == "/" {
        return Some("/");
    }
    if value == "~" || value == "~/" {
        return Some("~");
    }
    value
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|component| !component.is_empty())
}

fn command_basename(command: &str) -> Option<&str> {
    command
        .split_whitespace()
        .map(|token| token.trim_matches(['\'', '"']))
        .skip_while(|token| {
            matches!(*token, "exec" | "command" | "env" | "sudo") || token.contains('=')
        })
        .take_while(|token| !token.contains('$'))
        .find_map(|token| {
            token
                .rsplit('/')
                .next()
                .filter(|basename| !basename.is_empty() && *basename != "." && *basename != "..")
        })
}

fn sanitize_title_component(value: &str) -> Option<String> {
    let mut sanitized = String::with_capacity(value.len().min(MAX_PANE_TITLE_CHARS));
    let mut pending_space = false;
    for character in value.chars() {
        if character.is_control()
            || character.is_whitespace()
            || matches!(
                character,
                '|' | '·'
                    | '\u{061c}'
                    | '\u{200b}'..='\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
                    | '\u{feff}'
            )
        {
            pending_space = !sanitized.is_empty();
            continue;
        }
        if pending_space {
            sanitized.push(' ');
            pending_space = false;
        }
        sanitized.push(character);
    }
    (!sanitized.is_empty()).then_some(sanitized)
}

fn truncate_title(title: String) -> String {
    if title.chars().count() <= MAX_PANE_TITLE_CHARS {
        return title;
    }
    let mut bounded = title
        .chars()
        .take(MAX_PANE_TITLE_CHARS - 1)
        .collect::<String>();
    bounded.push('…');
    bounded
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

fn parse_herdr_version(stdout: &[u8]) -> Result<(u64, u64, u64)> {
    let text = std::str::from_utf8(stdout).context("Herdr version output was not UTF-8")?;
    let version = text
        .trim()
        .strip_prefix("herdr ")
        .ok_or_else(|| anyhow::anyhow!("Herdr version output did not start with `herdr `"))?;
    let core = version.split_once('-').map_or(version, |(core, _)| core);
    let mut components = core.split('.');
    let major = components
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| {
            anyhow::anyhow!("Herdr version output did not contain a numeric major version")
        })?;
    let minor = components
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| {
            anyhow::anyhow!("Herdr version output did not contain a numeric minor version")
        })?;
    let patch = components
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| {
            anyhow::anyhow!("Herdr version output did not contain a numeric patch version")
        })?;
    if components.next().is_some() {
        bail!("Herdr version output contained too many numeric components");
    }
    Ok((major, minor, patch))
}
fn require_result_type(envelope: &Value, expected: &str) -> Result<()> {
    require_result_type_in(envelope, &[expected])
}

fn require_result_type_in(envelope: &Value, expected: &[&str]) -> Result<()> {
    let actual = envelope
        .get("result")
        .and_then(|result| result.get("type"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("Herdr response result did not contain a type"))?;
    if !expected.contains(&actual) {
        bail!(
            "unexpected Herdr response type `{actual}`; expected one of {}",
            expected.join(", ")
        );
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
    fn strict_plugin_context_never_falls_back_to_the_managed_pane() {
        let placements = std::cell::Cell::new(0);
        let result = HerdrContext::from_plugin_values(
            PathBuf::from("/bin/herdr"),
            Some("w1".to_owned()),
            None,
        )
        .inspect(|_| {
            placements.set(placements.get() + 1);
        });

        let error = result.unwrap_err().to_string();
        assert!(error.contains("HERDR_PLUGIN_CONTEXT_JSON"), "{error}");
        assert_eq!(
            placements.get(),
            0,
            "missing plugin JSON must place no pane"
        );
        assert_eq!(
            HerdrContext::from_plugin_values(
                PathBuf::from("/bin/herdr"),
                Some("w1".to_owned()),
                Some(r#"{"focused_pane_id":"w1:p1"}"#),
            )
            .unwrap()
            .pane_id,
            "w1:p1"
        );
        assert!(
            HerdrContext::from_plugin_values(
                PathBuf::from("/bin/herdr"),
                Some("w1".to_owned()),
                Some(r#"{"pane_id":"w1:pF"}"#),
            )
            .is_err()
        );
    }

    #[test]
    fn popup_context_uses_invoking_pane_without_managed_pane_env() {
        let context = HerdrContext::from_runtime_values(
            PathBuf::from("/bin/herdr"),
            None,
            None,
            Some(r#"{"workspace_id":"w1","focused_pane_id":"w1:p1"}"#),
        )
        .unwrap();

        assert_eq!(context.pane_id, "w1:p1");
        assert_eq!(context.workspace_id, "w1");
    }

    #[test]
    fn parses_popup_capable_herdr_versions() {
        assert_eq!(parse_herdr_version(b"herdr 0.7.3\n").unwrap(), (0, 7, 3));
        assert_eq!(parse_herdr_version(b"herdr 0.7.4\n").unwrap(), (0, 7, 4));
        assert_eq!(
            parse_herdr_version(b"herdr 1.0.0-preview\n").unwrap(),
            (1, 0, 0)
        );
        assert!(parse_herdr_version(b"unknown\n").is_err());
    }

    #[test]
    fn attach_verification_unwraps_explicit_agent_hint() {
        let expected = CommandSpec::new(
            "env",
            vec![
                "HERDR_AGENT=codex".into(),
                "/usr/bin/tmux".into(),
                "attach-session".into(),
                "-t".into(),
                "=tether-session".into(),
            ],
        );
        let process = ForegroundProcess {
            pid: 42,
            name: "tmux".into(),
            argv: vec![
                "/usr/bin/tmux".into(),
                "attach-session".into(),
                "-t".into(),
                "=tether-session".into(),
            ],
        };

        assert!(HerdrClient::is_attach_process(&process, &expected));
    }

    #[test]
    fn pane_title_preserves_an_exactly_full_budget() {
        let exact = "x".repeat(MAX_PANE_TITLE_CHARS);
        assert_eq!(truncate_title(exact.clone()), exact);

        let over = truncate_title("x".repeat(MAX_PANE_TITLE_CHARS + 1));
        assert_eq!(over.chars().count(), MAX_PANE_TITLE_CHARS);
        assert!(over.ends_with('…'));
        assert_eq!(
            PaneTitle::owned(
                "local",
                "/srv/project",
                None,
                Some("exec ${SHELL:-/bin/sh}"),
            )
            .as_str(),
            "project",
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
