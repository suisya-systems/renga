use super::super::*;

#[test]
fn hotkey_mode_esc_closes_overlay_and_restores_buffer_on_reopen() {
    // Esc should close the overlay immediately, but the draft now
    // persists per pane so a close/reopen cycle can resume
    // composition instead of discarding the buffer.
    let mut app = App::new(40, 80).expect("App::new");
    let pane_a = app.ws().focused_pane_id;
    assert_eq!(app.ime_mode, crate::config::ImeMode::Hotkey);
    let mut state = crate::input::overlay::OverlayState::new(pane_a);
    state.insert_char('あ');
    app.overlay = Some(state);

    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    let consumed =
        crate::input::overlay::handle_overlay_key(&mut app, esc).expect("handle_overlay_key Esc");

    assert!(consumed, "Esc must be consumed by the overlay handler");
    assert!(
        app.overlay.is_none(),
        "Esc must close the overlay even with a non-empty buffer"
    );
    assert!(
        app.saved_overlay_drafts.contains_key(&pane_a),
        "Esc should preserve the draft for the target pane"
    );

    let open = KeyEvent::new(KeyCode::Char(';'), KeyModifiers::CONTROL);
    let reopened = app.handle_key_event(open).expect("reopen overlay");
    assert!(reopened);
    let overlay = app.overlay.as_ref().expect("overlay reopened");
    assert_eq!(overlay.target_pane, pane_a);
    assert_eq!(overlay.buffer, "あ");
    assert_eq!(overlay.cursor, 1);
    assert!(
        !app.saved_overlay_drafts.contains_key(&pane_a),
        "live overlay session should own the draft until it closes again"
    );
}

#[test]
fn hotkey_mode_ctrl_c_closes_overlay_and_saves_buffer() {
    // Ctrl+C shares the cancel branch with Esc and must preserve
    // the draft for a later reopen without forwarding Ctrl+C to
    // the target pane.
    let mut app = App::new(40, 80).expect("App::new");
    let pane_a = app.ws().focused_pane_id;
    let mut state = crate::input::overlay::OverlayState::new(pane_a);
    state.insert_char('x');
    app.overlay = Some(state);

    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    let consumed = crate::input::overlay::handle_overlay_key(&mut app, ctrl_c)
        .expect("handle_overlay_key Ctrl+C");

    assert!(consumed);
    assert!(app.overlay.is_none());
    assert!(app.saved_overlay_drafts.contains_key(&pane_a));
}

#[test]
fn hotkey_mode_placeholder_prompt_opens_empty_overlay() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    let pane = app
        .ws_mut()
        .panes
        .get_mut(&pane_id)
        .expect("focused pane exists");
    let mut parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
    parser.process(b"\x1b[3;1H> [Pasted text #2 +16 lines]\x1b[7m \x1b[27m");
    drop(parser);

    let open = KeyEvent::new(KeyCode::Char(';'), KeyModifiers::CONTROL);
    let consumed = app.handle_key_event(open).expect("open overlay");

    assert!(consumed);
    let overlay = app.overlay.as_ref().expect("overlay opened");
    assert_eq!(overlay.target_pane, pane_id);
    assert!(
        overlay.buffer.is_empty(),
        "paste-placeholder prompts should fall back to the legacy empty overlay"
    );
    assert_eq!(overlay.cursor, 0);
}

#[test]
fn hotkey_mode_bootstraps_visible_claude_input_into_overlay() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    let pane = app
        .ws_mut()
        .panes
        .get_mut(&pane_id)
        .expect("focused pane exists");
    let mut parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
    // Park the visible cursor at end-of-input (col 12, right after the
    // final 't'), like real Claude does after typing. The caret
    // resolver trusts a visible in-block cursor (modern Claude paints
    // no inverse cell), so its position now shapes `overlay.cursor`.
    parser.process(b"\x1b[2J\x1b[H> draft text\x1b[1;13H");
    drop(parser);

    let open = KeyEvent::new(KeyCode::Char(';'), KeyModifiers::CONTROL);
    let consumed = app.handle_key_event(open).expect("open overlay");

    assert!(consumed);
    let overlay = app.overlay.as_ref().expect("overlay opened");
    assert_eq!(overlay.target_pane, pane_id);
    assert_eq!(overlay.buffer, "draft text");
    assert_eq!(overlay.cursor, "draft text".chars().count());
}

#[test]
fn ctrl_u_clears_overlay_buffer() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    let mut state = crate::input::overlay::OverlayState::new(pane_id);
    for ch in "hello\nworld".chars() {
        state.insert_char(ch);
    }
    app.overlay = Some(state);

    let ctrl_u = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
    let consumed = crate::input::overlay::handle_overlay_key(&mut app, ctrl_u)
        .expect("handle_overlay_key Ctrl+U");

    assert!(consumed, "Ctrl+U must be consumed by the overlay handler");
    let overlay = app.overlay.as_ref().expect("overlay still open");
    assert!(overlay.buffer.is_empty(), "Ctrl+U should clear the buffer");
    assert_eq!(overlay.cursor, 0, "cursor should reset to 0 after clear");
}

#[test]
fn ctrl_shift_u_also_clears_overlay_buffer() {
    // Some terminals report Ctrl+U with the Shift bit still set
    // when the user happens to be holding Shift, and the handler
    // treats both 'u' and 'U' as the same chord — verify that path.
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    let mut state = crate::input::overlay::OverlayState::new(pane_id);
    for ch in "draft".chars() {
        state.insert_char(ch);
    }
    app.overlay = Some(state);

    let ctrl_shift_u = KeyEvent::new(
        KeyCode::Char('U'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    let consumed = crate::input::overlay::handle_overlay_key(&mut app, ctrl_shift_u)
        .expect("handle_overlay_key Ctrl+Shift+U");

    assert!(consumed);
    let overlay = app.overlay.as_ref().expect("overlay still open");
    assert!(overlay.buffer.is_empty());
    assert_eq!(overlay.cursor, 0);
}

#[test]
fn ctrl_u_on_empty_overlay_is_a_noop() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    app.overlay = Some(crate::input::overlay::OverlayState::new(pane_id));

    let ctrl_u = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
    let consumed = crate::input::overlay::handle_overlay_key(&mut app, ctrl_u)
        .expect("handle_overlay_key Ctrl+U on empty");

    assert!(consumed, "Ctrl+U must always be consumed by the overlay");
    let overlay = app.overlay.as_ref().expect("overlay still open");
    assert!(overlay.buffer.is_empty());
    assert_eq!(overlay.cursor, 0);
}

#[test]
fn hotkey_mode_skips_visible_input_bootstrap_for_codex_peer() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    app.peer_client_kinds.insert(pane_id, PeerClientKind::Codex);
    let pane = app
        .ws_mut()
        .panes
        .get_mut(&pane_id)
        .expect("focused pane exists");
    let mut parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
    parser.process(b"\x1b[2J\x1b[H\xE2\x80\xBA typed draft\x1b[1;14H");
    drop(parser);

    let open = KeyEvent::new(KeyCode::Char(';'), KeyModifiers::CONTROL);
    let consumed = app.handle_key_event(open).expect("open overlay");

    assert!(consumed);
    let overlay = app.overlay.as_ref().expect("overlay opened");
    assert_eq!(overlay.target_pane, pane_id);
    assert!(
        overlay.buffer.is_empty(),
        "Codex peer panes should not copy the visible composer into the IME overlay"
    );
    assert_eq!(overlay.cursor, 0);
}

#[test]
fn freeze_panes_suppresses_pty_output_repaint_when_overlay_open() {
    // With freeze enabled and the overlay open, a burst of
    // PtyOutput events must NOT mark the app dirty, so the screen
    // stays frozen while the user composes JP text. vt100 parser
    // work happens on reader threads and is unaffected by this
    // gate, so panes catch up instantly when the overlay closes.
    let mut app = App::new(40, 80).expect("App::new");
    let pane_a = app.ws().focused_pane_id;
    app.ime_freeze_panes_on_overlay = true;
    app.overlay = Some(crate::input::overlay::OverlayState::new(pane_a));
    // Swap in a private channel so the live shell pane spawned by
    // App::new (its reader threads hold the old sender) cannot inject
    // state-changing events (CwdChanged / PtyEof) that legitimately
    // dirty the app and race with the assertion below (#397).
    let (tx, rx) = std::sync::mpsc::channel();
    app.event_tx = tx;
    app.event_rx = rx;
    app.dirty = false;

    // Push a PtyOutput directly through the event channel —
    // drain_pty_events treats it as the pure-output no-op case.
    app.event_tx
        .send(AppEvent::PtyOutput(pane_a))
        .expect("send PtyOutput");
    app.drain_pty_events();

    assert!(
        !app.dirty,
        "PtyOutput must not dirty the app while overlay is open and freeze is on"
    );
}

#[test]
fn freeze_panes_still_repaints_on_pty_eof() {
    // State-changing events must punch through the freeze: PtyEof
    // flips `pane.exited` and emits PaneExited, which in turn
    // changes the pane chrome (title dim-out, etc.), so the user
    // needs to see it even mid-composition.
    let mut app = App::new(40, 80).expect("App::new");
    let pane_a = app.ws().focused_pane_id;
    app.ime_freeze_panes_on_overlay = true;
    app.overlay = Some(crate::input::overlay::OverlayState::new(pane_a));
    app.dirty = false;

    app.event_tx
        .send(AppEvent::PtyEof(pane_a))
        .expect("send PtyEof");
    app.drain_pty_events();

    assert!(
        app.dirty,
        "PtyEof must dirty the app even under freeze — pane chrome changes"
    );
}

#[test]
fn freeze_panes_still_repaints_on_cwd_changed() {
    // CwdChanged rewrites the workspace name and rebuilds the
    // file-tree sidebar; a frozen overlay must not starve those
    // UI-chrome updates.
    let mut app = App::new(40, 80).expect("App::new");
    let pane_a = app.ws().focused_pane_id;
    app.ime_freeze_panes_on_overlay = true;
    app.overlay = Some(crate::input::overlay::OverlayState::new(pane_a));
    app.dirty = false;

    // temp_dir() is canonicalizable and is_dir() on every tier-1
    // platform, so the CwdChanged handler will take the full
    // "update" branch instead of the early `continue`.
    let tmp = std::env::temp_dir();
    app.event_tx
        .send(AppEvent::CwdChanged(pane_a, tmp))
        .expect("send CwdChanged");
    app.drain_pty_events();

    assert!(
        app.dirty,
        "CwdChanged must dirty the app even under freeze — sidebar/tab label depend on it"
    );
}

#[test]
fn freeze_panes_mixed_batch_repaints_when_any_state_change_present() {
    // A single drain pass may pick up both a spinner PtyOutput
    // and a concurrent PtyEof; the state change must win so the
    // user isn't left with a stale pane title while the overlay
    // is open.
    let mut app = App::new(40, 80).expect("App::new");
    let pane_a = app.ws().focused_pane_id;
    app.ime_freeze_panes_on_overlay = true;
    app.overlay = Some(crate::input::overlay::OverlayState::new(pane_a));
    app.dirty = false;

    app.event_tx
        .send(AppEvent::PtyOutput(pane_a))
        .expect("send PtyOutput");
    app.event_tx
        .send(AppEvent::PtyEof(pane_a))
        .expect("send PtyEof");
    app.drain_pty_events();

    assert!(
        app.dirty,
        "mixed batch with any state-changing event must repaint"
    );
}

#[test]
fn overlay_catchup_noop_when_freeze_disabled() {
    // Catch-up is gated on freeze being on — otherwise the main
    // loop is already repainting at the overlay poll rate, and
    // an extra forced repaint would be redundant. Verify the
    // freeze-off short-circuit holds even with an elapsed timer.
    let mut app = App::new(40, 80).expect("App::new");
    let pane_a = app.ws().focused_pane_id;
    app.ime_freeze_panes_on_overlay = false;
    app.ime_overlay_catchup_ms = 100;
    app.overlay = Some(crate::input::overlay::OverlayState::new(pane_a));
    app.dirty = false;
    app.last_overlay_repaint = Some(Instant::now() - std::time::Duration::from_millis(500));

    app.maybe_tick_overlay_catchup();

    assert!(
        !app.dirty,
        "catch-up must not fire when freeze is disabled, even if interval elapsed"
    );
}

#[test]
fn overlay_commit_clears_saved_draft_for_target_pane() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_a = app.ws().focused_pane_id;
    let mut live = crate::input::overlay::OverlayState::new(pane_a);
    for ch in "draft".chars() {
        live.insert_char(ch);
    }
    app.overlay = Some(live);

    let mut stale = crate::input::overlay::OverlayState::new(pane_a);
    stale.insert_char('x');
    app.saved_overlay_drafts.insert(pane_a, stale);

    let commit = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
    let consumed = crate::input::overlay::handle_overlay_key(&mut app, commit)
        .expect("handle_overlay_key Alt+Enter");

    assert!(consumed);
    assert!(app.overlay.is_none());
    assert!(
        !app.saved_overlay_drafts.contains_key(&pane_a),
        "successful commit should clear any saved draft for that pane"
    );
}

#[test]
fn overlay_ctrl_j_commits_as_wsl_ctrl_enter_fallback() {
    // Issue #226: WSL / Windows Terminal swallow Alt+Enter for the
    // host's fullscreen shortcut and deliver Ctrl+Enter as the bare
    // LF byte (0x0A → Ctrl+J in crossterm). Pressing Ctrl+J inside
    // the overlay must therefore drive the same commit path Alt+Enter
    // does on other hosts: overlay closes, draft cache clears,
    // buffer is delivered to the target pane.
    let mut app = App::new(40, 80).expect("App::new");
    let pane_a = app.ws().focused_pane_id;
    let mut live = crate::input::overlay::OverlayState::new(pane_a);
    for ch in "wsl-fallback".chars() {
        live.insert_char(ch);
    }
    app.overlay = Some(live);

    let mut stale = crate::input::overlay::OverlayState::new(pane_a);
    stale.insert_char('x');
    app.saved_overlay_drafts.insert(pane_a, stale);

    let commit = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL);
    let consumed = crate::input::overlay::handle_overlay_key(&mut app, commit)
        .expect("handle_overlay_key Ctrl+J");

    assert!(consumed, "Ctrl+J must be consumed by the overlay");
    assert!(
        app.overlay.is_none(),
        "Ctrl+J must close the overlay just like Alt+Enter"
    );
    assert!(
        !app.saved_overlay_drafts.contains_key(&pane_a),
        "Ctrl+J commit must clear any saved draft for that pane"
    );
}

#[test]
fn overlay_bare_enter_inserts_newline_not_commit() {
    // Guards against a regression where the commit-path
    // restructuring for Ctrl+J might accidentally re-route bare
    // Enter to commit. Bare Enter must keep its multi-line role
    // and leave the overlay open with a newline appended.
    let mut app = App::new(40, 80).expect("App::new");
    let pane_a = app.ws().focused_pane_id;
    let mut live = crate::input::overlay::OverlayState::new(pane_a);
    live.insert_char('a');
    app.overlay = Some(live);

    let bare_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    let consumed = crate::input::overlay::handle_overlay_key(&mut app, bare_enter)
        .expect("handle_overlay_key Enter");

    assert!(consumed);
    let overlay = app
        .overlay
        .as_ref()
        .expect("overlay still open after Enter");
    assert_eq!(overlay.buffer, "a\n");
    assert_eq!(overlay.cursor, 2);
}

#[test]
fn closing_pane_drops_saved_overlay_draft() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_a = app.ws().focused_pane_id;
    let pane_b = app
        .split_focused_pane(SplitDirection::Vertical, None)
        .expect("split focused pane")
        .expect("split should create a pane");

    let mut saved = crate::input::overlay::OverlayState::new(pane_a);
    saved.insert_char('保');
    app.saved_overlay_drafts.insert(pane_a, saved);

    app.remove_pane_from_layout(0, pane_a)
        .expect("remove pane with saved draft");

    assert!(
        !app.saved_overlay_drafts.contains_key(&pane_a),
        "pane removal must drop its saved overlay draft"
    );
    assert!(
        app.ws().panes.contains_key(&pane_b),
        "control pane should remain after closing the original pane"
    );
}

#[test]
fn overlay_catchup_timer_reanchors_on_reopen_cycle() {
    // Close then re-open: the timer state from the previous
    // session must not carry over, or the first tick after
    // re-open could fire a catch-up with 0 ms elapsed.
    let mut app = App::new(40, 80).expect("App::new");
    let pane_a = app.ws().focused_pane_id;
    app.ime_freeze_panes_on_overlay = true;
    app.ime_overlay_catchup_ms = 500;

    // Simulate a prior session that advanced the timer.
    app.overlay = Some(crate::input::overlay::OverlayState::new(pane_a));
    app.maybe_tick_overlay_catchup();
    assert!(app.last_overlay_repaint.is_some());

    // Close the overlay; next tick should clear the timer.
    app.overlay = None;
    app.maybe_tick_overlay_catchup();
    assert!(
        app.last_overlay_repaint.is_none(),
        "closing the overlay must clear the catch-up anchor"
    );

    // Re-open and tick: first tick of the new session should
    // anchor WITHOUT painting, even though wall-time elapsed.
    app.overlay = Some(crate::input::overlay::OverlayState::new(pane_a));
    app.dirty = false;
    app.maybe_tick_overlay_catchup();
    assert!(
        !app.dirty,
        "first tick of a fresh re-open must not force a repaint"
    );
    assert!(
        app.last_overlay_repaint.is_some(),
        "first tick of a fresh re-open must re-anchor"
    );
}

#[test]
fn freeze_panes_off_still_repaints_on_pty_output() {
    // Sanity: with freeze disabled the legacy behavior is
    // preserved — PtyOutput dirties the app even while the
    // overlay is open. This is the pre-#37 baseline and how the
    // flag's default (false) has to behave.
    let mut app = App::new(40, 80).expect("App::new");
    let pane_a = app.ws().focused_pane_id;
    app.ime_freeze_panes_on_overlay = false;
    app.overlay = Some(crate::input::overlay::OverlayState::new(pane_a));
    app.dirty = false;

    app.event_tx
        .send(AppEvent::PtyOutput(pane_a))
        .expect("send PtyOutput");
    app.drain_pty_events();

    assert!(
        app.dirty,
        "PtyOutput should dirty the app when freeze is disabled"
    );
}

#[test]
fn overlay_catchup_noop_when_no_overlay() {
    // No overlay open → catch-up must never dirty or set the
    // anchor timer, regardless of config values.
    let mut app = App::new(40, 80).expect("App::new");
    app.ime_freeze_panes_on_overlay = true;
    app.ime_overlay_catchup_ms = 1000;
    app.overlay = None;
    app.dirty = false;

    app.maybe_tick_overlay_catchup();

    assert!(!app.dirty);
    assert!(app.last_overlay_repaint.is_none());
}

#[test]
fn overlay_catchup_noop_when_disabled() {
    // Catch-up interval of 0 = disabled. Even with freeze on and
    // overlay open, no repaint should be forced.
    let mut app = App::new(40, 80).expect("App::new");
    let pane_a = app.ws().focused_pane_id;
    app.ime_freeze_panes_on_overlay = true;
    app.ime_overlay_catchup_ms = 0;
    app.overlay = Some(crate::input::overlay::OverlayState::new(pane_a));
    app.dirty = false;

    app.maybe_tick_overlay_catchup();
    std::thread::sleep(std::time::Duration::from_millis(5));
    app.maybe_tick_overlay_catchup();

    assert!(!app.dirty);
}

#[test]
fn overlay_catchup_first_call_anchors_without_repaint() {
    // First tick of a freshly-opened overlay should seed the
    // timer at "now" without firing a repaint — the first real
    // catch-up lands `interval` later, not immediately on open.
    let mut app = App::new(40, 80).expect("App::new");
    let pane_a = app.ws().focused_pane_id;
    app.ime_freeze_panes_on_overlay = true;
    app.ime_overlay_catchup_ms = 500;
    app.overlay = Some(crate::input::overlay::OverlayState::new(pane_a));
    app.dirty = false;

    app.maybe_tick_overlay_catchup();

    assert!(!app.dirty, "first tick must not repaint");
    assert!(
        app.last_overlay_repaint.is_some(),
        "first tick must anchor the timer"
    );
}

#[test]
fn overlay_catchup_fires_after_interval() {
    // Simulate an elapsed interval by backdating last_overlay_repaint.
    // `maybe_tick_overlay_catchup` should then mark dirty and
    // re-anchor the timer.
    let mut app = App::new(40, 80).expect("App::new");
    let pane_a = app.ws().focused_pane_id;
    app.ime_freeze_panes_on_overlay = true;
    app.ime_overlay_catchup_ms = 100;
    app.overlay = Some(crate::input::overlay::OverlayState::new(pane_a));
    app.dirty = false;
    app.last_overlay_repaint = Some(Instant::now() - std::time::Duration::from_millis(150));

    app.maybe_tick_overlay_catchup();

    assert!(app.dirty, "elapsed interval must force a repaint");
    let anchor = app.last_overlay_repaint.expect("timer stays set");
    assert!(
        Instant::now().duration_since(anchor) < std::time::Duration::from_millis(50),
        "timer must be re-anchored to ~now"
    );
}

#[test]
fn overlay_catchup_clears_timer_when_overlay_closes() {
    // If the overlay closes between ticks, the timer must reset
    // so the next session doesn't inherit a stale anchor that
    // could fire a catch-up 0 ms after open.
    let mut app = App::new(40, 80).expect("App::new");
    app.ime_freeze_panes_on_overlay = true;
    app.ime_overlay_catchup_ms = 500;
    app.overlay = None;
    app.last_overlay_repaint = Some(Instant::now());

    app.maybe_tick_overlay_catchup();

    assert!(app.last_overlay_repaint.is_none());
}

#[test]
fn freeze_panes_does_not_suppress_without_overlay() {
    // Freeze is gated on overlay being open. With the overlay
    // closed the flag must have no effect, so normal editing
    // stays responsive.
    let mut app = App::new(40, 80).expect("App::new");
    let pane_a = app.ws().focused_pane_id;
    app.ime_freeze_panes_on_overlay = true;
    app.overlay = None;
    app.dirty = false;

    app.event_tx
        .send(AppEvent::PtyOutput(pane_a))
        .expect("send PtyOutput");
    app.drain_pty_events();

    assert!(app.dirty, "PtyOutput must dirty when overlay is closed");
}

#[test]
fn handle_paste_routes_to_overlay_when_open() {
    // The user-visible bug: on WSL2, terminal-level Ctrl+V emits a
    // bracketed-paste sequence which crossterm surfaces as
    // Event::Paste. Without overlay-aware routing the text leaked to
    // the back pane's PTY even though the IME composition overlay
    // was holding focus. handle_paste must intercept and deliver the
    // payload into the overlay buffer instead.
    let mut app = App::new(40, 80).expect("App::new");
    let pane_a = app.ws().focused_pane_id;
    let mut state = crate::input::overlay::OverlayState::new(pane_a);
    state.insert_char('a');
    app.overlay = Some(state);
    app.dirty = false;

    let routed_to_overlay = app.handle_paste("bcd").expect("handle_paste");

    assert!(routed_to_overlay, "overlay-open paste must report routed");
    let overlay = app.overlay.as_ref().expect("overlay still open");
    assert_eq!(overlay.buffer, "abcd");
    assert_eq!(overlay.cursor, 4);
    assert!(app.dirty, "overlay paste must mark dirty for redraw");
}

#[test]
fn handle_paste_inserts_at_cursor_position() {
    // The cursor isn't always at the buffer end (the user may have
    // arrowed back to fix earlier composition). Pasted text must
    // splice in at the cursor and advance it past the inserted run.
    let mut app = App::new(40, 80).expect("App::new");
    let pane_a = app.ws().focused_pane_id;
    let mut state = crate::input::overlay::OverlayState::new(pane_a);
    for ch in "ab".chars() {
        state.insert_char(ch);
    }
    state.cursor = 1;
    app.overlay = Some(state);

    app.handle_paste("XY").expect("handle_paste");

    let overlay = app.overlay.as_ref().expect("overlay still open");
    assert_eq!(overlay.buffer, "aXYb");
    assert_eq!(overlay.cursor, 3, "cursor advances past inserted run");
}

#[test]
fn handle_paste_truncates_at_buffer_cap() {
    // The overlay caps the composition buffer at 4096 chars to
    // bound memory on a stuck key. Pasted text must follow the same
    // cap by truncating the tail; dropping the entire paste would
    // be far worse UX than a clipped paste.
    let mut app = App::new(40, 80).expect("App::new");
    let pane_a = app.ws().focused_pane_id;
    let mut state = crate::input::overlay::OverlayState::new(pane_a);
    for _ in 0..4090 {
        state.insert_char('x');
    }
    app.overlay = Some(state);

    app.handle_paste("0123456789abcdef").expect("handle_paste");

    let overlay = app.overlay.as_ref().expect("overlay still open");
    assert_eq!(overlay.buffer.chars().count(), 4096);
    assert_eq!(overlay.cursor, 4096);
    assert!(
        overlay.buffer.ends_with("xxxxxx012345"),
        "tail of buffer must reflect the truncated paste prefix, got {:?}",
        &overlay.buffer[overlay.buffer.len().saturating_sub(20)..]
    );
}

#[test]
fn handle_paste_normalizes_crlf_from_windows_clipboard() {
    // The actual user environment is WSL2, where Ctrl+V pastes text
    // copied from Windows applications — clipboard payloads use CRLF
    // line endings. wrap_overlay_buffer (ui.rs) only recognizes \n
    // as a hard newline, so a stray \r would render as a zero-width
    // control glyph and offset the rendered cursor from the buffer
    // cursor by one char per pasted line. Normalize at intake.
    let mut app = App::new(40, 80).expect("App::new");
    let pane_a = app.ws().focused_pane_id;
    app.overlay = Some(crate::input::overlay::OverlayState::new(pane_a));

    app.handle_paste("first\r\nsecond\rthird")
        .expect("handle_paste");

    let overlay = app.overlay.as_ref().expect("overlay still open");
    assert_eq!(
        overlay.buffer, "first\nsecond\nthird",
        "CRLF and bare CR must collapse to \\n"
    );
    assert_eq!(overlay.cursor, "first\nsecond\nthird".chars().count());
    assert!(
        !overlay.buffer.contains('\r'),
        "no carriage returns should survive normalization"
    );
}

#[test]
fn handle_paste_falls_through_to_pty_when_overlay_closed() {
    // Behavior preservation guard: with no overlay open the paste
    // path must still reach the focused pane's PTY (and return false
    // so main.rs keeps applying the post-PTY-paste cooldown).
    let mut app = App::new(40, 80).expect("App::new");
    assert!(app.overlay.is_none());

    let routed_to_overlay = app.handle_paste("hello").expect("handle_paste");

    assert!(
        !routed_to_overlay,
        "no overlay → paste must report PTY-routed"
    );
}
