use std::{collections::HashSet, fs, io, path::PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{model::Placement, storage::atomic_write};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommandPreset {
    pub name: String,
    pub command: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HostConfig {
    pub name: String,
    pub target: String,
    pub roots: Vec<String>,
    pub presets: Vec<CommandPreset>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
pub struct Config {
    pub version: u32,
    pub hosts: Vec<HostConfig>,
    pub ui: UiDefaults,
}

impl Config {
    pub const CURRENT_VERSION: u32 = 1;
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
                    &format!("preset at index {preset_index} for host `{}` name", host.name),
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

    pub fn load(&self) -> Result<Config> {
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

        match version {
            1 => {
                let config: Config = toml::from_str(&source).with_context(|| {
                    format!("decode config version 1 from `{}`", self.path.display())
                })?;
                config.validate()?;
                Ok(config)
            }
            0 => {
                let legacy: ConfigV0 = toml::from_str(&source).with_context(|| {
                    format!("decode config version 0 from `{}`", self.path.display())
                })?;
                let config = legacy.migrate();
                config.validate()?;
                self.save(&config).with_context(|| {
                    format!("rewrite migrated config `{}`", self.path.display())
                })?;
                Ok(config)
            }
            other => bail!(
                "unsupported config version {other} in `{}`; supported versions are 0 and {}",
                self.path.display(),
                Config::CURRENT_VERSION
            ),
        }
    }

    pub fn save(&self, config: &Config) -> Result<()> {
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

#[derive(Deserialize)]
struct ConfigV0 {
    #[allow(dead_code)]
    version: u32,
    #[serde(default)]
    hosts: Vec<HostConfigV0>,
}

#[derive(Deserialize)]
struct HostConfigV0 {
    name: String,
    target: String,
    #[serde(default)]
    roots: Vec<String>,
}

impl ConfigV0 {
    fn migrate(self) -> Config {
        Config {
            version: Config::CURRENT_VERSION,
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
