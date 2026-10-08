//! `pane_prompt_detected` / `pane_waiting_input` sweep (Issue #72).

use super::super::*;

/// One-pane app whose shell is dead and fully drained, so the test owns
/// the vt100 screen without a reader thread racing it. The pane keeps
/// `exited = false` so the sweep treats it as live.
fn quiet_app() -> (App, usize) {
    let mut app = App::new(24, 80).expect("App::new");
    let id = app.ws().focused_pane_id;
    app.workspaces[0].panes.get_mut(&id).unwrap().kill();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match app.event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(AppEvent::PtyEof(p)) if p == id => break,
            _ => assert!(Instant::now() < deadline, "pane never hit EOF"),
        }
    }
    // `kill` flags the pane exited; the sweep would then skip it.
    app.workspaces[0].panes.get_mut(&id).unwrap().exited = false;
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
