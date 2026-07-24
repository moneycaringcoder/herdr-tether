use chrono::{DateTime, Utc};
use herdr_tether::observer_manager::{
    ObserverManagerAction, ObserverManagerEvent, ObserverManagerOutcome, ObserverManagerScreen,
    ObserverManagerState, render_to_text,
};
use herdr_tether::{
    agent_view::AgentViewFilter,
    model::{OrchestrationMembershipId, SessionId},
    state::{
        OrchestrationCapabilities, OrchestrationGroup, OrchestrationMember, SessionRecord,
        SessionStatus, State,
    },
};

fn session(suffix: u8, host: &str, directory: &str, preset: Option<&str>) -> SessionRecord {
    let now = DateTime::parse_from_rfc3339(&format!("2026-07-12T00:00:0{suffix}Z"))
        .unwrap()
        .with_timezone(&Utc);
    SessionRecord {
        herdr_agent: None,
        id: format!("tether-0197f1980000700080000000000000{suffix:02}")
            .parse()
            .unwrap(),
        host: host.to_owned(),
        target: "local".to_owned(),
        directory: directory.to_owned(),
        preset: preset.map(str::to_owned),
        command: Some("exec true".to_owned()),
        tmux_session_id: Some(format!("${suffix}").parse().unwrap()),
        ownership_proof: Some(
            format!("0197f1980000700080000000000000{suffix:02}")
                .parse()
                .unwrap(),
        ),
        status: SessionStatus::Running,
        created_at: now,
        last_used_at: now,
        closed_at: None,
        exit_status: None,
    }
}

fn state_with_sessions() -> State {
    let mut ended = session(4, "retired", "/srv/ended", None);
    ended.status = SessionStatus::Ended;
    ended.closed_at = Some(ended.last_used_at);
    let mut inexact = session(5, "unsafe", "/srv/inexact", None);
    inexact.ownership_proof = None;
    State {
        version: State::CURRENT_VERSION,
        sessions: vec![
            session(1, "build", "/srv/atlas", Some("dev")),
            session(2, "build", "/srv/atlas", Some("dev")),
            session(3, "test", "/srv/checks", None),
            ended,
            inexact,
        ],
        orchestration_groups: Vec::new(),
    }
}

fn group(state: &State) -> OrchestrationGroup {
    OrchestrationGroup {
        id: "observer-build-atlas".parse().unwrap(),
        title: "Observer build atlas".parse().unwrap(),
        orchestrator_session_id: state.sessions[0].id,
        workers: vec![OrchestrationMember {
            session_id: state.sessions[1].id,
            membership_id: OrchestrationMembershipId::new(),
            title: Some("build / atlas / dev (2)".parse().unwrap()),
            capabilities: OrchestrationCapabilities {
                observe_output: true,
                open_interactive: true,
                prompt_agent: false,
            },
        }],
    }
}

fn state_with_groups(count: usize) -> State {
    let mut state = state_with_sessions();
    let template = group(&state);
    state.orchestration_groups = (0..count)
        .map(|index| {
            let mut group = template.clone();
            group.id = format!("observer-group-{index:02}").parse().unwrap();
            group.title =
                format!("Observer group {index:02} with a deliberately narrow-terminal label")
                    .parse()
                    .unwrap();
            group
        })
        .collect();
    state
}

#[test]
fn manager_uses_one_resize_fallback_until_the_minimum_geometry() {
    let state = state_with_groups(20);
    let manager = ObserverManagerState::from_state(&state, None).unwrap();
    for (width, height) in [(39, 8), (40, 7)] {
        let rendered = render_to_text(width, height, &manager).unwrap();
        assert!(
            rendered.contains("Resize terminal to at least 40x8"),
            "{width}x{height}: {rendered:?}"
        );
        assert!(!rendered.contains("Observer group 00"), "{width}x{height}");
        assert!(!rendered.contains("Enter manage"), "{width}x{height}");
    }

    let rendered = render_to_text(40, 8, &manager).unwrap();
    assert!(!rendered.contains("Resize terminal"));
    assert!(rendered.contains('…'), "{rendered:?}");
    assert!(rendered.contains("Observer group 00"));
    assert!(rendered.contains("Enter manage"));
    assert!(rendered.contains("Esc/Backspace back"));
}

#[test]
fn manager_viewport_reports_position_and_both_directions_without_changing_selection() {
    let state = state_with_groups(19);
    let mut manager = ObserverManagerState::from_state(&state, None).unwrap();
    let cases = [
        (0, "1/20 · more below"),
        (9, "10/20 · more above · more below"),
        (19, "20/20 · more above"),
    ];
    let mut current = 0;
    for (target, metadata) in cases {
        for _ in current..target {
            manager.handle(ObserverManagerEvent::Next);
        }
        current = target;
        let selected = manager.selected_index();
        let rendered = render_to_text(40, 8, &manager).unwrap();
        assert!(rendered.contains(metadata), "{metadata}: {rendered:?}");
        assert_eq!(manager.selected_index(), selected);
        assert_eq!(rendered.lines().count(), 8);
        assert!(rendered.lines().all(|line| line.chars().count() <= 40));
    }
}

#[test]
fn create_flow_uses_safe_labels_and_default_capabilities_without_exposing_ids() {
    let state = state_with_sessions();
    let session_ids = state
        .sessions
        .iter()
        .map(|record| record.id.to_string())
        .collect::<Vec<_>>();
    let mut manager = ObserverManagerState::from_state(&state, None).unwrap();

    assert_eq!(manager.screen(), ObserverManagerScreen::Groups);
    assert_eq!(manager.item_labels(), vec!["+ Create Observer"]);
    assert!(manager.footer_text().contains("n Create"));
    assert_eq!(
        manager.handle(ObserverManagerEvent::Create),
        ObserverManagerOutcome::Continue
    );
    assert_eq!(manager.screen(), ObserverManagerScreen::CreateOrchestrator);
    let orchestrator_labels = manager.item_labels();
    assert_eq!(orchestrator_labels.len(), 3);
    assert!(
        orchestrator_labels
            .iter()
            .all(|label| label.starts_with("ORCHESTRATOR "))
    );
    assert!(
        orchestrator_labels
            .iter()
            .any(|label| label.contains("test / checks / shell"))
    );
    let duplicate_labels = orchestrator_labels
        .iter()
        .filter(|label| label.contains("build / atlas / dev"))
        .collect::<Vec<_>>();
    assert_eq!(duplicate_labels.len(), 2);
    assert_ne!(duplicate_labels[0], duplicate_labels[1]);
    for id in &session_ids {
        assert!(!manager.item_labels().join(" ").contains(id));
        assert!(!manager.footer_text().contains(id));
    }

    assert_eq!(
        manager.handle(ObserverManagerEvent::Confirm),
        ObserverManagerOutcome::Continue
    );
    assert_eq!(manager.screen(), ObserverManagerScreen::CreateWorkers);
    let worker_labels = manager.item_labels();
    assert_eq!(worker_labels.len(), 2);
    assert!(
        worker_labels
            .iter()
            .all(|label| label.starts_with("[ ] WORKER "))
    );
    manager.handle(ObserverManagerEvent::Toggle);
    manager.handle(ObserverManagerEvent::Next);
    manager.handle(ObserverManagerEvent::Toggle);
    assert_eq!(
        manager.handle(ObserverManagerEvent::Confirm),
        ObserverManagerOutcome::Continue
    );
    assert_eq!(manager.screen(), ObserverManagerScreen::ReviewTopology);
    let ObserverManagerOutcome::Action(ObserverManagerAction::Create {
        id,
        title,
        orchestrator_session_id,
        workers,
    }) = manager.handle(ObserverManagerEvent::Confirm)
    else {
        panic!("worker confirmation must request one atomic create");
    };
    assert_eq!(id.as_str(), "observer-test-checks");
    assert_eq!(title.as_str(), "Observer test checks");
    assert_eq!(orchestrator_session_id, state.sessions[2].id);
    assert_eq!(workers.len(), 2);
    assert!(workers.iter().all(|worker| {
        worker.capabilities.observe_output && worker.capabilities.open_interactive
    }));
    assert!(workers.iter().all(|worker| worker.title.is_some()));
}

#[test]
fn prompt_capability_requires_supported_herdr_and_an_explicit_agent_hint() {
    let mut state = state_with_sessions();
    state.sessions[1].herdr_agent = Some("codex".parse().unwrap());

    let mut manager = ObserverManagerState::from_state(&state, None).unwrap();
    manager.handle(ObserverManagerEvent::Create);
    manager.handle(ObserverManagerEvent::Confirm);
    manager.handle(ObserverManagerEvent::Toggle);
    manager.handle(ObserverManagerEvent::TogglePrompt);
    manager.handle(ObserverManagerEvent::Confirm);
    let ObserverManagerOutcome::Action(ObserverManagerAction::Create { workers, .. }) =
        manager.handle(ObserverManagerEvent::Confirm)
    else {
        panic!("reviewed prompt-capable topology must create atomically");
    };
    assert!(workers[0].capabilities.prompt_agent);

    let mut unsupported = ObserverManagerState::from_state(&state, None).unwrap();
    unsupported.set_mission_control_available(false);
    unsupported.handle(ObserverManagerEvent::Create);
    unsupported.handle(ObserverManagerEvent::Confirm);
    unsupported.handle(ObserverManagerEvent::Toggle);
    unsupported.handle(ObserverManagerEvent::TogglePrompt);
    assert!(
        unsupported
            .footer_text()
            .contains("requires Herdr 0.7.5 or newer")
    );

    let no_hint = state_with_sessions();
    let mut no_hint_manager = ObserverManagerState::from_state(&no_hint, None).unwrap();
    no_hint_manager.handle(ObserverManagerEvent::Create);
    no_hint_manager.handle(ObserverManagerEvent::Confirm);
    no_hint_manager.handle(ObserverManagerEvent::Toggle);
    no_hint_manager.handle(ObserverManagerEvent::TogglePrompt);
    assert!(
        no_hint_manager
            .footer_text()
            .contains("explicit --herdr-agent KIND hint")
    );
}

#[test]
fn unsupported_herdr_keeps_observer_available_with_an_upgrade_message() {
    let mut state = state_with_sessions();
    state.orchestration_groups.push(group(&state));
    let mut manager = ObserverManagerState::from_state(&state, None).unwrap();
    manager.set_mission_control_available(false);
    manager.handle(ObserverManagerEvent::Confirm);

    assert_eq!(
        manager.item_labels()[0],
        "Open Observer · Herdr 0.7.5+ unlocks Mission Control"
    );
    assert!(matches!(
        manager.handle(ObserverManagerEvent::Confirm),
        ObserverManagerOutcome::Action(ObserverManagerAction::Launch { .. })
    ));
}

#[test]
fn create_explains_missing_or_insufficient_running_workloads_immediately() {
    let mut empty = state_with_sessions();
    empty.sessions.clear();
    let mut manager = ObserverManagerState::from_state(&empty, None).unwrap();
    manager.handle(ObserverManagerEvent::Create);
    assert_eq!(manager.screen(), ObserverManagerScreen::Groups);
    assert!(
        manager
            .footer_text()
            .contains("No running exact-owned workloads")
    );

    let mut single = state_with_sessions();
    single.sessions.truncate(1);
    let mut manager = ObserverManagerState::from_state(&single, None).unwrap();
    manager.handle(ObserverManagerEvent::Create);
    assert_eq!(manager.screen(), ObserverManagerScreen::Groups);
    assert!(manager.footer_text().contains("at least two running"));
}

#[test]
fn existing_groups_open_edit_and_delete_without_lifecycle_actions() {
    let mut state = state_with_sessions();
    state.orchestration_groups.push(group(&state));
    let group_id = state.orchestration_groups[0].id.clone();

    let mut open = ObserverManagerState::from_state(&state, None).unwrap();
    open.handle(ObserverManagerEvent::Confirm);
    assert_eq!(open.screen(), ObserverManagerScreen::GroupActions);
    assert_eq!(
        open.item_labels(),
        vec![
            "Open Mission Control",
            "Edit workers",
            "Change orchestrator",
            "Show group in Agents sidebar",
            "Show attention in Agents sidebar",
            "Show remote agents in sidebar",
            "Restore default Agents sidebar",
            "Delete group"
        ]
    );
    assert_eq!(
        open.handle(ObserverManagerEvent::Confirm),
        ObserverManagerOutcome::Action(ObserverManagerAction::Launch {
            group_id: group_id.clone()
        })
    );

    let mut views = ObserverManagerState::from_state(&state, None).unwrap();
    views.handle(ObserverManagerEvent::Confirm);
    for _ in 0..3 {
        views.handle(ObserverManagerEvent::Next);
    }
    assert_eq!(
        views.handle(ObserverManagerEvent::Confirm),
        ObserverManagerOutcome::Action(ObserverManagerAction::SetAgentView {
            group_id: group_id.clone(),
            filter: AgentViewFilter::All,
        })
    );
    views.handle(ObserverManagerEvent::Next);
    assert_eq!(
        views.handle(ObserverManagerEvent::Confirm),
        ObserverManagerOutcome::Action(ObserverManagerAction::SetAgentView {
            group_id: group_id.clone(),
            filter: AgentViewFilter::NeedsAttention,
        })
    );
    views.handle(ObserverManagerEvent::Next);
    assert_eq!(
        views.handle(ObserverManagerEvent::Confirm),
        ObserverManagerOutcome::Action(ObserverManagerAction::SetAgentView {
            group_id: group_id.clone(),
            filter: AgentViewFilter::Remote,
        })
    );
    views.handle(ObserverManagerEvent::Next);
    assert_eq!(
        views.handle(ObserverManagerEvent::Confirm),
        ObserverManagerOutcome::Action(ObserverManagerAction::ClearAgentView)
    );

    let mut edit = ObserverManagerState::from_state(&state, None).unwrap();
    edit.handle(ObserverManagerEvent::Confirm);
    edit.handle(ObserverManagerEvent::Next);
    edit.handle(ObserverManagerEvent::Confirm);
    assert_eq!(edit.screen(), ObserverManagerScreen::EditWorkers);
    let edit_labels = edit.item_labels();
    assert_eq!(edit_labels.len(), 2);
    assert!(edit_labels[0].starts_with("[x] WORKER "));
    assert!(edit_labels[1].starts_with("[ ] WORKER "));
    edit.handle(ObserverManagerEvent::Toggle);
    edit.handle(ObserverManagerEvent::Next);
    edit.handle(ObserverManagerEvent::Toggle);
    assert_eq!(
        edit.handle(ObserverManagerEvent::Confirm),
        ObserverManagerOutcome::Continue
    );
    assert_eq!(edit.screen(), ObserverManagerScreen::ReviewTopology);
    let ObserverManagerOutcome::Action(ObserverManagerAction::ReplaceWorkers {
        expected_group,
        workers,
    }) = edit.handle(ObserverManagerEvent::Confirm)
    else {
        panic!("edit confirmation must request one metadata replacement");
    };
    assert_eq!(expected_group.id, group_id);
    assert_eq!(expected_group, state.orchestration_groups[0]);
    assert_eq!(workers.len(), 1);
    assert_eq!(workers[0].session_id, state.sessions[2].id);

    let mut delete = ObserverManagerState::from_state(&state, None).unwrap();
    delete.handle(ObserverManagerEvent::Confirm);
    delete.handle(ObserverManagerEvent::Delete);
    assert_eq!(delete.screen(), ObserverManagerScreen::ConfirmDelete);
    assert!(delete.item_labels()[0].contains("Observer build atlas"));
    assert_eq!(
        delete.handle(ObserverManagerEvent::Back),
        ObserverManagerOutcome::Continue
    );
    assert_eq!(delete.screen(), ObserverManagerScreen::GroupActions);
    delete.handle(ObserverManagerEvent::Delete);
    assert_eq!(
        delete.handle(ObserverManagerEvent::ConfirmDelete),
        ObserverManagerOutcome::Action(ObserverManagerAction::Delete {
            expected_group: state.orchestration_groups[0].clone()
        })
    );
}

#[test]
fn edit_without_changes_preserves_and_displays_non_running_existing_members() {
    let mut state = state_with_sessions();
    let mut existing = group(&state);
    existing.workers[0].title = None;
    state.orchestration_groups.push(existing);
    state.sessions[1].status = SessionStatus::Ended;
    state.sessions[1].closed_at = Some(state.sessions[1].last_used_at);
    let expected_worker = state.sessions[1].id;

    let mut manager = ObserverManagerState::from_state(&state, None).unwrap();
    manager.handle(ObserverManagerEvent::Confirm);
    manager.handle(ObserverManagerEvent::Next);
    manager.handle(ObserverManagerEvent::Confirm);

    assert_eq!(manager.screen(), ObserverManagerScreen::EditWorkers);
    let labels = manager.item_labels();
    assert!(
        labels
            .iter()
            .any(|label| label.contains("Unavailable worker 1")),
        "{labels:?}"
    );
    assert_eq!(
        manager.handle(ObserverManagerEvent::Confirm),
        ObserverManagerOutcome::Continue
    );
    let ObserverManagerOutcome::Action(ObserverManagerAction::ReplaceWorkers { workers, .. }) =
        manager.handle(ObserverManagerEvent::Confirm)
    else {
        panic!("an unchanged edit must preserve existing membership");
    };
    assert_eq!(workers.len(), 1);
    assert_eq!(workers[0].session_id, expected_worker);
    assert_eq!(workers[0].title, None);
}
#[test]
fn edit_without_changes_preserves_existing_worker_order() {
    let mut state = state_with_sessions();
    let mut existing = group(&state);
    existing.workers.push(OrchestrationMember {
        session_id: state.sessions[2].id,
        membership_id: OrchestrationMembershipId::new(),
        title: Some("test / checks / shell".parse().unwrap()),
        capabilities: OrchestrationCapabilities {
            observe_output: true,
            open_interactive: true,
            prompt_agent: false,
        },
    });
    let expected = existing
        .workers
        .iter()
        .map(|worker| worker.session_id)
        .collect::<Vec<_>>();
    state.orchestration_groups.push(existing);

    let mut manager = ObserverManagerState::from_state(&state, None).unwrap();
    manager.handle(ObserverManagerEvent::Confirm);
    manager.handle(ObserverManagerEvent::Next);
    manager.handle(ObserverManagerEvent::Confirm);
    assert_eq!(
        manager.handle(ObserverManagerEvent::Confirm),
        ObserverManagerOutcome::Continue
    );
    let ObserverManagerOutcome::Action(ObserverManagerAction::ReplaceWorkers { workers, .. }) =
        manager.handle(ObserverManagerEvent::Confirm)
    else {
        panic!("an unchanged edit must preserve worker order");
    };

    assert_eq!(
        workers
            .iter()
            .map(|worker| worker.session_id)
            .collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn delete_confirmation_leads_with_metadata_only_safety_text() {
    let mut state = state_with_sessions();
    let mut existing = group(&state);
    existing.title = format!("Observer {}", "x".repeat(100)).parse().unwrap();
    state.orchestration_groups.push(existing);
    let mut manager = ObserverManagerState::from_state(&state, None).unwrap();
    manager.handle(ObserverManagerEvent::Confirm);
    manager.handle(ObserverManagerEvent::Delete);

    assert!(manager.item_labels()[0].starts_with("Metadata only; workloads keep running."));
    let rendered = render_to_text(40, 10, &manager).unwrap();
    assert!(rendered.contains("Metadata only"));
}

#[test]
fn group_ids_get_deterministic_safe_suffixes() {
    let mut state = state_with_sessions();
    state.orchestration_groups.push(group(&state));
    let mut manager = ObserverManagerState::from_state(&state, None).unwrap();
    manager.handle(ObserverManagerEvent::Create);
    manager.handle(ObserverManagerEvent::Next);
    manager.handle(ObserverManagerEvent::Next);
    manager.handle(ObserverManagerEvent::Confirm);
    manager.handle(ObserverManagerEvent::Toggle);
    manager.handle(ObserverManagerEvent::Confirm);
    let ObserverManagerOutcome::Action(ObserverManagerAction::Create { id, title, .. }) =
        manager.handle(ObserverManagerEvent::Confirm)
    else {
        panic!("create expected");
    };
    assert_eq!(id.as_str(), "observer-build-atlas-2");
    assert_eq!(title.as_str(), "Observer build atlas 2");
}

#[test]
fn manager_render_is_dense_theme_default_and_contains_no_raw_ids() {
    let mut state = state_with_sessions();
    state.orchestration_groups.push(group(&state));
    let manager = ObserverManagerState::from_state(
        &state,
        Some("Workers updated; no workload lifecycle changed".to_owned()),
    )
    .unwrap();
    let rendered = render_to_text(60, 12, &manager).unwrap();
    assert!(rendered.contains("Tether · Observers"));
    assert!(rendered.contains("Observer build atlas"));
    assert!(rendered.contains("Workers updated"));
    for record in &state.sessions {
        assert!(!rendered.contains(&record.id.to_string()));
    }
    assert!(rendered.contains("> Observer build atlas"));
}

#[test]
fn generated_identity_never_exceeds_validation_bounds_for_long_digit_labels() {
    let state = State {
        version: State::CURRENT_VERSION,
        sessions: vec![
            session(
                1,
                "2024-build-cluster-eu-01-extra",
                "/srv/2025-repository-with-a-very-long-name",
                None,
            ),
            session(
                2,
                "2024-build-cluster-eu-01-extra",
                "/srv/2025-repository-with-a-very-long-name",
                None,
            ),
        ],
        orchestration_groups: Vec::new(),
    };
    let mut manager = ObserverManagerState::from_state(&state, None).unwrap();
    manager.handle(ObserverManagerEvent::Create);
    manager.handle(ObserverManagerEvent::Confirm);
    manager.handle(ObserverManagerEvent::Toggle);
    manager.handle(ObserverManagerEvent::Confirm);
    let ObserverManagerOutcome::Action(ObserverManagerAction::Create { id, title, .. }) =
        manager.handle(ObserverManagerEvent::Confirm)
    else {
        panic!("create expected");
    };
    assert!(id.as_str().len() <= 64);
    assert!(title.as_str().len() <= 128);
}

#[test]
fn create_edit_and_reassign_require_review_before_one_exact_action() {
    let mut state = state_with_sessions();
    state.orchestration_groups.push(group(&state));

    let mut create = ObserverManagerState::from_state(&state, None).unwrap();
    create.handle(ObserverManagerEvent::Create);
    create.handle(ObserverManagerEvent::Confirm);
    create.handle(ObserverManagerEvent::Toggle);
    assert_eq!(
        create.handle(ObserverManagerEvent::Confirm),
        ObserverManagerOutcome::Continue
    );
    assert_eq!(create.screen(), ObserverManagerScreen::ReviewTopology);
    assert!(
        create
            .item_labels()
            .iter()
            .any(|row| row.contains("ORCHESTRATOR"))
    );
    assert!(
        create
            .item_labels()
            .iter()
            .any(|row| row.contains("WORKER"))
    );
    assert_eq!(
        create.handle(ObserverManagerEvent::Back),
        ObserverManagerOutcome::Continue
    );
    assert_eq!(create.screen(), ObserverManagerScreen::CreateWorkers);
    assert!(create.item_labels()[0].starts_with("[x] WORKER"));
    create.handle(ObserverManagerEvent::Confirm);
    assert!(matches!(
        create.handle(ObserverManagerEvent::Confirm),
        ObserverManagerOutcome::Action(ObserverManagerAction::Create { .. })
    ));

    let mut edit = ObserverManagerState::from_state(&state, None).unwrap();
    edit.handle(ObserverManagerEvent::Confirm);
    edit.handle(ObserverManagerEvent::Edit);
    assert_eq!(edit.screen(), ObserverManagerScreen::EditWorkers);
    edit.handle(ObserverManagerEvent::Confirm);
    assert_eq!(edit.screen(), ObserverManagerScreen::ReviewTopology);
    edit.handle(ObserverManagerEvent::Back);
    assert_eq!(edit.screen(), ObserverManagerScreen::EditWorkers);
    edit.handle(ObserverManagerEvent::Confirm);
    let expected = state.orchestration_groups[0].clone();
    assert_eq!(
        edit.handle(ObserverManagerEvent::Confirm),
        ObserverManagerOutcome::Action(ObserverManagerAction::ReplaceWorkers {
            expected_group: expected.clone(),
            workers: vec![herdr_tether::orchestration::OrchestrationWorkerSpec {
                session_id: expected.workers[0].session_id,
                title: expected.workers[0].title.clone(),
                capabilities: expected.workers[0].capabilities,
            }],
        })
    );

    let mut reassign = ObserverManagerState::from_state(&state, None).unwrap();
    reassign.handle(ObserverManagerEvent::Confirm);
    reassign.handle(ObserverManagerEvent::Next);
    reassign.handle(ObserverManagerEvent::Next);
    reassign.handle(ObserverManagerEvent::Confirm);
    assert_eq!(reassign.screen(), ObserverManagerScreen::ChangeOrchestrator);
    let worker_index = reassign
        .item_labels()
        .iter()
        .position(|row| row.contains("build / atlas / dev"))
        .unwrap();
    while reassign.selected_index() != worker_index {
        reassign.handle(ObserverManagerEvent::Next);
    }
    reassign.handle(ObserverManagerEvent::Confirm);
    assert_eq!(reassign.screen(), ObserverManagerScreen::ReviewTopology);
    let review = reassign.item_labels().join("\n");
    assert!(review.contains("Promote selected WORKER"));
    assert!(review.contains("remove from workers"));
    assert_eq!(
        reassign.handle(ObserverManagerEvent::Confirm),
        ObserverManagerOutcome::Action(ObserverManagerAction::ReassignOrchestrator {
            expected_group: expected,
            orchestrator_session_id: state.sessions[1].id,
        })
    );
}

#[test]
fn reassignment_review_renders_only_the_proposed_orchestrator_topology() {
    let mut state = state_with_sessions();
    state.orchestration_groups.push(group(&state));
    let mut manager = ObserverManagerState::from_state(&state, None).unwrap();

    manager.handle(ObserverManagerEvent::Confirm);
    manager.handle(ObserverManagerEvent::Next);
    manager.handle(ObserverManagerEvent::Next);
    manager.handle(ObserverManagerEvent::Confirm);
    let worker_index = manager
        .item_labels()
        .iter()
        .position(|row| row.contains("build / atlas / dev"))
        .unwrap();
    while manager.selected_index() != worker_index {
        manager.handle(ObserverManagerEvent::Next);
    }
    let proposed_orchestrator = manager.item_labels()[worker_index].clone();
    manager.handle(ObserverManagerEvent::Confirm);

    assert_eq!(manager.screen(), ObserverManagerScreen::ReviewTopology);
    let rendered = render_to_text(100, 16, &manager).unwrap();
    assert_eq!(rendered.matches("ORCHESTRATOR").count(), 1);
    assert!(rendered.contains(&proposed_orchestrator));
    assert!(!rendered.contains("Observer build atlas · ORCHESTRATOR"));
}

#[test]
fn ambiguous_orchestrator_labels_keep_the_candidate_reference_token() {
    let mut state = state_with_sessions();
    state.orchestration_groups.push(group(&state));

    let manager = ObserverManagerState::from_state(&state, None).unwrap();
    let row = &manager.item_labels()[0];
    let reference = state.sessions[0]
        .id
        .reference_token(SessionId::MAX_REFERENCE_WIDTH);

    assert!(row.contains(&format!("ORCHESTRATOR build / atlas / dev [{reference}]")));
    assert_eq!(row.matches('[').count(), 1);
    assert_eq!(row.matches(']').count(), 1);
}

#[test]
fn group_rows_report_topology_health_without_raw_session_ids() {
    let mut state = state_with_sessions();
    let mut existing = group(&state);
    existing.workers.push(OrchestrationMember {
        session_id: "tether-0197f198000070008000000000000099".parse().unwrap(),
        membership_id: OrchestrationMembershipId::new(),
        title: Some("missing worker".parse().unwrap()),
        capabilities: OrchestrationCapabilities {
            observe_output: true,
            open_interactive: true,
            prompt_agent: false,
        },
    });
    state.sessions[1].status = SessionStatus::Ended;
    state.sessions[1].closed_at = Some(state.sessions[1].last_used_at);
    existing.orchestrator_session_id = state.sessions[2].id;
    state.orchestration_groups.push(existing);
    let manager = ObserverManagerState::from_state(&state, None).unwrap();
    let row = &manager.item_labels()[0];
    assert!(row.contains("ORCHESTRATOR test / checks / shell"));
    assert!(!row.contains('['));
    assert!(!row.contains(']'));
    assert!(row.contains("2 workers"));
    assert!(row.contains("2 unavailable"));
    for record in &state.sessions {
        assert!(!row.contains(&record.id.to_string()));
    }
}
#[test]
fn existing_prompt_permission_can_be_revoked_while_agent_control_is_unavailable() {
    let mut state = state_with_sessions();
    let mut existing = group(&state);
    existing.workers[0].capabilities.prompt_agent = true;
    state.orchestration_groups.push(existing);
    let mut manager = ObserverManagerState::from_state(&state, None).unwrap();
    manager.set_mission_control_available(false);
    manager.handle(ObserverManagerEvent::Confirm);
    manager.handle(ObserverManagerEvent::Next);
    manager.handle(ObserverManagerEvent::Confirm);
    assert!(manager.item_labels()[0].contains("[PROMPT]"));

    manager.handle(ObserverManagerEvent::TogglePrompt);
    manager.handle(ObserverManagerEvent::Confirm);
    let ObserverManagerOutcome::Action(ObserverManagerAction::ReplaceWorkers { workers, .. }) =
        manager.handle(ObserverManagerEvent::Confirm)
    else {
        panic!("revoking a grant must remain an atomic reviewed update");
    };
    assert!(!workers[0].capabilities.prompt_agent);
}

#[test]
fn back_from_group_list_returns_to_the_main_tether_picker() {
    let state = state_with_sessions();
    let mut manager = ObserverManagerState::from_state(&state, None).unwrap();
    assert_eq!(
        manager.handle(ObserverManagerEvent::Back),
        ObserverManagerOutcome::Action(ObserverManagerAction::BackToPicker)
    );
}
