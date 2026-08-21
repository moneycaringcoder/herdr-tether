#[path = "../src/observer.rs"]
mod observer;

#[allow(dead_code)]
#[path = "../src/model.rs"]
mod model;

use model::TmuxSessionId;
use observer::{
    AttentionReason, MAX_CAPTURE_BYTES, MAX_CAPTURE_CELLS, MAX_CAPTURE_LINES, MAX_PROMPT_TARGETS,
    ObserverAction, ObserverAgentState, ObserverCapabilities, ObserverCapture, ObserverInputKind,
    ObserverKey, ObserverLifecycle, ObserverOutcome, ObserverState, ObserverWorker,
    action_for_input, action_for_key, observer_theme_style, render, render_to_styles,
    render_to_text, sanitize_capture, worker_rects,
};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

fn worker(id: &str) -> ObserverWorker {
    ObserverWorker {
        id: id.to_owned(),
        title: Some(format!("Worker {id}")),
        capabilities: ObserverCapabilities {
            observe_output: true,
            open_interactive: true,
            prompt_agent: false,
        },
        lifecycle: ObserverLifecycle::Running,
        agent_state: ObserverAgentState::Unknown,
        live_agent: false,
        owned: true,
        last_observed: None,
        incarnation: None,
        latency_ms: None,
        capture: Some(format!("output-{id}")),
    }
}

fn state(count: usize) -> ObserverState {
    ObserverState::new((0..count).map(|index| worker(&index.to_string())).collect())
}

fn session_worker(id: &str, title: &str) -> ObserverWorker {
    ObserverWorker {
        title: Some(title.to_owned()),
        capture: Some("output".to_owned()),
        ..worker(id)
    }
}

#[test]
fn pages_and_layouts_are_deterministic_for_supported_counts() {
    for (count, pages, visible) in [
        (0, 1, 0),
        (1, 1, 1),
        (2, 1, 2),
        (3, 1, 3),
        (4, 1, 4),
        (5, 2, 4),
        (64, 16, 4),
    ] {
        let observer = state(count);
        assert_eq!(observer.page_count(), pages, "worker count {count}");
        assert_eq!(
            observer.visible_workers().len(),
            visible,
            "worker count {count}"
        );
    }
    let capped = state(65);
    assert_eq!(capped.workers().len(), 64);
    assert_eq!(capped.page_count(), 16);

    let area = Rect::new(0, 0, 12, 8);
    assert_eq!(worker_rects(area, 1), vec![area]);
    assert_eq!(
        worker_rects(area, 2),
        vec![Rect::new(0, 0, 6, 8), Rect::new(6, 0, 6, 8)]
    );
    assert_eq!(
        worker_rects(area, 3),
        vec![
            Rect::new(0, 0, 6, 4),
            Rect::new(6, 0, 6, 4),
            Rect::new(0, 4, 6, 4),
        ]
    );
    assert_eq!(worker_rects(area, 4).last(), Some(&Rect::new(6, 4, 6, 4)));
    let odd = Rect::new(2, 3, 5, 5);
    assert_eq!(
        worker_rects(odd, 4),
        vec![
            Rect::new(2, 3, 2, 2),
            Rect::new(4, 3, 3, 2),
            Rect::new(2, 5, 2, 3),
            Rect::new(4, 5, 3, 3),
        ]
    );
}

#[test]
fn selection_survives_insert_remove_and_reorder_by_identity() {
    let mut observer = state(5);
    assert_eq!(
        observer.apply(ObserverAction::NextWorker),
        ObserverOutcome::None
    );
    assert_eq!(
        observer.apply(ObserverAction::NextWorker),
        ObserverOutcome::None
    );
    assert_eq!(observer.selected_id(), Some("2"));

    observer.update_workers(vec![worker("9"), worker("2"), worker("0"), worker("1")]);
    assert_eq!(observer.selected_id(), Some("2"));
    assert_eq!(observer.selected_index(), Some(1));

    observer.update_workers(vec![worker("9"), worker("0"), worker("1")]);
    assert_eq!(observer.selected_id(), Some("0"));
    assert_eq!(observer.selected_index(), Some(1));

    observer.update_workers(Vec::new());
    assert_eq!(observer.selected_id(), None);
    observer.update_workers(vec![worker("later")]);
    assert_eq!(observer.selected_id(), Some("later"));
}

#[test]
fn paging_is_bounded_and_selects_a_visible_identity() {
    let mut observer = state(5);
    assert_eq!(observer.page(), 0);
    observer.apply(ObserverAction::PreviousPage);
    assert_eq!(observer.page(), 0);
    observer.apply(ObserverAction::NextPage);
    assert_eq!(observer.page(), 1);
    assert_eq!(observer.selected_id(), Some("4"));
    assert_eq!(observer.visible_workers()[0].id, "4");
    observer.apply(ObserverAction::NextPage);
    assert_eq!(observer.page(), 1);
    observer.apply(ObserverAction::PreviousPage);
    assert_eq!(observer.page(), 0);
    assert_eq!(observer.selected_id(), Some("0"));
}

#[test]
fn hostile_capture_is_sanitized_and_bounded() {
    let hostile =
        "ok\u{1b}[31mRED\u{1b}[0m\u{1b}]0;secret\u{7}x\r\n\u{202e}bi\u{0301}di\u{200b}\0\tend";
    let clean = sanitize_capture(hostile);
    assert_eq!(clean, "okREDx\nbi\u{301}di    end");
    assert!(!clean.contains('\u{1b}'));

    let enormous = format!(
        "{}\n{}",
        "界".repeat(MAX_CAPTURE_CELLS),
        "x\n".repeat(MAX_CAPTURE_LINES + 20)
    );
    let bounded = sanitize_capture(&enormous);
    assert!(bounded.len() <= MAX_CAPTURE_BYTES);
    assert!(bounded.lines().count() <= MAX_CAPTURE_LINES);
    assert!(
        bounded
            .chars()
            .map(|c| if c == '界' { 2 } else { 1 })
            .sum::<usize>()
            <= MAX_CAPTURE_CELLS
    );
    assert!(!bounded.contains('界'));
    assert!(bounded.ends_with("x\n"));
}

#[test]
fn unicode_grapheme_boundaries_are_atomic_at_capture_limits() {
    let clusters = [
        ("regional-indicator flag", "🇺🇸"),
        ("adjacent regional-indicator flags", "🇺🇸🇨🇦"),
        ("keycap", "1\u{fe0f}\u{20e3}"),
        (
            "emoji zwj with modifier and variation selector",
            "👩🏽\u{200d}⚕\u{fe0f}",
        ),
        ("standalone combining sequence", "\u{301}\u{327}"),
        ("combining character sequence", "e\u{301}\u{327}"),
        ("text variation selector", "♥\u{fe0e}"),
        ("emoji variation selector", "♥\u{fe0f}"),
    ];

    for (name, cluster) in clusters {
        let exactly_after = format!("{cluster}{}", "x".repeat(MAX_CAPTURE_BYTES - cluster.len()));
        assert_eq!(
            sanitize_capture(&exactly_after),
            exactly_after,
            "{name} was split at a limit exactly after the cluster"
        );

        let exactly_before = format!("{cluster}{}", "x".repeat(MAX_CAPTURE_BYTES));
        assert_eq!(
            sanitize_capture(&exactly_before),
            "x".repeat(MAX_CAPTURE_BYTES),
            "{name} was split at a limit exactly before the cluster"
        );
    }
}

#[test]
fn adjacent_flags_are_not_truncated_to_orphan_regional_indicators() {
    let adjacent_flags = "🇺🇸🇨🇦";
    let suffix = "x".repeat(MAX_CAPTURE_BYTES - '🇦'.len_utf8());
    let clean = sanitize_capture(&format!("{adjacent_flags}{suffix}"));

    assert_eq!(clean, suffix);
    assert!(!clean.starts_with(['🇺', '🇸', '🇨', '🇦']));
}

#[test]
fn hostile_controls_and_ansi_cannot_escape_grapheme_sanitization() {
    let hostile = concat!(
        "\u{1b}[31m",
        "🇺🇸",
        "\u{1b}[0m",
        "\u{1b}]8;;file:///secret\u{7}",
        "1\u{fe0f}\u{20e3}",
        "\u{1b}]8;;\u{7}",
        "\u{0}\u{7}\u{202e}",
        "👩🏽\u{200d}⚕\u{fe0f}"
    );

    assert_eq!(
        sanitize_capture(hostile),
        "🇺🇸1\u{fe0f}\u{20e3}👩🏽\u{200d}⚕\u{fe0f}"
    );
}

#[test]
fn outcomes_are_read_only_and_open_requires_all_eligibility() {
    assert_eq!(
        action_for_key(ObserverKey::Enter),
        Some(ObserverAction::OpenSelected)
    );
    assert_eq!(
        action_for_key(ObserverKey::Char('r')),
        Some(ObserverAction::Refresh)
    );
    assert_eq!(
        action_for_key(ObserverKey::Char('q')),
        Some(ObserverAction::Quit)
    );
    assert_eq!(
        action_for_key(ObserverKey::Char('e')),
        Some(ObserverAction::ExplainSelected)
    );
    // `x` stops or removes a workload in the picker. Mission Control has no
    // destructive action, so the key stays unmapped here rather than teaching a
    // gesture that destroys something one screen over.
    assert_eq!(action_for_key(ObserverKey::Char('x')), None);
    assert_eq!(
        action_for_key(ObserverKey::Up),
        Some(ObserverAction::PreviousWorker)
    );
    assert_eq!(
        action_for_key(ObserverKey::Left),
        Some(ObserverAction::PreviousWorker)
    );
    assert_eq!(
        action_for_key(ObserverKey::Down),
        Some(ObserverAction::NextWorker)
    );
    assert_eq!(
        action_for_key(ObserverKey::Right),
        Some(ObserverAction::NextWorker)
    );
    assert_eq!(
        action_for_key(ObserverKey::PageUp),
        Some(ObserverAction::PreviousPage)
    );
    assert_eq!(
        action_for_key(ObserverKey::PageDown),
        Some(ObserverAction::NextPage)
    );
    assert_eq!(
        action_for_key(ObserverKey::Tab),
        Some(ObserverAction::NextPage)
    );
    assert_eq!(
        action_for_key(ObserverKey::BackTab),
        Some(ObserverAction::PreviousPage)
    );
    assert_eq!(
        action_for_key(ObserverKey::ControlC),
        Some(ObserverAction::Quit)
    );
    assert_eq!(
        action_for_key(ObserverKey::Escape),
        Some(ObserverAction::Quit)
    );

    let mut eligible = state(1);
    assert_eq!(
        eligible.apply(ObserverAction::OpenSelected),
        ObserverOutcome::OpenSelected {
            worker_id: "0".to_owned()
        }
    );
    assert_eq!(
        eligible.apply(ObserverAction::Refresh),
        ObserverOutcome::Refresh
    );
    assert_eq!(eligible.apply(ObserverAction::Quit), ObserverOutcome::Quit);

    for ineligible in [
        ObserverWorker {
            owned: false,
            ..worker("not-owned")
        },
        ObserverWorker {
            lifecycle: ObserverLifecycle::Ended,
            ..worker("ended")
        },
        ObserverWorker {
            capabilities: ObserverCapabilities {
                observe_output: true,
                open_interactive: false,
                prompt_agent: false,
            },
            ..worker("observe-only")
        },
    ] {
        let id = ineligible.id.clone();
        let mut observer = ObserverState::new(vec![ineligible]);
        assert_eq!(
            observer.apply(ObserverAction::OpenSelected),
            ObserverOutcome::OpenUnavailable { worker_id: id }
        );
    }
}

#[test]
fn duplicate_worker_titles_get_stable_expandable_references_without_changing_identity() {
    let first = "tether-01890f1e7a0070008000000000000001";
    let second = "tether-01890f1e7a0070008000000000000002";
    let distinct = "tether-11890f1e7a0070008000000000000003";
    let workers = vec![
        session_worker(first, "Builder"),
        session_worker(second, "Builder"),
        session_worker(distinct, "Reviewer"),
    ];
    let mut observer = ObserverState::new(workers.clone());

    let rendered = render_to_text(160, 14, &observer).unwrap();
    assert!(rendered.contains("Builder · 01890f1e7a0070008000000000000001"));
    assert!(rendered.contains("Builder · 01890f1e7a0070008000000000000002"));
    assert!(rendered.contains("Reviewer · RUNNING"));
    assert!(!rendered.contains("Reviewer · 11890f1e"));
    assert!(!rendered.contains(first));
    assert!(!rendered.contains(second));

    observer.apply(ObserverAction::NextWorker);
    assert_eq!(observer.selected_id(), Some(second));
    observer.update_workers(vec![
        workers[1].clone(),
        workers[0].clone(),
        workers[2].clone(),
    ]);
    assert_eq!(observer.selected_id(), Some(second));
    assert_eq!(
        observer.apply(ObserverAction::OpenSelected),
        ObserverOutcome::OpenSelected {
            worker_id: second.to_owned(),
        }
    );

    let reordered = render_to_text(160, 14, &observer).unwrap();
    assert!(reordered.contains("Builder · 01890f1e7a0070008000000000000001"));
    assert!(reordered.contains("Builder · 01890f1e7a0070008000000000000002"));
}

#[test]
fn session_reference_tokens_are_deterministic_ascii_and_bounded() {
    use std::str::FromStr;

    let id = model::SessionId::from_str("tether-01890f1e7a0070008000000000000001").unwrap();
    assert_eq!(id.reference_token(0), "01890f1e");
    assert_eq!(id.reference_token(8), "01890f1e");
    assert_eq!(id.reference_token(12), "01890f1e7a00");
    assert_eq!(
        id.reference_token(usize::MAX),
        "01890f1e7a0070008000000000000001"
    );
    for width in 0..=64 {
        let token = id.reference_token(width);
        assert!(token.is_ascii());
        assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!((8..=32).contains(&token.len()));
    }
}

#[test]
fn capture_lifecycle_renders_loading_ready_empty_and_unavailable_distinctly() {
    let mut loading_worker = worker("capture");
    loading_worker.capture = None;
    let mut observer = ObserverState::new(vec![loading_worker]);
    let loading = render_to_text(40, 8, &observer).unwrap();
    assert!(loading.contains("Loading output"), "{loading}");
    assert!(!loading.contains("No captured output"), "{loading}");
    assert!(!loading.contains("Output unavailable"), "{loading}");
    observer.merge_capture("capture", ObserverCapture::Ready(String::new()));
    observer.set_connection_observation("capture", 12, None);
    let ready_empty = render_to_text(40, 8, &observer).unwrap();
    assert!(ready_empty.contains("No captured output"), "{ready_empty}");
    assert!(ready_empty.contains("12ms"), "{ready_empty}");
    assert!(!ready_empty.contains("Loading output"), "{ready_empty}");
    assert!(!ready_empty.contains("Output unavailable"), "{ready_empty}");
    observer.merge_capture("capture", ObserverCapture::Unavailable);
    let unavailable = render_to_text(40, 8, &observer).unwrap();
    assert!(unavailable.contains("Output unavailable"), "{unavailable}");
    assert!(!unavailable.contains("Loading output"), "{unavailable}");
    assert!(!unavailable.contains("No captured output"), "{unavailable}");
}

#[test]
fn truncated_capture_is_marked_and_never_reads_as_complete_output() {
    let mut observer = ObserverState::new(vec![worker("capture")]);

    observer.merge_capture("capture", ObserverCapture::Ready("full output".to_owned()));
    let complete = render_to_text(60, 10, &observer).unwrap();
    assert!(complete.contains("full output"), "{complete}");
    assert!(!complete.contains("TRUNCATED"), "{complete}");

    observer.merge_capture(
        "capture",
        ObserverCapture::Truncated("tail of output".to_owned()),
    );
    let truncated = render_to_text(60, 10, &observer).unwrap();
    // The text still renders, but it is explicitly labelled as incomplete.
    assert!(truncated.contains("tail of output"), "{truncated}");
    assert!(truncated.contains("TRUNCATED"), "{truncated}");
    assert!(!truncated.contains("Output unavailable"), "{truncated}");
    assert!(!truncated.contains("No captured output"), "{truncated}");

    // Truncation is not sticky: a later complete read clears the marker.
    observer.merge_capture("capture", ObserverCapture::Ready("full output".to_owned()));
    let recovered = render_to_text(60, 10, &observer).unwrap();
    assert!(!recovered.contains("TRUNCATED"), "{recovered}");
}

fn live_worker(id: &str, state: ObserverAgentState) -> ObserverWorker {
    ObserverWorker {
        live_agent: true,
        agent_state: state,
        ..worker(id)
    }
}

#[test]
fn unreachable_and_stale_tiles_say_why_and_what_to_do_about_each() {
    // Both mean the tile is not current. A user reading one has to be able to
    // tell which remedy applies without leaving the screen.
    let mut observer = ObserverState::new(vec![ObserverWorker {
        capture: Some("previous output".to_owned()),
        last_observed: Some("2026-08-21T11:00:00Z".to_owned()),
        ..live_worker("a", ObserverAgentState::Working)
    }]);

    // A capture that cannot be re-read, with something retained: the tile shows
    // remembered output and says so, with the time it was live.
    observer.merge_capture("a", ObserverCapture::Unavailable);
    let stale = render_to_text(96, 12, &observer).unwrap();
    assert!(stale.contains("STALE"), "{stale}");
    assert!(stale.contains("retained, not current"), "{stale}");
    assert!(stale.contains("last live 2026-08-21T11:00:00Z"), "{stale}");
    assert!(stale.contains("r retry"), "{stale}");
    assert!(stale.contains("previous output"), "{stale}");

    // Nothing retained: the remedy is to restore access, and the tile must not
    // imply there is old output to look at.
    let mut observer = ObserverState::new(vec![ObserverWorker {
        capabilities: ObserverCapabilities {
            observe_output: true,
            open_interactive: true,
            prompt_agent: true,
        },
        ..live_worker("a", ObserverAgentState::Working)
    }]);
    observer.merge_capture("a", ObserverCapture::Unavailable);
    let unreachable = render_to_text(96, 12, &observer).unwrap();
    assert!(unreachable.contains("UNREACHABLE"), "{unreachable}");
    assert!(
        unreachable.contains("did not answer"),
        "an unreachable tile must say the check failed: {unreachable}"
    );
    assert!(
        unreachable.contains("nothing retained"),
        "an unreachable tile must not imply retained output: {unreachable}"
    );
    assert!(unreachable.contains("r retry"), "{unreachable}");
    assert!(
        !unreachable.contains("retained, not current"),
        "the two states must not share one explanation: {unreachable}"
    );

    // A binding that is no longer exactly one recognized occupant. The output
    // may still read fine, so the tile has to say the claim is what went bad,
    // and that remedy is not the same as waiting for a reread.
    let mut observer = ObserverState::new(vec![live_worker("a", ObserverAgentState::Stale)]);
    observer.merge_capture("a", ObserverCapture::Ready("looks fine".to_owned()));
    let binding = render_to_text(96, 12, &observer).unwrap();
    assert!(binding.contains("STALE"), "{binding}");
    assert!(binding.contains("binding no longer exact"), "{binding}");
    assert!(
        binding.contains("reopen if the pane moved"),
        "a stale binding needs its own remedy: {binding}"
    );
    assert!(binding.contains("looks fine"), "{binding}");

    // A narrow tile keeps the state and the key even though the reason has to
    // shorten: a clipped explanation that loses "r retry" explains nothing. The
    // controls footer carries its own "r retry", so this reads the tile's line.
    let narrow = render_to_text(32, 10, &observer).unwrap();
    let tile_line = narrow
        .lines()
        // The border title carries the bare state; the body line carries the
        // explanation after it.
        .find(|line| line.contains("STALE ·"))
        .unwrap_or_else(|| panic!("{narrow}"));
    assert!(
        tile_line.contains("r retry"),
        "a narrow tile must keep the remedy on the tile: {tile_line:?}"
    );
}

#[test]
fn attention_reports_each_transition_into_blocked_or_done_exactly_once() {
    let mut observer = ObserverState::new(Vec::new());

    // First sight of a working agent is not attention-worthy.
    let attention = observer.update_workers(vec![live_worker("a", ObserverAgentState::Working)]);
    assert!(attention.is_empty());

    let attention = observer.update_workers(vec![live_worker("a", ObserverAgentState::Blocked)]);
    assert_eq!(attention.len(), 1);
    assert_eq!(
        attention[0].reason,
        AttentionReason::Agent(ObserverAgentState::Blocked)
    );
    assert_eq!(attention[0].worker_id, "a");

    // Staying blocked across refreshes must not notify again; the Observer
    // refreshes on a timer and would otherwise notify continuously.
    let attention = observer.update_workers(vec![live_worker("a", ObserverAgentState::Blocked)]);
    assert!(attention.is_empty(), "{attention:?}");

    // A genuine change back into an attention state notifies once more.
    observer.update_workers(vec![live_worker("a", ObserverAgentState::Working)]);
    let attention = observer.update_workers(vec![live_worker("a", ObserverAgentState::Done)]);
    assert_eq!(attention.len(), 1);
    assert_eq!(
        attention[0].reason,
        AttentionReason::Agent(ObserverAgentState::Done)
    );
}

#[test]
fn a_workload_that_ends_with_a_failing_status_is_reported_once_per_incarnation() {
    let first: TmuxSessionId = "$1".parse().unwrap();
    let second: TmuxSessionId = "$2".parse().unwrap();
    let mut observer = ObserverState::new(Vec::new());
    let running = |incarnation: TmuxSessionId| ObserverWorker {
        lifecycle: ObserverLifecycle::Running,
        incarnation: Some(incarnation),
        ..worker("w")
    };
    let failed = |incarnation: TmuxSessionId| ObserverWorker {
        lifecycle: ObserverLifecycle::Failed { exit_status: 2 },
        incarnation: Some(incarnation),
        ..worker("w")
    };

    assert!(observer.update_workers(vec![running(first)]).is_empty());

    let attention = observer.update_workers(vec![failed(first)]);
    assert_eq!(attention.len(), 1);
    assert_eq!(attention[0].worker_id, "w");
    assert_eq!(
        attention[0].reason,
        AttentionReason::Failed { exit_status: 2 }
    );

    // The Observer refreshes on a timer, so a workload that is still failed is
    // not news.
    assert!(
        observer.update_workers(vec![failed(first)]).is_empty(),
        "a failing end must be reported once, not on every refresh"
    );

    // A restart that fails the same way is a different incarnation, and the
    // whole restart can land between two refreshes, so the exit status cannot be
    // what tells the two failures apart.
    let attention = observer.update_workers(vec![failed(second)]);
    assert_eq!(attention.len(), 1, "a re-failure is a new event");
    assert_eq!(
        attention[0].reason,
        AttentionReason::Failed { exit_status: 2 }
    );

    // Still the same incarnation on the next refresh: silent again.
    assert!(observer.update_workers(vec![failed(second)]).is_empty());
}

#[test]
fn a_clean_end_and_a_first_sighting_are_not_reported_as_failures() {
    let mut observer = ObserverState::new(Vec::new());
    // A workload already failed before this Observer opened. Announcing it now
    // would report history as news.
    let attention = observer.update_workers(vec![ObserverWorker {
        lifecycle: ObserverLifecycle::Failed { exit_status: 1 },
        ..worker("first-sight")
    }]);
    assert!(attention.is_empty(), "{attention:?}");

    let mut observer = ObserverState::new(Vec::new());
    observer.update_workers(vec![ObserverWorker {
        lifecycle: ObserverLifecycle::Running,
        ..worker("clean")
    }]);
    let attention = observer.update_workers(vec![ObserverWorker {
        lifecycle: ObserverLifecycle::Ended,
        ..worker("clean")
    }]);
    assert!(attention.is_empty(), "a clean end is not attention-worthy");
}

#[test]
fn a_failing_end_is_labelled_apart_from_a_clean_one_in_the_tile() {
    assert_eq!(ObserverLifecycle::Ended.label(), "ENDED");
    assert_eq!(
        ObserverLifecycle::Failed { exit_status: 1 }.label(),
        "FAILED"
    );

    let observer = ObserverState::new(vec![ObserverWorker {
        lifecycle: ObserverLifecycle::Failed { exit_status: 1 },
        ..worker("failed")
    }]);
    let rendered = render_to_text(60, 10, &observer).unwrap();
    assert!(rendered.contains("FAILED"), "{rendered}");
    assert!(!rendered.contains("ENDED"), "{rendered}");
}

#[test]
fn attention_ignores_states_and_workers_that_are_not_waiting_on_a_person() {
    let mut observer = ObserverState::new(Vec::new());
    let detached = ObserverWorker {
        live_agent: false,
        agent_state: ObserverAgentState::Blocked,
        ..worker("detached")
    };
    // Without a live agent binding the state is not authoritative.
    assert!(observer.update_workers(vec![detached]).is_empty());

    let mut observer = ObserverState::new(Vec::new());
    for state in [
        ObserverAgentState::Idle,
        ObserverAgentState::Working,
        ObserverAgentState::Unknown,
        ObserverAgentState::Unreachable,
        ObserverAgentState::Stale,
    ] {
        let attention = observer.update_workers(vec![live_worker("w", state)]);
        assert!(attention.is_empty(), "{state:?} should not notify");
    }
}

#[test]
fn attention_references_never_carry_host_directory_or_command_text() {
    let mut observer = ObserverState::new(Vec::new());
    let secret = ObserverWorker {
        live_agent: true,
        agent_state: ObserverAgentState::Blocked,
        title: Some("builder@example.test /srv/secret exec deploy --token abc".to_owned()),
        ..worker("tether-0197f198000070008000000000000001")
    };
    let attention = observer.update_workers(vec![secret]);
    assert_eq!(attention.len(), 1);

    // The tile shows the friendly title; a notification leaves the surface that
    // produced it, so it carries a reference to the work rather than a
    // description of it. The title here is what the Observer manager generates:
    // host, repository name, and preset.
    assert_eq!(attention[0].reference, "…00000001");
    let title = observer.workers()[0].title.clone().unwrap();
    assert_ne!(attention[0].reference, title);
    for fragment in ["builder@example.test", "/srv/secret", "deploy", "token"] {
        assert!(
            !attention[0].reference.contains(fragment),
            "{fragment} must not reach a notification"
        );
    }
}

#[test]
fn wide_mission_control_advertises_explain_and_narrow_keeps_its_pinned_layout() {
    let mut agent = worker("agent");
    agent.live_agent = true;
    let observer = ObserverState::new(vec![agent]);

    // A full-width surface uses the single-line footer, which is what most
    // sessions see. The control has to be discoverable there, not only in the
    // narrow multi-line layouts.
    let wide = render_to_text(120, 16, &observer).unwrap();
    assert!(wide.contains("e explain"), "{wide}");
    assert!(wide.contains("v read"), "{wide}");

    // At 80 columns the footer must not overflow the pinned keyboard geometry.
    let narrow = render_to_text(80, 24, &observer).unwrap();
    for line in narrow.lines() {
        assert!(
            line.chars().count() <= 80,
            "footer line exceeded 80 columns: {line:?}"
        );
    }
}

#[test]
fn truncated_capture_with_no_text_still_reports_truncation_not_emptiness() {
    let mut observer = ObserverState::new(vec![worker("capture")]);
    observer.merge_capture("capture", ObserverCapture::Truncated(String::new()));
    let rendered = render_to_text(60, 10, &observer).unwrap();
    assert!(rendered.contains("TRUNCATED"), "{rendered}");
    // "No captured output" would claim Herdr saw nothing, which is not what a
    // truncated empty read means.
    assert!(!rendered.contains("No captured output"), "{rendered}");
}

#[test]
fn capture_merge_supports_loading_and_ready_to_unavailable_transitions() {
    let mut loading_worker = worker("loading");
    loading_worker.capture = None;
    let mut ready_worker = worker("ready");
    ready_worker.capture = Some("existing output".to_owned());
    let mut observer = ObserverState::new(vec![loading_worker, ready_worker]);
    observer.merge_capture("loading", ObserverCapture::Unavailable);
    observer.merge_capture("ready", ObserverCapture::Unavailable);
    let unavailable = render_to_text(64, 10, &observer).unwrap();
    assert_eq!(unavailable.matches("Output unavailable").count(), 2);
    assert!(!unavailable.contains("existing output"), "{unavailable}");
    observer.merge_capture("loading", ObserverCapture::Loading);
    observer.update_workers(vec![ObserverWorker {
        capture: Some(String::new()),
        ..worker("loading")
    }]);
    let ready_empty = render_to_text(40, 8, &observer).unwrap();
    assert!(ready_empty.contains("No captured output"), "{ready_empty}");
    assert!(!ready_empty.contains("Loading output"), "{ready_empty}");
}

#[test]
fn render_text_is_deterministic_in_normal_and_small_terminals() {
    let one = render_to_text(30, 8, &state(1)).unwrap();
    assert!(one.contains("Observer  1 worker"));
    assert!(one.contains("Worker 0"));
    assert!(one.contains("RUNNING"));
    assert!(one.contains("output-0"));

    let five = render_to_text(48, 14, &state(5)).unwrap();
    assert!(five.contains("page 1/2"));
    assert!(five.contains("+1 more"));
    assert!(five.contains("Worker 0"));
    assert!(five.contains("Worker 3"));
    assert!(!five.contains("Worker 4"));
    assert_eq!(five, render_to_text(48, 14, &state(5)).unwrap());
    for line in five.lines() {
        assert_eq!(line.chars().count(), 48);
    }

    let mut tailed_worker = worker("tail");
    tailed_worker.capture = Some((0..30).map(|line| format!("line-{line:02}\n")).collect());
    let tailed = ObserverState::new(vec![tailed_worker]);
    let tailed = render_to_text(30, 8, &tailed).unwrap();
    assert!(tailed.contains("line-29"), "{tailed}");
    assert!(!tailed.contains("line-00"), "{tailed}");

    let empty = render_to_text(24, 5, &state(0)).unwrap();
    assert!(empty.contains("No workers registered"));
    assert_eq!(render_to_text(0, 5, &state(1)).unwrap(), "");

    let tiny = render_to_text(1, 1, &state(4)).unwrap();
    assert_eq!(tiny.lines().count(), 1);
    assert_eq!(tiny.chars().count(), 1);
    let short = render_to_text(12, 2, &state(3)).unwrap();
    assert_eq!(short.lines().count(), 2);
    assert_eq!(render_to_text(0, 0, &state(4)).unwrap(), "");
    let degenerate = render_to_text(8, 2, &state(4)).unwrap();
    assert_eq!(degenerate.lines().count(), 2);
    assert!(degenerate.lines().all(|line| line.chars().count() == 8));
}

#[test]
fn cramped_observer_geometry_renders_one_useful_bounded_resize_message() {
    for (width, height, workers) in [(20, 4, 2), (20, 7, 4), (12, 3, 1)] {
        let rendered = render_to_text(width, height, &state(workers)).unwrap();
        assert!(
            rendered.contains("Observer"),
            "{width}x{height}:\n{rendered}"
        );
        assert!(
            rendered.contains("Resize pane"),
            "{width}x{height}:\n{rendered}"
        );
        assert!(
            !rendered.contains(['┌', '┐', '└', '┘', '─', '│']),
            "cramped fallback must not collapse borders into punctuation:\n{rendered}"
        );
        assert!(
            !rendered.contains("output-"),
            "{width}x{height}:\n{rendered}"
        );
        assert_eq!(rendered.lines().count(), usize::from(height));
        assert!(
            rendered
                .lines()
                .all(|line| line.chars().count() == usize::from(width))
        );
    }
}

#[test]
fn runtime_notices_are_sanitized_without_changing_worker_state() {
    let mut observer = state(1);
    observer.set_notice(Some("open failed\u{1b}[31m:\nunsafe".to_owned()));

    let rendered = render_to_text(48, 8, &observer).unwrap();
    assert!(rendered.contains("! open failed: unsafe"));
    assert!(!rendered.contains('\u{1b}'));
    assert_eq!(observer.selected_id(), Some("0"));
    assert_eq!(observer.workers().len(), 1);

    let mut unauthorized = worker("restricted");
    unauthorized.capabilities.observe_output = false;
    unauthorized.capture = Some("must-not-render".to_owned());
    let rendered = render_to_text(48, 8, &ObserverState::new(vec![unauthorized])).unwrap();
    assert!(rendered.contains("Output not authorized"));
    assert!(!rendered.contains("must-not-render"));
}

#[test]
fn state_action_projection_accepts_only_possible_zero_worker_actions() {
    let mut empty = state(0);
    let impossible = [
        ObserverKey::Up,
        ObserverKey::Down,
        ObserverKey::Left,
        ObserverKey::Right,
        ObserverKey::PageUp,
        ObserverKey::PageDown,
        ObserverKey::Tab,
        ObserverKey::BackTab,
        ObserverKey::Enter,
        ObserverKey::Char('j'),
        ObserverKey::Char('['),
    ];
    let mut impossible_invocations = 0;
    for key in impossible {
        for kind in [ObserverInputKind::Press, ObserverInputKind::Repeat] {
            for busy in [false, true] {
                let projected = empty.action_for_input(key, kind, busy);
                if let Some(action) = projected {
                    impossible_invocations += 1;
                    let _ = empty.apply(action);
                }
                assert_eq!(
                    projected, None,
                    "{key:?} {kind:?} busy={busy} must be inert without workers"
                );
            }
        }
    }
    assert_eq!(impossible_invocations, 0);

    let mut refresh_invocations = 0;
    let mut back_invocations = 0;
    for key in [ObserverKey::Char('r'), ObserverKey::Escape] {
        let action = empty
            .action_for_key(key)
            .unwrap_or_else(|| panic!("{key:?} must remain actionable"));
        match empty.apply(action) {
            ObserverOutcome::Refresh => refresh_invocations += 1,
            ObserverOutcome::Quit => back_invocations += 1,
            outcome => panic!("unexpected empty-state outcome: {outcome:?}"),
        }
    }
    assert_eq!(refresh_invocations, 1);
    assert_eq!(back_invocations, 1);

    let nonempty = state(1);
    for key in [
        ObserverKey::Up,
        ObserverKey::Down,
        ObserverKey::PageUp,
        ObserverKey::PageDown,
        ObserverKey::Enter,
        ObserverKey::Char('r'),
        ObserverKey::Char('q'),
    ] {
        assert_eq!(nonempty.action_for_key(key), action_for_key(key), "{key:?}");
    }
}

#[test]
fn notices_coexist_with_every_valid_control_in_normal_and_narrow_views() {
    let mut observer = state(1);
    observer.set_notice(Some(
        "Refresh failed after a long recoverable transport interruption; showing previous output"
            .to_owned(),
    ));

    for (width, height) in [(72, 9), (30, 10)] {
        let rendered = render_to_text(width, height, &observer).unwrap();
        for expected in [
            "! Refresh failed",
            "select",
            "page",
            "r refresh",
            "Enter open",
            "q back",
            "Worker 0",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?} at {width}x{height}:\n{rendered}"
            );
        }
    }
}

#[test]
fn zero_worker_view_is_actionable_without_impossible_controls() {
    let observer = state(0);
    for (width, height) in [(48, 6), (24, 6)] {
        let rendered = render_to_text(width, height, &observer).unwrap();
        for expected in ["No workers", "r refresh", "q back"] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?} at {width}x{height}:\n{rendered}"
            );
        }
        for impossible in ["select", "page", "open"] {
            assert!(
                !rendered.contains(impossible),
                "impossible control {impossible:?} shown at {width}x{height}:\n{rendered}"
            );
        }
        assert!(!rendered.trim().is_empty());
    }
}

#[test]
fn observer_chrome_uses_terminal_default_colors_without_weakening_capture_safety() {
    let normal = observer_theme_style(false);
    assert_eq!(normal.fg, Some(Color::Reset));
    assert_eq!(normal.bg, Some(Color::Reset));
    assert!(!normal.add_modifier.contains(Modifier::BOLD));

    let selected = observer_theme_style(true);
    assert_eq!(selected.fg, Some(Color::Reset));
    assert_eq!(selected.bg, Some(Color::Reset));
    assert!(selected.add_modifier.contains(Modifier::BOLD));

    for (width, height) in [(48, 14), (20, 4), (8, 2)] {
        let styles = render_to_styles(width, height, &state(4)).unwrap();
        assert!(
            styles
                .iter()
                .all(|(fg, bg, _)| *fg == Color::Reset && *bg == Color::Reset),
            "Observer chrome forced a color at {width}x{height}"
        );
    }

    assert_eq!(sanitize_capture("\u{1b}[31mred\u{1b}[0m safe"), "red safe");
}

#[test]
fn lifecycle_and_capability_labels_are_visible() {
    let variants = [
        ObserverLifecycle::Starting,
        ObserverLifecycle::Running,
        ObserverLifecycle::Stopping,
        ObserverLifecycle::Ended,
        ObserverLifecycle::Missing,
        ObserverLifecycle::Removed,
        ObserverLifecycle::Unknown,
    ];
    let workers = variants
        .into_iter()
        .enumerate()
        .map(|(index, lifecycle)| ObserverWorker {
            lifecycle,
            ..worker(&index.to_string())
        })
        .collect();
    let mut observer = ObserverState::new(workers);
    let first = render_to_text(64, 16, &observer).unwrap();
    for label in ["STARTING", "RUNNING", "STOPPING", "ENDED"] {
        assert!(first.contains(label), "missing {label}:\n{first}");
    }
    observer.apply(ObserverAction::NextPage);
    let second = render_to_text(64, 10, &observer).unwrap();
    for label in ["MISSING", "REMOVED", "UNKNOWN"] {
        assert!(second.contains(label), "missing {label}:\n{second}");
    }
}

#[test]
fn press_repeat_boundary_and_busy_matrix_is_consistent() {
    for busy in [false, true] {
        for kind in [ObserverInputKind::Press, ObserverInputKind::Repeat] {
            for (key, expected) in [
                (ObserverKey::Up, Some(ObserverAction::PreviousWorker)),
                (ObserverKey::Down, Some(ObserverAction::NextWorker)),
                (ObserverKey::PageUp, Some(ObserverAction::PreviousPage)),
                (ObserverKey::PageDown, Some(ObserverAction::NextPage)),
            ] {
                assert_eq!(
                    action_for_input(key, kind, busy),
                    expected,
                    "{key:?} {kind:?} busy={busy}"
                );
            }
            let single_action = kind == ObserverInputKind::Press && !busy;
            assert_eq!(
                action_for_input(ObserverKey::Enter, kind, busy),
                single_action.then_some(ObserverAction::OpenSelected)
            );
            assert_eq!(
                action_for_input(ObserverKey::Char('r'), kind, busy),
                single_action.then_some(ObserverAction::Refresh)
            );
            for (key, expected) in [
                (ObserverKey::Char(' '), ObserverAction::TogglePromptTarget),
                (ObserverKey::Char('p'), ObserverAction::ComposePrompt),
                (ObserverKey::Char('f'), ObserverAction::FocusSelected),
                (ObserverKey::Char('v'), ObserverAction::ReadSelected),
                (ObserverKey::Char('w'), ObserverAction::WaitSelected),
            ] {
                assert_eq!(
                    action_for_input(key, kind, busy),
                    single_action.then_some(expected),
                    "{key:?} {kind:?} busy={busy}"
                );
            }
            assert_eq!(
                action_for_input(ObserverKey::Escape, kind, busy),
                (kind == ObserverInputKind::Press).then_some(ObserverAction::Quit)
            );
        }
    }

    for start in [0, 2, 4] {
        let mut observer = state(5);
        for _ in 0..start {
            observer.apply(ObserverAction::NextWorker);
        }
        let before = observer.selected_index().unwrap();
        for key in [ObserverKey::Up, ObserverKey::Down] {
            for kind in [ObserverInputKind::Press, ObserverInputKind::Repeat] {
                let mut candidate = observer.clone();
                candidate.apply(action_for_input(key, kind, false).unwrap());
                let expected = match key {
                    ObserverKey::Up => before.saturating_sub(1),
                    ObserverKey::Down => before.saturating_add(1).min(4),
                    _ => unreachable!(),
                };
                assert_eq!(candidate.selected_index(), Some(expected));
            }
        }
    }
}

#[test]
fn unicode_capture_renders_only_valid_bounded_buffer_cells() {
    use ratatui::{Terminal, backend::TestBackend};

    let capture = concat!(
        "界e\u{301} 🇺🇸🇨🇦 1\u{fe0f}\u{20e3} ",
        "👩🏽\u{200d}⚕\u{fe0f} ♥\u{fe0e} ♥\u{fe0f}",
        "\t\u{1b}[31mRED\u{1b}[0m\u{7} tail"
    );
    let clean = sanitize_capture(capture);
    assert!(
        clean.contains("e\u{301}"),
        "combining mark was discarded: {clean:?}"
    );
    assert!(
        clean.contains("👩🏽\u{200d}⚕\u{fe0f}"),
        "emoji grapheme was split: {clean:?}"
    );
    assert!(!clean.contains('\u{1b}'));
    assert!(!clean.contains('\u{7}'));

    let mut observed = worker("unicode");
    observed.capture = Some(clean);
    let observer = ObserverState::new(vec![observed]);
    let width = 24;
    let height = 8;
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| render(frame, frame.area(), &observer))
        .unwrap();
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer.area.width, width);
    assert_eq!(buffer.area.height, height);
    assert_eq!(
        buffer.content().len(),
        usize::from(width) * usize::from(height)
    );
    for y in 0..height {
        for x in 0..width {
            let cell = &buffer[(x, y)];
            assert!(
                !cell.symbol().contains(['\n', '\r', '\u{1b}', '\u{7}']),
                "unsafe cell at ({x}, {y}): {:?}",
                cell.symbol()
            );
        }
    }
    for grapheme in ["🇺🇸", "🇨🇦", "1\u{fe0f}\u{20e3}", "👩🏽\u{200d}⚕\u{fe0f}"] {
        assert!(
            buffer
                .content()
                .iter()
                .any(|cell| cell.symbol() == grapheme),
            "rendered buffer split or discarded {grapheme:?}"
        );
    }
    assert!(
        buffer
            .content()
            .iter()
            .all(|cell| !matches!(cell.symbol(), "🇺" | "🇸" | "🇨" | "🇦")),
        "rendered buffer contains an orphan regional indicator"
    );
    assert!(
        buffer
            .content()
            .iter()
            .any(|cell| cell.symbol().contains("e\u{301}"))
    );
    assert!(
        buffer
            .content()
            .iter()
            .any(|cell| cell.symbol().contains("👩🏽\u{200d}⚕"))
    );
}

fn mission_worker(id: &str, agent_state: ObserverAgentState) -> ObserverWorker {
    ObserverWorker {
        capabilities: ObserverCapabilities {
            observe_output: true,
            open_interactive: true,
            prompt_agent: true,
        },
        agent_state,
        live_agent: true,
        last_observed: Some("2026-07-24T12:00:00Z".to_owned()),
        ..worker(id)
    }
}

#[test]
fn read_only_live_agent_uses_events_and_non_input_actions_without_prompt_grant() {
    let mut read_only = mission_worker("read-only", ObserverAgentState::Working);
    read_only.capabilities.prompt_agent = false;
    let observer = ObserverState::new(vec![read_only]);

    assert_eq!(observer.workers()[0].status_label(), "WORKING");
    assert!(observer.workers()[0].uses_live_agent());
    assert!(observer.workers()[0].can_focus());
    assert!(observer.workers()[0].can_observe_agent());
    assert!(!observer.workers()[0].can_prompt());
    assert_eq!(observer.action_for_key(ObserverKey::Char('p')), None);
    assert_eq!(
        observer.action_for_key(ObserverKey::Char('f')),
        Some(ObserverAction::FocusSelected)
    );
    assert_eq!(
        observer.action_for_key(ObserverKey::Char('v')),
        Some(ObserverAction::ReadSelected)
    );
    assert_eq!(
        observer.action_for_key(ObserverKey::Char('w')),
        Some(ObserverAction::WaitSelected)
    );
    let rendered = render_to_text(64, 10, &observer).unwrap();
    assert!(rendered.contains("Mission Control"), "{rendered}");
    assert!(rendered.contains("WORKING"), "{rendered}");
    assert!(rendered.contains("v read"), "{rendered}");
    assert!(!rendered.contains("p prompt"), "{rendered}");
}

#[test]
fn mission_control_states_and_prompt_authority_are_explicit() {
    let states = [
        (ObserverAgentState::Detached, "DETACHED", false),
        (ObserverAgentState::Idle, "IDLE", true),
        (ObserverAgentState::Working, "WORKING", false),
        (ObserverAgentState::Blocked, "BLOCKED", false),
        (ObserverAgentState::Done, "DONE", true),
        (ObserverAgentState::Unknown, "UNKNOWN", false),
        (ObserverAgentState::Unreachable, "UNREACHABLE", false),
        (ObserverAgentState::Stale, "STALE", false),
    ];
    for (state, label, can_prompt) in states {
        let candidate = mission_worker(label, state);
        assert_eq!(candidate.status_label(), label);
        assert_eq!(candidate.can_prompt(), can_prompt, "{label}");
    }
    assert_eq!(action_for_key(ObserverKey::Char('d')), None);
    assert_eq!(action_for_key(ObserverKey::Char('s')), None);
    let observer_only = state(1);
    assert_eq!(observer_only.action_for_key(ObserverKey::Char('p')), None);
    let mission = ObserverState::new(vec![mission_worker("active", ObserverAgentState::Idle)]);
    assert_eq!(
        mission.action_for_key(ObserverKey::Char('p')),
        Some(ObserverAction::ComposePrompt)
    );
}

#[test]
fn mission_control_bounds_multi_target_prompt_selection() {
    let workers = (0..=MAX_PROMPT_TARGETS)
        .map(|index| mission_worker(&index.to_string(), ObserverAgentState::Idle))
        .collect();
    let mut observer = ObserverState::new(workers);
    for index in 0..MAX_PROMPT_TARGETS {
        assert_eq!(
            observer.apply(ObserverAction::TogglePromptTarget),
            ObserverOutcome::None
        );
        if index + 1 < MAX_PROMPT_TARGETS {
            observer.apply(ObserverAction::NextWorker);
        }
    }
    observer.apply(ObserverAction::NextWorker);
    observer.apply(ObserverAction::TogglePromptTarget);
    assert!(
        observer
            .notice()
            .unwrap()
            .contains("At most 8 prompt destinations")
    );
    assert_eq!(observer.prompt_target_ids().len(), MAX_PROMPT_TARGETS);
    assert!(matches!(
        observer.apply(ObserverAction::ComposePrompt),
        ObserverOutcome::ComposePrompt { worker_ids }
            if worker_ids.len() == MAX_PROMPT_TARGETS
    ));
}

#[test]
fn live_agent_loss_preserves_last_known_information_as_stale() {
    let mut observer = ObserverState::new(vec![mission_worker("agent", ObserverAgentState::Idle)]);
    let mut unreachable = mission_worker("agent", ObserverAgentState::Unreachable);
    unreachable.capture = None;
    unreachable.last_observed = None;
    observer.update_workers(vec![unreachable]);

    let retained = &observer.workers()[0];
    assert_eq!(retained.agent_state, ObserverAgentState::Stale);
    assert_eq!(retained.capture.as_deref(), Some("output-agent"));
    assert_eq!(
        retained.last_observed.as_deref(),
        Some("2026-07-24T12:00:00Z")
    );
    assert!(!retained.can_prompt());
    let rendered = render_to_text(64, 10, &observer).unwrap();
    assert!(rendered.contains("STALE"));
    assert!(rendered.contains("last live 2026-07-24T12:00:00Z"));
    let mut recovered = mission_worker("agent", ObserverAgentState::Done);
    recovered.capture = None;
    recovered.last_observed = Some("2026-07-24T12:01:00Z".to_owned());
    observer.update_workers(vec![recovered]);
    assert_eq!(observer.workers()[0].agent_state, ObserverAgentState::Done);
    assert!(observer.workers()[0].can_prompt());
    assert!(!render_to_text(64, 10, &observer).unwrap().contains("STALE"));
}
