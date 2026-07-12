use chrono::{DateTime, Utc};
use herdr_tether::observer_manager::{
    ObserverManagerAction, ObserverManagerEvent, ObserverManagerOutcome, ObserverManagerScreen,
    ObserverManagerState, render_to_text,
};
use herdr_tether::{
    model::OrchestrationMembershipId,
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
            },
        }],
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
    assert_eq!(
        manager.item_labels(),
        vec![
            "test / checks / shell",
            "build / atlas / dev (2)",
            "build / atlas / dev",
        ]
    );
    for id in &session_ids {
        assert!(!manager.item_labels().join(" ").contains(id));
        assert!(!manager.footer_text().contains(id));
    }

    assert_eq!(
        manager.handle(ObserverManagerEvent::Confirm),
        ObserverManagerOutcome::Continue
    );
    assert_eq!(manager.screen(), ObserverManagerScreen::CreateWorkers);
    assert_eq!(
        manager.item_labels(),
        vec!["[ ] build / atlas / dev (2)", "[ ] build / atlas / dev"]
    );
    manager.handle(ObserverManagerEvent::Toggle);
    manager.handle(ObserverManagerEvent::Next);
    manager.handle(ObserverManagerEvent::Toggle);
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
        vec!["Open Observer", "Edit workers", "Delete group"]
    );
    assert_eq!(
        open.handle(ObserverManagerEvent::Confirm),
        ObserverManagerOutcome::Action(ObserverManagerAction::Launch {
            group_id: group_id.clone()
        })
    );

    let mut edit = ObserverManagerState::from_state(&state, None).unwrap();
    edit.handle(ObserverManagerEvent::Confirm);
    edit.handle(ObserverManagerEvent::Next);
    edit.handle(ObserverManagerEvent::Confirm);
    assert_eq!(edit.screen(), ObserverManagerScreen::EditWorkers);
    assert_eq!(
        edit.item_labels(),
        vec!["[x] build / atlas / dev (2)", "[ ] test / checks / shell",]
    );
    edit.handle(ObserverManagerEvent::Toggle);
    edit.handle(ObserverManagerEvent::Next);
    edit.handle(ObserverManagerEvent::Toggle);
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
    let ObserverManagerOutcome::Action(ObserverManagerAction::Create { id, title, .. }) =
        manager.handle(ObserverManagerEvent::Confirm)
    else {
        panic!("create expected");
    };
    assert!(id.as_str().len() <= 64);
    assert!(title.as_str().len() <= 128);
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
