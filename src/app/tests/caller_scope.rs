//! Issue #288 — the pane-control IPC requests resolve against the
//! **caller's** tab, not the tab the user happens to be looking at.
//! Issue #296 extends the same rule to `close_pane` /
//! `set_pane_identity`, the two mutating requests #288 left behind.
//!
//! Every test here builds the same shape: workspace 0 holds the caller
//! (a background tab), workspace 1 is the active tab the human is
//! watching. Pre-#288 all of these operated on workspace 1.

use super::super::*;

/// Two tabs. Returns `(caller_pane_in_ws0, active_pane_in_ws1)` with
/// workspace 1 active — i.e. the caller is *not* in the visible tab.
fn two_tabs() -> (App, usize, usize) {
    let mut app = App::new(40, 120).expect("App::new");
    let caller = app.ws().focused_pane_id;
    app.new_tab().expect("new_tab");
    let active = app.ws().focused_pane_id;
    assert_eq!(app.active_tab, 1, "the new tab is the visible one");
    assert_ne!(caller, active);
    (app, caller, active)
}

// ─── list_panes ───────────────────────────────────────────

#[test]
fn list_with_from_pane_returns_the_callers_tab_not_the_active_one() {
    let (mut app, caller, active) = two_tabs();

    let scoped = app.handle_list(Some(caller), None).expect("scoped list");
    let ids: Vec<usize> = scoped.iter().map(|p| p.id).collect();
    assert_eq!(ids, vec![caller], "caller's tab holds exactly its own pane");

    // And the legacy (CLI) call still describes the visible tab.
    let legacy = app.handle_list(None, None).expect("legacy list");
    assert_eq!(
        legacy.iter().map(|p| p.id).collect::<Vec<_>>(),
        vec![active]
    );

    app.shutdown();
}

#[test]
fn list_rejects_an_unknown_from_pane_instead_of_falling_back() {
    let (mut app, _caller, _active) = two_tabs();
    let err = app
        .handle_list(Some(9999), None)
        .expect_err("unknown caller pane");
    assert_eq!(err.code, Some(ipc::err_code::PANE_NOT_FOUND));
    app.shutdown();
}

/// The geometry fields exist so an agent can act on them. A hidden tab
/// is not relaid out on a terminal resize, so listing it without
/// refreshing hands back coordinates from a terminal that is gone.
#[test]
fn list_reports_geometry_for_the_current_terminal_not_the_one_at_last_render() {
    let (mut app, caller, _active) = two_tabs();
    app.relayout_workspace(0);

    app.on_terminal_resize(60, 20);
    let infos = app.handle_list(Some(caller), None).expect("scoped list");
    let rect = infos
        .iter()
        .find(|p| p.id == caller)
        .expect("caller in its own list");
    assert!(
        rect.width <= 60 && rect.height <= 20,
        "hidden tab reported {}x{} for a 60x20 terminal",
        rect.width,
        rect.height
    );
    assert!(rect.width > 0 && rect.height > 0);
    app.shutdown();
}

// ─── inspect_pane ─────────────────────────────────────────

#[test]
fn inspect_focused_resolves_in_the_callers_tab() {
    let (mut app, caller, active) = two_tabs();

    let payload = app
        .handle_inspect(&ipc::PaneRef::Focused, None, false, Some(caller))
        .expect("inspect focused");
    assert_eq!(
        payload["pane"]["id"].as_u64(),
        Some(caller as u64),
        "`focused` must mean the caller tab's focused pane"
    );

    let legacy = app
        .handle_inspect(&ipc::PaneRef::Focused, None, false, None)
        .expect("legacy inspect");
    assert_eq!(legacy["pane"]["id"].as_u64(), Some(active as u64));

    app.shutdown();
}

/// The vt100 screen size of a pane in workspace `ws_index`.
///
/// This is the observable for "was this pane resized?": `Pane::resize`
/// is the only caller of `set_size`, and it is also what clears the
/// screen. Asserting on it tests the mechanism directly.
///
/// The obvious alternative — write a marker into the parser, then check
/// it is still there — cannot work here. Every pane hosts a real shell
/// whose reader thread feeds the same parser, so the marker survives
/// only until the child's startup output lands. That window is long
/// enough on Linux and too short on macOS and Windows, which is exactly
/// how this started life as a green test that failed on two of three CI
/// platforms.
fn pty_size(app: &App, ws_index: usize, pane_id: usize) -> (u16, u16) {
    let pane = app.workspaces[ws_index]
        .panes
        .get(&pane_id)
        .expect("pane exists");
    let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
    parser.screen().size()
}

/// The size `Pane::resize` *would* apply to `pane_id` if the workspace
/// were relaid out right now. Used to assert that a resize would have
/// been observable, so a "nothing was resized" assertion cannot pass
/// vacuously.
fn would_be_pty_size(app: &App, ws_index: usize, pane_id: usize) -> (u16, u16) {
    let area = app.main_area_layout_for(ws_index).panes;
    let rects = app.workspaces[ws_index].layout.calculate_rects(area);
    let (_, rect) = rects
        .iter()
        .find(|(id, _)| *id == pane_id)
        .expect("pane has a rect");
    (rect.height.saturating_sub(2), rect.width.saturating_sub(2))
}

/// `Pane::resize` clears the vt100 buffer and leaves the child to
/// redraw on SIGWINCH. Refreshing a hidden pane's geometry on the way
/// into `inspect_pane` would therefore erase the screen this call exists
/// to report and snapshot the blank — the caller sees nothing wrong,
/// just an empty pane.
#[test]
fn inspect_does_not_resize_the_pane_it_is_about_to_read() {
    let (mut app, caller, _active) = two_tabs();
    app.relayout_workspace(0);

    // A global layout change: applied to the active tab only, so
    // workspace 0's geometry is now out of date and a refresh here would
    // resize — and therefore clear — the pane being inspected.
    app.status_bar_visible = !app.status_bar_visible;
    app.mark_layout_change();

    let before = pty_size(&app, 0, caller);
    assert_ne!(
        before,
        would_be_pty_size(&app, 0, caller),
        "precondition: a refresh would have resized this pane"
    );

    let (reply_tx, reply_rx) = oneshot::channel();
    app.handle_app_command(AppCommand::Inspect {
        target: ipc::PaneRef::Focused,
        lines: None,
        include_cursor: false,
        from_pane: Some(caller),
        reply: reply_tx,
    });
    let _ = reply_rx.recv().expect("inspect reply").expect("inspect ok");

    assert_eq!(
        pty_size(&app, 0, caller),
        before,
        "inspect resized the pane, clearing the screen it exists to report"
    );
    app.shutdown();
}

/// Resizing a pane clears its vt100 screen and leaves the child to
/// repaint on SIGWINCH — which a TUI does and a plain shell does not.
/// So neither a terminal resize nor a `list_panes` from some other tab
/// may push a resize into a hidden pane: the output would be gone, with
/// nothing to regenerate it. Only rects are recomputed; the real resize
/// waits until that tab is rendered.
#[test]
fn a_resize_and_an_unrelated_list_never_resize_a_hidden_pane() {
    let (mut app, caller, active) = two_tabs();
    app.relayout_workspace(0);
    let before = pty_size(&app, 0, caller);

    app.on_terminal_resize(70, 24);
    assert_ne!(
        before,
        would_be_pty_size(&app, 0, caller),
        "precondition: the new terminal size implies a different pane size"
    );
    assert_eq!(
        pty_size(&app, 0, caller),
        before,
        "a terminal resize resized a hidden pane, clearing its screen"
    );
    // The pure half still ran: the cached rects describe the new
    // terminal even though no PTY was touched.
    let rect_width = app.workspaces[0]
        .last_pane_rects
        .iter()
        .find(|(id, _)| *id == caller)
        .map(|(_, r)| r.width)
        .expect("caller rect");
    assert!(
        rect_width <= 70,
        "hidden tab still reports {rect_width} cols for a 70-col terminal"
    );

    // `list_panes` issued from the *other* tab must not touch this one.
    let (tx, rx) = oneshot::channel();
    app.handle_app_command(AppCommand::List {
        from_pane: Some(active),
        tab: None,
        reply: tx,
    });
    let _ = rx.recv().expect("list reply").expect("list ok");
    assert_eq!(
        pty_size(&app, 0, caller),
        before,
        "a list_panes for another tab resized this pane, clearing its screen"
    );

    app.shutdown();
}

#[test]
fn inspect_by_id_reaches_across_tabs() {
    let (mut app, caller, active) = two_tabs();
    let payload = app
        .handle_inspect(&ipc::PaneRef::Id(active), None, false, Some(caller))
        .expect("explicit cross-tab id");
    assert_eq!(payload["pane"]["id"].as_u64(), Some(active as u64));
    app.shutdown();
}

// ─── send_keys ────────────────────────────────────────────

#[test]
fn send_focused_writes_to_the_callers_pane() {
    let (mut app, caller, _active) = two_tabs();
    app.handle_send(&ipc::PaneRef::Focused, b"hi", false, Some(caller))
        .expect("send to caller's focused pane");
    app.shutdown();
}

#[test]
fn send_by_name_never_leaves_the_callers_tab() {
    let (mut app, caller, active) = two_tabs();
    // Same name registered in both tabs; the caller must get its own.
    app.workspaces[0].pane_names.insert("worker".into(), caller);
    app.workspaces[1].pane_names.insert("worker".into(), active);

    let (ws_idx, pane_id) = app
        .resolve_request_target(Some(caller), &ipc::PaneRef::Name("worker".into()))
        .expect("name resolves");
    assert_eq!((ws_idx, pane_id), (0, caller));

    // A name that only exists in the *active* tab is invisible to the
    // caller — no silent cross-tab fallback.
    app.workspaces[1]
        .pane_names
        .insert("only-active".into(), active);
    let err = app
        .resolve_request_target(Some(caller), &ipc::PaneRef::Name("only-active".into()))
        .expect_err("name from another tab must not resolve");
    assert_eq!(err.code, Some(ipc::err_code::PANE_NOT_FOUND));

    app.shutdown();
}

#[test]
fn a_dead_from_pane_is_rejected_even_when_the_target_id_is_valid() {
    let (mut app, _caller, active) = two_tabs();
    let err = app
        .handle_send(&ipc::PaneRef::Id(active), b"x", false, Some(4242))
        .expect_err("bogus caller");
    assert_eq!(err.code, Some(ipc::err_code::PANE_NOT_FOUND));
    assert!(
        err.message.contains("caller pane"),
        "error must name the caller, not the target: {}",
        err.message
    );
    app.shutdown();
}

// ─── focus_pane ───────────────────────────────────────────

#[test]
fn focus_when_the_caller_is_already_visible_switches_nothing() {
    let mut app = App::new(40, 120).expect("App::new");
    let a = app.ws().focused_pane_id;
    let b = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
            Some(a),
            None,
        )
        .expect("split");
    assert_eq!(app.active_tab, 0);

    app.handle_focus(&ipc::PaneRef::Id(a), Some(b))
        .expect("focus sibling");
    assert_eq!(app.active_tab, 0, "no tab switch inside the visible tab");
    assert_eq!(app.ws().focused_pane_id, a);
    app.shutdown();
}

/// The contract is "focus means the keystrokes land there". A pane in a
/// tab the user cannot see cannot receive keystrokes, so *any* focus
/// that resolves outside the visible tab brings that tab forward —
/// including the caller focusing a pane in its own (hidden) tab. The
/// alternative, quietly setting `focused_pane_id` on a hidden
/// workspace, reports success while changing nothing the user or the
/// keyboard can observe.
#[test]
fn focus_resolving_into_a_hidden_tab_brings_that_tab_forward() {
    let (mut app, caller, _active) = two_tabs();
    app.handle_focus(&ipc::PaneRef::Focused, Some(caller))
        .expect("focus own pane from a background tab");
    assert_eq!(
        app.active_tab, 0,
        "focus the keyboard cannot reach is not focus — the tab must follow"
    );
    assert_eq!(app.workspaces[0].focused_pane_id, caller);
    assert!(matches!(app.workspaces[0].focus_target, FocusTarget::Pane));
    app.shutdown();
}

/// A cross-tab focus is a tab switch, so it has to do a tab switch's
/// bookkeeping. Every one of these caches is keyed to a pane or a tab
/// index in the tab being left; leaving them behind means the next
/// click or copy acts on geometry that is no longer on screen.
#[test]
fn focus_across_tabs_drops_the_state_keyed_to_the_tab_it_leaves() {
    let (mut app, caller, active) = two_tabs();
    app.selection = Some(TextSelection {
        target: SelectionTarget::Pane(active),
        start_row: 0,
        start_col: 0,
        end_row: 0,
        end_col: 3,
        content_rect: Rect::new(0, 0, 10, 2),
    });
    app.last_tab_click = Some((1, std::time::Instant::now()));
    app.last_edge_click = Some((1, 0, 0, std::time::Instant::now()));

    app.handle_focus(&ipc::PaneRef::Id(caller), Some(caller))
        .expect("cross-tab focus");

    assert_eq!(app.active_tab, 0);
    assert!(app.selection.is_none(), "selection is bound to the old tab");
    assert!(app.last_tab_click.is_none());
    assert!(app.last_edge_click.is_none());
    assert!(app.last_boundary_click.is_none());
    // The request named a pane, so the keyboard goes to the pane even
    // though `switch_tab` would otherwise carry sidebar focus over.
    assert!(matches!(app.workspaces[0].focus_target, FocusTarget::Pane));
    assert_eq!(app.workspaces[0].focused_pane_id, caller);
    app.shutdown();
}

#[test]
fn focus_across_tabs_by_id_also_switches_the_visible_tab() {
    let (mut app, caller, active) = two_tabs();
    // Caller sits in hidden tab 0 and explicitly names a pane in the
    // visible tab 1 — allowed, and the visible tab stays put because
    // that is already where the target lives.
    app.handle_focus(&ipc::PaneRef::Id(active), Some(caller))
        .expect("cross-tab focus into the visible tab");
    assert_eq!(app.active_tab, 1);
    assert_eq!(app.workspaces[1].focused_pane_id, active);

    // Now the other direction: name the hidden tab's pane by id.
    app.handle_focus(&ipc::PaneRef::Id(caller), Some(active))
        .expect("cross-tab focus into the hidden tab");
    assert_eq!(app.active_tab, 0, "the hidden tab is brought forward");
    assert_eq!(app.workspaces[0].focused_pane_id, caller);
    app.shutdown();
}

// ─── spawn_* (Split) ──────────────────────────────────────

#[test]
fn split_lands_in_the_callers_tab_and_leaves_the_active_one_alone() {
    let (mut app, caller, active) = two_tabs();
    let active_focus_before = app.workspaces[1].focused_pane_id;
    let active_panes_before = app.workspaces[1].layout.pane_count();

    let new_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            Some("spawned".into()),
            None,
            None,
            Some(caller),
            None,
        )
        .expect("split in caller's tab");

    assert!(
        app.workspaces[0].panes.contains_key(&new_id),
        "new pane belongs to the caller's workspace"
    );
    assert_eq!(
        app.workspaces[0].focused_pane_id, new_id,
        "focus follows the new pane inside its own tab"
    );
    assert_eq!(
        app.workspaces[0].pane_names.get("spawned").copied(),
        Some(new_id)
    );

    // The visible tab is completely untouched.
    assert_eq!(app.active_tab, 1);
    assert_eq!(app.workspaces[1].focused_pane_id, active_focus_before);
    assert_eq!(app.workspaces[1].layout.pane_count(), active_panes_before);
    assert!(!app.workspaces[1].panes.contains_key(&new_id));
    assert_ne!(new_id, active);

    app.shutdown();
}

#[test]
fn split_in_a_hidden_tab_reports_real_geometry_not_zeros() {
    let (mut app, caller, _active) = two_tabs();
    // Give the hidden workspace the geometry a render would have left.
    app.relayout_workspace(0);

    let new_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
            Some(caller),
            None,
        )
        .expect("split hidden tab");

    let infos = app
        .handle_list(Some(caller), None)
        .expect("list caller tab");
    let new_info = infos
        .iter()
        .find(|p| p.id == new_id)
        .expect("new pane in list");
    assert!(
        new_info.width > 0 && new_info.height > 0,
        "a hidden workspace never renders, so the split has to refresh its \
         rects itself — otherwise list_panes reports {new_info:?}"
    );
    app.shutdown();
}

#[test]
fn a_refused_split_leaves_both_workspaces_focus_untouched() {
    let (mut app, caller, _active) = two_tabs();
    let caller_focus_before = app.workspaces[0].focused_pane_id;
    let active_focus_before = app.workspaces[1].focused_pane_id;

    // Force a refusal without touching the layout: a minimum pane size
    // wider than the terminal makes every split too small.
    app.min_pane_width = 10_000;
    app.relayout_workspace(0);

    let err = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
            Some(caller),
            None,
        )
        .expect_err("split must be refused");
    assert_eq!(err.code, Some(ipc::err_code::SPLIT_REFUSED));

    assert_eq!(app.workspaces[0].focused_pane_id, caller_focus_before);
    assert_eq!(app.workspaces[1].focused_pane_id, active_focus_before);
    assert_eq!(app.active_tab, 1);
    app.shutdown();
}

#[test]
fn split_inherits_the_target_panes_cwd_across_tabs() {
    let (mut app, caller, _active) = two_tabs();
    let base = std::env::temp_dir()
        .canonicalize()
        .expect("temp dir canonicalizes");
    if let Some(pane) = app.workspaces[0].panes.get_mut(&caller) {
        pane.cwd = base.clone();
    }

    let new_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            // Relative cwd is resolved against the *target* pane, which
            // is the caller here. `.` keeps us inside `base`.
            Some(".".into()),
            Some(caller),
            None,
        )
        .expect("split with relative cwd");

    let new_cwd = app.workspaces[0]
        .panes
        .get(&new_id)
        .map(|p| p.cwd.clone())
        .expect("new pane exists");
    assert_eq!(
        new_cwd,
        crate::app::layout_ops::strip_verbatim_prefix(base),
        "relative cwd resolves against the target pane in the caller's tab"
    );
    app.shutdown();
}

/// `poll_events` is process-wide, so an orchestrator in a background
/// tab waits on the event stream for the worker it just named. Pane ids
/// are unique App-wide, so resolving the new pane's metadata in the
/// *active* workspace does not error — it quietly returns nothing, and
/// the event goes out with `name: null, role: null`.
#[test]
fn a_cross_tab_spawn_emits_pane_started_with_its_name_and_role() {
    let (mut app, caller, _active) = two_tabs();
    let (_sub_id, rx) = app.event_bus.subscribe();

    let id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            Some("worker-1".into()),
            Some("worker".into()),
            None,
            Some(caller),
            None,
        )
        .expect("cross-tab split");

    let mut observed: Option<(Option<String>, Option<String>)> = None;
    while let Ok(ev) = rx.try_recv() {
        if let ipc::Event::PaneStarted {
            id: ev_id,
            name,
            role,
            ..
        } = ev
        {
            if ev_id == id {
                observed = Some((name, role));
                break;
            }
        }
    }
    let (name, role) = observed.expect("PaneStarted for the new pane");
    assert_eq!(name.as_deref(), Some("worker-1"));
    assert_eq!(role.as_deref(), Some("worker"));
    app.shutdown();
}

/// The root invariant behind the caller-scoped geometry: a terminal
/// resize relayouts *every* workspace, not just the visible one. Without
/// it, a hidden workspace keeps the rects it had when it was last on
/// screen, and every reader (`list_panes`, `inspect_pane`, the split
/// min-size guard) has to remember to refresh — three chances to forget
/// the fourth.
#[test]
fn a_terminal_resize_relayouts_hidden_workspaces_too() {
    let (mut app, caller, _active) = two_tabs();
    app.relayout_workspace(0);

    app.on_terminal_resize(46, 20);
    let width = app.workspaces[0]
        .last_pane_rects
        .iter()
        .find(|(id, _)| *id == caller)
        .map(|(_, r)| r.width)
        .expect("caller rect");
    assert!(
        width <= 46,
        "hidden tab still reports {width} cols after a resize to 46"
    );
    app.shutdown();
}

/// A resize is not the only thing that moves every workspace's pane
/// area — a status-bar toggle, a sidebar drag and a layout swap do too,
/// and those refresh only the active tab. The IPC boundary is what
/// guarantees a background caller never reads the difference, so these
/// go through `handle_app_command` rather than calling the handler
/// directly.
#[test]
fn a_global_layout_change_does_not_leak_stale_geometry_to_a_background_caller() {
    let (mut app, caller, _active) = two_tabs();
    app.relayout_workspace(0);
    let before = app.workspaces[0]
        .last_pane_rects
        .iter()
        .find(|(id, _)| *id == caller)
        .map(|(_, r)| r.height)
        .expect("caller rect");

    // Alt+S: a global metric, refreshed for the active tab only.
    app.status_bar_visible = !app.status_bar_visible;
    app.mark_layout_change();

    let (reply_tx, reply_rx) = oneshot::channel();
    app.handle_app_command(AppCommand::List {
        from_pane: Some(caller),
        tab: None,
        reply: reply_tx,
    });
    let infos = reply_rx.recv().expect("list reply").expect("list ok");
    let reported = infos
        .iter()
        .find(|p| p.id == caller)
        .map(|p| p.height)
        .expect("caller listed");

    assert_ne!(
        before, reported,
        "precondition: toggling the status bar changes the pane height"
    );
    assert_eq!(
        reported, app.workspaces[0].last_pane_rects[0].1.height,
        "the reported height is the workspace's current one"
    );
    app.shutdown();
}

/// Consequence of that invariant: a cross-tab split is judged against
/// the terminal that exists now, so it refuses exactly where the same
/// split in the visible tab would.
#[test]
fn a_cross_tab_split_guards_against_the_current_terminal() {
    let (mut app, caller, _active) = two_tabs();
    app.relayout_workspace(0);
    app.set_min_pane_size(20, 5);
    app.on_terminal_resize(46, 20);

    let err = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
            Some(caller),
            None,
        )
        .expect_err("46 cols cannot hold two 20-column panes");
    assert_eq!(err.code, Some(ipc::err_code::SPLIT_REFUSED));
    app.shutdown();
}

/// Below the layout threshold `relayout_workspace` cannot run at all, so
/// every workspace's rects describe a terminal that is gone. Splitting
/// on them would be guesswork; refuse instead.
#[test]
fn a_split_is_refused_while_the_terminal_is_too_small_to_lay_out() {
    let (mut app, caller, _active) = two_tabs();
    app.relayout_workspace(0);
    app.on_terminal_resize(10, 3);

    let err = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
            Some(caller),
            None,
        )
        .expect_err("no usable geometry at 10x3");
    assert_eq!(err.code, Some(ipc::err_code::SPLIT_REFUSED));
    app.shutdown();
}

/// A background agent spawning a pane must not drop the text the user
/// is selecting in the tab they are looking at — that tab's geometry did
/// not move. A selection anchored to the *split* workspace is stale and
/// still has to go.
#[test]
fn a_cross_tab_split_keeps_a_selection_that_belongs_to_another_tab() {
    let (mut app, caller, active) = two_tabs();
    let sel = |pane_id| TextSelection {
        target: SelectionTarget::Pane(pane_id),
        start_row: 0,
        start_col: 0,
        end_row: 0,
        end_col: 4,
        content_rect: Rect::new(0, 0, 10, 2),
    };

    app.selection = Some(sel(active));
    app.handle_split(
        &ipc::PaneRef::Focused,
        ipc::Direction::Vertical,
        None,
        None,
        None,
        None,
        Some(caller),
        None,
    )
    .expect("split the hidden tab");
    assert!(
        app.selection.is_some(),
        "the visible tab's selection survives a split in another tab"
    );

    // Anchored in the workspace being split: that geometry just moved.
    app.selection = Some(sel(caller));
    app.handle_split(
        &ipc::PaneRef::Id(caller),
        ipc::Direction::Horizontal,
        None,
        None,
        None,
        None,
        Some(caller),
        None,
    )
    .expect("second split");
    assert!(
        app.selection.is_none(),
        "a selection in the split workspace is stale and must be dropped"
    );
    app.shutdown();
}

// ─── close_pane / set_pane_identity (Issue #296) ──────────

/// [`two_tabs`] with a second pane in each tab, so closing one pane
/// neither closes a whole tab nor trips the `last_pane` guard. Returns
/// `(app, caller_ws0, sibling_ws0, active_ws1, sibling_ws1)`; the two
/// siblings are also registered under the names `bg-sibling` /
/// `fg-sibling`.
fn two_tabs_two_panes() -> (App, usize, usize, usize, usize) {
    let (mut app, caller, active) = two_tabs();
    app.relayout_workspace(0);
    let bg_sibling = app
        .handle_split(
            &ipc::PaneRef::Id(caller),
            ipc::Direction::Vertical,
            None,
            Some("bg-sibling".into()),
            None,
            None,
            Some(caller),
            None,
        )
        .expect("split the background tab");
    let fg_sibling = app
        .handle_split(
            &ipc::PaneRef::Id(active),
            ipc::Direction::Vertical,
            None,
            Some("fg-sibling".into()),
            None,
            None,
            None,
            None,
        )
        .expect("split the visible tab");
    (app, caller, bg_sibling, active, fg_sibling)
}

/// The bug in one test: a background orchestrator asking to close "the
/// focused pane" used to kill a pane in whatever tab the human was
/// typing in, and closing is not undoable.
#[test]
fn close_focused_from_a_background_caller_never_touches_the_visible_tab() {
    let (mut app, caller, bg_sibling, active, fg_sibling) = two_tabs_two_panes();
    let bg_focus = app.workspaces[0].focused_pane_id;
    assert!(
        bg_focus == caller || bg_focus == bg_sibling,
        "precondition: the caller's tab has its own focus"
    );

    let closed = app
        .handle_close(&ipc::PaneRef::Focused, Some(caller))
        .expect("close the caller tab's focused pane");

    assert_eq!(closed, bg_focus, "`focused` means the caller's tab");
    assert!(!app.workspaces[0].panes.contains_key(&bg_focus));
    assert!(
        app.workspaces[1].panes.contains_key(&active)
            && app.workspaces[1].panes.contains_key(&fg_sibling),
        "the tab the user is watching must be untouched"
    );
    app.shutdown();
}

#[test]
fn close_focused_without_from_pane_still_targets_the_visible_tab() {
    let (mut app, caller, bg_sibling, _active, _fg_sibling) = two_tabs_two_panes();
    let visible_focus = app.workspaces[1].focused_pane_id;

    let closed = app
        .handle_close(&ipc::PaneRef::Focused, None)
        .expect("legacy close");

    assert_eq!(
        closed, visible_focus,
        "`renga close --focused` is unchanged"
    );
    assert!(
        app.workspaces[0].panes.contains_key(&caller)
            && app.workspaces[0].panes.contains_key(&bg_sibling),
        "the other tab keeps both panes"
    );
    app.shutdown();
}

/// The escape hatch #296 must not remove: a numeric id is globally
/// unique, so naming one is an explicit cross-tab request.
#[test]
fn close_by_numeric_id_still_reaches_another_tab() {
    let (mut app, caller, _bg_sibling, _active, fg_sibling) = two_tabs_two_panes();
    let closed = app
        .handle_close(&ipc::PaneRef::Id(fg_sibling), Some(caller))
        .expect("numeric ids stay cross-tab");
    assert_eq!(closed, fg_sibling);
    assert!(!app.workspaces[1].panes.contains_key(&fg_sibling));
    app.shutdown();
}

/// Names are unique per tab, never globally — a caller-scoped name that
/// silently matched another tab's pane would be the #288 bug with a
/// destructive payload. The legacy CLI path keeps its cross-tab search.
#[test]
fn close_by_name_stays_inside_the_callers_tab() {
    let (mut app, caller, bg_sibling, _active, fg_sibling) = two_tabs_two_panes();

    let err = app
        .handle_close(&ipc::PaneRef::Name("fg-sibling".into()), Some(caller))
        .expect_err("a name from another tab is not addressable");
    assert_eq!(err.code, Some(ipc::err_code::PANE_NOT_FOUND));
    assert!(
        app.workspaces[1].panes.contains_key(&fg_sibling),
        "a refused close must not have closed anything"
    );

    // `renga close --name bg-sibling` still finds a pane in a
    // non-visible tab, exactly as before #296.
    let closed = app
        .handle_close(&ipc::PaneRef::Name("bg-sibling".into()), None)
        .expect("the CLI's cross-tab name search is preserved");
    assert_eq!(closed, bg_sibling);
    app.shutdown();
}

/// A caller we cannot attribute is an error, not a reason to fall back
/// to the visible tab — including for an `Id` target that would have
/// resolved on its own.
#[test]
fn close_rejects_an_unknown_from_pane_instead_of_falling_back() {
    let (mut app, _caller, _bg_sibling, active, fg_sibling) = two_tabs_two_panes();

    for target in [ipc::PaneRef::Focused, ipc::PaneRef::Id(fg_sibling)] {
        let err = app
            .handle_close(&target, Some(9999))
            .expect_err("unknown caller pane");
        assert_eq!(err.code, Some(ipc::err_code::PANE_NOT_FOUND));
    }
    assert!(
        app.workspaces[1].panes.contains_key(&active)
            && app.workspaces[1].panes.contains_key(&fg_sibling),
        "nothing was closed"
    );
    app.shutdown();
}

/// The scoping has to survive the IPC boundary, not just the handler:
/// a dropped `from_pane` in `AppCommand` plumbing would restore the bug
/// while every handler-level test above stayed green.
#[test]
fn close_through_the_app_command_boundary_is_caller_scoped() {
    let (mut app, caller, _bg_sibling, active, fg_sibling) = two_tabs_two_panes();
    let bg_focus = app.workspaces[0].focused_pane_id;

    let (reply_tx, reply_rx) = oneshot::channel();
    app.handle_app_command(AppCommand::Close {
        target: ipc::PaneRef::Focused,
        from_pane: Some(caller),
        reply: reply_tx,
    });
    let closed = reply_rx.recv().expect("close reply").expect("close ok");

    assert_eq!(closed, bg_focus);
    assert!(
        app.workspaces[1].panes.contains_key(&active)
            && app.workspaces[1].panes.contains_key(&fg_sibling)
    );
    app.shutdown();
}

#[test]
fn set_identity_focused_from_a_background_caller_renames_its_own_pane() {
    let (mut app, caller, _bg_sibling, _active, _fg_sibling) = two_tabs_two_panes();
    let bg_focus = app.workspaces[0].focused_pane_id;
    let visible_focus = app.workspaces[1].focused_pane_id;

    let info = app
        .handle_set_pane_identity(
            &ipc::PaneRef::Focused,
            Some(Some("renamed".into())),
            Some(Some("worker".into())),
            Some(caller),
        )
        .expect("rename the caller tab's focused pane");

    assert_eq!(info.id, bg_focus);
    assert_eq!(
        app.workspaces[0].pane_names.get("renamed").copied(),
        Some(bg_focus)
    );
    assert!(
        !app.workspaces[1].pane_names.contains_key("renamed"),
        "the visible tab must not have been renamed"
    );
    assert_eq!(
        app.workspaces[1].focused_pane_id, visible_focus,
        "and its focus is untouched"
    );
    app.shutdown();
}

#[test]
fn set_identity_focused_without_from_pane_still_targets_the_visible_tab() {
    let (mut app, _caller, _bg_sibling, _active, _fg_sibling) = two_tabs_two_panes();
    let visible_focus = app.workspaces[1].focused_pane_id;

    let info = app
        .handle_set_pane_identity(
            &ipc::PaneRef::Focused,
            Some(Some("renamed".into())),
            None,
            None,
        )
        .expect("legacy rename");

    assert_eq!(info.id, visible_focus, "`renga rename --focused` unchanged");
    assert!(app.workspaces[1].pane_names.contains_key("renamed"));
    app.shutdown();
}

/// Cross-tab by id survives, and the `name_in_use` check keeps running
/// against the *resolved* pane's tab — which is what makes per-tab name
/// uniqueness coherent once a caller can address two tabs.
#[test]
fn set_identity_by_id_crosses_tabs_and_judges_uniqueness_in_the_targets_tab() {
    let (mut app, caller, bg_sibling, _active, fg_sibling) = two_tabs_two_panes();

    // `bg-sibling` is taken in tab 0 and free in tab 1.
    let info = app
        .handle_set_pane_identity(
            &ipc::PaneRef::Id(fg_sibling),
            Some(Some("bg-sibling".into())),
            None,
            Some(caller),
        )
        .expect("names are unique per tab, not globally");
    assert_eq!(info.id, fg_sibling);
    assert_eq!(
        app.workspaces[0].pane_names.get("bg-sibling").copied(),
        Some(bg_sibling),
        "tab 0's holder of the name is undisturbed"
    );

    // A collision *inside* the resolved pane's own tab still refuses.
    let err = app
        .handle_set_pane_identity(
            &ipc::PaneRef::Id(caller),
            Some(Some("bg-sibling".into())),
            None,
            Some(caller),
        )
        .expect_err("same-tab collision");
    assert_eq!(err.code, Some(ipc::err_code::NAME_IN_USE));
    app.shutdown();
}

#[test]
fn set_identity_by_name_stays_inside_the_callers_tab() {
    let (mut app, caller, _bg_sibling, _active, _fg_sibling) = two_tabs_two_panes();

    let err = app
        .handle_set_pane_identity(
            &ipc::PaneRef::Name("fg-sibling".into()),
            Some(Some("stolen".into())),
            None,
            Some(caller),
        )
        .expect_err("a name from another tab is not addressable");
    assert_eq!(err.code, Some(ipc::err_code::PANE_NOT_FOUND));
    assert!(
        app.workspaces[1].pane_names.contains_key("fg-sibling")
            && !app.workspaces[1].pane_names.contains_key("stolen"),
        "the other tab's naming is untouched"
    );

    // The CLI's cross-tab name search is preserved.
    let info = app
        .handle_set_pane_identity(
            &ipc::PaneRef::Name("bg-sibling".into()),
            None,
            Some(Some("worker".into())),
            None,
        )
        .expect("legacy cross-tab name lookup");
    assert_eq!(info.role.as_deref(), Some("worker"));
    app.shutdown();
}

#[test]
fn set_identity_rejects_an_unknown_from_pane_instead_of_falling_back() {
    let (mut app, _caller, _bg_sibling, _active, fg_sibling) = two_tabs_two_panes();

    for target in [ipc::PaneRef::Focused, ipc::PaneRef::Id(fg_sibling)] {
        let err = app
            .handle_set_pane_identity(&target, Some(Some("renamed".into())), None, Some(9999))
            .expect_err("unknown caller pane");
        assert_eq!(err.code, Some(ipc::err_code::PANE_NOT_FOUND));
    }
    assert!(
        !app.workspaces[1].pane_names.contains_key("renamed"),
        "nothing was renamed"
    );
    app.shutdown();
}

#[test]
fn set_identity_through_the_app_command_boundary_is_caller_scoped() {
    let (mut app, caller, _bg_sibling, _active, _fg_sibling) = two_tabs_two_panes();
    let bg_focus = app.workspaces[0].focused_pane_id;

    let (reply_tx, reply_rx) = oneshot::channel();
    app.handle_app_command(AppCommand::SetPaneIdentity {
        target: ipc::PaneRef::Focused,
        name: Some(Some("renamed".into())),
        role: None,
        from_pane: Some(caller),
        reply: reply_tx,
    });
    let info = reply_rx.recv().expect("identity reply").expect("ok");

    assert_eq!(info.id, bg_focus);
    assert!(!app.workspaces[1].pane_names.contains_key("renamed"));
    app.shutdown();
}

// ─── legacy (CLI) semantics ───────────────────────────────

#[test]
fn without_from_pane_an_id_stays_inside_the_active_tab() {
    let (mut app, caller, _active) = two_tabs();
    // `renga send --id <caller>` from a shell must keep behaving the
    // way it did before #288: active tab only, no cross-tab widening.
    let err = app
        .resolve_request_target(None, &ipc::PaneRef::Id(caller))
        .expect_err("legacy id must not reach into another tab");
    assert_eq!(err.code, Some(ipc::err_code::PANE_NOT_FOUND));
    app.shutdown();
}
