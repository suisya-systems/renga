//! `pane_prompt_detected` / `pane_waiting_input` sweep (Issue #72).

use super::super::*;

/// One-pane app whose pane reads a private vt100 screen: the reader
/// thread keeps writing the live shell's output into its own clone of
/// the old parser, so nothing races the test. (Killing the shell and
/// waiting for EOF doesn't work on Windows: conpty never reports it.)
/// `event_rx` is never drained, so the shell's own `PtyOutput` can't
/// touch the pane's idle state either.
fn quiet_app() -> (App, usize) {
    let mut app = App::new(24, 80).expect("App::new");
    let id = app.ws().focused_pane_id;
    app.workspaces[0].panes.get_mut(&id).unwrap().parser =
        std::sync::Arc::new(std::sync::Mutex::new(vt100::Parser::new(24, 80, 0)));
    (app, id)
}

fn show(app: &mut App, id: usize, bytes: &str) {
    let pane = app.workspaces[0].panes.get_mut(&id).unwrap();
    pane.parser
        .lock()
        .unwrap()
        .process(format!("\x1b[2J\x1b[H{bytes}").as_bytes());
    pane.output_seen = true;
    pane.last_output_at = Instant::now();
    pane.waiting_input_reported = false;
    app.last_prompt_sweep = None;
}

fn sweep(app: &mut App, rx: &std::sync::mpsc::Receiver<ipc::Event>) -> Vec<ipc::Event> {
    app.last_prompt_sweep = None;
    app.tick_prompt_events();
    rx.try_iter().collect()
}

#[test]
fn prompt_fires_once_per_distinct_prompt_and_rearms_when_it_clears() {
    let (mut app, id) = quiet_app();
    let (_sub, rx) = app.event_bus.subscribe();

    show(
        &mut app,
        id,
        "Do you want to proceed?\r\n❯ 1. Yes\r\n  2. No",
    );
    let evs = sweep(&mut app, &rx);
    assert!(
        matches!(&evs[..], [ipc::Event::PanePromptDetected { id: i, kind, prompt, .. }]
            if *i == id && kind == "choice" && prompt == "Do you want to proceed?"),
        "{evs:?}"
    );

    // Same prompt redrawn: no repeat.
    show(
        &mut app,
        id,
        "Do you want to proceed?\r\n❯ 1. Yes\r\n  2. No",
    );
    assert!(sweep(&mut app, &rx).is_empty());

    // Prompt gone, then the same text again: a new approval, fires again.
    show(&mut app, id, "working...");
    assert!(sweep(&mut app, &rx).is_empty());
    show(
        &mut app,
        id,
        "Do you want to proceed?\r\n❯ 1. Yes\r\n  2. No",
    );
    assert_eq!(sweep(&mut app, &rx).len(), 1);
    app.shutdown();
}

#[test]
fn waiting_input_fires_once_per_quiet_spell() {
    let (mut app, id) = quiet_app();
    let (_sub, rx) = app.event_bus.subscribe();
    show(&mut app, id, "$ ");
    assert!(sweep(&mut app, &rx).is_empty(), "not idle yet");

    let pane = app.workspaces[0].panes.get_mut(&id).unwrap();
    pane.last_output_at = Instant::now() - Duration::from_secs(6);
    let evs = sweep(&mut app, &rx);
    assert!(
        matches!(&evs[..], [ipc::Event::PaneWaitingInput { id: i, idle_ms, .. }]
            if *i == id && *idle_ms >= 5000),
        "{evs:?}"
    );
    assert!(sweep(&mut app, &rx).is_empty(), "once per quiet spell");
    app.shutdown();
}

#[test]
fn mode_changed_fires_on_each_claude_mode_switch_only() {
    let (mut app, id) = quiet_app();
    let (_sub, rx) = app.event_bus.subscribe();
    let footer = |f: &str| format!("────────\r\n> \r\n────────\r\n  {f}");
    let modes = |evs: Vec<ipc::Event>| -> Vec<(String, Option<String>)> {
        evs.into_iter()
            .filter_map(|e| match e {
                ipc::Event::PaneModeChanged {
                    mode, prev_mode, ..
                } => Some((mode, prev_mode)),
                _ => None,
            })
            .collect()
    };

    // Not a Claude pane: the same footer text is ignored.
    show(&mut app, id, &footer("? for shortcuts"));
    assert!(modes(sweep(&mut app, &rx)).is_empty());

    app.workspaces[0].panes[&id]
        .claude_seen
        .store(true, std::sync::atomic::Ordering::Relaxed);
    show(&mut app, id, &footer("? for shortcuts"));
    assert_eq!(modes(sweep(&mut app, &rx)), [("default".into(), None)]);
    show(&mut app, id, &footer("⏸ plan mode on (shift+tab to cycle)"));
    assert_eq!(
        modes(sweep(&mut app, &rx)),
        [("plan".into(), Some("default".into()))]
    );
    // Redraw in the same mode, then a frame with no reading: silent.
    show(&mut app, id, &footer("⏸ plan mode on (shift+tab to cycle)"));
    show(&mut app, id, &footer(""));
    assert!(modes(sweep(&mut app, &rx)).is_empty());
    show(
        &mut app,
        id,
        &footer("⏵⏵ accept edits on (shift+tab to cycle)"),
    );
    assert_eq!(
        modes(sweep(&mut app, &rx)),
        [("accept_edits".into(), Some("plan".into()))]
    );
    app.shutdown();
}
