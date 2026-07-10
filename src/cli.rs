use std::{
    env,
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration as StdDuration,
};

use anyhow::{Context, Result, bail};
use chrono::{Duration, Utc};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use crate::{
    backend::{CommandSpec, DurableBackend, LaunchSpec, ProcessBinaries},
    config::{CommandPreset, Config, ConfigStore, HostConfig},
    herdr::{HerdrClient, HerdrContext},
    lifecycle::{CleanupEligibility, cleanup_eligibility},
    model::{Placement, SessionId},
    paths::AppPaths,
    sshcfg::discover_aliases,
    state::{SessionRecord, SessionStatus, State, StateStore},
    status::StatusService,
    tmux::TmuxBackend,
    tui::{OpenSelection, PickerOptions, PickerSelection, run_picker},
};

#[derive(Debug, Parser)]
#[command(name = "herdr-tether", version, about)]
pub struct Cli {
    #[command(subcommand)]
    command: TopLevel,
}

#[derive(Debug, Subcommand)]
enum TopLevel {
    /// Create Tether's private configuration and state files.
    Setup(SetupArgs),
    /// Add, inspect, and remove connection targets.
    Host {
        #[command(subcommand)]
        command: HostCommand,
    },
    /// Create a durable terminal session.
    Open(OpenArgs),
    /// Inspect and manage durable sessions.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Report local installation and configuration health.
    Doctor,
    /// Herdr plugin action entrypoints.
    #[command(hide = true)]
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
}

#[derive(Clone, Debug, Args)]
struct SetupArgs {
    /// Accept defaults without prompting.
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Subcommand)]
enum HostCommand {
    /// Save an explicit SSH target.
    Add(HostAddArgs),
    /// List configured targets and literal OpenSSH aliases.
    List(OutputArgs),
    /// Remove a configured target.
    Remove { name: String },
    /// Verify SSH, tmux, and an optional remote Herdr installation.
    Check { name: String },
}

#[derive(Clone, Debug, Args)]
struct HostAddArgs {
    name: String,
    target: String,
    /// Suggested working directory. May be supplied more than once.
    #[arg(long = "root")]
    roots: Vec<String>,
    /// Named command in NAME=COMMAND form. May be supplied more than once.
    #[arg(long = "preset", value_parser = parse_preset)]
    presets: Vec<CommandPresetArg>,
}

#[derive(Clone, Debug)]
struct CommandPresetArg {
    name: String,
    command: String,
}

#[derive(Clone, Debug, Args)]
struct OutputArgs {
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Debug, Args)]
struct OpenArgs {
    /// Configured host, literal SSH alias, or `local`.
    #[arg(long)]
    host: Option<String>,
    /// Initial working directory.
    #[arg(long)]
    directory: Option<String>,
    /// Command executed inside the durable tmux session.
    #[arg(long, conflicts_with = "preset")]
    command: Option<String>,
    /// Named command preset from the selected host.
    #[arg(long, conflicts_with = "command")]
    preset: Option<String>,
    /// Placement used when invoked from a Herdr plugin pane.
    #[arg(long, value_enum)]
    placement: Option<PlacementArg>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum PlacementArg {
    SplitRight,
    SplitDown,
    NewTab,
}

impl From<PlacementArg> for Placement {
    fn from(value: PlacementArg) -> Self {
        match value {
            PlacementArg::SplitRight => Self::SplitRight,
            PlacementArg::SplitDown => Self::SplitDown,
            PlacementArg::NewTab => Self::NewTab,
        }
    }
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    /// List persisted session metadata.
    List(OutputArgs),
    /// Attach to an existing durable session without creating it.
    Resume { id: SessionId },
    /// Kill a durable session and mark its metadata closed.
    Close { id: SessionId },
    /// Remove old, closed metadata whose workload was killed.
    Prune(PruneArgs),
}

#[derive(Clone, Debug, Args)]
struct PruneArgs {
    /// Show eligible records without changing state.
    #[arg(long)]
    dry_run: bool,
    /// Minimum age since close.
    #[arg(long, default_value_t = 30)]
    older_than_days: u64,
}

#[derive(Debug, Subcommand)]
enum PluginCommand {
    /// Open the managed Tether picker overlay.
    Open,
    /// Open the managed Tether setup overlay.
    Setup,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let paths = AppPaths::from_env()?;
    dispatch(cli.command, &paths)
}

fn dispatch(command: TopLevel, paths: &AppPaths) -> Result<()> {
    match command {
        TopLevel::Setup(args) => setup(paths, args),
        TopLevel::Host { command } => host_command(paths, command),
        TopLevel::Open(args) => open(paths, args),
        TopLevel::Session { command } => session_command(paths, command),
        TopLevel::Doctor => doctor(paths),
        TopLevel::Plugin { command } => plugin_command(command),
    }
}

fn setup(paths: &AppPaths, args: SetupArgs) -> Result<()> {
    if !args.yes && !stdio_is_terminal() {
        bail!("setup requires --yes when standard input is not a terminal");
    }

    let config_store = ConfigStore::new(paths.config_file.clone());
    let state_store = StateStore::new(paths.state_file.clone());
    config_store.update(|_| Ok(()))?;
    state_store.update(|_| Ok(()))?;

    println!("Tether configuration: {}", paths.config_file.display());
    println!("Tether state: {}", paths.state_file.display());
    println!("Herdr keybindings are not edited automatically.");
    println!("Suggested binding: plugin_action moneycaringcoder.tether.open");
    Ok(())
}

fn stdio_is_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

fn host_command(paths: &AppPaths, command: HostCommand) -> Result<()> {
    let store = ConfigStore::new(paths.config_file.clone());
    match command {
        HostCommand::Add(args) => {
            let host = HostConfig {
                name: args.name,
                target: args.target,
                roots: args.roots,
                presets: args
                    .presets
                    .into_iter()
                    .map(|preset| CommandPreset {
                        name: preset.name,
                        command: preset.command,
                    })
                    .collect(),
            };
            let name = host.name.clone();
            store.update(|config| config.add_host(host))?;
            println!("added host {name}");
            Ok(())
        }
        HostCommand::List(args) => list_hosts(paths, &store.load()?, args.json),
        HostCommand::Remove { name } => {
            store.update(|config| {
                if !config.remove_host(&name) {
                    bail!("unknown configured host `{name}`");
                }
                Ok(())
            })?;
            println!("removed host {name}");
            Ok(())
        }
        HostCommand::Check { name } => {
            let config = store.load()?;
            let target = resolve_host(paths, &config, &name)?.target;
            check_host(&name, &target)
        }
    }
}

#[derive(Serialize)]
struct HostListEntry<'a> {
    name: &'a str,
    target: &'a str,
    source: &'static str,
}

fn list_hosts(paths: &AppPaths, config: &Config, json: bool) -> Result<()> {
    let aliases = discover_aliases(&paths.ssh_config_file)?;
    let mut entries = Vec::with_capacity(config.hosts.len() + aliases.len() + 1);
    entries.push(HostListEntry {
        name: "local",
        target: "local",
        source: "builtin",
    });
    entries.extend(config.hosts.iter().map(|host| HostListEntry {
        name: &host.name,
        target: &host.target,
        source: "configured",
    }));
    entries.extend(
        aliases
            .iter()
            .filter(|alias| !config.hosts.iter().any(|host| host.name == **alias))
            .map(|alias| HostListEntry {
                name: alias,
                target: alias,
                source: "ssh_config",
            }),
    );

    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        for entry in entries {
            println!("{}\t{}\t{}", entry.name, entry.target, entry.source);
        }
    }
    Ok(())
}

fn check_host(name: &str, target: &str) -> Result<()> {
    if target == "local" {
        let output = Command::new("tmux")
            .arg("-V")
            .output()
            .context("run local tmux version probe")?;
        require_probe_success("tmux", &output)?;
        print!("{}", String::from_utf8_lossy(&output.stdout));
        return Ok(());
    }

    let tmux = ssh_probe(target, "tmux -V")?;
    require_probe_success("remote tmux", &tmux)?;
    print!("{}", String::from_utf8_lossy(&tmux.stdout));

    let herdr = ssh_probe(
        target,
        "command -v herdr >/dev/null 2>&1 && herdr --version",
    )?;
    if herdr.status.success() {
        print!("{}", String::from_utf8_lossy(&herdr.stdout));
    } else {
        println!("herdr not found on {name} (optional)");
    }
    Ok(())
}

fn ssh_probe(target: &str, remote_command: &str) -> Result<std::process::Output> {
    Command::new("ssh")
        .args(["-o", "BatchMode=yes", "--", target, remote_command])
        .output()
        .with_context(|| format!("probe SSH target `{target}`"))
}

fn require_probe_success(name: &str, output: &std::process::Output) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr);
    bail!(
        "{name} probe failed with status {}: {}",
        output.status,
        detail.trim()
    )
}

fn open(paths: &AppPaths, args: OpenArgs) -> Result<()> {
    let config_store = ConfigStore::new(paths.config_file.clone());
    let state_store = StateStore::new(paths.state_file.clone());
    let config = config_store.load()?;
    let aliases = discover_aliases(&paths.ssh_config_file)?;

    if let Some(name) = args.host.as_deref() {
        // Fail before loading or creating the state file.
        resolve_host_from(&config, &aliases, name)?;
    }

    let complete = args.host.is_some()
        && args.directory.is_some()
        && (args.command.is_some() || args.preset.is_some());
    let selection = if complete {
        PickerSelection::Create(selection_from_args(&config, &aliases, args)?)
    } else {
        let state = state_store.load()?;
        selection_from_picker(&config, &aliases, &state, args)?
            .context("session selection was cancelled")?
    };

    match selection {
        PickerSelection::Create(selection) => create_and_attach(paths, &config, selection),
        PickerSelection::Resume { id, placement } => resume_and_attach(paths, id, placement),
    }
}

fn selection_from_args(
    config: &Config,
    aliases: &[String],
    args: OpenArgs,
) -> Result<OpenSelection> {
    let host_name = args.host.context("--host is required")?;
    let host = resolve_host_from(config, aliases, &host_name)?;
    let (preset, command) = resolve_command(&host, args.preset, args.command)?;
    Ok(OpenSelection {
        host: host_name,
        directory: args.directory.context("--directory is required")?,
        preset,
        command,
        placement: args
            .placement
            .map(Placement::from)
            .unwrap_or(config.ui.placement),
    })
}

fn selection_from_picker(
    config: &Config,
    aliases: &[String],
    state: &State,
    args: OpenArgs,
) -> Result<Option<PickerSelection>> {
    let requested_host = args.host.as_deref();
    let mut picker_config = config.clone();
    append_alias_hosts(&mut picker_config, aliases);
    let include_local = match requested_host {
        Some("local") | None => true,
        Some(_) => false,
    };
    if let Some(requested) = requested_host {
        picker_config.hosts.retain(|host| host.name == requested);
    }
    let home = env::var("HOME").unwrap_or_else(|_| "~".to_owned());
    let mut options = PickerOptions::from_config_state(&picker_config, state, &home, include_local);
    if args.directory.is_some() || args.command.is_some() || args.preset.is_some() {
        for host in &mut options.hosts {
            host.workloads.clear();
        }
    }
    let status_service = StatusService::new(
        ProcessBinaries::new("ssh", "tmux"),
        StdDuration::from_secs(3),
        4,
    );
    let Some(selection) = run_picker(options, status_service)? else {
        return Ok(None);
    };

    match selection {
        PickerSelection::Create(mut selection) => {
            if let Some(host_name) = args.host {
                resolve_host_from(config, aliases, &host_name)?;
                selection.host = host_name;
            }
            if let Some(directory) = args.directory {
                selection.directory = directory;
            }
            if args.command.is_some() || args.preset.is_some() {
                let host = resolve_host_from(config, aliases, &selection.host)?;
                let (preset, command) = resolve_command(&host, args.preset, args.command)?;
                selection.preset = preset;
                selection.command = command;
            }
            if let Some(placement) = args.placement {
                selection.placement = placement.into();
            }
            Ok(Some(PickerSelection::Create(selection)))
        }
        PickerSelection::Resume { id, placement } => Ok(Some(PickerSelection::Resume {
            id,
            placement: args.placement.map(Placement::from).unwrap_or(placement),
        })),
    }
}

fn create_and_attach(paths: &AppPaths, config: &Config, selection: OpenSelection) -> Result<()> {
    if selection.directory.trim().is_empty() {
        bail!("session directory must not be empty");
    }
    if selection.command.trim().is_empty() {
        bail!("session command must not be empty");
    }
    let aliases = discover_aliases(&paths.ssh_config_file)?;
    let host = resolve_host_from(config, &aliases, &selection.host)?;
    let id = SessionId::new();
    let backend = backend_for(&host.target)?;
    let launch = LaunchSpec {
        id,
        directory: selection.directory.clone(),
        command: selection.command,
    };
    backend.create(&launch)?;

    let now = Utc::now();
    let record = SessionRecord {
        id,
        host: selection.host,
        target: host.target,
        directory: selection.directory,
        preset: selection.preset,
        status: SessionStatus::Active,
        created_at: now,
        last_used_at: now,
        closed_at: None,
    };
    let store = StateStore::new(paths.state_file.clone());
    if let Err(error) = store.update(|state| {
        state.sessions.push(record);
        Ok(())
    }) {
        let cleanup = backend.close(&id);
        return match cleanup {
            Ok(()) => Err(error).context("persist newly created session"),
            Err(cleanup_error) => Err(error).context(format!(
                "persist newly created session; rollback close for `{id}` also failed: {cleanup_error:#}; the workload may still be running"
            )),
        };
    }

    println!("created {id}");
    if env::var_os("HERDR_BIN_PATH").is_some() {
        let context = HerdrContext::from_env()?;
        let executable = env::current_exe().context("locate the Tether executable")?;
        let resume = resume_command(executable, id);
        HerdrClient::new(context).place(&resume, selection.placement)?;
        Ok(())
    } else {
        run_attach(backend.attach_command(&id)?)
    }
}
fn resume_and_attach(paths: &AppPaths, id: SessionId, placement: Placement) -> Result<()> {
    if env::var_os("HERDR_BIN_PATH").is_some() {
        let context = HerdrContext::from_env()?;
        let executable = env::current_exe().context("locate the Tether executable")?;
        let resume = resume_command(executable, id);
        HerdrClient::new(context).place(&resume, placement)?;
        Ok(())
    } else {
        session_command(paths, SessionCommand::Resume { id })
    }
}
fn resume_command(executable: PathBuf, id: SessionId) -> CommandSpec {
    CommandSpec::new(
        executable,
        vec!["session".to_owned(), "resume".to_owned(), id.to_string()],
    )
}

fn session_command(paths: &AppPaths, command: SessionCommand) -> Result<()> {
    let store = StateStore::new(paths.state_file.clone());
    match command {
        SessionCommand::List(args) => {
            let state = store.load()?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&state.sessions)?);
            } else {
                for session in state.sessions {
                    println!(
                        "{}\t{}\t{}\t{}\t{:?}",
                        session.id,
                        session.host,
                        session.directory,
                        session.last_used_at,
                        session.status
                    );
                }
            }
            Ok(())
        }
        SessionCommand::Resume { id } => {
            let attach = store.update(|state| {
                let record = state
                    .sessions
                    .iter_mut()
                    .find(|record| record.id == id)
                    .with_context(|| format!("unknown session `{id}`"))?;
                match record.status {
                    SessionStatus::Active => {}
                    SessionStatus::Closing => bail!("session `{id}` is closing; retry close"),
                    SessionStatus::Closed => bail!("session `{id}` is closed"),
                }
                let backend = backend_for(&record.target)?;
                match backend.inspect(&id)? {
                    crate::backend::WorkloadState::Running { .. } => {}
                    crate::backend::WorkloadState::Missing => {
                        bail!("session `{id}` no longer exists")
                    }
                    crate::backend::WorkloadState::Unknown => {
                        bail!("could not determine whether session `{id}` exists")
                    }
                }
                record.last_used_at = Utc::now();
                backend.attach_command(&id)
            })?;
            run_attach(attach)
        }
        SessionCommand::Close { id } => {
            store.exclusive(|store| {
                let mut state = store.load()?;
                let index = state
                    .sessions
                    .iter()
                    .position(|record| record.id == id)
                    .with_context(|| format!("unknown session `{id}`"))?;
                let status = state.sessions[index].status;
                if status == SessionStatus::Closed {
                    bail!("session `{id}` is already closed");
                }
                let backend = backend_for(&state.sessions[index].target)?;
                match backend.inspect(&id)? {
                    crate::backend::WorkloadState::Missing => {}
                    crate::backend::WorkloadState::Running { .. } => {
                        if status == SessionStatus::Active {
                            state.sessions[index].status = SessionStatus::Closing;
                            store.save(&state)?;
                        }
                        backend.close(&id)?;
                    }
                    crate::backend::WorkloadState::Unknown => {
                        bail!("could not determine whether session `{id}` exists")
                    }
                }
                let now = Utc::now();
                let record = &mut state.sessions[index];
                record.status = SessionStatus::Closed;
                record.last_used_at = now;
                record.closed_at = Some(now);
                store.save(&state)
            })?;
            println!("closed {id}");
            Ok(())
        }
        SessionCommand::Prune(args) => prune(&store, args),
    }
}

fn prune(store: &StateStore, args: PruneArgs) -> Result<()> {
    let days = i64::try_from(args.older_than_days).context("--older-than-days is too large")?;
    let retention = Duration::try_days(days).context("--older-than-days is too large")?;
    let now = Utc::now();
    let collect = |state: &State| {
        state
            .sessions
            .iter()
            .filter(|record| {
                cleanup_eligibility(
                    record,
                    // A Closed record is written only after kill-session succeeds. Do
                    // not reconnect merely to rediscover that intentional absence.
                    crate::backend::WorkloadState::Missing,
                    now,
                    retention,
                ) == CleanupEligibility::RemoveMetadata
            })
            .map(|record| record.id)
            .collect::<Vec<_>>()
    };

    if args.dry_run {
        for id in collect(&store.load()?) {
            println!("{id}");
        }
        return Ok(());
    }

    store.update(|state| {
        let remove = collect(state);
        for id in &remove {
            println!("{id}");
        }
        state.sessions.retain(|record| !remove.contains(&record.id));
        Ok(())
    })
}

fn plugin_command(command: PluginCommand) -> Result<()> {
    let context = HerdrContext::from_env()?;
    let entrypoint = match command {
        PluginCommand::Open => "picker",
        PluginCommand::Setup => "setup",
    };
    HerdrClient::new(context).open_plugin_pane(entrypoint)
}

fn doctor(paths: &AppPaths) -> Result<()> {
    let config = ConfigStore::new(paths.config_file.clone()).load();
    let state = StateStore::new(paths.state_file.clone()).load();
    println!(
        "config {}: {}",
        paths.config_file.display(),
        if config.is_ok() { "ok" } else { "error" }
    );
    println!(
        "state {}: {}",
        paths.state_file.display(),
        if state.is_ok() { "ok" } else { "error" }
    );
    report_binary("tmux", &["-V"]);
    report_binary("ssh", &["-V"]);
    if let Some(binary) = env::var_os("HERDR_BIN_PATH") {
        report_binary_path(PathBuf::from(binary), &["--version"]);
    } else {
        report_binary("herdr", &["--version"]);
    }
    config?;
    state?;
    Ok(())
}

fn report_binary(program: &str, args: &[&str]) {
    report_binary_path(PathBuf::from(program), args);
}

fn report_binary_path(program: PathBuf, args: &[&str]) {
    let status = Command::new(&program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    println!(
        "{}: {}",
        program.display(),
        if status.is_ok_and(|status| status.success()) {
            "ok"
        } else {
            "not found"
        }
    );
}

fn resolve_host(paths: &AppPaths, config: &Config, name: &str) -> Result<HostConfig> {
    let aliases = discover_aliases(&paths.ssh_config_file)?;
    resolve_host_from(config, &aliases, name)
}

fn resolve_host_from(config: &Config, aliases: &[String], name: &str) -> Result<HostConfig> {
    if name == "local" {
        return Ok(HostConfig {
            name: "local".to_owned(),
            target: "local".to_owned(),
            roots: Vec::new(),
            presets: Vec::new(),
        });
    }
    if let Some(host) = config.hosts.iter().find(|host| host.name == name) {
        return Ok(host.clone());
    }
    if aliases.iter().any(|alias| alias == name) {
        return Ok(HostConfig {
            name: name.to_owned(),
            target: name.to_owned(),
            roots: Vec::new(),
            presets: Vec::new(),
        });
    }
    bail!("unknown host `{name}`")
}

fn append_alias_hosts(config: &mut Config, aliases: &[String]) {
    for alias in aliases {
        if !config.hosts.iter().any(|host| host.name == *alias) {
            config.hosts.push(HostConfig {
                name: alias.clone(),
                target: alias.clone(),
                roots: Vec::new(),
                presets: Vec::new(),
            });
        }
    }
}

fn resolve_command(
    host: &HostConfig,
    preset: Option<String>,
    command: Option<String>,
) -> Result<(Option<String>, String)> {
    match (preset, command) {
        (Some(name), None) => {
            let command = host
                .presets
                .iter()
                .find(|preset| preset.name == name)
                .with_context(|| format!("unknown preset `{name}` for host `{}`", host.name))?
                .command
                .clone();
            Ok((Some(name), command))
        }
        (None, Some(command)) if !command.trim().is_empty() => Ok((None, command)),
        (None, Some(_)) => bail!("--command must not be empty"),
        _ => bail!("exactly one of --command or --preset is required"),
    }
}

fn backend_for(target: &str) -> Result<TmuxBackend> {
    let binaries = ProcessBinaries::new("ssh", "tmux");
    if target == "local" {
        Ok(TmuxBackend::local(binaries))
    } else {
        TmuxBackend::remote(target.to_owned(), binaries)
    }
}

fn run_attach(spec: CommandSpec) -> Result<()> {
    let status = Command::new(&spec.program)
        .args(&spec.args)
        .status()
        .with_context(|| format!("run attach command `{}`", spec.program.display()))?;
    if !status.success() {
        bail!("attach command failed with status {status}");
    }
    Ok(())
}

fn parse_preset(value: &str) -> std::result::Result<CommandPresetArg, String> {
    let Some((name, command)) = value.split_once('=') else {
        return Err("expected NAME=COMMAND".to_owned());
    };
    if name.trim().is_empty() || command.trim().is_empty() {
        return Err("preset name and command must not be empty".to_owned());
    }
    Ok(CommandPresetArg {
        name: name.to_owned(),
        command: command.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_command_targets_the_exact_selected_session() {
        let id = "tether-0197f198000070008000000000000001"
            .parse::<SessionId>()
            .unwrap();

        let command = resume_command(PathBuf::from("/plugin/herdr-tether"), id);

        assert_eq!(command.program, PathBuf::from("/plugin/herdr-tether"));
        assert_eq!(
            command.args,
            [
                "session",
                "resume",
                "tether-0197f198000070008000000000000001"
            ]
        );
    }
}
