//! Issue #329 — `list_panes` can enumerate tabs other than the
//! caller's, and every record carries the tab it lives in.
//!
//! Same stage as the #288 / #290 tests: workspace 0 holds the caller (a
//! background tab), workspace 1 is the tab the human is watching. What
//! #329 adds is the ability to see *both* from either one. Before it, a
//! pane parked in a background tab was reachable by numeric id but
//! absent from every enumeration that carries geometry — so an
//! orchestrator monitoring its workers read absence as exit, and
//! counted its own capacity short.

use super::super::*;

/// Two tabs. Returns `(caller_pane_in_ws0, active_pane_in_ws1)` with
/// workspace 1 active — the caller is *not* in the visible tab.
fn two_tabs() -> (App, usize, usize) {
    let mut app = App::new(40, 120).expect("App::new");
    let caller = app.ws().focused_pane_id;
    app.new_tab().expect("new_tab");
    let active = app.ws().focused_pane_id;
    assert_eq!(app.active_tab, 1, "the new tab is the visible one");
    assert_ne!(caller, active);
    (app, caller, active)
}

/// The PTY size `pane_id` currently believes it has. A resize clears
/// the pane's screen, so this is the observable proxy for "reading the
/// pane list disturbed a pane it had no business touching".
fn pty_size(app: &App, ws_index: usize, pane_id: usize) -> (u16, u16) {
    let pane = app.workspaces[ws_index]
        .panes
        .get(&pane_id)
        .expect("pane exists");
    let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
    parser.screen().size()
}

// ─── all-tabs enumeration ─────────────────────────────────

#[test]
fn list_with_tab_all_spans_every_tab_caller_first() {
    let (mut app, caller, active) = two_tabs();

    let infos = app
        .handle_list(Some(caller), Some(&ipc::ListTabSelector::All))
        .expect("all-tabs list");

    assert_eq!(
        infos.iter().map(|p| p.id).collect::<Vec<_>>(),
        vec![caller, active],
        "the caller's own (background) tab comes first, then the rest in index order"
    );
    assert_eq!(
        infos.iter().map(|p| p.tab).collect::<Vec<_>>(),
        vec![Some(0), Some(1)]
    );
    assert_eq!(
        infos.iter().map(|p| p.same_tab).collect::<Vec<_>>(),
        vec![Some(true), Some(false)]
    );
    app.shutdown();
}

/// The ordering is anchored on the *caller*, not on the visible tab or
/// on the index order — the same rule `handle_peer_list` follows.
#[test]
fn list_with_tab_all_from_the_visible_tab_puts_the_visible_tab_first() {
    let (mut app, caller, active) = two_tabs();

    let infos = app
        .handle_list(Some(active), Some(&ipc::ListTabSelector::All))
        .expect("all-tabs list");

    assert_eq!(
        infos.iter().map(|p| p.id).collect::<Vec<_>>(),
        vec![active, caller]
    );
    assert_eq!(
        infos.iter().map(|p| p.same_tab).collect::<Vec<_>>(),
        vec![Some(true), Some(false)]
    );
    app.shutdown();
}

/// The identity half of #329: a record must be attributable to a tab,
/// and — because two independent orchestrations reuse the same role
/// names — carry the `cwd` that tells identically-named panes apart.
#[test]
fn list_with_a_tab_selector_returns_only_that_tab_with_identity() {
    let (mut app, caller, active) = two_tabs();
    app.workspaces[0].custom_name = Some("workers".into());

    let infos = app
        .handle_list(
            Some(active),
            Some(&ipc::ListTabSelector::Name("workers".into())),
        )
        .expect("named-tab list");

    assert_eq!(infos.iter().map(|p| p.id).collect::<Vec<_>>(), vec![caller]);
    let rec = &infos[0];
    assert_eq!(rec.tab, Some(0));
    assert_eq!(rec.tab_name.as_deref(), Some("workers"));
    assert_eq!(rec.same_tab, Some(false));
    assert!(rec.cwd.is_some(), "cwd is the cross-org discriminator");
    app.shutdown();
}

/// A pane in a *background* tab is what #329 exists to surface, and a
/// caller in that same background tab must still see the visible tab.
/// Both directions at once, through the command path.
#[test]
fn all_tabs_list_reports_real_geometry_for_a_hidden_tab() {
    let (mut app, caller, active) = two_tabs();

    let (tx, rx) = oneshot::channel();
    app.handle_app_command(AppCommand::List {
        from_pane: Some(active),
        tab: Some(ipc::ListTabSelector::All),
        reply: tx,
    });
    let infos = rx.recv().expect("list reply").expect("list ok");

    let hidden = infos
        .iter()
        .find(|p| p.id == caller)
        .expect("the hidden tab's pane is in the list");
    assert!(
        hidden.width > 0 && hidden.height > 0,
        "hidden tab reported placeholder geometry {}x{}",
        hidden.width,
        hidden.height
    );
    app.shutdown();
}

/// `recompute_hidden_rects_for_ipc` is a *read*: it recomputes rects
/// without resizing PTYs. The all-tabs path now touches every workspace
/// on that read path, so re-prove what `caller_scope.rs` proves for the
/// single-tab one — listing must not clear anybody's screen.
#[test]
fn an_all_tabs_list_never_resizes_or_clears_a_hidden_pane() {
    let (mut app, caller, active) = two_tabs();
    app.relayout_workspace(0);
    let before = pty_size(&app, 0, caller);

    let (tx, rx) = oneshot::channel();
    app.handle_app_command(AppCommand::List {
        from_pane: Some(active),
        tab: Some(ipc::ListTabSelector::All),
        reply: tx,
    });
    let _ = rx.recv().expect("list reply").expect("list ok");

    assert_eq!(
        pty_size(&app, 0, caller),
        before,
        "an all-tabs list resized a hidden pane, clearing its screen"
    );
    app.shutdown();
}

// ─── selector errors come from the shared resolver ────────

#[test]
fn list_tab_selector_index_out_of_range_is_tab_not_found() {
    let (mut app, caller, _active) = two_tabs();
    let err = app
        .handle_list(Some(caller), Some(&ipc::ListTabSelector::Index(99)))
        .expect_err("index out of range");
    assert_eq!(err.code, Some(ipc::err_code::TAB_NOT_FOUND));
    app.shutdown();
}

#[test]
fn list_tab_selector_ambiguous_name_is_tab_ambiguous() {
    let (mut app, caller, _active) = two_tabs();
    app.workspaces[0].custom_name = Some("workers".into());
    app.workspaces[1].custom_name = Some("workers".into());

    let err = app
        .handle_list(
            Some(caller),
            Some(&ipc::ListTabSelector::Name("workers".into())),
        )
        .expect_err("ambiguous label");
    assert_eq!(err.code, Some(ipc::err_code::TAB_AMBIGUOUS));
    // Same resolver as the spawn path, so the same actionable remedy.
    assert!(err.message.contains("pane_id"), "got: {}", err.message);
    app.shutdown();
}

#[test]
fn list_tab_selector_unknown_pane_anchor_is_pane_not_found() {
    let (mut app, caller, _active) = two_tabs();
    let err = app
        .handle_list(Some(caller), Some(&ipc::ListTabSelector::PaneId(9999)))
        .expect_err("unknown anchor pane");
    assert_eq!(err.code, Some(ipc::err_code::PANE_NOT_FOUND));
    app.shutdown();
}

/// The caller is resolved first and unconditionally, even when the
/// selector — not the caller's tab — decides the scope. A stale
/// `from_pane` must never degrade into "answer about the visible tab".
#[test]
fn list_rejects_an_unknown_from_pane_even_with_an_explicit_tab() {
    let (mut app, _caller, _active) = two_tabs();
    let err = app
        .handle_list(Some(9999), Some(&ipc::ListTabSelector::All))
        .expect_err("unknown caller pane");
    assert_eq!(err.code, Some(ipc::err_code::PANE_NOT_FOUND));
    app.shutdown();
}

// ─── the default path is an addition, not a replacement ───

#[test]
fn default_list_still_returns_only_the_callers_tab() {
    let (mut app, caller, _active) = two_tabs();
    let infos = app.handle_list(Some(caller), None).expect("default list");
    assert_eq!(infos.iter().map(|p| p.id).collect::<Vec<_>>(), vec![caller]);
    app.shutdown();
}

/// `same_tab` answers "can I address this by bare name", which is only
/// in question when the set can span tabs. `tab` / `tab_name` are
/// always present — they are identity, not a cross-tab hint.
#[test]
fn default_list_omits_same_tab_and_still_reports_its_tab() {
    let (mut app, caller, _active) = two_tabs();
    let infos = app.handle_list(Some(caller), None).expect("default list");
    let rec = &infos[0];
    assert!(rec.same_tab.is_none(), "got: {:?}", rec.same_tab);
    assert_eq!(rec.tab, Some(0));
    assert!(rec.tab_name.is_some());
    app.shutdown();
}

/// The `renga list` CLI path has no caller pane, so there is nothing
/// for `same_tab` to be true *of*.
#[test]
fn legacy_cli_list_omits_same_tab() {
    let (mut app, _caller, active) = two_tabs();
    let infos = app.handle_list(None, None).expect("legacy list");
    assert_eq!(infos.iter().map(|p| p.id).collect::<Vec<_>>(), vec![active]);
    assert!(infos.iter().all(|p| p.same_tab.is_none()));
    app.shutdown();
}

/// Focus is per-workspace, so an all-tabs list carries one focused
/// pane *per tab*. Pinned because a consumer that assumes exactly one
/// would silently mis-read the wider set.
#[test]
fn an_all_tabs_list_reports_focus_per_tab_not_once_overall() {
    let (mut app, _caller, active) = two_tabs();
    let infos = app
        .handle_list(Some(active), Some(&ipc::ListTabSelector::All))
        .expect("all-tabs list");
    assert_eq!(
        infos.iter().filter(|p| p.focused).count(),
        2,
        "one focused pane per tab, not one overall"
    );
    // The one the keyboard actually reaches is the focused pane of the
    // caller's own tab.
    let mine: Vec<usize> = infos
        .iter()
        .filter(|p| p.focused && p.same_tab == Some(true))
        .map(|p| p.id)
        .collect();
    assert_eq!(mine, vec![active]);
    app.shutdown();
}

/// Unlike `list_peers`, `list_panes` has always included the caller
/// itself — capacity accounting that dropped the asking pane would be
/// wrong in the same direction #329 is fixing.
#[test]
fn an_all_tabs_list_still_includes_the_caller_itself() {
    let (mut app, caller, _active) = two_tabs();
    let infos = app
        .handle_list(Some(caller), Some(&ipc::ListTabSelector::All))
        .expect("all-tabs list");
    assert!(infos.iter().any(|p| p.id == caller));
    app.shutdown();
}
