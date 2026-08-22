use std::{
    collections::HashSet,
    env,
    fs::{self, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use crate::{
    model::{HerdrAgentKind, Placement},
    storage::{atomic_write_resolved, with_advisory_lock, with_advisory_lock_preserving_parent},
};

const MAX_SERIALIZABLE_INTEGER: u64 = i64::MAX as u64;

pub const TETHER_KEYBINDING: &str = r#"[[keys.command]]
key = "prefix+t"
type = "plugin_action"
command = "moneycaringcoder.tether.open"
description = "Tether: Open"
"#;

const TETHER_KEY: &str = "prefix+t";
const TETHER_ACTION: &str = "moneycaringcoder.tether.open";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HerdrKeybindingInstall {
    Installed { backup: PathBuf },
    AlreadyInstalled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HerdrKeybindingRollback {
    Restored,
}

#[derive(Clone, Debug)]
pub struct HerdrKeybindingStore {
    path: PathBuf,
}

impl HerdrKeybindingStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path_from_env() -> Result<PathBuf> {
        if let Some(path) = env::var_os("HERDR_CONFIG_PATH") {
            return Ok(PathBuf::from(path));
        }
        if let Some(config_home) = env::var_os("XDG_CONFIG_HOME") {
            return Ok(PathBuf::from(config_home).join("herdr/config.toml"));
        }
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set and XDG_CONFIG_HOME is unavailable")?;
        Ok(home.join(".config/herdr/config.toml"))
    }

    /// Names the keybinding backup beside `path`.
    ///
    /// Callers pass the link-resolved config path, so the backup is a sibling
    /// of the real config file. The backup itself is Tether-owned and is never
    /// link-resolved: an existing non-regular backup is rejected rather than
    /// followed.
    pub fn backup_path_for(path: &Path) -> PathBuf {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".tether-keybinding.bak");
        path.with_file_name(name)
    }

    pub fn install(&self) -> Result<HerdrKeybindingInstall> {
        with_advisory_lock_preserving_parent(&self.path, |path| {
            Self::new(path.to_owned()).install_unlocked()
        })
    }

    pub fn rollback(&self) -> Result<HerdrKeybindingRollback> {
        with_advisory_lock_preserving_parent(&self.path, |path| {
            let store = Self::new(path.to_owned());
            let backup = Self::backup_path_for(&store.path);
            let (bytes, permissions) = read_regular_bytes(&backup).with_context(|| {
                format!(
                    "read Tether keybinding backup `{}`; no rollback was performed",
                    backup.display()
                )
            })?;
            let (current, _) = read_regular_bytes(&store.path).with_context(|| {
                format!(
                    "read current Herdr config `{}`; no rollback was performed",
                    store.path.display()
                )
            })?;
            let installed = append_keybinding(&bytes);
            if current != installed && current != bytes {
                bail!(
                    "Herdr config changed after Tether installed the keybinding; rollback refused without overwriting those edits"
                );
            }
            atomic_write_resolved(&store.path, &bytes)
                .with_context(|| format!("restore Herdr config from `{}`", backup.display()))?;
            open_regular_file(&store.path)
                .with_context(|| {
                    format!(
                        "open Herdr config `{}` to restore permissions",
                        store.path.display()
                    )
                })?
                .set_permissions(permissions)
                .with_context(|| format!("restore permissions on `{}`", store.path.display()))?;
            fs::remove_file(&backup)
                .with_context(|| format!("consume restored backup `{}`", backup.display()))?;
            Ok(HerdrKeybindingRollback::Restored)
        })
    }

    fn install_unlocked(&self) -> Result<HerdrKeybindingInstall> {
        let (source, permissions) = match read_regular_bytes(&self.path) {
            Ok((source, permissions)) => (source, Some(permissions)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (Vec::new(), None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read Herdr config `{}`", self.path.display()));
            }
        };
        let text = std::str::from_utf8(&source)
            .with_context(|| format!("Herdr config `{}` is not UTF-8", self.path.display()))?;
        let document: toml::Value = if text.trim().is_empty() {
            toml::Value::Table(toml::map::Map::new())
        } else {
            toml::from_str(text)
                .with_context(|| format!("parse Herdr config `{}` as TOML", self.path.display()))?
        };

        let mut identical_binding = false;
        if let Some(keys) = document.get("keys").and_then(toml::Value::as_table) {
            if let Some(commands) = keys.get("command").and_then(toml::Value::as_array) {
                for command in commands {
                    if !command.get("key").is_some_and(value_contains_tether_key) {
                        continue;
                    }
                    if !identical_binding
                        && command.get("type").and_then(toml::Value::as_str)
                            == Some("plugin_action")
                        && command.get("command").and_then(toml::Value::as_str)
                            == Some(TETHER_ACTION)
                    {
                        identical_binding = true;
                        continue;
                    }
                    bail!(
                        "Herdr key `prefix+t` is already bound; config was not changed and the existing command was not displayed"
                    );
                }
            }
            if keys.iter().any(|(name, value)| {
                name != "command" && name != "prefix" && value_contains_tether_key(value)
            }) {
                bail!(
                    "Herdr key `prefix+t` is already bound; config was not changed and the existing action was not displayed"
                );
            }
        }
        if identical_binding {
            return Ok(HerdrKeybindingInstall::AlreadyInstalled);
        }

        let backup = Self::backup_path_for(&self.path);
        if backup.exists() {
            let (backup_bytes, _) = read_regular_bytes(&backup).with_context(|| {
                format!("read existing keybinding backup `{}`", backup.display())
            })?;
            if backup_bytes == source {
                fs::remove_file(&backup).with_context(|| {
                    format!("remove consumed keybinding backup `{}`", backup.display())
                })?;
            } else {
                bail!(
                    "Tether keybinding backup `{}` already exists; config was not changed",
                    backup.display()
                );
            }
        }

        let updated = append_keybinding(&source);
        let updated_text =
            std::str::from_utf8(&updated).context("Tether keybinding candidate was not UTF-8")?;
        toml::from_str::<toml::Value>(updated_text).with_context(|| {
            format!(
                "Tether keybinding cannot be merged into Herdr config `{}`; config was not changed",
                self.path.display()
            )
        })?;
        atomic_write_resolved(&backup, &source)
            .with_context(|| format!("create keybinding backup `{}`", backup.display()))?;
        if let Some(permissions) = permissions.as_ref() {
            open_regular_file(&backup)
                .with_context(|| {
                    format!(
                        "open keybinding backup `{}` to preserve permissions",
                        backup.display()
                    )
                })?
                .set_permissions(permissions.clone())
                .with_context(|| format!("preserve permissions on `{}`", backup.display()))?;
        }

        atomic_write_resolved(&self.path, &updated).with_context(|| {
            format!(
                "install Tether keybinding; original remains at `{}`",
                backup.display()
            )
        })?;
        if let Some(permissions) = permissions {
            open_regular_file(&self.path)
                .with_context(|| {
                    format!(
                        "open Herdr config `{}` to preserve permissions",
                        self.path.display()
                    )
                })?
                .set_permissions(permissions)
                .with_context(|| format!("preserve permissions on `{}`", self.path.display()))?;
        }
        Ok(HerdrKeybindingInstall::Installed { backup })
    }
}

fn append_keybinding(source: &[u8]) -> Vec<u8> {
    let mut updated = Vec::with_capacity(source.len() + TETHER_KEYBINDING.len() + 2);
    updated.extend_from_slice(source);
    if !source.is_empty() && !source.ends_with(b"\n") {
        updated.push(b'\n');
    }
    if !source.is_empty() {
        updated.push(b'\n');
    }
    updated.extend_from_slice(TETHER_KEYBINDING.as_bytes());
    updated
}

fn value_contains_tether_key(value: &toml::Value) -> bool {
    match value {
        toml::Value::String(key) => key == TETHER_KEY,
        toml::Value::Array(keys) => keys.iter().any(value_contains_tether_key),
        _ => false,
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommandPreset {
    pub name: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub herdr_agent: Option<HerdrAgentKind>,
    /// Optional command that reports whether the workload is serving.
    ///
    /// "The process is alive" and "the thing is actually serving" are different
    /// facts, and for a dev server or a watcher the second is the one that
    /// matters. Absent, Tether says nothing about serving rather than guessing
    /// from liveness.
    ///
    /// Trusted code, exactly like `command`: it runs through `/bin/sh -c` on the
    /// workload's host, in the workload's directory. Exit zero means serving;
    /// any other exit means not serving; a probe that cannot run or does not
    /// finish is unknown, never a pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_command: Option<String>,
}

impl CommandPreset {
    pub const MAX_COMMAND_BYTES: usize = 256 * 1024;
}

/// Configured text with anything that could break a line or reorder one made
/// visible.
///
/// Names, targets, and command bodies are validated for length and uniqueness,
/// never for characters, and a configuration file is text a user may not have
/// written. Three classes are spelled out rather than passed through: C0 and C1
/// controls, which is where a terminal escape sequence arrives; the Unicode line
/// and paragraph separators, which let one field forge another's line; and the
/// bidirectional and zero-width formatting characters, which can display a
/// command in an order or a spelling it will not run in.
///
/// Every surface that shows configured text to a person goes through this, so
/// the picker and `host presets` describe a preset the same way. Anything that
/// needs the original reads `host presets --json`, which escapes for a parser
/// rather than for a terminal.
pub fn escaped_config_text(text: &str) -> String {
    text.chars()
        .map(|character| match character {
            '\n' => "\\n".to_owned(),
            '\t' => "\\t".to_owned(),
            '\r' => "\\r".to_owned(),
            '\u{2028}' | '\u{2029}' | '\u{200b}'..='\u{200f}' => {
                format!("\\u{{{:04x}}}", character as u32)
            }
            '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => {
                format!("\\u{{{:04x}}}", character as u32)
            }
            character if character.is_control() => format!("\\u{{{:04x}}}", character as u32),
            character => character.to_string(),
        })
        .collect()
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    pub name: String,
    pub target: String,
    pub roots: Vec<String>,
    pub presets: Vec<CommandPreset>,
}

impl HostConfig {
    pub const MAX_ROOTS: usize = 1_024;
    pub const MAX_PRESETS: usize = 256;
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UiDefaults {
    pub placement: Placement,
}

impl Default for UiDefaults {
    fn default() -> Self {
        Self {
            placement: Placement::SplitRight,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryDefaults {
    pub local_roots: Vec<String>,
    pub max_depth: usize,
    pub max_entries: usize,
    pub max_results: usize,
    pub timeout_seconds: u64,
    pub workers: usize,
}

impl DiscoveryDefaults {
    pub const MAX_LOCAL_ROOTS: usize = 1_024;
}

impl Default for DiscoveryDefaults {
    fn default() -> Self {
        Self {
            local_roots: Vec::new(),
            max_depth: 4,
            max_entries: 4096,
            max_results: 64,
            timeout_seconds: 3,
            workers: 4,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionDefaults {
    pub closed_days: u64,
}

impl Default for RetentionDefaults {
    fn default() -> Self {
        Self { closed_days: 30 }
    }
}

/// Opt-in Herdr toasts for events that want a person's attention.
///
/// These are advisory. Herdr delivers a toast only when the user has enabled
/// `ui.toast.delivery`, and Tether's own surfaces remain the authoritative view
/// either way.
///
/// Missing fields fall back to these defaults rather than to `false`, so a
/// configuration written before a setting existed keeps behaving as documented
/// instead of silently opting out of it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotificationDefaults {
    /// Notify when a Mission Control agent starts waiting on a person.
    pub agent_blocked: bool,
    /// Notify when a Mission Control agent settles as done.
    pub agent_done: bool,
    /// Notify when a workload's command ends with a failing status.
    pub workload_failed: bool,
}

impl Default for NotificationDefaults {
    fn default() -> Self {
        Self {
            agent_blocked: true,
            agent_done: true,
            workload_failed: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub hosts: Vec<HostConfig>,
    pub ui: UiDefaults,
    #[serde(default)]
    pub notifications: NotificationDefaults,
    pub discovery: DiscoveryDefaults,
    pub retention: RetentionDefaults,
}

impl Config {
    pub const CURRENT_VERSION: u32 = 3;
    pub const MAX_HOSTS: usize = 256;
    pub const MAX_STRING_BYTES: usize = 16 * 1024;
    pub fn add_host(&mut self, host: HostConfig) -> Result<()> {
        if self.hosts.iter().any(|existing| existing.name == host.name) {
            bail!("host `{}` already exists", host.name);
        }

        self.hosts.push(host);
        if let Err(error) = self.validate() {
            self.hosts.pop();
            return Err(error);
        }
        Ok(())
    }

    /// Appends discovered SSH aliases after configured hosts without allowing
    /// aliases to shadow a configured host name.
    pub(crate) fn append_alias_hosts(&mut self, aliases: &[String]) {
        for alias in aliases {
            if !self.hosts.iter().any(|host| host.name == *alias) {
                self.hosts.push(HostConfig {
                    name: alias.clone(),
                    target: alias.clone(),
                    roots: Vec::new(),
                    presets: Vec::new(),
                });
            }
        }
    }

    pub fn remove_host(&mut self, name: &str) -> bool {
        let Some(index) = self.hosts.iter().position(|host| host.name == name) else {
            return false;
        };
        self.hosts.remove(index);
        true
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != Self::CURRENT_VERSION {
            bail!(
                "unsupported config version {}; expected {}",
                self.version,
                Self::CURRENT_VERSION
            );
        }
        if self.hosts.len() > Self::MAX_HOSTS {
            bail!(
                "config hosts may contain at most {} entries",
                Self::MAX_HOSTS
            );
        }
        if self.discovery.local_roots.len() > DiscoveryDefaults::MAX_LOCAL_ROOTS {
            bail!(
                "discovery local roots may contain at most {} entries",
                DiscoveryDefaults::MAX_LOCAL_ROOTS
            );
        }

        for (root_index, root) in self.discovery.local_roots.iter().enumerate() {
            require_nonempty(root, &format!("discovery local root at index {root_index}"))?;
            require_max_bytes(root, Self::MAX_STRING_BYTES, "discovery local root")?;
        }
        require_positive_usize(self.discovery.max_depth, "discovery max_depth")?;
        require_positive_usize(self.discovery.max_entries, "discovery max_entries")?;
        require_positive_usize(self.discovery.max_results, "discovery max_results")?;
        require_positive_u64(self.discovery.timeout_seconds, "discovery timeout_seconds")?;
        if self.discovery.timeout_seconds > 3600 {
            bail!("discovery timeout_seconds must be at most 3600");
        }
        require_positive_usize(self.discovery.workers, "discovery workers")?;
        require_positive_u64(self.retention.closed_days, "retention closed_days")?;

        let mut host_names = HashSet::with_capacity(self.hosts.len());
        for (host_index, host) in self.hosts.iter().enumerate() {
            let location = format!("host at index {host_index}");
            if host.roots.len() > HostConfig::MAX_ROOTS {
                bail!(
                    "host roots may contain at most {} entries",
                    HostConfig::MAX_ROOTS
                );
            }
            if host.presets.len() > HostConfig::MAX_PRESETS {
                bail!(
                    "host presets may contain at most {} entries",
                    HostConfig::MAX_PRESETS
                );
            }
            require_nonempty(&host.name, &format!("{location} name"))?;
            require_max_bytes(&host.name, Self::MAX_STRING_BYTES, "name")?;
            if host.name.eq_ignore_ascii_case("local") {
                bail!("host name `{}` is reserved", host.name);
            }
            if !host_names.insert(host.name.as_str()) {
                bail!("duplicate host name `{}`", host.name);
            }
            require_nonempty(&host.target, &format!("host `{}` target", host.name))?;
            require_max_bytes(&host.target, Self::MAX_STRING_BYTES, "target")?;
            crate::sshcfg::validate_ssh_target(&host.target)
                .with_context(|| format!("invalid target for host `{}`", host.name))?;

            for (root_index, root) in host.roots.iter().enumerate() {
                require_nonempty(
                    root,
                    &format!("root at index {root_index} for host `{}`", host.name),
                )?;
                require_max_bytes(root, Self::MAX_STRING_BYTES, "root")?;
            }

            let mut preset_names = HashSet::with_capacity(host.presets.len());
            for (preset_index, preset) in host.presets.iter().enumerate() {
                require_nonempty(
                    &preset.name,
                    &format!(
                        "preset at index {preset_index} for host `{}` name",
                        host.name
                    ),
                )?;
                require_max_bytes(&preset.name, Self::MAX_STRING_BYTES, "preset name")?;
                if !preset_names.insert(preset.name.as_str()) {
                    bail!(
                        "duplicate preset name `{}` for host `{}`",
                        preset.name,
                        host.name
                    );
                }
                require_nonempty(
                    &preset.command,
                    &format!("preset `{}` for host `{}` command", preset.name, host.name),
                )?;
                require_max_bytes(
                    &preset.command,
                    CommandPreset::MAX_COMMAND_BYTES,
                    "preset command",
                )?;
                if let Some(health_command) = &preset.health_command {
                    // Held to the same standard as the command it checks: an
                    // empty or oversized probe is a configuration mistake, not
                    // a workload that cannot be checked.
                    require_nonempty(
                        health_command,
                        &format!(
                            "preset `{}` for host `{}` health command",
                            preset.name, host.name
                        ),
                    )?;
                    require_max_bytes(
                        health_command,
                        CommandPreset::MAX_COMMAND_BYTES,
                        "preset health command",
                    )?;
                }
            }
        }
        Ok(())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: Self::CURRENT_VERSION,
            hosts: Vec::new(),
            ui: UiDefaults::default(),
            notifications: NotificationDefaults::default(),
            discovery: DiscoveryDefaults::default(),
            retention: RetentionDefaults::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub const MAX_PERSISTED_BYTES: usize = 8 * 1024 * 1024;
    pub const MAX_INPUT_BYTES: usize = Self::MAX_PERSISTED_BYTES;

    pub fn update<T>(&self, operation: impl FnOnce(&mut Config) -> Result<T>) -> Result<T> {
        with_advisory_lock(&self.path, |path| {
            let store = Self::new(path.to_owned());
            let mut config = store.load_unlocked()?;
            let result = operation(&mut config)?;
            store.save_unlocked(&config)?;
            Ok(result)
        })
    }

    pub fn load(&self) -> Result<Config> {
        with_advisory_lock(&self.path, |path| {
            Self::new(path.to_owned()).load_unlocked()
        })
    }

    /// Loads and validates config while migrating legacy schemas only in memory.
    pub fn load_read_only(&self) -> Result<Config> {
        with_advisory_lock(&self.path, |path| {
            Self::new(path.to_owned()).load_unlocked_with_migration(false)
        })
    }

    pub fn save(&self, config: &Config) -> Result<()> {
        with_advisory_lock(&self.path, |path| {
            Self::new(path.to_owned()).save_unlocked(config)
        })
    }

    fn load_unlocked(&self) -> Result<Config> {
        self.load_unlocked_with_migration(true)
    }

    fn load_unlocked_with_migration(&self, persist_migration: bool) -> Result<Config> {
        let source = match read_config_file(&self.path, Self::MAX_INPUT_BYTES) {
            Ok(source) => source,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(error) if error.kind() == io::ErrorKind::InvalidData => return Err(error.into()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read config `{}`", self.path.display()));
            }
        };

        let document: toml::Value = toml::from_str(&source)
            .with_context(|| format!("parse config `{}` as TOML", self.path.display()))?;
        let version = document
            .get("version")
            .and_then(toml::Value::as_integer)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "config `{}` must contain an integer `version`",
                    self.path.display()
                )
            })?;

        let (config, migrated) = match version {
            3 => {
                let config: Config = toml::from_str(&source).with_context(|| {
                    format!("decode config version 3 from `{}`", self.path.display())
                })?;
                (config, false)
            }
            2 => {
                let legacy: ConfigV2 = toml::from_str(&source).with_context(|| {
                    format!("decode config version 2 from `{}`", self.path.display())
                })?;
                (legacy.migrate(), true)
            }
            1 => {
                let legacy: ConfigV1 = toml::from_str(&source).with_context(|| {
                    format!("decode config version 1 from `{}`", self.path.display())
                })?;
                (legacy.migrate(), true)
            }
            0 => {
                let legacy: ConfigV0 = toml::from_str(&source).with_context(|| {
                    format!("decode config version 0 from `{}`", self.path.display())
                })?;
                (legacy.migrate().migrate(), true)
            }
            other => bail!(
                "unsupported config version {other} in `{}`; supported versions are 0, 1, and {}",
                self.path.display(),
                Config::CURRENT_VERSION
            ),
        };
        config.validate()?;
        if migrated && persist_migration {
            self.save_unlocked(&config)
                .with_context(|| format!("rewrite migrated config `{}`", self.path.display()))?;
        }
        Ok(config)
    }

    fn save_unlocked(&self, config: &Config) -> Result<()> {
        config.validate()?;
        let serialized = toml::to_string_pretty(config).context("serialize config as TOML")?;
        require_serialized_config_size(&serialized)?;
        atomic_write_resolved(&self.path, serialized.as_bytes())
            .with_context(|| format!("save config `{}`", self.path.display()))
    }
}

fn require_serialized_config_size(serialized: &str) -> Result<()> {
    if serialized.len() > ConfigStore::MAX_PERSISTED_BYTES {
        bail!(
            "serialized config may contain at most {} bytes",
            ConfigStore::MAX_PERSISTED_BYTES
        );
    }
    Ok(())
}

fn read_config_file(path: &Path, max_bytes: usize) -> io::Result<String> {
    let file = open_regular_file(path)?;
    let size = file.metadata()?.len();
    if size > max_bytes as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config input may contain at most {max_bytes} bytes"),
        ));
    }
    let mut source = String::with_capacity(size as usize);
    file.take(max_bytes as u64 + 1)
        .read_to_string(&mut source)?;
    if source.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config input may contain at most {max_bytes} bytes"),
        ));
    }
    Ok(source)
}

fn read_regular_bytes(path: &Path) -> io::Result<(Vec<u8>, fs::Permissions)> {
    let mut file = open_regular_file(path)?;
    let permissions = file.metadata()?.permissions();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok((bytes, permissions))
}

fn open_regular_file(path: &Path) -> io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path is not a regular file",
        ));
    }
    Ok(file)
}

fn require_max_bytes(value: &str, max_bytes: usize, field: &str) -> Result<()> {
    if value.len() > max_bytes {
        bail!("{field} may contain at most {max_bytes} bytes");
    }
    Ok(())
}

fn require_nonempty(value: &str, field: &str) -> Result<()> {
    if value.contains('\0') {
        bail!("{field} must not contain NUL");
    }
    if value.trim().is_empty() {
        bail!("{field} must not be empty");
    }
    Ok(())
}

fn require_positive_usize(value: usize, field: &str) -> Result<()> {
    if value == 0 {
        bail!("{field} must be greater than zero");
    }
    if (value as u128) > (MAX_SERIALIZABLE_INTEGER as u128) {
        bail!("{field} exceeds the maximum TOML integer");
    }
    Ok(())
}

fn require_positive_u64(value: u64, field: &str) -> Result<()> {
    if value == 0 {
        bail!("{field} must be greater than zero");
    }
    if value > MAX_SERIALIZABLE_INTEGER {
        bail!("{field} exceeds the maximum TOML integer");
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigV2 {
    #[allow(dead_code)]
    version: u32,
    #[serde(default)]
    hosts: Vec<HostConfig>,
    ui: UiDefaults,
    discovery: DiscoveryDefaults,
    retention: RetentionDefaults,
}

impl ConfigV2 {
    fn migrate(self) -> Config {
        Config {
            version: Config::CURRENT_VERSION,
            hosts: self.hosts,
            ui: self.ui,
            notifications: NotificationDefaults::default(),
            discovery: self.discovery,
            retention: self.retention,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigV1 {
    #[allow(dead_code)]
    version: u32,
    #[serde(default)]
    hosts: Vec<HostConfig>,
    ui: UiDefaults,
}

impl ConfigV1 {
    fn migrate(self) -> Config {
        Config {
            version: Config::CURRENT_VERSION,
            hosts: self.hosts,
            ui: self.ui,
            notifications: NotificationDefaults::default(),
            discovery: DiscoveryDefaults::default(),
            retention: RetentionDefaults::default(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigV0 {
    #[allow(dead_code)]
    version: u32,
    #[serde(default)]
    hosts: Vec<HostConfigV0>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostConfigV0 {
    name: String,
    target: String,
    #[serde(default)]
    roots: Vec<String>,
}

impl ConfigV0 {
    fn migrate(self) -> ConfigV1 {
        ConfigV1 {
            version: 1,
            hosts: self
                .hosts
                .into_iter()
                .map(|host| HostConfig {
                    name: host.name,
                    target: host.target,
                    roots: host.roots,
                    presets: Vec::new(),
                })
                .collect(),
            ui: UiDefaults::default(),
        }
    }
}
