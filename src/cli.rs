use std::{
    env, path::PathBuf, process::Command, sync::atomic::AtomicBool, time::Duration as StdDuration,
};

use anyhow::{Context, Result, bail};
use chrono::{Duration, Utc};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use crate::{
    backend::{CommandSpec, DurableBackend, LaunchSpec, ProcessBinaries},
    config::{CommandPreset, Config, ConfigStore, HostConfig},
    discovery::{DiscoveryLimits, DiscoveryService},
    herdr::{HerdrClient, HerdrContext},
    lifecycle::{CleanupEligibility, cleanup_eligibility},
    model::{ExternalSessionName, Placement, SessionId},
    paths::AppPaths,
    sshcfg::discover_aliases,
    state::{SessionRecord, SessionStatus, State, StateStore},
    status::{BoundedOutput, StatusService, run_bounded},
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

fn placement_name(placement: Placement) -> &'static str {
    match placement {
        Placement::SplitRight => "split-right",
        Placement::SplitDown => "split-down",
        Placement::NewTab => "new-tab",
    }
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    /// List persisted session metadata.
    List(OutputArgs),
    /// Attach to an existing durable session without creating it.
    Resume { id: SessionId },
    /// Attach to a discovered non-owned external tmux session.
    #[command(hide = true)]
    AttachExternal {
        #[arg(long)]
        target: String,
        name: ExternalSessionName,
    },
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
    /// Minimum age since close. Uses configured retention when omitted.
    #[arg(long)]
    older_than_days: Option<u64>,
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
    let config = config_store.load()?;

    println!("Tether configuration: {}", paths.config_file.display());
    println!("Tether state: {}", paths.state_file.display());
    let roots = if config.discovery.local_roots.is_empty() {
        "HOME fallback".to_owned()
    } else {
        config.discovery.local_roots.join(", ")
    };
    println!(
        "Effective discovery: roots {roots}; depth {}; entries {}; results {}; timeout {}s; workers {}",
        config.discovery.max_depth,
        config.discovery.max_entries,
        config.discovery.max_results,
        config.discovery.timeout_seconds,
        config.discovery.workers
    );
    println!("Effective retention: {} days", config.retention.closed_days);
    println!(
        "Effective placement: {}",
        placement_name(config.ui.placement)
    );
    println!("Configured targets: {}", config.hosts.len());
    println!("Prerequisites: Herdr, tmux, SSH, and Cargo must be installed and executable.");
    println!("Herdr keybindings are not edited automatically.");
    println!("Suggested binding: plugin_action moneycaringcoder.tether.open");
    println!("Next: herdr-tether doctor");
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
        PickerSelection::AttachExternal {
            target,
            name,
            placement,
            ..
        } => attach_external(target, name, placement),
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
            host.allow_existing = false;
        }
    }
    let status_service = StatusService::new(
        ProcessBinaries::new("ssh", "tmux"),
        StdDuration::from_secs(3),
        4,
    );
    let discovery_service = DiscoveryService::new(
        ProcessBinaries::new("ssh", "tmux"),
        DiscoveryLimits {
            max_depth: config.discovery.max_depth,
            max_entries: config.discovery.max_entries,
            max_results: config.discovery.max_results,
            timeout: StdDuration::from_secs(config.discovery.timeout_seconds),
            workers: config.discovery.workers,
        },
    );
    let Some(selection) = run_picker(options, status_service, discovery_service)? else {
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
        PickerSelection::AttachExternal {
            host,
            target,
            name,
            placement,
        } => Ok(Some(PickerSelection::AttachExternal {
            host,
            target,
            name,
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

fn external_attach_command(
    executable: PathBuf,
    target: &str,
    name: &ExternalSessionName,
) -> CommandSpec {
    CommandSpec::new(
        executable,
        vec![
            "session".to_owned(),
            "attach-external".to_owned(),
            "--target".to_owned(),
            target.to_owned(),
            "--".to_owned(),
            name.to_string(),
        ],
    )
}

fn attach_external(
    target: Option<String>,
    name: ExternalSessionName,
    placement: Placement,
) -> Result<()> {
    let target = target.unwrap_or_else(|| "local".to_owned());
    if env::var_os("HERDR_BIN_PATH").is_some() {
        let context = HerdrContext::from_env()?;
        let executable = env::current_exe().context("locate the Tether executable")?;
        let attach = external_attach_command(executable, &target, &name);
        HerdrClient::new(context).place(&attach, placement)?;
        Ok(())
    } else {
        let backend = backend_for(&target)?;
        run_attach(backend.attach_external_command(&name)?)
    }
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
        SessionCommand::AttachExternal { target, name } => {
            let backend = backend_for(&target)?;
            run_attach(backend.attach_external_command(&name)?)
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
        SessionCommand::Prune(args) => {
            let (days, source) = match args.older_than_days {
                Some(days) => (days, "--older-than-days"),
                None => (
                    ConfigStore::new(paths.config_file.clone())
                        .load()?
                        .retention
                        .closed_days,
                    "retention.closed_days",
                ),
            };
            prune(&store, args, days, source)
        }
    }
}

fn prune(store: &StateStore, args: PruneArgs, days: u64, source: &str) -> Result<()> {
    let days = i64::try_from(days).with_context(|| format!("{source} is too large"))?;
    let retention = Duration::try_days(days).with_context(|| format!("{source} is too large"))?;
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
    let mut failures = 0usize;

    if !paths.config_file.exists() {
        println!(
            "config {}: missing (run `herdr-tether setup`)",
            paths.config_file.display()
        );
        failures += 1;
    } else {
        match ConfigStore::new(paths.config_file.clone()).load() {
            Ok(_) => println!("config {}: ok", paths.config_file.display()),
            Err(error) => {
                println!(
                    "config {}: unusable ({error:#})",
                    paths.config_file.display()
                );
                failures += 1;
            }
        }
    }

    if !paths.state_file.exists() {
        println!(
            "state {}: missing (run `herdr-tether setup`)",
            paths.state_file.display()
        );
        failures += 1;
    } else {
        match StateStore::new(paths.state_file.clone()).load() {
            Ok(_) => println!("state {}: ok", paths.state_file.display()),
            Err(error) => {
                println!("state {}: unusable ({error:#})", paths.state_file.display());
                failures += 1;
            }
        }
    }

    failures += usize::from(!report_binary("tmux", &["-V"]));
    failures += usize::from(!report_binary("ssh", &["-V"]));
    failures += usize::from(!report_binary("cargo", &["--version"]));

    let herdr = env::var_os("HERDR_BIN_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("herdr"));
    println!("Herdr binary: {}", herdr.display());
    failures += usize::from(!report_binary_path(herdr, &["--version"]));
    let herdr_binary_provided = env::var_os("HERDR_BIN_PATH")
        .is_some_and(|value| !value.to_string_lossy().trim().is_empty());
    let plugin_context_signaled = herdr_binary_provided
        || env::var_os("HERDR_PANE_ID").is_some()
        || env::var_os("HERDR_WORKSPACE_ID").is_some();
    if !plugin_context_signaled {
        println!("Herdr context: standalone (no plugin pane selected)");
    } else {
        let pane = env::var("HERDR_PANE_ID")
            .or_else(|_| env::var("PANE_ID"))
            .ok()
            .filter(|value| !value.trim().is_empty());
        let workspace = env::var("HERDR_WORKSPACE_ID")
            .or_else(|_| env::var("WORKSPACE_ID"))
            .ok()
            .filter(|value| !value.trim().is_empty());
        match (pane, workspace, herdr_binary_provided) {
            (Some(pane), Some(workspace), true) => {
                println!("Herdr context: {pane} in workspace {workspace}");
            }
            (pane, workspace, binary) => {
                let mut missing = Vec::new();
                if pane.is_none() {
                    missing.push("HERDR_PANE_ID");
                }
                if workspace.is_none() {
                    missing.push("HERDR_WORKSPACE_ID");
                }
                if !binary {
                    missing.push("HERDR_BIN_PATH");
                }
                println!(
                    "Herdr context: incomplete (missing {}; invoke Tether through a Herdr plugin action)",
                    missing.join(", ")
                );
                failures += 1;
            }
        }
    }

    if failures == 0 {
        Ok(())
    } else {
        bail!(
            "doctor found {failures} required failure{}",
            if failures == 1 { "" } else { "s" }
        )
    }
}

fn report_binary(program: &str, args: &[&str]) -> bool {
    report_binary_path(PathBuf::from(program), args)
}

fn report_binary_path(program: PathBuf, args: &[&str]) -> bool {
    let spec = CommandSpec::new(
        program.clone(),
        args.iter().map(|argument| (*argument).to_owned()).collect(),
    );
    let cancelled = AtomicBool::new(false);
    match run_bounded(&spec, StdDuration::from_secs(3), &cancelled) {
        BoundedOutput::Completed { status, .. } if status.success() => {
            println!("{}: ok", program.display());
            true
        }
        BoundedOutput::Completed {
            status,
            stdout,
            stdout_truncated,
            stderr,
            stderr_truncated,
        } => {
            let (bytes, truncated) = if stderr.is_empty() {
                (&stdout, stdout_truncated)
            } else {
                (&stderr, stderr_truncated)
            };
            let detail = concise_probe_detail(bytes, truncated)
                .map(|detail| format!("; {detail}"))
                .unwrap_or_default();
            println!(
                "{}: failed ({}{detail}); verify the executable works with `{}`",
                program.display(),
                status,
                program.display()
            );
            false
        }
        BoundedOutput::TimedOut => {
            println!(
                "{}: timed out after 3s (run `{}` directly to diagnose it)",
                program.display(),
                program.display()
            );
            false
        }
        BoundedOutput::SpawnError(std::io::ErrorKind::NotFound) => {
            println!(
                "{}: missing (install it or add it to PATH)",
                program.display()
            );
            false
        }
        BoundedOutput::SpawnError(std::io::ErrorKind::PermissionDenied) => {
            println!(
                "{}: permission denied (make the selected file executable)",
                program.display()
            );
            false
        }
        BoundedOutput::SpawnError(error) => {
            println!("{}: unavailable ({error})", program.display());
            false
        }
        BoundedOutput::Error => {
            println!(
                "{}: probe failed (run it directly to diagnose it)",
                program.display()
            );
            false
        }
        BoundedOutput::Cancelled => unreachable!("doctor probes are never cancelled"),
    }
}

fn concise_probe_detail(bytes: &[u8], truncated: bool) -> Option<String> {
    const MAX_CHARS: usize = 240;
    let safe = String::from_utf8_lossy(bytes)
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let detail = safe.split_whitespace().collect::<Vec<_>>().join(" ");
    if detail.is_empty() {
        return truncated.then(|| "output exceeded 64 KiB".to_owned());
    }
    let mut chars = detail.chars();
    let mut bounded = chars.by_ref().take(MAX_CHARS).collect::<String>();
    if chars.next().is_some() || truncated {
        bounded.push('…');
    }
    Some(bounded)
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

    #[test]
    fn external_attach_command_preserves_target_and_name_as_arguments() {
        let name = "-work 'quoted'".parse::<ExternalSessionName>().unwrap();
        let command =
            external_attach_command(PathBuf::from("/plugin/herdr-tether"), "user@dev", &name);

        assert_eq!(command.program, PathBuf::from("/plugin/herdr-tether"));
        assert_eq!(
            command.args,
            [
                "session",
                "attach-external",
                "--target",
                "user@dev",
                "--",
                "-work 'quoted'"
            ]
        );
    }
}
