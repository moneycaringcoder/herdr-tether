use std::{env, ffi::OsString, path::PathBuf, process::Command};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::{backend::CommandSpec, model::Placement};

pub const PLUGIN_ID: &str = "moneycaringcoder.tether";

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
        Ok(Self {
            binary: required_os_env("HERDR_BIN_PATH")?.into(),
            pane_id: required_string_env("HERDR_PANE_ID", Some("PANE_ID"))?,
            workspace_id: required_string_env("HERDR_WORKSPACE_ID", Some("WORKSPACE_ID"))?,
        })
    }
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
        };

        let command_line = command.posix_command_line()?;
        let response = self.invoke(
            "run command in pane",
            &[
                "pane".to_owned(),
                "run".to_owned(),
                pane_id.clone(),
                command_line,
            ],
        )?;
        require_result_type(&response, "pane_ran")?;
        let ran_pane = result_string(&response, &["pane_id"], "pane run response pane_id")?;
        if ran_pane != pane_id {
            bail!("Herdr ran the command in pane `{ran_pane}`, not newly created pane `{pane_id}`");
        }

        Ok(PlacedPane { pane_id })
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
            ],
        )?;
        require_result_type(&response, "pane_split")?;
        result_string(&response, &["pane", "pane_id"], "split response pane_id")
    }

    fn create_tab(&self) -> Result<String> {
        let response = self.invoke(
            "create tab",
            &[
                "tab".to_owned(),
                "create".to_owned(),
                "--workspace".to_owned(),
                self.context.workspace_id.clone(),
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
        let output = Command::new(&self.context.binary)
            .args(arguments)
            .output()
            .with_context(|| {
                format!(
                    "run Herdr {operation} via `{}`",
                    self.context.binary.display()
                )
            })?;

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
