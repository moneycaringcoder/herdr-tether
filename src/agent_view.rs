use std::{
    env,
    fs::OpenOptions,
    io::{self, Read},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use crate::{
    herdr_socket::HerdrSocketClient,
    model::{OrchestrationGroupId, SessionId},
    state::State,
    storage::{atomic_write_resolved, with_advisory_lock},
};

pub const AGENT_VIEW_SOURCE: &str = "plugin:moneycaringcoder.tether";
pub const GROUP_TOKEN: &str = "tether_group";
pub const REMOTE_TOKEN: &str = "tether_remote";

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentViewFilter {
    #[default]
    All,
    NeedsAttention,
    Remote,
}

impl AgentViewFilter {
    const fn is_all(&self) -> bool {
        matches!(self, Self::All)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "group",
            Self::NeedsAttention => "group agents needing attention",
            Self::Remote => "remote group agents",
        }
    }
}

const PREFERENCE_SCHEMA_VERSION: u32 = 1;
const MAX_PREFERENCE_BYTES: usize = 4096;
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct AgentViewPreference {
    schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    group_id: Option<OrchestrationGroupId>,
    #[serde(default, skip_serializing_if = "AgentViewFilter::is_all")]
    filter: AgentViewFilter,
}

impl Default for AgentViewPreference {
    fn default() -> Self {
        Self {
            schema_version: PREFERENCE_SCHEMA_VERSION,
            group_id: None,
            filter: AgentViewFilter::All,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AgentViewService {
    preference_file: PathBuf,
    client: Option<HerdrSocketClient>,
}

impl AgentViewService {
    pub fn from_env(preference_file: PathBuf) -> Result<Self> {
        let client = env::var_os("HERDR_SOCKET_PATH")
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
            .map(HerdrSocketClient::new);
        Ok(Self {
            preference_file,
            client,
        })
    }

    #[cfg(test)]
    fn with_socket(preference_file: PathBuf, socket_path: PathBuf) -> Self {
        Self {
            preference_file,
            client: Some(HerdrSocketClient::new(socket_path)),
        }
    }

    pub fn set_group(&self, state: &State, group_id: &OrchestrationGroupId) -> Result<()> {
        self.set_group_filter(state, group_id, AgentViewFilter::All)
    }

    pub fn set_group_filter(
        &self,
        state: &State,
        group_id: &OrchestrationGroupId,
        filter: AgentViewFilter,
    ) -> Result<()> {
        if !state
            .orchestration_groups
            .iter()
            .any(|group| &group.id == group_id)
        {
            bail!("unknown orchestration group");
        }
        let group_id = group_id.clone();
        with_advisory_lock(&self.preference_file, |preference_file| {
            let previous = load_preference(preference_file)?;
            let next = AgentViewPreference {
                schema_version: PREFERENCE_SCHEMA_VERSION,
                group_id: Some(group_id.clone()),
                filter,
            };
            save_preference(preference_file, &next)?;
            if let Err(error) = self.apply_group(&group_id, filter) {
                save_preference(preference_file, &previous)
                    .context("restore prior Agent view preference after Herdr rejected the view")?;
                return Err(error);
            }
            Ok(())
        })
    }

    pub fn clear(&self) -> Result<()> {
        with_advisory_lock(&self.preference_file, |preference_file| {
            let previous = load_preference(preference_file)?;
            save_preference(preference_file, &AgentViewPreference::default())?;
            if let Err(error) = self.clear_runtime() {
                save_preference(preference_file, &previous).context(
                    "restore prior Agent view preference after Herdr rejected the clear",
                )?;
                return Err(error);
            }
            Ok(())
        })
    }

    pub fn restore(&self, state: &State) -> Result<()> {
        with_advisory_lock(&self.preference_file, |preference_file| {
            let preference = load_preference(preference_file)?;
            let Some(group_id) = preference.group_id else {
                return Ok(());
            };
            if !state
                .orchestration_groups
                .iter()
                .any(|group| group.id == group_id)
            {
                save_preference(preference_file, &AgentViewPreference::default())?;
                return Ok(());
            }
            self.apply_group(&group_id, preference.filter)
        })
    }

    pub fn clear_group_if_active(&self, group_id: &OrchestrationGroupId) -> Result<bool> {
        with_advisory_lock(&self.preference_file, |preference_file| {
            let previous = load_preference(preference_file)?;
            if previous.group_id.as_ref() != Some(group_id) {
                return Ok(false);
            }
            save_preference(preference_file, &AgentViewPreference::default())?;
            if let Err(error) = self.clear_runtime() {
                save_preference(preference_file, &previous).context(
                    "restore prior Agent view preference after Herdr rejected the clear",
                )?;
                return Err(error);
            }
            Ok(true)
        })
    }

    pub fn group_for_session(
        &self,
        state: &State,
        session_id: SessionId,
    ) -> Result<Option<OrchestrationGroupId>> {
        let preference = with_advisory_lock(&self.preference_file, load_preference)?;
        let Some(group_id) = preference.group_id else {
            return Ok(None);
        };
        Ok(state
            .orchestration_groups
            .iter()
            .find(|group| {
                group.id == group_id
                    && (group.orchestrator_session_id == session_id
                        || group
                            .workers
                            .iter()
                            .any(|worker| worker.session_id == session_id))
            })
            .map(|group| group.id.clone()))
    }

    fn apply_group(&self, group_id: &OrchestrationGroupId, mode: AgentViewFilter) -> Result<()> {
        let group = json!({
            "op": "eq",
            "field": {"token": GROUP_TOKEN},
            "value": group_id.to_string(),
        });
        let filter = match mode {
            AgentViewFilter::All => group,
            AgentViewFilter::NeedsAttention => json!({
                "op": "all",
                "filters": [
                    group,
                    {
                        "op": "in",
                        "field": "status",
                        "values": ["blocked", "done"],
                    },
                ],
            }),
            AgentViewFilter::Remote => json!({
                "op": "all",
                "filters": [
                    group,
                    {
                        "op": "eq",
                        "field": {"token": REMOTE_TOKEN},
                        "value": "true",
                    },
                ],
            }),
        };
        let response = self.request(
            "agent.view.set",
            json!({
                "source": AGENT_VIEW_SOURCE,
                "label": format!("Tether {}", mode.label()),
                "filter": filter,
                "sort": [
                    {"field": "attention", "order": "desc"},
                    {"field": "state_change_seq", "order": "desc"},
                ],
            }),
        )?;
        require_agent_view_response(&response, true)
    }

    fn clear_runtime(&self) -> Result<()> {
        let response = self.request("agent.view.clear", json!({"source": AGENT_VIEW_SOURCE}))?;
        require_agent_view_response(&response, false)
    }

    fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.client
            .as_ref()
            .context("Herdr did not provide HERDR_SOCKET_PATH")?
            .request_value(method, params, SOCKET_TIMEOUT)
    }
}

fn require_agent_view_response(result: &Value, active: bool) -> Result<()> {
    if result.get("type").and_then(Value::as_str) != Some("agent_view")
        || result.get("active").and_then(Value::as_bool) != Some(active)
    {
        bail!("Herdr Agent view response had an unexpected shape");
    }
    Ok(())
}

fn load_preference(path: &Path) -> Result<AgentViewPreference> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOFOLLOW);
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(AgentViewPreference::default());
        }
        Err(error) => return Err(error.into()),
    };
    if !file.metadata()?.is_file() {
        bail!("Agent view preference path is not a regular file");
    }
    let size = file.metadata()?.len();
    if size > MAX_PREFERENCE_BYTES as u64 {
        bail!("Agent view preference exceeds {MAX_PREFERENCE_BYTES} bytes");
    }
    let mut source = String::with_capacity(size as usize);
    file.take(MAX_PREFERENCE_BYTES as u64 + 1)
        .read_to_string(&mut source)?;
    if source.len() > MAX_PREFERENCE_BYTES {
        bail!("Agent view preference exceeds {MAX_PREFERENCE_BYTES} bytes");
    }
    let preference: AgentViewPreference = serde_json::from_str(&source)?;
    if preference.schema_version != PREFERENCE_SCHEMA_VERSION {
        bail!("unsupported Agent view preference schema");
    }
    Ok(preference)
}

fn save_preference(path: &Path, preference: &AgentViewPreference) -> Result<()> {
    let mut serialized = serde_json::to_vec_pretty(preference)?;
    serialized.push(b'\n');
    if serialized.len() > MAX_PREFERENCE_BYTES {
        bail!("serialized Agent view preference exceeds {MAX_PREFERENCE_BYTES} bytes");
    }
    atomic_write_resolved(path, &serialized).context("save Agent view preference")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        io::Write,
        os::unix::{fs::symlink, net::UnixListener},
        thread,
    };

    use crate::state::{OrchestrationGroup, State};

    fn state(group_id: &str) -> State {
        State {
            version: State::CURRENT_VERSION,
            sessions: Vec::new(),
            orchestration_groups: vec![OrchestrationGroup {
                id: group_id.parse().unwrap(),
                title: "Build group".parse().unwrap(),
                orchestrator_session_id: "tether-0197f198000070008000000000000001".parse().unwrap(),
                workers: Vec::new(),
            }],
        }
    }

    #[test]
    fn preference_io_follows_relative_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("stowed-agent-view.json");
        let link = temp.path().join("agent-view.json");
        let expected = AgentViewPreference {
            schema_version: PREFERENCE_SCHEMA_VERSION,
            group_id: Some("build-group".parse().unwrap()),
            filter: AgentViewFilter::Remote,
        };
        save_preference(&target, &expected).unwrap();
        symlink("stowed-agent-view.json", &link).unwrap();

        assert_eq!(
            with_advisory_lock(&link, load_preference).unwrap(),
            expected
        );
        with_advisory_lock(&link, |path| {
            save_preference(path, &AgentViewPreference::default())
        })
        .unwrap();

        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(
            load_preference(&target).unwrap(),
            AgentViewPreference::default()
        );
    }

    #[test]
    fn set_group_persists_and_sends_source_owned_token_query() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("herdr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            stream.read_to_string(&mut request).unwrap();
            let request: Value = serde_json::from_str(request.trim()).unwrap();
            assert_eq!(request["method"], "agent.view.set");
            assert_eq!(request["params"]["source"], AGENT_VIEW_SOURCE);
            assert_eq!(request["params"]["filter"]["field"]["token"], GROUP_TOKEN);
            assert_eq!(request["params"]["filter"]["value"], "build-group");
            writeln!(
                stream,
                "{}",
                json!({
                    "id": request["id"],
                    "result": {"type": "agent_view", "active": true}
                })
            )
            .unwrap();
        });
        let preference = temp.path().join("agent-view.json");
        let service = AgentViewService::with_socket(preference.clone(), socket);

        service
            .set_group(&state("build-group"), &"build-group".parse().unwrap())
            .unwrap();
        server.join().unwrap();

        let persisted: Value = serde_json::from_slice(&std::fs::read(preference).unwrap()).unwrap();
        assert_eq!(persisted["schema_version"], 1);
        assert_eq!(persisted["group_id"], "build-group");
    }

    #[test]
    fn filtered_views_combine_exact_group_with_status_or_remote_metadata() {
        for (index, mode) in [AgentViewFilter::NeedsAttention, AgentViewFilter::Remote]
            .into_iter()
            .enumerate()
        {
            let temp = tempfile::tempdir().unwrap();
            let socket = temp.path().join(format!("herdr-{index}.sock"));
            let listener = UnixListener::bind(&socket).unwrap();
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = String::new();
                stream.read_to_string(&mut request).unwrap();
                let request: Value = serde_json::from_str(request.trim()).unwrap();
                let filters = request["params"]["filter"]["filters"].as_array().unwrap();
                assert_eq!(filters[0]["field"]["token"], GROUP_TOKEN);
                assert_eq!(filters[0]["value"], "build-group");
                match mode {
                    AgentViewFilter::NeedsAttention => {
                        assert_eq!(filters[1]["field"], "status");
                        assert_eq!(filters[1]["values"], json!(["blocked", "done"]));
                    }
                    AgentViewFilter::Remote => {
                        assert_eq!(filters[1]["field"]["token"], REMOTE_TOKEN);
                        assert_eq!(filters[1]["value"], "true");
                    }
                    AgentViewFilter::All => unreachable!(),
                }
                writeln!(
                    stream,
                    "{}",
                    json!({
                        "id": request["id"],
                        "result": {"type": "agent_view", "active": true}
                    })
                )
                .unwrap();
            });
            let preference = temp.path().join("agent-view.json");
            AgentViewService::with_socket(preference.clone(), socket)
                .set_group_filter(&state("build-group"), &"build-group".parse().unwrap(), mode)
                .unwrap();
            server.join().unwrap();
            let persisted: AgentViewPreference =
                serde_json::from_slice(&std::fs::read(preference).unwrap()).unwrap();
            assert_eq!(persisted.filter, mode);
        }
    }

    #[test]
    fn startup_restore_reapplies_the_persisted_group_view() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("herdr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            stream.read_to_string(&mut request).unwrap();
            let request: Value = serde_json::from_str(request.trim()).unwrap();
            assert_eq!(request["method"], "agent.view.set");
            assert_eq!(request["params"]["filter"]["value"], "build-group");
            writeln!(
                stream,
                "{}",
                json!({
                    "id": request["id"],
                    "result": {"type": "agent_view", "active": true}
                })
            )
            .unwrap();
        });
        let preference = temp.path().join("agent-view.json");
        save_preference(
            &preference,
            &AgentViewPreference {
                schema_version: PREFERENCE_SCHEMA_VERSION,
                group_id: Some("build-group".parse().unwrap()),
                filter: AgentViewFilter::All,
            },
        )
        .unwrap();
        let service = AgentViewService::with_socket(preference, socket);

        service.restore(&state("build-group")).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn rejected_view_change_restores_the_previous_preference() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("herdr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            stream.read_to_string(&mut request).unwrap();
            let request: Value = serde_json::from_str(request.trim()).unwrap();
            writeln!(
                stream,
                "{}",
                json!({
                    "id": request["id"],
                    "error": {"code": "unsupported", "message": "unsupported"}
                })
            )
            .unwrap();
        });
        let preference = temp.path().join("agent-view.json");
        let service = AgentViewService::with_socket(preference.clone(), socket);

        assert!(
            service
                .set_group(&state("build-group"), &"build-group".parse().unwrap())
                .is_err()
        );
        server.join().unwrap();
        let persisted = load_preference(&preference).unwrap();
        assert_eq!(persisted, AgentViewPreference::default());
    }

    #[test]
    fn inactive_group_cleanup_needs_no_herdr_socket() {
        let temp = tempfile::tempdir().unwrap();
        let service = AgentViewService {
            preference_file: temp.path().join("agent-view.json"),
            client: None,
        };

        assert!(
            !service
                .clear_group_if_active(&"build-group".parse().unwrap())
                .unwrap()
        );
    }

    #[test]
    fn active_group_cleanup_without_a_socket_fails_and_preserves_preference() {
        let temp = tempfile::tempdir().unwrap();
        let preference = temp.path().join("agent-view.json");
        let expected = AgentViewPreference {
            schema_version: PREFERENCE_SCHEMA_VERSION,
            group_id: Some("build-group".parse().unwrap()),
            filter: AgentViewFilter::All,
        };
        save_preference(&preference, &expected).unwrap();
        let service = AgentViewService {
            preference_file: preference.clone(),
            client: None,
        };

        assert!(
            service
                .clear_group_if_active(&"build-group".parse().unwrap())
                .is_err()
        );
        assert_eq!(load_preference(&preference).unwrap(), expected);
    }

    #[test]
    fn legacy_group_preference_defaults_to_the_full_group_filter() {
        let temp = tempfile::tempdir().unwrap();
        let preference = temp.path().join("agent-view.json");
        std::fs::write(
            &preference,
            br#"{"schema_version":1,"group_id":"build-group"}"#,
        )
        .unwrap();

        assert_eq!(
            load_preference(&preference).unwrap(),
            AgentViewPreference {
                schema_version: PREFERENCE_SCHEMA_VERSION,
                group_id: Some("build-group".parse().unwrap()),
                filter: AgentViewFilter::All,
            }
        );
    }
}
