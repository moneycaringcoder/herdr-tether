use std::{
    env,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{atomic::AtomicBool, mpsc},
    thread,
    time::Duration as StdDuration,
};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use crate::{
    agent_view::AgentViewService,
    backend::{
        CommandSpec, DurableBackend, LaunchSpec, ProcessBinaries, create_outcome_is_uncertain,
    },
    config::{
        CommandPreset, Config, ConfigStore, HerdrKeybindingInstall, HerdrKeybindingStore,
        HostConfig,
    },
    discovery::{DiscoveryLimits, DiscoveryService},
    herdr::{HerdrClient, HerdrContext, PaneTitle, PlacedPane},
    herdr_socket::{
        HerdrSocketClient, MAX_AUDITED_HERDR_PROTOCOL, MIN_HERDR_PROTOCOL, ProtocolConfidence,
    },
    lifecycle::{LifecycleService, PruneError, PruneService},
    model::{
        ExternalSessionName, HerdrAgentKind, OrchestrationGroupId, OrchestrationTitle,
        OwnershipProof, Placement, SessionId,
    },
    observer_manager::{ObserverManagerAction, ObserverManagerState, run_observer_manager},
    orchestration::{MANAGER_STALE_GROUP_ERROR, OrchestrationService, companion_placement},
    paths::AppPaths,
    snapshot::collect as collect_snapshot,
    sshcfg::{discover_aliases, openssh_connection_args, openssh_target},
    state::{
        OrchestrationCapabilities, OrchestrationGroup, SessionRecord, SessionStatus, State,
        StateStore, compare_normal_sessions, is_normal_session,
    },
    status::{BoundedOutput, StatusService, run_bounded},
    tmux::TmuxBackend,
    tui::{
        OpenSelection, PickerHostOrigin, PickerOptions, PickerSelection,
        run_picker_with_operation_error,
    },
};

const OBSERVER_PLUGIN_CONTEXT_ERROR: &str = "Observer must be launched from the Tether plugin pane";
/// How long the picker will spend confirming reported worktree paths.
///
/// The preference is cosmetic and the picker is waiting on it, so a wedged
/// mount costs this much and then nothing.
const WORKTREE_PROBE_TIMEOUT: StdDuration = StdDuration::from_millis(250);

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
    /// Print a bounded read-only JSON view of hosts, repositories, and sessions.
    ///
    /// Output uses schema version 1. Expected host/probe/scan degradation is
    /// represented by `completion: "partial"` and typed status values while
    /// the command exits successfully. Fatal local load/serialization errors
    /// retain the normal nonzero command error.
    Snapshot(SnapshotArgs),
    /// Open the Tether picker, or create and open a workload with options.
    Open(OpenArgs),
    /// Inspect and manage Tether-owned workloads.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Manage opt-in orchestration groups and observe their workers.
    Orchestration {
        #[command(subcommand)]
        command: OrchestrationCommand,
    },
    /// Report local installation and configuration health.
    Doctor(DoctorArgs),
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
    #[command(subcommand)]
    command: Option<SetupCommand>,
}

#[derive(Clone, Debug, Subcommand)]
enum SetupCommand {
    /// Install or roll back the prefix+t Tether plugin-action binding.
    Keybinding(KeybindingArgs),
}

#[derive(Clone, Debug, Args)]
struct KeybindingArgs {
    /// Restore the exact config saved by the keybinding installer.
    #[arg(long)]
    rollback: bool,
}

#[derive(Clone, Debug, Args)]
struct SnapshotArgs {
    /// Indent the JSON output for human readability.
    #[arg(long)]
    pretty: bool,
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
    /// Explicit Herdr screen-manifest hint for an agent hidden behind tmux or SSH.
    #[arg(long)]
    herdr_agent: Option<HerdrAgentKind>,
    /// Placement used when invoked from a Herdr plugin pane.
    #[arg(long, value_enum)]
    placement: Option<PlacementArg>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum PlacementArg {
    SplitRight,
    SplitDown,
    NewTab,
    ReplaceCurrentPane,
}

impl From<PlacementArg> for Placement {
    fn from(value: PlacementArg) -> Self {
        match value {
            PlacementArg::SplitRight => Self::SplitRight,
            PlacementArg::SplitDown => Self::SplitDown,
            PlacementArg::NewTab => Self::NewTab,
            PlacementArg::ReplaceCurrentPane => Self::ReplaceCurrentPane,
        }
    }
}

fn placement_name(placement: Placement) -> &'static str {
    match placement {
        Placement::SplitRight => "split-right",
        Placement::SplitDown => "split-down",
        Placement::NewTab => "new-tab",
        Placement::ReplaceCurrentPane => "replace-current-pane",
    }
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    /// List persisted workload metadata.
    List(OutputArgs),
    /// Open an existing running workload without creating it.
    Open { id: SessionId },
    /// Restart an ended workload and open it.
    Restart {
        id: SessionId,
        /// Placement used when invoked from a Herdr plugin pane.
        #[arg(long, value_enum)]
        placement: Option<PlacementArg>,
    },
    /// Attach to a discovered non-owned external tmux session.
    #[command(hide = true)]
    AttachExternal {
        #[arg(long)]
        target: String,
        name: ExternalSessionName,
    },
    /// Stop the exact Tether-owned workload and retain its history.
    Stop { id: SessionId },
    /// Remove an ended workload or safely forget legacy metadata without contacting it.
    Remove { id: SessionId },
    /// Clear old ended or removed history without touching workloads.
    Prune(PruneArgs),
}

#[derive(Debug, Subcommand)]
enum OrchestrationCommand {
    /// Create an empty orchestration group without launching panes.
    Create {
        id: OrchestrationGroupId,
        #[arg(long)]
        title: OrchestrationTitle,
        #[arg(long)]
        orchestrator: SessionId,
    },
    /// Delete one exact orchestration group without touching sessions or panes.
    Delete { id: OrchestrationGroupId },
    /// List persisted orchestration groups.
    List(OutputArgs),
    /// Add one exact worker membership with explicit capabilities.
    AddWorker {
        group: OrchestrationGroupId,
        session: SessionId,
        #[arg(long)]
        title: Option<OrchestrationTitle>,
        #[arg(long)]
        observe_output: bool,
        #[arg(long)]
        open_interactive: bool,
        #[arg(long)]
        prompt_agent: bool,
    },
    /// Remove one exact worker membership without touching its session or panes.
    RemoveWorker {
        group: OrchestrationGroupId,
        session: SessionId,
    },
    /// Launch one read-only Observer pane for an orchestration group.
    Observe {
        group: OrchestrationGroupId,
        #[arg(long, value_enum)]
        placement: Option<PlacementArg>,
    },
    /// Run Observer inside an already-created exact Herdr pane.
    #[command(hide = true)]
    ObserverRuntime {
        group: OrchestrationGroupId,
        #[arg(long)]
        pane_id: String,
        #[arg(long)]
        workspace_id: String,
        #[arg(long)]
        herdr_bin: PathBuf,
    },
}

#[derive(Clone, Debug, Args)]
struct DoctorArgs {
    /// Emit bounded, redacted, schema-versioned JSON instead of human output.
    #[arg(long)]
    json: bool,
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
    /// Open the managed Tether picker.
    Open,
    /// Open the managed Tether setup.
    Setup,
    /// Restore an opted-in Agent sidebar view after Herdr startup or handoff.
    RestoreAgentView,
}

pub fn run() -> Result<()> {
    let plugin_managed = env::var_os("HERDR_PLUGIN_ACTION_ID").is_some()
        || env::var_os("HERDR_PLUGIN_ENTRYPOINT_ID").is_some()
        || env::var_os("HERDR_PLUGIN_EVENT").is_some();
    let result = run_inner();
    if plugin_managed {
        match result {
            Ok(()) => Ok(()),
            Err(_) => {
                let correlation = plugin_correlation_reference();
                Err(anyhow::anyhow!(
                    "plugin command failed; correlation {correlation}"
                ))
            }
        }
    } else {
        result
    }
}

fn run_inner() -> Result<()> {
    let cli = Cli::parse();
    let paths = AppPaths::from_env()?;
    dispatch(cli.command, &paths)
}

fn plugin_correlation_reference() -> String {
    env::var("HERDR_PLUGIN_CONTEXT_JSON")
        .ok()
        .and_then(|context| serde_json::from_str::<serde_json::Value>(&context).ok())
        .and_then(|context| {
            context
                .get("correlation_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .filter(|correlation| {
            !correlation.is_empty()
                && correlation.len() <= 64
                && correlation.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b':' | b'-' | b'_')
                })
        })
        .unwrap_or_else(|| "unavailable".to_owned())
}

fn dispatch(command: TopLevel, paths: &AppPaths) -> Result<()> {
    match command {
        TopLevel::Setup(args) => match args.command.clone() {
            Some(SetupCommand::Keybinding(keybinding)) => setup_keybinding(keybinding),
            None => setup(paths, args),
        },
        TopLevel::Host { command } => host_command(paths, command),
        TopLevel::Open(args) => open(paths, args),
        TopLevel::Snapshot(args) => snapshot(paths, args),
        TopLevel::Session { command } => session_command(paths, command),
        TopLevel::Plugin { command } => plugin_command(paths, command),
        TopLevel::Orchestration { command } => orchestration_command(paths, command),
        TopLevel::Doctor(args) => doctor(paths, args),
    }
}

fn snapshot(paths: &AppPaths, args: SnapshotArgs) -> Result<()> {
    let config = ConfigStore::new(paths.config_file.clone()).load_read_only()?;
    let state = StateStore::new(paths.state_file.clone()).load_read_only()?;
    let aliases = discover_aliases(&paths.ssh_config_file)?;
    let home = env::var("HOME").unwrap_or_else(|_| "~".to_owned());
    let snapshot = collect_snapshot(
        &config,
        &state,
        &aliases,
        &home,
        ProcessBinaries::new("ssh", "tmux"),
    );
    let json = if args.pretty {
        serde_json::to_string_pretty(&snapshot)
    } else {
        serde_json::to_string(&snapshot)
    }
    .context("serialize snapshot JSON")?;
    println!("{json}");
    Ok(())
}

fn setup(paths: &AppPaths, args: SetupArgs) -> Result<()> {
    if !args.yes && !stdio_is_terminal() {
        bail!("setup requires --yes when standard input is not a terminal");
    }

    preflight_setup_runtime()?;
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
    println!("Prerequisites: Herdr, tmux, and SSH must be installed and executable.");
    println!("Herdr keybindings are changed only by the explicit keybinding command.");
    println!("Binding to install: plugin_action moneycaringcoder.tether.open");
    println!(
        "Next: install prefix+t with `herdr-tether setup keybinding`, or run `herdr-tether open` now."
    );
    Ok(())
}

fn setup_keybinding(args: KeybindingArgs) -> Result<()> {
    preflight_runtime_tool("Herdr", herdr_executable(), "--version")?;
    let path = HerdrKeybindingStore::path_from_env()?;
    let store = HerdrKeybindingStore::new(path.clone());
    if args.rollback {
        store.rollback()?;
        reload_herdr_config(&path, None)?;
        println!("restored Herdr config from the Tether keybinding backup");
        return Ok(());
    }

    match store.install()? {
        HerdrKeybindingInstall::AlreadyInstalled => {
            reload_herdr_config(&path, None)?;
            println!("Herdr prefix+t is already bound to moneycaringcoder.tether.open");
        }
        HerdrKeybindingInstall::Installed { backup } => {
            reload_herdr_config(&path, Some(backup.clone()))?;
            println!("installed Herdr prefix+t binding for moneycaringcoder.tether.open");
            println!("backup: {}", backup.display());
        }
    }
    println!("Next: press prefix+t in Herdr to open Tether.");
    Ok(())
}

fn preflight_setup_runtime() -> Result<()> {
    let binaries = ProcessBinaries::new("ssh", "tmux");
    preflight_runtime_tool("Herdr", herdr_executable(), "--version")?;
    preflight_runtime_tool("tmux", binaries.tmux().to_owned(), "-V")?;
    preflight_runtime_tool("SSH", binaries.ssh().to_owned(), "-V")
}

fn herdr_executable() -> PathBuf {
    env::var_os("HERDR_BIN_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("herdr"))
}

fn preflight_runtime_tool(
    name: &'static str,
    executable: PathBuf,
    version_flag: &str,
) -> Result<()> {
    let check = probe_binary(name, executable, version_flag);
    if !matches!(check.status, DoctorStatus::Ok) {
        bail!("setup prerequisite unavailable: {name}; install {name} and retry");
    }
    Ok(())
}

fn reload_herdr_config(config: &Path, backup: Option<PathBuf>) -> Result<()> {
    let executable = herdr_executable();
    let status = Command::new(&executable)
        .args(["server", "reload-config"])
        .status()
        .with_context(|| {
            format!(
                "Herdr config at `{}` was updated but reload could not be started{}",
                config.display(),
                rollback_diagnostic(backup.as_deref())
            )
        })?;
    if !status.success() {
        bail!(
            "Herdr config at `{}` was updated but reload failed with status {}{}",
            config.display(),
            status,
            rollback_diagnostic(backup.as_deref())
        );
    }
    Ok(())
}

fn rollback_diagnostic(backup: Option<&Path>) -> String {
    backup.map_or_else(
        || "; rerun `herdr server reload-config` when the server is available".to_owned(),
        |backup| {
            format!(
                "; rollback remains available with `herdr-tether setup keybinding --rollback` from backup `{}`",
                backup.display()
            )
        },
    )
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
                        herdr_agent: None,
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
    let binaries = ProcessBinaries::new("ssh", "tmux");
    if target == "local" {
        let output = Command::new(binaries.tmux())
            .arg("-V")
            .output()
            .context("run local tmux version probe")?;
        require_probe_success("tmux", &output)?;
        print!("{}", String::from_utf8_lossy(&output.stdout));
        return Ok(());
    }

    let tmux = ssh_probe(binaries.ssh(), target, "tmux -V")?;
    require_probe_success("remote tmux", &tmux)?;
    print!("{}", String::from_utf8_lossy(&tmux.stdout));

    let herdr = ssh_probe(
        binaries.ssh(),
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

fn ssh_probe(ssh: &Path, target: &str, remote_command: &str) -> Result<std::process::Output> {
    let parsed = openssh_target(target)?;
    let mut arguments = openssh_connection_args(false);
    if let Some(port) = parsed.port {
        arguments.extend(["-p".to_owned(), port.to_string()]);
    }
    arguments.extend([
        "--".to_owned(),
        parsed.destination,
        remote_command.to_owned(),
    ]);
    Command::new(ssh)
        .args(arguments)
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
    if complete {
        let selection = PickerSelection::Create(selection_from_args(&config, &aliases, args)?);
        return execute_selection(paths, &config, selection);
    }

    let observer_placement = args
        .placement
        .map(Placement::from)
        .unwrap_or(config.ui.placement);
    let mut operation_error = None;
    PruneService::new(state_store.clone())
        .automatic_cleanup()
        .context("automatically clear expired removed session history")?;
    loop {
        let state = state_store.load()?;
        let Some(selection) = selection_from_picker(
            &config,
            &aliases,
            &state_store,
            &state,
            args.clone(),
            operation_error.take(),
        )?
        else {
            return Ok(());
        };
        if selection == PickerSelection::ManageObservers {
            match run_observer_manager_flow(observer_placement, paths, &state_store)? {
                ObserverManagerFlow::BackToPicker => continue,
                ObserverManagerFlow::Launched => return Ok(()),
            }
        }
        match execute_selection(paths, &config, selection.clone()) {
            Ok(()) => return Ok(()),
            Err(error) => {
                operation_error = Some((selection, format!("{error:#}")));
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ObserverManagerFlow {
    BackToPicker,
    Launched,
}
fn run_observer_manager_flow(
    placement: Placement,
    paths: &AppPaths,
    state_store: &StateStore,
) -> Result<ObserverManagerFlow> {
    let service = OrchestrationService::new(state_store.clone());
    let mut notice = None;
    loop {
        let state = state_store.load().context("load Observer manager state")?;
        let mut manager = ObserverManagerState::from_state(&state, notice.take())?;
        manager.set_herdr_connected(
            HerdrSocketClient::from_env()
                .and_then(|client| client.snapshot())
                .is_ok_and(|snapshot| snapshot.supports_protocol()),
        );
        match run_observer_manager(manager)? {
            ObserverManagerAction::BackToPicker => return Ok(ObserverManagerFlow::BackToPicker),
            ObserverManagerAction::Create {
                id,
                title,
                orchestrator_session_id,
                workers,
            } => {
                match service.create_group_with_workers(id, title, orchestrator_session_id, workers)
                {
                    Ok(group) => {
                        notice = Some(format!(
                            "Created {}; workload lifecycle unchanged",
                            group.title.as_str()
                        ));
                    }
                    Err(_) => {
                        notice =
                            Some("Could not create Observer; metadata was unchanged".to_owned());
                    }
                }
            }
            ObserverManagerAction::ReplaceWorkers {
                expected_group,
                workers,
            } => match service.replace_workers(&expected_group, workers) {
                Ok(group) => {
                    notice = Some(format!(
                        "Updated {}; workload lifecycle unchanged",
                        group.title.as_str()
                    ));
                }
                Err(error) => {
                    notice = Some(observer_metadata_failure_notice("update", &error));
                }
            },
            ObserverManagerAction::ReassignOrchestrator {
                expected_group,
                orchestrator_session_id,
            } => {
                notice = Some(dispatch_reassign_orchestrator(
                    expected_group,
                    orchestrator_session_id,
                    |expected_group, orchestrator_session_id| {
                        service.reassign_orchestrator(expected_group, orchestrator_session_id)
                    },
                ));
            }
            ObserverManagerAction::SetAgentView { group_id, filter } => {
                let title = state
                    .orchestration_groups
                    .iter()
                    .find(|group| group.id == group_id)
                    .map(|group| group.title.as_str().to_owned());
                let result =
                    AgentViewService::from_env(paths.agent_view_file()).and_then(|agent_views| {
                        agent_views.set_group_filter(&state, &group_id, filter)
                    });
                notice = Some(match (result, title) {
                    (Ok(()), Some(title)) => format!(
                        "Agents sidebar now shows {} for {title}; open members to add them",
                        filter.label()
                    ),
                    (Ok(()), None) => "Agents sidebar view applied".to_owned(),
                    (Err(_), _) => {
                        "Could not apply Agents sidebar view; previous view was preserved"
                            .to_owned()
                    }
                });
            }
            ObserverManagerAction::ClearAgentView => {
                let result = AgentViewService::from_env(paths.agent_view_file())
                    .and_then(|agent_views| agent_views.clear());
                notice = Some(if result.is_ok() {
                    "Restored default Agents sidebar".to_owned()
                } else {
                    "Could not restore default Agents sidebar; previous view was preserved"
                        .to_owned()
                });
            }
            ObserverManagerAction::Delete { expected_group } => {
                let active_view = AgentViewService::from_env(paths.agent_view_file())
                    .and_then(|agent_views| agent_views.clear_group_if_active(&expected_group.id));
                let active_view = match active_view {
                    Ok(active_view) => active_view,
                    Err(_) => {
                        notice = Some(
                            "Could not delete Observer; active Agents view was preserved"
                                .to_owned(),
                        );
                        continue;
                    }
                };
                match service.delete_group_if_unchanged(&expected_group) {
                    Ok(group) => {
                        notice = Some(format!(
                            "Deleted {} metadata; workloads keep running",
                            group.title.as_str()
                        ));
                    }
                    Err(error) => {
                        if active_view {
                            let _ = AgentViewService::from_env(paths.agent_view_file()).and_then(
                                |agent_views| agent_views.set_group(&state, &expected_group.id),
                            );
                        }
                        notice = Some(observer_metadata_failure_notice("delete", &error));
                    }
                }
            }
            ObserverManagerAction::Launch { group_id } => {
                let launch = (|| {
                    let group = service.group(&group_id)?;
                    let context =
                        HerdrContext::from_plugin_env().context(OBSERVER_PLUGIN_CONTEXT_ERROR)?;
                    let executable =
                        env::current_exe().context("locate the bundled Tether executable")?;
                    place_observer_in_herdr(context, executable, &group, placement)
                })();
                match launch {
                    Ok(()) => return Ok(ObserverManagerFlow::Launched),
                    Err(error) => notice = Some(observer_launch_failure_notice(&error)),
                }
            }
        }
    }
}

fn dispatch_reassign_orchestrator<F>(
    expected_group: OrchestrationGroup,
    orchestrator_session_id: SessionId,
    reassign: F,
) -> String
where
    F: FnOnce(&OrchestrationGroup, SessionId) -> Result<OrchestrationGroup>,
{
    match reassign(&expected_group, orchestrator_session_id) {
        Ok(group) => format!(
            "Updated {} metadata; workload lifecycle and placement unchanged",
            group.title.as_str()
        ),
        Err(error)
            if error
                .chain()
                .any(|cause| cause.to_string() == MANAGER_STALE_GROUP_ERROR) =>
        {
            observer_metadata_failure_notice("reassign", &error)
        }
        Err(_) => "Could not reassign Observer; metadata was unchanged; \
                   workload lifecycle and placement unchanged"
            .to_owned(),
    }
}

fn observer_metadata_failure_notice(operation: &str, error: &anyhow::Error) -> String {
    if error
        .chain()
        .any(|cause| cause.to_string() == MANAGER_STALE_GROUP_ERROR)
    {
        return "Observer changed while this screen was open; refreshed current metadata; \
                review and retry"
            .to_owned();
    }
    format!("Could not {operation} Observer; metadata was unchanged")
}

fn observer_launch_failure_notice(error: &anyhow::Error) -> String {
    if error
        .chain()
        .any(|cause| cause.to_string() == OBSERVER_PLUGIN_CONTEXT_ERROR)
    {
        return "Could not launch Observer; open it through prefix+t in a Herdr pane; \
                source pane was preserved"
            .to_owned();
    }
    "Could not launch Observer; source pane was preserved".to_owned()
}

fn execute_selection(paths: &AppPaths, config: &Config, selection: PickerSelection) -> Result<()> {
    match selection {
        PickerSelection::Create(selection) => create_and_attach(paths, config, selection),
        PickerSelection::Resume { id, placement } => resume_and_attach(paths, id, placement),
        PickerSelection::Restart { id, placement } => restart_and_attach(paths, id, placement),
        PickerSelection::AttachExternal {
            host,
            target,
            name,
            placement,
        } => attach_external(host, target, name, placement),
        PickerSelection::ManageObservers => {
            bail!("Observer management must be entered through the interactive Tether picker")
        }
    }
}

fn selection_from_args(
    config: &Config,
    aliases: &[String],
    args: OpenArgs,
) -> Result<OpenSelection> {
    let host_name = args.host.context("--host is required")?;
    let host = resolve_host_from(config, aliases, &host_name)?;
    let (preset, command, preset_agent) = resolve_command(&host, args.preset, args.command)?;
    Ok(OpenSelection {
        host: host_name,
        directory: args.directory.context("--directory is required")?,
        preset,
        herdr_agent: args.herdr_agent.or(preset_agent),
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
    state_store: &StateStore,
    state: &State,
    args: OpenArgs,
    operation_error: Option<(PickerSelection, String)>,
) -> Result<Option<PickerSelection>> {
    let requested_host = args.host.as_deref();
    let mut picker_config = config.clone();
    picker_config.append_alias_hosts(aliases);
    let include_local = match requested_host {
        Some("local") | None => true,
        Some(_) => false,
    };
    if let Some(requested) = requested_host {
        picker_config.hosts.retain(|host| host.name == requested);
    }
    let home = env::var("HOME").unwrap_or_else(|_| "~".to_owned());
    let mut options = PickerOptions::from_config_state(&picker_config, state, &home, include_local);
    let invocation = crate::herdr::InvocationLocation::from_plugin_env();
    options.prefer_invocation_location_with_worktrees(
        invocation.as_ref(),
        state,
        &invocation_worktrees(invocation.as_ref()),
    );
    retain_requested_host_groups(&mut options, requested_host);
    if args.directory.is_some() || args.command.is_some() || args.preset.is_some() {
        options
            .hosts
            .retain(|host| host.origin == PickerHostOrigin::Effective);
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
    let lifecycle_service =
        LifecycleService::new(state_store.clone(), ProcessBinaries::new("ssh", "tmux"));
    let prune_service = PruneService::new(state_store.clone());
    let Some(selection) = run_picker_with_operation_error(
        options,
        status_service,
        discovery_service,
        lifecycle_service,
        prune_service,
        config.retention.closed_days,
        operation_error,
    )?
    else {
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
                let (preset, command, preset_agent) =
                    resolve_command(&host, args.preset, args.command)?;
                selection.preset = preset;
                selection.command = command;
                selection.herdr_agent = args.herdr_agent.clone().or(preset_agent);
            }
            if let Some(herdr_agent) = args.herdr_agent {
                selection.herdr_agent = Some(herdr_agent);
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
        PickerSelection::Restart { id, placement } => Ok(Some(PickerSelection::Restart {
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
        PickerSelection::ManageObservers => Ok(Some(PickerSelection::ManageObservers)),
    }
}

fn retain_requested_host_groups(options: &mut PickerOptions, requested_host: Option<&str>) {
    if let Some(requested_host) = requested_host {
        options.hosts.retain(|host| host.name == requested_host);
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
    let ownership_proof = OwnershipProof::new();
    let backend = backend_for(&host.target)?;
    let launch = LaunchSpec {
        id,
        ownership_proof,
        directory: selection.directory.clone(),
        command: selection.command.clone(),
    };
    let now = Utc::now();
    let title = PaneTitle::owned(
        &selection.host,
        &selection.directory,
        selection.preset.as_deref(),
        Some(&selection.command),
    );
    let herdr_agent = selection.herdr_agent.clone();
    if let Some(kind) = herdr_agent.as_ref() {
        warn_unrecognized_agent_kind(kind);
    }
    let record = SessionRecord {
        id,
        host: selection.host,
        target: host.target,
        directory: selection.directory,
        preset: selection.preset,
        herdr_agent: herdr_agent.clone(),
        command: Some(selection.command),
        tmux_session_id: None,
        ownership_proof: Some(ownership_proof),
        status: SessionStatus::Creating,
        created_at: now,
        last_used_at: now,
        closed_at: None,
        exit_status: None,
    };
    let store = StateStore::new(paths.state_file.clone());
    store.update(|state| {
        state.sessions.push(record);
        Ok(())
    })?;
    let identity = match backend.create(&launch) {
        Ok(identity) => identity,
        Err(error) => {
            if !create_outcome_is_uncertain(&error) {
                let _ = store.update(|state| {
                    state.sessions.retain(|record| {
                        record.id != id
                            || record.status != SessionStatus::Creating
                            || record.tmux_session_id.is_some()
                    });
                    Ok(())
                });
            }
            return Err(error).context(format!("create reserved session `{id}`"));
        }
    };
    store
        .update(|state| {
            let current = state
                .sessions
                .iter_mut()
                .find(|record| record.id == id)
                .with_context(|| format!("reserved session `{id}` disappeared after creation"))?;
            if current.status != SessionStatus::Creating || current.tmux_session_id.is_some() {
                bail!("reserved session `{id}` changed while it was being created");
            }
            current.tmux_session_id = Some(identity);
            current.status = SessionStatus::Running;
            current.last_used_at = Utc::now();
            Ok(())
        })
        .with_context(|| {
            format!(
                "record created session `{id}`; the workload remains safely recoverable from its creating reservation"
            )
        })?;

    if env::var_os("HERDR_BIN_PATH").is_some() {
        let context = HerdrContext::from_env()?;
        let attach = attach_with_agent_hint(
            backend.attach_command(&id, &ownership_proof, identity)?,
            herdr_agent.as_ref(),
        )?;
        let client = HerdrClient::new(context);
        let placed =
            place_in_herdr(&client, &attach, &title, selection.placement).with_context(|| {
                format!(
                    "place newly created session `{id}`; it remains running and recorded for retry"
                )
            })?;
        if report_agent_view_group_for_session(paths, &client, &placed.pane_id, id).is_err() {
            eprintln!("warning: could not label the pane for the active Agents sidebar view");
        }
        println!("created {id}");
        Ok(())
    } else {
        println!("created {id}");
        run_attach(attach_with_agent_hint(
            backend.attach_command(&id, &ownership_proof, identity)?,
            herdr_agent.as_ref(),
        )?)
    }
}
fn restart_and_attach(paths: &AppPaths, id: SessionId, placement: Placement) -> Result<()> {
    let service = LifecycleService::new(
        StateStore::new(paths.state_file.clone()),
        ProcessBinaries::new("ssh", "tmux"),
    );
    let record = service
        .owned_record(id)?
        .with_context(|| format!("unknown Tether session `{id}`"))?;
    match record.status {
        SessionStatus::Ended | SessionStatus::Creating => {
            service
                .restart_owned(id)
                .with_context(|| format!("restart ended session `{id}`"))?;
        }
        SessionStatus::Running => {
            service
                .observe_owned(id)
                .with_context(|| format!("verify restarted session `{id}`"))?;
        }
        SessionStatus::Stopping | SessionStatus::Removed => {
            bail!(
                "session `{id}` cannot be restarted while it is {:?}",
                record.status
            );
        }
    }
    resume_and_attach(paths, id, placement)
}

fn resume_and_attach(paths: &AppPaths, id: SessionId, placement: Placement) -> Result<()> {
    if env::var_os("HERDR_BIN_PATH").is_some() {
        let context = HerdrContext::from_env()?;
        let service = LifecycleService::new(
            StateStore::new(paths.state_file.clone()),
            ProcessBinaries::new("ssh", "tmux"),
        );
        let record = service
            .owned_record(id)?
            .with_context(|| format!("unknown Tether session `{id}`"))?;
        let title = pane_title_for_record(&record);
        let attach = attach_with_agent_hint(service.open_owned(id)?, record.herdr_agent.as_ref())?;
        let client = HerdrClient::new(context);
        let placed = place_in_herdr(&client, &attach, &title, placement)?;
        if report_agent_view_group_for_session(paths, &client, &placed.pane_id, id).is_err() {
            eprintln!("warning: could not label the pane for the active Agents sidebar view");
        }
        Ok(())
    } else {
        session_command(paths, SessionCommand::Open { id })
    }
}

fn pane_title_for_record(record: &SessionRecord) -> PaneTitle {
    PaneTitle::owned(
        &record.host,
        &record.directory,
        record.preset.as_deref(),
        record.command.as_deref(),
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
    host: String,
    target: Option<String>,
    name: ExternalSessionName,
    placement: Placement,
) -> Result<()> {
    let target = target.unwrap_or_else(|| "local".to_owned());
    let title = PaneTitle::external(&host, name.as_str());
    if env::var_os("HERDR_BIN_PATH").is_some() {
        let context = HerdrContext::from_env()?;
        let executable = env::current_exe().context("locate the Tether executable")?;
        let attach = external_attach_command(executable, &target, &name);
        let client = HerdrClient::new(context);
        place_in_herdr(&client, &attach, &title, placement)?;
        Ok(())
    } else {
        let backend = backend_for(&target)?;
        run_attach(backend.attach_external_command(&name)?)
    }
}

fn place_in_herdr(
    client: &HerdrClient,
    command: &CommandSpec,
    title: &PaneTitle,
    placement: Placement,
) -> Result<PlacedPane> {
    if placement != Placement::ReplaceCurrentPane {
        return client.place(command, title, placement);
    }

    confirm_replacement(client)?;
    let placed = client.replace_current(command, title)?;
    if let Some(warning) = &placed.warning {
        eprintln!("warning: {warning}");
    }
    Ok(placed)
}

fn report_agent_view_group_for_session(
    paths: &AppPaths,
    client: &HerdrClient,
    pane_id: &str,
    session_id: SessionId,
) -> Result<()> {
    let state = StateStore::new(paths.state_file.clone()).load_read_only()?;
    let agent_views = AgentViewService::from_env(paths.agent_view_file())?;
    if let Some(group_id) = agent_views.group_for_session(&state, session_id)? {
        let remote = state
            .sessions
            .iter()
            .find(|record| record.id == session_id)
            .is_some_and(|record| record.target != "local");
        client.report_agent_view_group(pane_id, &group_id, remote)?;
    }
    Ok(())
}

fn place_built_in_herdr<F>(
    client: HerdrClient,
    title: &PaneTitle,
    placement: Placement,
    build: F,
) -> Result<()>
where
    F: FnOnce(&HerdrContext) -> Result<CommandSpec>,
{
    if placement != Placement::ReplaceCurrentPane {
        client.place_with_destination(title, placement, build)?;
        return Ok(());
    }
    confirm_replacement(&client)?;
    if let Some(warning) = client
        .replace_current_with_destination(title, build)?
        .warning
    {
        eprintln!("warning: {warning}");
    }
    Ok(())
}

fn place_observer_in_herdr(
    context: HerdrContext,
    executable: PathBuf,
    group: &OrchestrationGroup,
    placement: Placement,
) -> Result<()> {
    let title = PaneTitle::observer(group.title.as_str());
    let group_id = group.id.clone();
    place_built_in_herdr(
        HerdrClient::new(context),
        &title,
        companion_placement(placement),
        |destination| {
            Ok(CommandSpec::new(
                executable,
                vec![
                    "orchestration".to_owned(),
                    "observer-runtime".to_owned(),
                    group_id.to_string(),
                    "--pane-id".to_owned(),
                    destination.pane_id.clone(),
                    "--workspace-id".to_owned(),
                    destination.workspace_id.clone(),
                    "--herdr-bin".to_owned(),
                    destination.binary.display().to_string(),
                ],
            ))
        },
    )
}

fn confirm_replacement(client: &HerdrClient) -> Result<()> {
    let inspection = client.inspect_replacement_source()?;
    if !inspection.requires_confirmation() {
        return Ok(());
    }
    if !stdio_is_terminal() {
        bail!(
            "Replace current pane would terminate {}; an interactive confirmation is required and the source pane was preserved",
            inspection.safe_summary()
        );
    }
    print!(
        "Replace current pane will terminate {}. Continue? [y/N] ",
        inspection.safe_summary()
    );
    io::stdout()
        .flush()
        .context("show replacement confirmation")?;
    let mut response = String::new();
    io::stdin()
        .read_line(&mut response)
        .context("read replacement confirmation")?;
    if !matches!(response.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        bail!("Replace current pane was cancelled; the source pane was preserved");
    }
    Ok(())
}

/// The listed status of a workload, naming a failing exit rather than hiding it.
///
/// `Ended` covers a clean finish, a failing one, and an outcome `tmux` could not
/// report. Only the failing case is renamed, and it carries the status that made
/// it one, because "it ended" and "it failed with 2" call for different next
/// steps.
fn session_status_text(session: &SessionRecord) -> String {
    match (session.status, session.exit_status) {
        (SessionStatus::Ended, Some(exit_status)) if exit_status != 0 => {
            format!("Failed (exit {exit_status})")
        }
        (status, _) => format!("{status:?}"),
    }
}

fn session_command(paths: &AppPaths, command: SessionCommand) -> Result<()> {
    let store = StateStore::new(paths.state_file.clone());
    match command {
        SessionCommand::List(args) => {
            let state = store.load()?;
            let mut sessions = state
                .sessions
                .iter()
                .filter(|record| is_normal_session(record))
                .collect::<Vec<_>>();
            sessions.sort_by(|left, right| {
                compare_normal_sessions(
                    left.status,
                    left.last_used_at,
                    left.id,
                    right.status,
                    right.last_used_at,
                    right.id,
                )
            });
            if args.json {
                let mut sessions = serde_json::to_value(&sessions)?;
                if let Some(records) = sessions.as_array_mut() {
                    for record in records {
                        record
                            .as_object_mut()
                            .expect("serialized session record is an object")
                            .remove("ownership_proof");
                    }
                }
                println!("{}", serde_json::to_string_pretty(&sessions)?);
            } else {
                for session in sessions {
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        session.id,
                        session.host,
                        session.directory,
                        session.last_used_at,
                        session_status_text(session)
                    );
                }
            }
            Ok(())
        }
        SessionCommand::Open { id } => {
            let record = store
                .load()?
                .sessions
                .into_iter()
                .find(|record| record.id == id)
                .with_context(|| format!("unknown session `{id}`"))?;
            match record.status {
                SessionStatus::Running => {}
                SessionStatus::Creating => bail!(
                    "session `{id}` creation is incomplete; run `herdr-tether session restart {id}`"
                ),
                SessionStatus::Stopping => bail!(
                    "session `{id}` is stopping; retry `herdr-tether session open {id}` after Stop completes"
                ),
                SessionStatus::Ended => {
                    bail!("session `{id}` has ended; run `herdr-tether session restart {id}`")
                }
                SessionStatus::Removed => bail!(
                    "session `{id}` was removed; run `herdr-tether open` to create a new workload"
                ),
            }
            let attach = LifecycleService::new(store.clone(), ProcessBinaries::new("ssh", "tmux"))
                .open_owned(id)?;
            run_attach(attach_with_agent_hint(attach, record.herdr_agent.as_ref())?)
        }
        SessionCommand::AttachExternal { target, name } => {
            let backend = backend_for(&target)?;
            run_attach(backend.attach_external_command(&name)?)
        }
        SessionCommand::Restart { id, placement } => {
            let placement = placement.map(Placement::from).unwrap_or(
                ConfigStore::new(paths.config_file.clone())
                    .load()?
                    .ui
                    .placement,
            );
            restart_and_attach(paths, id, placement).with_context(|| {
                format!(
                    "restart session `{id}`; retry `herdr-tether session restart {id}` after resolving the reported error"
                )
            })
        }
        SessionCommand::Stop { id } => {
            LifecycleService::new(store.clone(), ProcessBinaries::new("ssh", "tmux"))
                .close_owned(id)
                .with_context(|| {
                    format!(
                        "stop session `{id}`; retry `herdr-tether session stop {id}` after resolving the reported error"
                    )
                })?;
            println!("stopped {id}");
            Ok(())
        }
        SessionCommand::Remove { id } => {
            let legacy = store
                .load()?
                .sessions
                .iter()
                .find(|record| record.id == id)
                .is_some_and(|record| record.ownership_proof.is_none());
            LifecycleService::new(store.clone(), ProcessBinaries::new("ssh", "tmux"))
                .remove_owned(id)
                .with_context(|| {
                    format!(
                        "remove session metadata `{id}`; retry `herdr-tether session remove {id}` after resolving the reported error"
                    )
                })?;
            if legacy {
                println!("removed legacy metadata {id}; no workload was contacted");
            } else {
                println!("removed {id}");
            }
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

fn orchestration_command(paths: &AppPaths, command: OrchestrationCommand) -> Result<()> {
    let service = OrchestrationService::new(StateStore::new(paths.state_file.clone()));
    match command {
        OrchestrationCommand::Create {
            id,
            title,
            orchestrator,
        } => {
            service.create_group(id.clone(), title, orchestrator)?;
            println!("created {id}");
            Ok(())
        }
        OrchestrationCommand::Delete { id } => {
            service.delete_group(&id)?;
            println!("deleted {id}");
            Ok(())
        }
        OrchestrationCommand::List(args) => {
            let groups = service.list_groups()?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&groups)?);
            } else {
                for group in groups {
                    println!(
                        "{}\t{}\t{}\t{} workers",
                        group.id,
                        group.title.as_str(),
                        group.orchestrator_session_id,
                        group.workers.len()
                    );
                }
            }
            Ok(())
        }
        OrchestrationCommand::AddWorker {
            group,
            session,
            title,
            observe_output,
            open_interactive,
            prompt_agent,
        } => {
            service.add_worker(
                &group,
                session,
                title,
                OrchestrationCapabilities {
                    observe_output,
                    open_interactive,
                    prompt_agent,
                },
            )?;
            println!("added {session} to {group}");
            Ok(())
        }
        OrchestrationCommand::RemoveWorker { group, session } => {
            service.remove_worker(&group, session)?;
            println!("removed {session} from {group}");
            Ok(())
        }
        OrchestrationCommand::Observe { group, placement } => {
            let persisted = service.group(&group)?;
            let context = HerdrContext::from_env()
                .context("orchestration observe must be launched from a Herdr pane")?;
            let executable = env::current_exe().context("locate the Tether executable")?;
            let placement = placement.map(Placement::from).unwrap_or(
                ConfigStore::new(paths.config_file.clone())
                    .load()?
                    .ui
                    .placement,
            );
            place_observer_in_herdr(context, executable, &persisted, placement)
        }
        OrchestrationCommand::ObserverRuntime {
            group,
            pane_id,
            workspace_id,
            herdr_bin,
        } => crate::orchestration::run_observer(
            paths,
            group,
            HerdrContext {
                binary: herdr_bin,
                pane_id,
                workspace_id,
            },
        ),
    }
}

fn prune(store: &StateStore, args: PruneArgs, days: u64, source: &str) -> Result<()> {
    let service = PruneService::new(store.clone());
    let preview = match service.preview(days) {
        Ok(preview) => preview,
        Err(PruneError::RetentionTooLarge(_)) => bail!("{source} is too large"),
        Err(error) => return Err(error.into()),
    };

    if args.dry_run {
        for id in preview.ids() {
            println!("{id}");
        }
        return Ok(());
    }
    let result = service.apply(&preview)?;
    for id in result.removed_ids {
        println!("{id}");
    }
    for id in result.skipped_ids {
        eprintln!("skipped {id}: changed since preview");
    }
    Ok(())
}

/// Lists the worktrees sharing a repository with the invoking pane.
///
/// Purely a picker preference, so an absent capability returns nothing: no
/// Herdr socket, a directory outside a repository, or a rejected request all
/// simply leave the ordering alone, and none of them is worth a warning on
/// every picker open. An answer Tether refuses to use is different: something
/// was reported and deliberately left out, so it is said out loud.
fn invocation_worktrees(location: Option<&crate::herdr::InvocationLocation>) -> Vec<PathBuf> {
    let Some(directory) = location.map(crate::herdr::InvocationLocation::directory) else {
        return Vec::new();
    };
    let Ok(client) = HerdrSocketClient::from_env() else {
        return Vec::new();
    };
    let Ok((paths, refused)) = offered_worktrees(&client, directory) else {
        return Vec::new();
    };
    if let Some(warning) = refused_worktree_warning(refused) {
        eprintln!("{warning}");
    }
    paths
}

/// The reported worktrees Tether will offer, with the number it refused.
///
/// Herdr resolves the repository layout, but a reported path still has to be
/// somewhere the user can work. `--separate-git-dir` and submodule layouts put
/// a repository's Git directory outside its checkout, and a Git directory holds
/// no `.git` entry of its own, so the filesystem separates the two. A path that
/// cannot be confirmed is left out rather than offered.
///
/// A bare repository is left out without being counted. `git worktree list`
/// reports one as a worktree of itself, so it arrives in an ordinary
/// bare-clone-plus-worktrees layout and is not a mistake anybody needs telling
/// about.
///
/// The probes are bounded. Confirming a path is two `stat` calls, and a `stat`
/// on a wedged network mount blocks with no timeout of its own, which would
/// hold the picker closed. The picker preference is worth a few milliseconds
/// and nothing more, so a probe that overruns leaves the ordering alone.
fn offered_worktrees(
    client: &HerdrSocketClient,
    directory: &Path,
) -> Result<(Vec<PathBuf>, usize)> {
    let worktrees = client.worktree_paths(directory)?;
    let reported = worktrees.paths.len();
    let confirmed = run_probe_with_timeout(WORKTREE_PROBE_TIMEOUT, move || {
        let mut paths = Vec::with_capacity(worktrees.paths.len());
        let mut ordinary = 0;
        for path in worktrees.paths {
            if crate::discovery::is_checkout_directory(&path) {
                paths.push(path);
            } else if crate::discovery::is_bare_repository(&path) {
                ordinary += 1;
            }
        }
        (paths, ordinary)
    });
    let Ok((paths, ordinary)) = confirmed else {
        return Ok((Vec::new(), 0));
    };
    let refused = worktrees.rejected + reported - paths.len() - ordinary;
    Ok((paths, refused))
}

/// The warning for worktree paths Tether refused, if there were any.
fn refused_worktree_warning(rejected: usize) -> Option<String> {
    if rejected == 0 {
        return None;
    }
    Some(format!(
        "warning: Herdr reported {rejected} worktree path(s) that Tether cannot use, so they were \
         left out of the picker ordering. A path is left out when it is not absolute, when it \
         names a Git directory rather than a checkout, when there is no readable checkout there, \
         or when the repository reported more worktrees than Tether considers. \
         `--separate-git-dir` and submodule layouts put a repository's Git directory outside its \
         checkout, which is where a checkout and a Git directory get confused."
    ))
}

/// Warns when Herdr does not recognize an explicit agent hint.
///
/// Deliberately advisory. Herdr fetches its agent manifests remotely and updates
/// them independently of Tether, so treating an unknown kind as an error would
/// block a legitimate agent on a Herdr that has not refreshed yet. Silence is
/// the real problem being fixed: an unrecognized kind produces no sidebar row
/// and no Mission Control binding, with nothing to explain why.
fn warn_unrecognized_agent_kind(kind: &HerdrAgentKind) {
    let Ok(client) = HerdrSocketClient::from_env() else {
        return;
    };
    let Ok(kinds) = client.agent_manifest_kinds() else {
        return;
    };
    if kinds.is_empty() || kinds.iter().any(|known| known == kind.as_str()) {
        return;
    }
    eprintln!(
        "warning: Herdr does not recognize agent kind `{kind}`. The workload will run normally, but it will not appear as an agent and Mission Control cannot bind it. Recognized kinds: {}.",
        kinds.join(", ")
    );
}

fn plugin_command(paths: &AppPaths, command: PluginCommand) -> Result<()> {
    match command {
        PluginCommand::Open | PluginCommand::Setup => {
            let context = HerdrContext::from_env()?;
            let entrypoint = match command {
                PluginCommand::Open => "picker",
                PluginCommand::Setup => "setup",
                PluginCommand::RestoreAgentView => unreachable!(),
            };
            HerdrClient::new(context).open_plugin_pane(entrypoint)
        }
        PluginCommand::RestoreAgentView => {
            let state = StateStore::new(paths.state_file.clone()).load_read_only()?;
            AgentViewService::from_env(paths.agent_view_file())?.restore(&state)
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum DoctorStatus {
    Ok,
    Advisory,
    NotChecked,
    Unsupported,
    Missing,
    Unusable,
    Failed,
    TimedOut,
    PermissionDenied,
    Unavailable,
    Incomplete,
    Standalone,
}

#[derive(Debug, Serialize)]
struct DoctorCheck {
    name: &'static str,
    status: DoctorStatus,
    required: bool,
    diagnostic: Option<&'static str>,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    observed_protocol: Option<u32>,
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    schema_version: u8,
    completion: &'static str,
    checks: Vec<DoctorCheck>,
    failure_count: usize,
    truncated: bool,
}

fn doctor(paths: &AppPaths, args: DoctorArgs) -> Result<()> {
    if !args.json {
        return doctor_human(paths);
    }
    let mut checks = Vec::with_capacity(8);

    let config_status = if !paths.config_file.exists() {
        DoctorStatus::Missing
    } else if ConfigStore::new(paths.config_file.clone()).load().is_ok() {
        DoctorStatus::Ok
    } else {
        DoctorStatus::Unusable
    };
    checks.push(doctor_check("config", config_status, true));

    let state_status = if !paths.state_file.exists() {
        DoctorStatus::Missing
    } else if StateStore::new(paths.state_file.clone()).load().is_ok() {
        DoctorStatus::Ok
    } else {
        DoctorStatus::Unusable
    };
    checks.push(doctor_check("state", state_status, true));

    let binaries = ProcessBinaries::new("ssh", "tmux");
    checks.push(probe_binary("tmux", binaries.tmux().to_owned(), "-V"));
    checks.push(probe_binary("ssh", binaries.ssh().to_owned(), "-V"));
    checks.push(probe_binary("cargo", PathBuf::from("cargo"), "--version"));

    let herdr = env::var_os("HERDR_BIN_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("herdr"));
    checks.push(probe_binary("herdr", herdr, "--version"));
    checks.push(doctor_protocol_check());
    let herdr_binary_provided = env::var_os("HERDR_BIN_PATH")
        .is_some_and(|value| !value.to_string_lossy().trim().is_empty());
    let plugin_context_signaled = herdr_binary_provided
        || env::var_os("HERDR_PANE_ID").is_some()
        || env::var_os("HERDR_WORKSPACE_ID").is_some();
    let context_status = if !plugin_context_signaled {
        DoctorStatus::Standalone
    } else {
        let pane = env::var("HERDR_PANE_ID")
            .or_else(|_| env::var("PANE_ID"))
            .ok()
            .is_some_and(|value| !value.trim().is_empty());
        let workspace = env::var("HERDR_WORKSPACE_ID")
            .or_else(|_| env::var("WORKSPACE_ID"))
            .ok()
            .is_some_and(|value| !value.trim().is_empty());
        if pane && workspace && herdr_binary_provided {
            DoctorStatus::Ok
        } else {
            DoctorStatus::Incomplete
        }
    };
    checks.push(doctor_check("herdr_context", context_status, true));

    let failures = checks
        .iter()
        .filter(|check| check.required && !doctor_status_passes(check.status))
        .count();
    if args.json {
        let report = DoctorReport {
            schema_version: 1,
            completion: if failures == 0 { "complete" } else { "failed" },
            checks,
            failure_count: failures,
            truncated: false,
        };
        println!(
            "{}",
            serde_json::to_string(&report).context("serialize doctor JSON")?
        );
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

fn doctor_check(name: &'static str, status: DoctorStatus, required: bool) -> DoctorCheck {
    let diagnostic = match status {
        DoctorStatus::Ok | DoctorStatus::Standalone => None,
        DoctorStatus::Advisory => Some("newer_protocol_unverified"),
        DoctorStatus::NotChecked => Some("socket_not_provided"),
        DoctorStatus::Unsupported => Some("protocol_too_old"),
        DoctorStatus::Missing => Some("not_found"),
        DoctorStatus::Unusable => Some("invalid_data"),
        DoctorStatus::Failed => Some("nonzero_exit"),
        DoctorStatus::TimedOut => Some("timeout"),
        DoctorStatus::PermissionDenied => Some("permission_denied"),
        DoctorStatus::Unavailable => Some("io_error"),
        DoctorStatus::Incomplete => Some("missing_context"),
    };
    DoctorCheck {
        name,
        status,
        required,
        diagnostic,
        truncated: false,
        observed_protocol: None,
    }
}

fn doctor_status_passes(status: DoctorStatus) -> bool {
    matches!(
        status,
        DoctorStatus::Ok
            | DoctorStatus::Advisory
            | DoctorStatus::NotChecked
            | DoctorStatus::Standalone
    )
}

fn doctor_protocol_check() -> DoctorCheck {
    let Some(socket_path) = env::var_os("HERDR_SOCKET_PATH").map(PathBuf::from) else {
        return doctor_check("herdr_protocol", DoctorStatus::NotChecked, false);
    };
    if socket_path.as_os_str().is_empty() {
        return doctor_check("herdr_protocol", DoctorStatus::Incomplete, true);
    }

    let timeout = StdDuration::from_secs(3);
    let snapshot = match run_probe_with_timeout(timeout, move || {
        HerdrSocketClient::new_with_timeout(socket_path, timeout).snapshot()
    }) {
        Ok(Ok(snapshot)) => snapshot,
        Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => {
            return doctor_check("herdr_protocol", DoctorStatus::Unavailable, true);
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            return doctor_check("herdr_protocol", DoctorStatus::TimedOut, true);
        }
    };
    let status = match snapshot.protocol_confidence() {
        ProtocolConfidence::Unsupported => DoctorStatus::Unsupported,
        ProtocolConfidence::Audited => DoctorStatus::Ok,
        ProtocolConfidence::NewerUnverified => DoctorStatus::Advisory,
    };
    let mut check = doctor_check("herdr_protocol", status, true);
    check.observed_protocol = Some(snapshot.protocol);
    check
}

/// Runs a blocking probe on another thread and gives up on it after `timeout`.
///
/// Used where a probe can block with no bound of its own — a socket that never
/// answers, a `stat` on a wedged network mount — and the caller has something
/// better to do than wait. The thread is left to finish on its own; there is no
/// way to cancel a blocking syscall, and the abandoned answer is discarded.
fn run_probe_with_timeout<T, F>(
    timeout: StdDuration,
    probe: F,
) -> std::result::Result<T, mpsc::RecvTimeoutError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = sender.send(probe());
    });
    receiver.recv_timeout(timeout)
}

fn probe_binary(name: &'static str, program: PathBuf, version_flag: &str) -> DoctorCheck {
    let spec = CommandSpec::new(program, vec![version_flag.to_owned()]);
    let cancelled = AtomicBool::new(false);
    let status = match run_bounded(&spec, StdDuration::from_secs(3), &cancelled) {
        BoundedOutput::Completed { status, .. } if status.success() => DoctorStatus::Ok,
        BoundedOutput::Completed { .. } => DoctorStatus::Failed,
        BoundedOutput::TimedOut => DoctorStatus::TimedOut,
        BoundedOutput::SpawnError(std::io::ErrorKind::NotFound) => DoctorStatus::Missing,
        BoundedOutput::SpawnError(std::io::ErrorKind::PermissionDenied) => {
            DoctorStatus::PermissionDenied
        }
        BoundedOutput::SpawnError(_) | BoundedOutput::Error => DoctorStatus::Unavailable,
        BoundedOutput::Cancelled => unreachable!("doctor probes are never cancelled"),
    };
    doctor_check(name, status, true)
}

fn doctor_human(paths: &AppPaths) -> Result<()> {
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
    let binaries = ProcessBinaries::new("ssh", "tmux");
    failures += usize::from(!report_binary_path(binaries.tmux().to_owned(), &["-V"]));
    failures += usize::from(!report_binary_path(binaries.ssh().to_owned(), &["-V"]));
    failures += usize::from(!report_binary("cargo", &["--version"]));
    let herdr = env::var_os("HERDR_BIN_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("herdr"));
    println!("Herdr binary: {}", herdr.display());
    failures += usize::from(!report_binary_path(herdr, &["--version"]));
    failures += usize::from(!report_herdr_protocol());
    let binary = env::var_os("HERDR_BIN_PATH")
        .is_some_and(|value| !value.to_string_lossy().trim().is_empty());
    let signaled = binary
        || env::var_os("HERDR_PANE_ID").is_some()
        || env::var_os("HERDR_WORKSPACE_ID").is_some();
    if !signaled {
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
        match (pane, workspace, binary) {
            (Some(pane), Some(workspace), true) => {
                println!("Herdr context: {pane} in workspace {workspace}")
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

fn report_herdr_protocol() -> bool {
    let check = doctor_protocol_check();
    match check.status {
        DoctorStatus::Ok => println!(
            "Herdr protocol: {} audited",
            check
                .observed_protocol
                .expect("audited protocol is observed")
        ),
        DoctorStatus::Advisory => println!(
            "Herdr protocol: {} newer than audited {}; continuing (update Tether when compatibility support ships)",
            check
                .observed_protocol
                .expect("advisory protocol is observed"),
            MAX_AUDITED_HERDR_PROTOCOL
        ),
        DoctorStatus::Unsupported => println!(
            "Herdr protocol: {} unsupported; requires {}+",
            check
                .observed_protocol
                .expect("unsupported protocol is observed"),
            MIN_HERDR_PROTOCOL
        ),
        DoctorStatus::NotChecked => {
            println!("Herdr protocol: not checked (HERDR_SOCKET_PATH not provided)")
        }
        DoctorStatus::Incomplete => {
            println!("Herdr protocol: unavailable (HERDR_SOCKET_PATH is empty)")
        }
        DoctorStatus::Unavailable => println!(
            "Herdr protocol: unavailable (verify HERDR_SOCKET_PATH points to a running Herdr)"
        ),
        DoctorStatus::TimedOut => println!(
            "Herdr protocol: timed out after 3s (verify HERDR_SOCKET_PATH points to a responsive Herdr)"
        ),
        _ => unreachable!("protocol probe uses only protocol-specific doctor statuses"),
    }
    doctor_status_passes(check.status)
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

fn resolve_command(
    host: &HostConfig,
    preset: Option<String>,
    command: Option<String>,
) -> Result<(Option<String>, String, Option<HerdrAgentKind>)> {
    match (preset, command) {
        (Some(name), None) => {
            let preset = host
                .presets
                .iter()
                .find(|preset| preset.name == name)
                .with_context(|| format!("unknown preset `{name}` for host `{}`", host.name))?;
            Ok((
                Some(name),
                preset.command.clone(),
                preset.herdr_agent.clone(),
            ))
        }
        (None, Some(command)) if !command.trim().is_empty() => Ok((None, command, None)),
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

fn attach_with_agent_hint(
    spec: CommandSpec,
    herdr_agent: Option<&HerdrAgentKind>,
) -> Result<CommandSpec> {
    let Some(herdr_agent) = herdr_agent else {
        return Ok(spec);
    };
    let program = spec.program.to_str().with_context(|| {
        format!(
            "attach program path `{}` is not valid UTF-8",
            spec.program.display()
        )
    })?;
    let mut arguments = Vec::with_capacity(spec.args.len() + 2);
    arguments.push(format!("HERDR_AGENT={herdr_agent}"));
    arguments.push(program.to_owned());
    arguments.extend(spec.args);
    Ok(CommandSpec::new("env", arguments))
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
    fn a_probe_deadline_bounds_a_stalled_operation() {
        let result = run_probe_with_timeout(StdDuration::from_millis(20), || {
            std::thread::sleep(StdDuration::from_secs(1));
        });

        assert!(matches!(result, Err(mpsc::RecvTimeoutError::Timeout)));
    }

    #[test]
    fn a_listed_workload_names_a_failing_exit_and_nothing_else() {
        let mut record = SessionRecord {
            herdr_agent: None,
            id: "tether-0197f198000070008000000000000001".parse().unwrap(),
            host: "local".to_owned(),
            target: "local".to_owned(),
            directory: "/tmp".to_owned(),
            preset: None,
            command: None,
            tmux_session_id: None,
            ownership_proof: None,
            status: SessionStatus::Ended,
            created_at: Utc::now(),
            last_used_at: Utc::now(),
            closed_at: Some(Utc::now()),
            exit_status: Some(2),
        };
        assert_eq!(session_status_text(&record), "Failed (exit 2)");

        record.exit_status = Some(0);
        assert_eq!(session_status_text(&record), "Ended");
        record.exit_status = None;
        assert_eq!(
            session_status_text(&record),
            "Ended",
            "an outcome tmux could not report is not a failure"
        );

        record.status = SessionStatus::Running;
        record.exit_status = None;
        record.closed_at = None;
        assert_eq!(session_status_text(&record), "Running");
    }

    #[test]
    fn refused_worktree_paths_are_reported_rather_than_dropped_in_silence() {
        // A shortened list is indistinguishable from a repository with fewer
        // worktrees, so the picker would just order things oddly with no
        // explanation. Nothing is said when nothing was refused, because an
        // older Herdr that cannot answer at all is an ordinary state.
        assert_eq!(refused_worktree_warning(0), None);
        let warning = refused_worktree_warning(2).expect("a refusal is worth saying");
        assert!(warning.starts_with("warning: Herdr reported 2 worktree path(s)"));
        assert!(
            warning.contains("--separate-git-dir") && warning.contains("submodule"),
            "the message must name the layouts that produce the refused shape"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_reported_path_that_is_not_a_checkout_is_left_out_and_counted() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;

        let temp = tempfile::tempdir().unwrap();
        // A `--separate-git-dir` checkout, and the Git directory it points at.
        // Nothing about the second path says what it is, so only the filesystem
        // can tell them apart.
        let checkout = temp.path().join("app");
        let separate = temp.path().join("gitdirs/app");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::create_dir_all(&separate).unwrap();
        std::fs::write(
            checkout.join(".git"),
            format!("gitdir: {}\n", separate.display()),
        )
        .unwrap();
        // A submodule's Git directory, built as a real checkout-looking
        // directory so that only its shape can be what refuses it.
        let submodule_gitdir = temp.path().join(".git/modules/lib");
        std::fs::create_dir_all(&submodule_gitdir).unwrap();
        std::fs::write(submodule_gitdir.join(".git"), "gitdir: elsewhere\n").unwrap();
        // A bare repository, which `git worktree list` reports as a worktree of
        // itself. Not somewhere to work, and not a mistake either.
        let bare = temp.path().join("proj.git");
        std::fs::create_dir_all(bare.join("objects")).unwrap();
        std::fs::create_dir_all(bare.join("refs")).unwrap();
        std::fs::write(bare.join("HEAD"), "ref: refs/heads/main\n").unwrap();

        let socket = temp.path().join("herdr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let reported = [
            checkout.display().to_string(),
            separate.display().to_string(),
            submodule_gitdir.display().to_string(),
            bare.display().to_string(),
        ];
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            let request: serde_json::Value = serde_json::from_str(&request).unwrap();
            let id = request["id"].as_str().unwrap();
            let worktrees: Vec<serde_json::Value> = reported
                .iter()
                .map(|path| serde_json::json!({"path": path}))
                .collect();
            writeln!(
                stream,
                "{}",
                serde_json::json!({
                    "id": id,
                    "result": {"type": "worktree_list", "worktrees": worktrees},
                })
            )
            .unwrap();
        });

        let client = HerdrSocketClient::new(socket);
        let (paths, refused) = offered_worktrees(&client, &checkout).unwrap();
        server.join().unwrap();

        assert_eq!(paths, [checkout]);
        assert_eq!(
            refused, 2,
            "the Git directory and the gitdir path, but not the bare repository"
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

    #[test]
    fn explicit_agent_hint_wraps_the_host_visible_attach_process() {
        let command = CommandSpec::new(
            "/usr/bin/tmux",
            vec![
                "attach-session".into(),
                "-t".into(),
                "=tether-session".into(),
            ],
        );
        let agent = "codex".parse::<HerdrAgentKind>().unwrap();

        let hinted = attach_with_agent_hint(command, Some(&agent)).unwrap();

        assert_eq!(hinted.program, PathBuf::from("env"));
        assert_eq!(
            hinted.args,
            [
                "HERDR_AGENT=codex",
                "/usr/bin/tmux",
                "attach-session",
                "-t",
                "=tether-session"
            ]
        );
    }

    #[test]
    fn reopening_uses_only_safe_stored_presentation_metadata() {
        let now = Utc::now();
        let record = SessionRecord {
            herdr_agent: None,
            id: SessionId::new(),
            host: "build-box".into(),
            target: "builder@example.test".into(),
            directory: "/srv/repository".into(),
            preset: None,
            command: Some("exec /opt/agents/codex --token raw-secret".into()),
            tmux_session_id: None,
            ownership_proof: None,
            status: SessionStatus::Running,
            created_at: now,
            last_used_at: now,
            closed_at: None,
            exit_status: None,
        };

        assert_eq!(
            pane_title_for_record(&record),
            PaneTitle::owned("build-box", "/srv/repository", None, Some("codex"),)
        );
    }

    #[test]
    fn requested_host_scope_keeps_only_matching_effective_and_retained_groups() {
        use crate::tui::{PickerCommand, PickerHost};

        let host = |name: &str, target: &str, origin: PickerHostOrigin| PickerHost {
            name: name.into(),
            label: name.into(),
            target: Some(target.into()),
            origin,
            directories: vec!["/repo".into()],
            scan_roots: vec!["/repo".into()],
            commands: vec![PickerCommand::Shell],
            workloads: Vec::new(),
            allow_existing: origin == PickerHostOrigin::Effective,
            allow_create: origin == PickerHostOrigin::Effective,
        };
        let mut options = PickerOptions {
            hosts: vec![
                host("build", "new.example", PickerHostOrigin::Effective),
                host("build", "old.example", PickerHostOrigin::Retained),
                host("foreign", "foreign.example", PickerHostOrigin::Retained),
                host("local", "local", PickerHostOrigin::Retained),
            ],
            default_placement: Placement::SplitRight,
        };

        retain_requested_host_groups(&mut options, Some("build"));

        assert_eq!(options.hosts.len(), 2);
        assert!(options.hosts.iter().all(|host| host.name == "build"));
        assert_eq!(options.hosts[0].origin, PickerHostOrigin::Effective);
        assert_eq!(options.hosts[1].origin, PickerHostOrigin::Retained);
    }

    #[test]
    fn missing_plugin_context_launch_notice_is_actionable_and_safe() {
        let error = anyhow::anyhow!("HERDR_PANE_ID is not set")
            .context("Observer must be launched from the Tether plugin pane");

        let notice = observer_launch_failure_notice(&error);

        assert!(notice.contains("prefix+t"));
        assert!(notice.contains("Herdr pane"));
        assert!(notice.contains("source pane was preserved"));
        assert!(!notice.contains("HERDR_PANE_ID"));
    }

    #[test]
    fn stale_manager_snapshot_notice_reports_refresh_without_identifiers() {
        let error = anyhow::anyhow!(MANAGER_STALE_GROUP_ERROR);

        let notice = observer_metadata_failure_notice("delete", &error);

        assert!(notice.contains("changed while this screen was open"));
        assert!(notice.contains("refreshed current metadata"));
        assert!(notice.contains("review and retry"));
        assert!(!notice.contains("observer-"));
    }
    fn manager_group(orchestrator_session_id: SessionId) -> OrchestrationGroup {
        OrchestrationGroup {
            id: "observer-build-atlas".parse().unwrap(),
            title: "Observer build atlas".parse().unwrap(),
            orchestrator_session_id,
            workers: Vec::new(),
        }
    }

    #[test]
    fn reassign_dispatch_forwards_exact_snapshot_and_replacement_once() {
        let original = SessionId::new();
        let replacement = SessionId::new();
        let expected = manager_group(original);
        let returned = manager_group(replacement);
        let mut calls = 0;

        let notice = dispatch_reassign_orchestrator(
            expected.clone(),
            replacement,
            |actual_expected, actual_replacement| {
                calls += 1;
                assert_eq!(actual_expected, &expected);
                assert_eq!(actual_replacement, replacement);
                Ok(returned.clone())
            },
        );

        assert_eq!(calls, 1);
        assert!(notice.contains("Updated Observer build atlas metadata"));
        assert!(notice.contains("workload lifecycle and placement unchanged"));
        assert!(!notice.contains(&replacement.to_string()));
    }

    #[test]
    fn stale_reassign_dispatch_notice_is_actionable_and_identifier_free() {
        let replacement = SessionId::new();
        let expected = manager_group(SessionId::new());

        let notice = dispatch_reassign_orchestrator(expected, replacement, |_, _| {
            Err(anyhow::anyhow!(MANAGER_STALE_GROUP_ERROR))
        });

        assert!(notice.contains("changed while this screen was open"));
        assert!(notice.contains("refreshed current metadata"));
        assert!(notice.contains("review and retry"));
        assert!(!notice.contains(&replacement.to_string()));
        assert!(!notice.contains("observer-"));
    }

    #[test]
    fn failed_reassign_dispatch_reports_metadata_unchanged_without_identifiers() {
        let replacement = SessionId::new();
        let expected = manager_group(SessionId::new());
        let mut calls = 0;

        let notice = dispatch_reassign_orchestrator(expected, replacement, |_, _| {
            calls += 1;
            Err(anyhow::anyhow!("replacement became ineligible"))
        });

        assert_eq!(calls, 1);
        assert_eq!(
            notice,
            "Could not reassign Observer; metadata was unchanged; workload lifecycle and placement unchanged"
        );
        assert!(!notice.contains(&replacement.to_string()));
    }
}
