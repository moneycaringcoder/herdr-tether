use std::{collections::HashSet, fs, io, path::PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    model::Placement,
    storage::{atomic_write, with_advisory_lock},
};

const MAX_SERIALIZABLE_INTEGER: u64 = i64::MAX as u64;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommandPreset {
    pub name: String,
    pub command: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    pub name: String,
    pub target: String,
    pub roots: Vec<String>,
    pub presets: Vec<CommandPreset>,
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub hosts: Vec<HostConfig>,
    pub ui: UiDefaults,
    pub discovery: DiscoveryDefaults,
    pub retention: RetentionDefaults,
}

impl Config {
    pub const CURRENT_VERSION: u32 = 2;
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

        for (root_index, root) in self.discovery.local_roots.iter().enumerate() {
            require_nonempty(root, &format!("discovery local root at index {root_index}"))?;
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
            require_nonempty(&host.name, &format!("{location} name"))?;
            if host.name.eq_ignore_ascii_case("local") {
                bail!("host name `{}` is reserved", host.name);
            }
            if !host_names.insert(host.name.as_str()) {
                bail!("duplicate host name `{}`", host.name);
            }
            require_nonempty(&host.target, &format!("host `{}` target", host.name))?;
            crate::sshcfg::validate_ssh_target(&host.target)
                .with_context(|| format!("invalid target for host `{}`", host.name))?;

            for (root_index, root) in host.roots.iter().enumerate() {
                require_nonempty(
                    root,
                    &format!("root at index {root_index} for host `{}`", host.name),
                )?;
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

    pub fn update<T>(&self, operation: impl FnOnce(&mut Config) -> Result<T>) -> Result<T> {
        with_advisory_lock(&self.path, || {
            let mut config = self.load_unlocked()?;
            let result = operation(&mut config)?;
            self.save_unlocked(&config)?;
            Ok(result)
        })
    }

    pub fn load(&self) -> Result<Config> {
        with_advisory_lock(&self.path, || self.load_unlocked())
    }

    pub fn save(&self, config: &Config) -> Result<()> {
        with_advisory_lock(&self.path, || self.save_unlocked(config))
    }

    fn load_unlocked(&self) -> Result<Config> {
        let source = match fs::read_to_string(&self.path) {
            Ok(source) => source,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
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
            2 => {
                let config: Config = toml::from_str(&source).with_context(|| {
                    format!("decode config version 2 from `{}`", self.path.display())
                })?;
                (config, false)
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
        if migrated {
            self.save_unlocked(&config)
                .with_context(|| format!("rewrite migrated config `{}`", self.path.display()))?;
        }
        Ok(config)
    }

    fn save_unlocked(&self, config: &Config) -> Result<()> {
        config.validate()?;
        let serialized = toml::to_string_pretty(config).context("serialize config as TOML")?;
        atomic_write(&self.path, serialized.as_bytes())
            .with_context(|| format!("save config `{}`", self.path.display()))
    }
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
