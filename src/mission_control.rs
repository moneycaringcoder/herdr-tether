use std::{
    collections::{HashMap, HashSet},
    thread,
    time::Duration,
};

use anyhow::{Context, Result};

use crate::{
    agent_view::{AGENT_VIEW_SOURCE, GROUP_TOKEN, REMOTE_TOKEN},
    herdr_socket::{
        AgentStatus, HerdrAgentInfo, HerdrSessionSnapshot, HerdrSocketClient, PromptDeliveryError,
    },
    model::{OrchestrationGroupId, OrchestrationMembershipId, SessionId},
    state::{OrchestrationMember, SessionRecord, SessionStatus, State, StateStore},
};

pub const SESSION_TOKEN: &str = "tether_session";
pub const MEMBERSHIP_TOKEN: &str = "tether_membership";
pub const MAX_PROMPT_TARGETS: usize = 8;
const PROMPT_WAIT: Duration = Duration::from_secs(30 * 60);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MissionAgentState {
    Detached,
    Idle,
    Working,
    Blocked,
    Done,
    #[default]
    Unknown,
    Unreachable,
    Stale,
}

impl MissionAgentState {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Detached => "DETACHED",
            Self::Idle => "IDLE",
            Self::Working => "WORKING",
            Self::Blocked => "BLOCKED",
            Self::Done => "DONE",
            Self::Unknown => "UNKNOWN",
            Self::Unreachable => "UNREACHABLE",
            Self::Stale => "STALE",
        }
    }

    pub const fn permits_prompt(self) -> bool {
        matches!(self, Self::Idle | Self::Done)
    }

    pub const fn permits_destructive_action(self) -> bool {
        false
    }
}

impl From<AgentStatus> for MissionAgentState {
    fn from(value: AgentStatus) -> Self {
        match value {
            AgentStatus::Idle => Self::Idle,
            AgentStatus::Working => Self::Working,
            AgentStatus::Blocked => Self::Blocked,
            AgentStatus::Done => Self::Done,
            AgentStatus::Unknown => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemberExpectation {
    pub group_id: OrchestrationGroupId,
    pub session_id: SessionId,
    pub membership_id: OrchestrationMembershipId,
    pub expected_agent: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentBinding {
    pub group_id: OrchestrationGroupId,
    pub session_id: SessionId,
    pub membership_id: OrchestrationMembershipId,
    pub pane_id: String,
    pub terminal_id: String,
    pub name: Option<String>,
    pub agent: String,
    pub status: AgentStatus,
    pub state_change_seq: u64,
    pub revision: u64,
}

impl AgentBinding {
    pub fn target(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.pane_id)
    }

    fn same_occupant_or_move(&self, next: &Self) -> bool {
        self.group_id == next.group_id
            && self.session_id == next.session_id
            && self.membership_id == next.membership_id
            && self.terminal_id == next.terminal_id
            && self.agent == next.agent
            && self.name == next.name
            && next.state_change_seq == self.state_change_seq
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BindingFailure {
    Detached,
    StaleMembership,
    Ambiguous,
    UnknownAgent,
    AgentKindMismatch,
    Replaced,
}

impl BindingFailure {
    pub const fn state(self) -> MissionAgentState {
        match self {
            Self::Detached => MissionAgentState::Detached,
            Self::StaleMembership | Self::Ambiguous | Self::Replaced => MissionAgentState::Stale,
            Self::UnknownAgent | Self::AgentKindMismatch => MissionAgentState::Unknown,
        }
    }

    pub const fn message(self) -> &'static str {
        match self {
            Self::Detached => "worker is not materialized in Herdr",
            Self::StaleMembership => "pane belongs to an earlier worker membership",
            Self::Ambiguous => "more than one Herdr pane claims this worker membership",
            Self::UnknownAgent => "Herdr does not recognize an agent in the materialized pane",
            Self::AgentKindMismatch => "Herdr recognized a different agent kind",
            Self::Replaced => "the materialized pane occupant changed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemberTarget {
    pub session_id: SessionId,
    pub membership_id: OrchestrationMembershipId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TargetDelivery {
    Delivered {
        session_id: SessionId,
        final_state: MissionAgentState,
    },
    Rejected {
        session_id: SessionId,
        reason: String,
    },
    Uncertain {
        session_id: SessionId,
    },
}

pub trait MissionHerdr: Clone + Send + Sync {
    fn snapshot(&self) -> Result<HerdrSessionSnapshot>;
    fn prompt_and_wait(
        &self,
        target: &str,
        prompt: &str,
        timeout: Duration,
    ) -> std::result::Result<HerdrAgentInfo, PromptDeliveryError>;
}

#[derive(Clone, Copy)]
enum RequiredCapability {
    ObserveOutput,
    OpenInteractive,
    PromptAgent,
}

impl RequiredCapability {
    const fn allowed(self, member: &OrchestrationMember) -> bool {
        match self {
            Self::ObserveOutput => member.capabilities.observe_output,
            Self::OpenInteractive => member.capabilities.open_interactive,
            Self::PromptAgent => member.capabilities.prompt_agent,
        }
    }

    const fn rejection(self) -> &'static str {
        match self {
            Self::ObserveOutput => "worker does not authorize agent observation",
            Self::OpenInteractive => "worker does not authorize interactive open",
            Self::PromptAgent => "worker does not authorize agent prompts",
        }
    }
}

impl MissionHerdr for HerdrSocketClient {
    fn snapshot(&self) -> Result<HerdrSessionSnapshot> {
        HerdrSocketClient::snapshot(self)
    }

    fn prompt_and_wait(
        &self,
        target: &str,
        prompt: &str,
        timeout: Duration,
    ) -> std::result::Result<HerdrAgentInfo, PromptDeliveryError> {
        HerdrSocketClient::prompt_and_wait(self, target, prompt, timeout)
    }
}

#[derive(Clone, Debug)]
pub struct MissionControlService<C = HerdrSocketClient> {
    store: StateStore,
    herdr: C,
}

impl<C: MissionHerdr> MissionControlService<C> {
    pub fn new(store: StateStore, herdr: C) -> Self {
        Self { store, herdr }
    }

    pub fn deliver_reviewed_prompt(
        &self,
        group_id: &OrchestrationGroupId,
        targets: &[MemberTarget],
        prompt: &str,
        reviewed: bool,
    ) -> Vec<TargetDelivery> {
        if targets.is_empty() {
            return Vec::new();
        }
        if !reviewed {
            return targets
                .iter()
                .map(|target| TargetDelivery::Rejected {
                    session_id: target.session_id,
                    reason: "prompt destinations were not reviewed".to_owned(),
                })
                .collect();
        }
        if targets.len() > MAX_PROMPT_TARGETS {
            return targets
                .iter()
                .map(|target| TargetDelivery::Rejected {
                    session_id: target.session_id,
                    reason: format!("at most {MAX_PROMPT_TARGETS} prompt destinations are allowed"),
                })
                .collect();
        }

        let mut seen = HashSet::with_capacity(targets.len());
        thread::scope(|scope| {
            let pending = targets
                .iter()
                .map(|target| {
                    if seen.insert((target.session_id, target.membership_id)) {
                        Some(scope.spawn(move || self.deliver_one(group_id, target, prompt)))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            targets
                .iter()
                .zip(pending)
                .map(|(target, pending)| match pending {
                    Some(handle) => handle.join().unwrap_or(TargetDelivery::Uncertain {
                        session_id: target.session_id,
                    }),
                    None => TargetDelivery::Rejected {
                        session_id: target.session_id,
                        reason: "duplicate prompt destination".to_owned(),
                    },
                })
                .collect()
        })
    }

    fn deliver_one(
        &self,
        group_id: &OrchestrationGroupId,
        target: &MemberTarget,
        prompt: &str,
    ) -> TargetDelivery {
        let attempt = self.authorize(group_id, target).and_then(|first| {
            let state = self
                .store
                .load_read_only()
                .context("revalidate Mission Control state")?;
            let expectation =
                expectation_for(&state, group_id, target, RequiredCapability::PromptAgent)?;
            if expectation != first.expectation {
                anyhow::bail!("worker authorization changed before prompt delivery");
            }
            let snapshot = self
                .herdr
                .snapshot()
                .context("revalidate Herdr pane binding")?;
            let binding = resolve_binding(&snapshot, &expectation)
                .map_err(|failure| anyhow::anyhow!(failure.message()))?;
            if !first.binding.same_occupant_or_move(&binding) {
                anyhow::bail!("materialized pane occupant changed before prompt delivery");
            }
            if !binding.status.is_settled() {
                anyhow::bail!("agent must be IDLE or DONE before prompt delivery");
            }
            Ok(binding)
        });
        let binding = match attempt {
            Ok(binding) => binding,
            Err(error) => {
                return TargetDelivery::Rejected {
                    session_id: target.session_id,
                    reason: format!("{error:#}"),
                };
            }
        };

        match self
            .herdr
            .prompt_and_wait(binding.target(), prompt, PROMPT_WAIT)
        {
            Ok(agent)
                if agent.terminal_id == binding.terminal_id
                    && agent.agent.as_deref() == Some(binding.agent.as_str()) =>
            {
                TargetDelivery::Delivered {
                    session_id: target.session_id,
                    final_state: agent.agent_status.into(),
                }
            }
            Ok(_) | Err(PromptDeliveryError::Uncertain) => TargetDelivery::Uncertain {
                session_id: target.session_id,
            },
            Err(PromptDeliveryError::Rejected { code, message }) => TargetDelivery::Rejected {
                session_id: target.session_id,
                reason: format!("Herdr rejected prompt: {code}: {message}"),
            },
        }
    }

    pub fn binding_for_observation(
        &self,
        group_id: &OrchestrationGroupId,
        target: &MemberTarget,
    ) -> Result<AgentBinding> {
        self.authorize_binding(group_id, target, false, RequiredCapability::ObserveOutput)
            .map(|authorized| authorized.binding)
    }

    pub fn binding_for_open(
        &self,
        group_id: &OrchestrationGroupId,
        target: &MemberTarget,
    ) -> Result<AgentBinding> {
        self.authorize_binding(group_id, target, false, RequiredCapability::OpenInteractive)
            .map(|authorized| authorized.binding)
    }

    fn authorize(
        &self,
        group_id: &OrchestrationGroupId,
        target: &MemberTarget,
    ) -> Result<AuthorizedBinding> {
        self.authorize_binding(group_id, target, true, RequiredCapability::PromptAgent)
    }
    fn authorize_binding(
        &self,
        group_id: &OrchestrationGroupId,
        target: &MemberTarget,
        require_settled: bool,
        capability: RequiredCapability,
    ) -> Result<AuthorizedBinding> {
        let state = self
            .store
            .load_read_only()
            .context("load Mission Control state")?;
        let expectation = expectation_for(&state, group_id, target, capability)?;
        let snapshot = self
            .herdr
            .snapshot()
            .context("read Herdr Mission Control snapshot")?;
        if !snapshot.supports_mission_control() {
            anyhow::bail!("Mission Control requires Herdr 0.7.5 or newer");
        }
        let binding = resolve_binding(&snapshot, &expectation)
            .map_err(|failure| anyhow::anyhow!(failure.message()))?;
        if require_settled && !binding.status.is_settled() {
            anyhow::bail!("agent must be IDLE or DONE before prompt delivery");
        }
        Ok(AuthorizedBinding {
            expectation,
            binding,
        })
    }
}

struct AuthorizedBinding {
    expectation: MemberExpectation,
    binding: AgentBinding,
}

pub fn member_metadata_tokens(
    group_id: &OrchestrationGroupId,
    member: &OrchestrationMember,
    remote: bool,
) -> HashMap<String, String> {
    HashMap::from([
        (GROUP_TOKEN.to_owned(), group_id.to_string()),
        (SESSION_TOKEN.to_owned(), member.session_id.to_string()),
        (
            MEMBERSHIP_TOKEN.to_owned(),
            member.membership_id.to_string(),
        ),
        (REMOTE_TOKEN.to_owned(), remote.to_string()),
    ])
}

pub fn label_materialized_member(
    client: &HerdrSocketClient,
    pane_id: &str,
    group_id: &OrchestrationGroupId,
    member: &OrchestrationMember,
    remote: bool,
) -> Result<()> {
    client.report_pane_metadata(
        pane_id,
        AGENT_VIEW_SOURCE,
        &member_metadata_tokens(group_id, member, remote),
    )
}

/// Builds identity-only binding expectations for read-only status projection.
///
/// Action paths must use `authorize_binding` with an explicit capability.
pub(crate) fn binding_expectation(
    state: &State,
    group_id: &OrchestrationGroupId,
    target: &MemberTarget,
) -> Result<MemberExpectation> {
    member_expectation(state, group_id, target, None)
}

fn expectation_for(
    state: &State,
    group_id: &OrchestrationGroupId,
    target: &MemberTarget,
    capability: RequiredCapability,
) -> Result<MemberExpectation> {
    member_expectation(state, group_id, target, Some(capability))
}

fn member_expectation(
    state: &State,
    group_id: &OrchestrationGroupId,
    target: &MemberTarget,
    capability: Option<RequiredCapability>,
) -> Result<MemberExpectation> {
    let group = state
        .orchestration_groups
        .iter()
        .find(|group| &group.id == group_id)
        .context("Mission Control group no longer exists")?;
    let member = group
        .workers
        .iter()
        .find(|member| member.session_id == target.session_id)
        .context("worker is no longer a member of the Mission Control group")?;
    if member.membership_id != target.membership_id {
        anyhow::bail!("worker membership changed");
    }
    if let Some(capability) = capability
        && !capability.allowed(member)
    {
        anyhow::bail!(capability.rejection());
    }
    let record = require_exact_running_record(state, member.session_id)?;
    let expected_agent = record
        .herdr_agent
        .as_ref()
        .context("worker has no explicit Herdr agent identity")?
        .as_str()
        .to_owned();
    Ok(MemberExpectation {
        group_id: group.id.clone(),
        session_id: member.session_id,
        membership_id: member.membership_id,
        expected_agent,
    })
}

fn require_exact_running_record(state: &State, session_id: SessionId) -> Result<&SessionRecord> {
    let record = state
        .sessions
        .iter()
        .find(|record| record.id == session_id)
        .context("worker session no longer exists")?;
    if record.status != SessionStatus::Running
        || record.ownership_proof.is_none()
        || record.tmux_session_id.is_none()
    {
        anyhow::bail!("worker is not a running exact-owned workload");
    }
    Ok(record)
}

pub fn resolve_binding(
    snapshot: &HerdrSessionSnapshot,
    expected: &MemberExpectation,
) -> std::result::Result<AgentBinding, BindingFailure> {
    let group = expected.group_id.to_string();
    let session = expected.session_id.to_string();
    let membership = expected.membership_id.to_string();
    let related = snapshot
        .agents
        .iter()
        .filter(|agent| {
            agent.tokens.get(GROUP_TOKEN) == Some(&group)
                && agent.tokens.get(SESSION_TOKEN) == Some(&session)
        })
        .collect::<Vec<_>>();
    let exact = related
        .iter()
        .copied()
        .filter(|agent| agent.tokens.get(MEMBERSHIP_TOKEN) == Some(&membership))
        .collect::<Vec<_>>();
    if exact.is_empty() {
        return Err(if related.is_empty() {
            BindingFailure::Detached
        } else {
            BindingFailure::StaleMembership
        });
    }
    if exact.len() != 1 {
        return Err(BindingFailure::Ambiguous);
    }
    let agent = exact[0];
    let recognized = agent
        .agent
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or(BindingFailure::UnknownAgent)?;
    if recognized != expected.expected_agent {
        return Err(BindingFailure::AgentKindMismatch);
    }
    Ok(AgentBinding {
        group_id: expected.group_id.clone(),
        session_id: expected.session_id,
        membership_id: expected.membership_id,
        pane_id: agent.pane_id.clone(),
        terminal_id: agent.terminal_id.clone(),
        name: agent.name.clone(),
        agent: recognized.to_owned(),
        status: agent.agent_status,
        state_change_seq: agent.state_change_seq,
        revision: agent.revision,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use parking_lot::Mutex;

    use chrono::Utc;
    use tempfile::tempdir;

    use super::*;
    use crate::{
        herdr_socket::HerdrPaneInfo,
        model::{HerdrAgentKind, OwnershipProof, TmuxSessionId},
        state::{OrchestrationCapabilities, OrchestrationGroup},
    };

    const SESSION: &str = "tether-0197f198000070008000000000000002";
    const MEMBERSHIP: &str = "0197f198000070008000000000000011";

    #[derive(Clone)]
    struct FakeHerdr {
        snapshots: Arc<Mutex<Vec<HerdrSessionSnapshot>>>,
        prompts: Arc<Mutex<Vec<String>>>,
        result: Arc<Mutex<std::result::Result<HerdrAgentInfo, PromptDeliveryError>>>,
    }

    impl MissionHerdr for FakeHerdr {
        fn snapshot(&self) -> Result<HerdrSessionSnapshot> {
            let mut snapshots = self.snapshots.lock();
            if snapshots.len() > 1 {
                Ok(snapshots.remove(0))
            } else {
                Ok(snapshots[0].clone())
            }
        }

        fn prompt_and_wait(
            &self,
            target: &str,
            _prompt: &str,
            _timeout: Duration,
        ) -> std::result::Result<HerdrAgentInfo, PromptDeliveryError> {
            self.prompts.lock().push(target.to_owned());
            self.result.lock().clone()
        }
    }

    fn member(prompt_agent: bool) -> OrchestrationMember {
        OrchestrationMember {
            session_id: SESSION.parse().unwrap(),
            membership_id: MEMBERSHIP.parse().unwrap(),
            title: Some("Worker".parse().unwrap()),
            capabilities: OrchestrationCapabilities {
                observe_output: true,
                open_interactive: true,
                prompt_agent,
            },
        }
    }

    fn test_state(prompt_agent: bool) -> State {
        let now = Utc::now();
        let member = member(prompt_agent);
        State {
            version: State::CURRENT_VERSION,
            sessions: vec![SessionRecord {
                id: member.session_id,
                host: "local".to_owned(),
                target: "local".to_owned(),
                directory: "/work".to_owned(),
                preset: None,
                herdr_agent: Some("codex".parse::<HerdrAgentKind>().unwrap()),
                command: Some("codex".to_owned()),
                tmux_session_id: Some("$1".parse::<TmuxSessionId>().unwrap()),
                ownership_proof: Some(
                    "0197f198000070008000000000000099"
                        .parse::<OwnershipProof>()
                        .unwrap(),
                ),
                status: SessionStatus::Running,
                created_at: now,
                last_used_at: now,
                closed_at: None,
                exit_status: None,
            }],
            orchestration_groups: vec![OrchestrationGroup {
                id: "build-group".parse().unwrap(),
                title: "Build group".parse().unwrap(),
                orchestrator_session_id: "tether-0197f198000070008000000000000001".parse().unwrap(),
                workers: vec![member],
            }],
        }
    }

    fn agent(
        membership: &str,
        pane_id: &str,
        terminal_id: &str,
        kind: Option<&str>,
        status: AgentStatus,
    ) -> HerdrAgentInfo {
        HerdrAgentInfo {
            terminal_id: terminal_id.to_owned(),
            name: Some("worker".to_owned()),
            agent: kind.map(str::to_owned),
            title: None,
            agent_status: status,
            tokens: HashMap::from([
                (GROUP_TOKEN.to_owned(), "build-group".to_owned()),
                (SESSION_TOKEN.to_owned(), SESSION.to_owned()),
                (MEMBERSHIP_TOKEN.to_owned(), membership.to_owned()),
            ]),
            workspace_id: "w1".to_owned(),
            tab_id: "w1:t1".to_owned(),
            pane_id: pane_id.to_owned(),
            focused: false,
            state_change_seq: 4,
            revision: 9,
        }
    }

    fn snapshot(agents: Vec<HerdrAgentInfo>) -> HerdrSessionSnapshot {
        HerdrSessionSnapshot {
            version: "0.7.5".to_owned(),
            protocol: 17,
            focused_workspace_id: None,
            focused_tab_id: None,
            focused_pane_id: None,
            panes: agents
                .iter()
                .map(|agent| HerdrPaneInfo {
                    pane_id: agent.pane_id.clone(),
                    terminal_id: agent.terminal_id.clone(),
                    workspace_id: agent.workspace_id.clone(),
                    tab_id: agent.tab_id.clone(),
                    agent: agent.agent.clone(),
                    agent_status: agent.agent_status,
                    tokens: agent.tokens.clone(),
                    revision: agent.revision,
                })
                .collect(),
            agents,
        }
    }

    fn expected() -> MemberExpectation {
        MemberExpectation {
            group_id: "build-group".parse().unwrap(),
            session_id: SESSION.parse().unwrap(),
            membership_id: MEMBERSHIP.parse().unwrap(),
            expected_agent: "codex".to_owned(),
        }
    }

    fn service(
        state: &State,
        snapshots: Vec<HerdrSessionSnapshot>,
        result: std::result::Result<HerdrAgentInfo, PromptDeliveryError>,
    ) -> (
        MissionControlService<FakeHerdr>,
        Arc<Mutex<Vec<String>>>,
        tempfile::TempDir,
    ) {
        let temp = tempdir().unwrap();
        let store = StateStore::new(temp.path().join("state.json"));
        store.save(state).unwrap();
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let herdr = FakeHerdr {
            snapshots: Arc::new(Mutex::new(snapshots)),
            prompts: Arc::clone(&prompts),
            result: Arc::new(Mutex::new(result)),
        };
        (MissionControlService::new(store, herdr), prompts, temp)
    }

    #[test]
    fn binding_requires_exact_group_session_and_membership_tokens() {
        let exact = agent(
            MEMBERSHIP,
            "w1:p1",
            "term-1",
            Some("codex"),
            AgentStatus::Idle,
        );
        assert_eq!(
            resolve_binding(&snapshot(vec![exact.clone()]), &expected())
                .unwrap()
                .pane_id,
            "w1:p1"
        );
        assert_eq!(
            resolve_binding(
                &snapshot(vec![agent(
                    "0197f198000070008000000000000012",
                    "w1:p1",
                    "term-1",
                    Some("codex"),
                    AgentStatus::Idle,
                )]),
                &expected(),
            ),
            Err(BindingFailure::StaleMembership)
        );
        assert_eq!(
            resolve_binding(&snapshot(vec![exact.clone(), exact]), &expected()),
            Err(BindingFailure::Ambiguous)
        );
    }

    #[test]
    fn read_only_live_binding_never_inherits_prompt_authority() {
        let idle = agent(
            MEMBERSHIP,
            "w1:p1",
            "term-1",
            Some("codex"),
            AgentStatus::Idle,
        );
        let (service, prompts, _temp) = service(
            &test_state(false),
            vec![snapshot(vec![idle.clone()]), snapshot(vec![idle.clone()])],
            Ok(idle),
        );
        let group_id = "build-group".parse().unwrap();
        let target = MemberTarget {
            session_id: SESSION.parse().unwrap(),
            membership_id: MEMBERSHIP.parse().unwrap(),
        };

        assert_eq!(
            service
                .binding_for_observation(&group_id, &target)
                .unwrap()
                .pane_id,
            "w1:p1"
        );
        assert_eq!(
            service
                .binding_for_open(&group_id, &target)
                .unwrap()
                .pane_id,
            "w1:p1"
        );
        let result = service.deliver_reviewed_prompt(&group_id, &[target], "Do work", true);
        assert!(matches!(result[0], TargetDelivery::Rejected { .. }));
        assert!(prompts.lock().is_empty());
    }

    #[test]
    fn binding_rejects_unknown_and_different_agents() {
        assert_eq!(
            resolve_binding(
                &snapshot(vec![agent(
                    MEMBERSHIP,
                    "w1:p1",
                    "term-1",
                    None,
                    AgentStatus::Unknown
                )]),
                &expected(),
            ),
            Err(BindingFailure::UnknownAgent)
        );
        assert_eq!(
            resolve_binding(
                &snapshot(vec![agent(
                    MEMBERSHIP,
                    "w1:p1",
                    "term-1",
                    Some("claude"),
                    AgentStatus::Idle
                )]),
                &expected(),
            ),
            Err(BindingFailure::AgentKindMismatch)
        );
    }

    #[test]
    fn reviewed_authorized_settled_prompt_is_delivered_once() {
        let idle = agent(
            MEMBERSHIP,
            "w1:p1",
            "term-1",
            Some("codex"),
            AgentStatus::Idle,
        );
        let done = agent(
            MEMBERSHIP,
            "w1:p1",
            "term-1",
            Some("codex"),
            AgentStatus::Done,
        );
        let (service, prompts, _temp) = service(
            &test_state(true),
            vec![snapshot(vec![idle.clone()]), snapshot(vec![idle])],
            Ok(done),
        );
        let result = service.deliver_reviewed_prompt(
            &"build-group".parse().unwrap(),
            &[MemberTarget {
                session_id: SESSION.parse().unwrap(),
                membership_id: MEMBERSHIP.parse().unwrap(),
            }],
            "Review the change",
            true,
        );
        assert_eq!(
            result,
            vec![TargetDelivery::Delivered {
                session_id: SESSION.parse().unwrap(),
                final_state: MissionAgentState::Done,
            }]
        );
        assert_eq!(prompts.lock().as_slice(), ["worker"]);
    }

    #[test]
    fn revoked_or_unreviewed_prompt_delivers_no_input() {
        let idle = agent(
            MEMBERSHIP,
            "w1:p1",
            "term-1",
            Some("codex"),
            AgentStatus::Idle,
        );
        for (state, reviewed) in [(test_state(false), true), (test_state(true), false)] {
            let (service, prompts, _temp) =
                service(&state, vec![snapshot(vec![idle.clone()])], Ok(idle.clone()));
            let result = service.deliver_reviewed_prompt(
                &"build-group".parse().unwrap(),
                &[MemberTarget {
                    session_id: SESSION.parse().unwrap(),
                    membership_id: MEMBERSHIP.parse().unwrap(),
                }],
                "Do work",
                reviewed,
            );
            assert!(matches!(result[0], TargetDelivery::Rejected { .. }));
            assert!(prompts.lock().is_empty());
        }
    }

    #[test]
    fn pane_replacement_between_checks_delivers_no_input() {
        let first = agent(
            MEMBERSHIP,
            "w1:p1",
            "term-1",
            Some("codex"),
            AgentStatus::Idle,
        );
        let replacement = agent(
            MEMBERSHIP,
            "w1:p1",
            "term-2",
            Some("codex"),
            AgentStatus::Idle,
        );
        let (service, prompts, _temp) = service(
            &test_state(true),
            vec![snapshot(vec![first.clone()]), snapshot(vec![replacement])],
            Ok(first),
        );
        let result = service.deliver_reviewed_prompt(
            &"build-group".parse().unwrap(),
            &[MemberTarget {
                session_id: SESSION.parse().unwrap(),
                membership_id: MEMBERSHIP.parse().unwrap(),
            }],
            "Do work",
            true,
        );
        assert!(matches!(result[0], TargetDelivery::Rejected { .. }));
        assert!(prompts.lock().is_empty());
    }

    #[test]
    fn pane_move_between_checks_preserves_exact_agent_delivery() {
        let first = agent(
            MEMBERSHIP,
            "w1:p1",
            "term-1",
            Some("codex"),
            AgentStatus::Idle,
        );
        let mut moved = agent(
            MEMBERSHIP,
            "w2:p9",
            "term-1",
            Some("codex"),
            AgentStatus::Done,
        );
        moved.workspace_id = "w2".to_owned();
        moved.tab_id = "w2:t3".to_owned();
        moved.state_change_seq = first.state_change_seq;
        let (service, prompts, _temp) = service(
            &test_state(true),
            vec![snapshot(vec![first]), snapshot(vec![moved.clone()])],
            Ok(moved),
        );
        let result = service.deliver_reviewed_prompt(
            &"build-group".parse().unwrap(),
            &[MemberTarget {
                session_id: SESSION.parse().unwrap(),
                membership_id: MEMBERSHIP.parse().unwrap(),
            }],
            "Continue after the move",
            true,
        );
        assert!(matches!(
            result[0],
            TargetDelivery::Delivered {
                final_state: MissionAgentState::Done,
                ..
            }
        ));
        assert_eq!(prompts.lock().as_slice(), ["worker"]);
    }

    #[test]
    fn mixed_multi_target_fanout_reports_each_target_independently() {
        const SECOND_SESSION: &str = "tether-0197f198000070008000000000000003";
        const SECOND_MEMBERSHIP: &str = "0197f198000070008000000000000012";
        let mut state = test_state(true);
        let mut second_record = state.sessions[0].clone();
        second_record.id = SECOND_SESSION.parse().unwrap();
        second_record.tmux_session_id = Some("$2".parse().unwrap());
        second_record.ownership_proof = Some("0197f198000070008000000000000098".parse().unwrap());
        state.sessions.push(second_record);
        state.orchestration_groups[0]
            .workers
            .push(OrchestrationMember {
                session_id: SECOND_SESSION.parse().unwrap(),
                membership_id: SECOND_MEMBERSHIP.parse().unwrap(),
                title: Some("Worker two".parse().unwrap()),
                capabilities: OrchestrationCapabilities {
                    observe_output: true,
                    open_interactive: true,
                    prompt_agent: true,
                },
            });

        let first = agent(
            MEMBERSHIP,
            "w1:p1",
            "term-1",
            Some("codex"),
            AgentStatus::Idle,
        );
        let mut blocked = agent(
            SECOND_MEMBERSHIP,
            "w1:p2",
            "term-2",
            Some("codex"),
            AgentStatus::Working,
        );
        blocked.name = Some("worker-two".to_owned());
        blocked
            .tokens
            .insert(SESSION_TOKEN.to_owned(), SECOND_SESSION.to_owned());
        let combined = snapshot(vec![first.clone(), blocked]);
        let done = agent(
            MEMBERSHIP,
            "w1:p1",
            "term-1",
            Some("codex"),
            AgentStatus::Done,
        );
        let (service, prompts, _temp) = service(
            &state,
            vec![combined.clone(), combined.clone(), combined],
            Ok(done),
        );
        let result = service.deliver_reviewed_prompt(
            &"build-group".parse().unwrap(),
            &[
                MemberTarget {
                    session_id: SESSION.parse().unwrap(),
                    membership_id: MEMBERSHIP.parse().unwrap(),
                },
                MemberTarget {
                    session_id: SECOND_SESSION.parse().unwrap(),
                    membership_id: SECOND_MEMBERSHIP.parse().unwrap(),
                },
            ],
            "Do one reviewed thing",
            true,
        );
        assert!(matches!(result[0], TargetDelivery::Delivered { .. }));
        assert!(matches!(result[1], TargetDelivery::Rejected { .. }));
        assert_eq!(prompts.lock().as_slice(), ["worker"]);
    }

    #[test]
    fn uncertain_delivery_is_never_retried() {
        let idle = agent(
            MEMBERSHIP,
            "w1:p1",
            "term-1",
            Some("codex"),
            AgentStatus::Idle,
        );
        let (service, prompts, _temp) = service(
            &test_state(true),
            vec![snapshot(vec![idle.clone()]), snapshot(vec![idle])],
            Err(PromptDeliveryError::Uncertain),
        );
        let target = MemberTarget {
            session_id: SESSION.parse().unwrap(),
            membership_id: MEMBERSHIP.parse().unwrap(),
        };
        let result = service.deliver_reviewed_prompt(
            &"build-group".parse().unwrap(),
            &[target],
            "Do work",
            true,
        );
        assert!(matches!(result[0], TargetDelivery::Uncertain { .. }));
        assert_eq!(prompts.lock().len(), 1);
    }
}
