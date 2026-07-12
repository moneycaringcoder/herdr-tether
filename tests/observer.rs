#[path = "../src/observer.rs"]
mod observer;

#[allow(dead_code)]
#[path = "../src/model.rs"]
mod model;

use observer::{
    MAX_CAPTURE_BYTES, MAX_CAPTURE_CELLS, MAX_CAPTURE_LINES, ObserverAction, ObserverCapabilities,
    ObserverCapture, ObserverKey, ObserverLifecycle, ObserverOutcome, ObserverState,
    ObserverWorker, action_for_key, observer_theme_style, render_to_styles, render_to_text,
    sanitize_capture, worker_rects,
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
        },
        lifecycle: ObserverLifecycle::Running,
        owned: true,
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
    assert_eq!(clean, "okREDx\nbidi    end");
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
    let ready_empty = render_to_text(40, 8, &observer).unwrap();
    assert!(ready_empty.contains("No captured output"), "{ready_empty}");
    assert!(!ready_empty.contains("Loading output"), "{ready_empty}");
    assert!(!ready_empty.contains("Output unavailable"), "{ready_empty}");
    observer.merge_capture("capture", ObserverCapture::Unavailable);
    let unavailable = render_to_text(40, 8, &observer).unwrap();
    assert!(unavailable.contains("Output unavailable"), "{unavailable}");
    assert!(!unavailable.contains("Loading output"), "{unavailable}");
    assert!(!unavailable.contains("No captured output"), "{unavailable}");
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
        let projected = empty.action_for_key(key);
        if let Some(action) = projected {
            impossible_invocations += 1;
            let _ = empty.apply(action);
        }
        assert_eq!(projected, None, "{key:?} must be inert without workers");
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
