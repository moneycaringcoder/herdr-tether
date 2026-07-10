use std::{env, path::PathBuf};

use anyhow::{Context, Result};

/// Files and user-owned inputs used by the command-line application.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppPaths {
    pub config_file: PathBuf,
    pub state_file: PathBuf,
    pub ssh_config_file: PathBuf,
}

impl AppPaths {
    /// Resolve paths from the process environment.
    ///
    /// Herdr's per-plugin directories take precedence independently. Outside a
    /// plugin, the XDG variables are honored before their HOME-based defaults.
    pub fn from_env() -> Result<Self> {
        let home = env::var_os("HOME").map(PathBuf::from);

        let config_dir = match env::var_os("HERDR_PLUGIN_CONFIG_DIR") {
            Some(path) => PathBuf::from(path),
            None => match env::var_os("XDG_CONFIG_HOME") {
                Some(path) => PathBuf::from(path).join("herdr-tether"),
                None => home
                    .as_ref()
                    .context("HOME is not set and neither HERDR_PLUGIN_CONFIG_DIR nor XDG_CONFIG_HOME is available")?
                    .join(".config/herdr-tether"),
            },
        };
        let state_dir = match env::var_os("HERDR_PLUGIN_STATE_DIR") {
            Some(path) => PathBuf::from(path),
            None => match env::var_os("XDG_STATE_HOME") {
                Some(path) => PathBuf::from(path).join("herdr-tether"),
                None => home
                    .as_ref()
                    .context("HOME is not set and neither HERDR_PLUGIN_STATE_DIR nor XDG_STATE_HOME is available")?
                    .join(".local/state/herdr-tether"),
            },
        };
        let ssh_config_file = home
            .map(|home| home.join(".ssh/config"))
            .unwrap_or_default();

        Ok(Self {
            config_file: config_dir.join("config.toml"),
            state_file: state_dir.join("state.json"),
            ssh_config_file,
        })
    }
}
