//! Issue #290 — tab-directed spawning: the `tab` selector on `Split`
//! and the background `SpawnTab` request.
//!
//! Same stage as the #288 caller-scope tests: workspace 0 holds the
//! caller (a background tab), workspace 1 is the tab the human is
//! watching. What #290 adds is the ability to *name* a third place.

use super::super::*;

/// Two tabs. Returns `(caller_pane_in_ws0, active_pane_in_ws1)` with
/// workspace 1 active — the caller is *not* in the visible tab.
fn two_tabs() -> (App, usize, usize) {
    let mut app = App::new(40, 120).expect("App::new");
    let caller = app.ws().focused_pane_id;
    app.new_tab().expect("new_tab");
    let active = app.ws().focused_pane_id;
    assert_eq!(app.active_tab, 1, "the new tab is the visible one");
    (app, caller, active)
}

// ─── tab selector resolution on Split ─────────────────────

#[test]
fn split_lands_in_the_tab_named_by_the_selector() {
    let (mut app, caller, _active) = two_tabs();
    app.relayout_workspace(0);
    app.workspaces[0].custom_name = Some("workers".into());

    // The caller sits in the *active* tab here on purpose: the selector,
    // not the caller's own tab, must decide placement.
    let active_pane = app.workspaces[1].focused_pane_id;
    let new_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
            Some(active_pane),
            Some(&ipc::TabSelector::Name("workers".into())),
        )
        .expect("split into the named tab");

    assert!(
        app.workspaces[0].panes.contains_key(&new_id),
        "new pane must live in the selected tab"
    );
    assert_eq!(app.active_tab, 1, "visible tab unchanged");
    let _ = caller;
    app.shutdown();
}

#[test]
fn tab_name_with_no_match_is_tab_not_found() {
    let (mut app, caller, _active) = two_tabs();
    let err = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
            Some(caller),
            Some(&ipc::TabSelector::Name("nope".into())),
        )
        .expect_err("no tab is named nope");
    assert_eq!(err.code, Some(ipc::err_code::TAB_NOT_FOUND));
    app.shutdown();
}

/// Labels are not unique. Guessing between two tabs with the same name
/// is the wrong-tab bug again, so the server must refuse — never
/// first-match.
#[test]
fn tab_name_with_two_matches_is_tab_ambiguous() {
    let (mut app, caller, _active) = two_tabs();
    app.workspaces[0].custom_name = Some("workers".into());
    app.workspaces[1].custom_name = Some("workers".into());
    let err = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
            Some(caller),
            Some(&ipc::TabSelector::Name("workers".into())),
        )
        .expect_err("two tabs share the name");
    assert_eq!(err.code, Some(ipc::err_code::TAB_AMBIGUOUS));
    app.shutdown();
}

#[test]
fn tab_index_is_zero_based_and_bounds_checked() {
    let (mut app, caller, _active) = two_tabs();
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
            Some(&ipc::TabSelector::Index(0)),
        )
        .expect("index 0 is the first tab");
    assert!(app.workspaces[0].panes.contains_key(&new_id));

    let err = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
            Some(caller),
            Some(&ipc::TabSelector::Index(5)),
        )
        .expect_err("only two tabs exist");
    assert_eq!(err.code, Some(ipc::err_code::TAB_NOT_FOUND));
    app.shutdown();
}

/// `{pane_id}` is the stable anchor: it selects whatever tab owns that
/// pane, however the tabs have been renamed or reordered since.
#[test]
fn tab_pane_id_anchor_selects_the_owning_tab() {
    let (mut app, caller, _active) = two_tabs();
    app.relayout_workspace(0);
    let active_pane = app.workspaces[1].focused_pane_id;

    let new_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
            Some(active_pane),
            Some(&ipc::TabSelector::PaneId(caller)),
        )
        .expect("anchor on the caller pane");
    assert!(
        app.workspaces[0].panes.contains_key(&new_id),
        "pane_id anchor must select the anchor's tab"
    );
    app.shutdown();
}

/// A numeric `target` in a different tab than the selector picked is a
/// contradiction between the two halves of the request. The implicit
/// cross-tab escape hatch of plain `Split` must not apply here.
#[test]
fn foreign_numeric_target_is_target_tab_mismatch() {
    let (mut app, caller, active) = two_tabs();
    let err = app
        .handle_split(
            &ipc::PaneRef::Id(active),
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
            Some(caller),
            Some(&ipc::TabSelector::Index(0)),
        )
        .expect_err("target lives in tab 1, selector says tab 0");
    assert_eq!(err.code, Some(ipc::err_code::TARGET_TAB_MISMATCH));
    app.shutdown();
}

/// `{new: …}` is `spawn_tab`'s job; a raw wire client sending it on a
/// `split` gets a `protocol` error, never a fallthrough into some
/// existing tab.
#[test]
fn split_with_tab_new_is_a_protocol_error() {
    let (mut app, caller, _active) = two_tabs();
    let tabs_before = app.workspaces.len();
    for selector in [
        ipc::TabSelector::New { name: None },
        ipc::TabSelector::New {
            name: Some("workers".into()),
        },
    ] {
        let err = app
            .handle_split(
                &ipc::PaneRef::Focused,
                ipc::Direction::Vertical,
                None,
                None,
                None,
                None,
                Some(caller),
                Some(&selector),
            )
            .expect_err("tab.new is not a split");
        assert_eq!(err.code, Some(ipc::err_code::PROTOCOL));
    }
    assert_eq!(app.workspaces.len(), tabs_before, "no tab was created");
    assert_eq!(
        app.workspaces
            .iter()
            .map(|ws| ws.panes.len())
            .sum::<usize>(),
        2,
        "no pane was created"
    );
    app.shutdown();
}

#[test]
fn tab_selector_with_unknown_caller_is_pane_not_found() {
    let (mut app, _caller, _active) = two_tabs();
    let err = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
            Some(9999),
            Some(&ipc::TabSelector::Index(0)),
        )
        .expect_err("unknown caller pane");
    assert_eq!(err.code, Some(ipc::err_code::PANE_NOT_FOUND));
    app.shutdown();
}

// ─── SpawnTab: background tab creation ────────────────────

#[test]
fn spawn_tab_creates_a_background_tab_without_switching_focus() {
    let (mut app, caller, _active) = two_tabs();
    let (new_id, ws_idx) = app
        .handle_spawn_tab(None, None, None, None, None, Some(caller))
        .expect("spawn background tab");

    assert_eq!(ws_idx, 2, "the new tab appends at the end");
    assert_eq!(app.active_tab, 1, "the visible tab must not change");
    assert!(app.workspaces[2].panes.contains_key(&new_id));
    assert_eq!(app.workspaces[2].focused_pane_id, new_id);
    app.shutdown();
}

/// The success reply is the only geometry a background tab ever gets
/// until the user switches to it, so it must be real — not the 10x40
/// placeholder the pane is born with.
#[test]
fn spawn_tab_finalizes_geometry_before_returning() {
    let (mut app, caller, _active) = two_tabs();
    let (new_id, ws_idx) = app
        .handle_spawn_tab(None, None, None, None, None, Some(caller))
        .expect("spawn background tab");

    let rect = app.workspaces[ws_idx]
        .last_pane_rects
        .iter()
        .find(|(id, _)| *id == new_id)
        .map(|&(_, r)| r)
        .expect("background tab has rects");
    assert!(
        rect.width > 0 && rect.height > 0,
        "rects must describe the current terminal, got {}x{}",
        rect.width,
        rect.height
    );

    // And the PTY itself was resized to match, not just the cache.
    {
        let pane = app.workspaces[ws_idx].panes.get(&new_id).expect("pane");
        let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
        let (rows, cols) = parser.screen().size();
        assert_eq!(
            (rows, cols),
            (rect.height.saturating_sub(2), rect.width.saturating_sub(2)),
            "PTY size must match the finalized rect"
        );
    }
    app.shutdown();
}

#[test]
fn spawn_tab_emits_exactly_one_pane_started_with_name_and_role() {
    let (mut app, caller, _active) = two_tabs();
    let (_sub_id, rx) = app.event_bus.subscribe();

    let (new_id, _ws_idx) = app
        .handle_spawn_tab(
            None,
            Some("worker-9".into()),
            None,
            Some("worker".into()),
            None,
            Some(caller),
        )
        .expect("spawn background tab");

    let mut started: Vec<(Option<String>, Option<String>)> = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let ipc::Event::PaneStarted {
            id: ev_id,
            name,
            role,
            ..
        } = ev
        {
            if ev_id == new_id {
                started.push((name, role));
            }
        }
    }
    assert_eq!(started.len(), 1, "exactly one PaneStarted for the pane");
    let (name, role) = &started[0];
    assert_eq!(name.as_deref(), Some("worker-9"));
    assert_eq!(role.as_deref(), Some("worker"));
    app.shutdown();
}

#[test]
fn spawn_tab_applies_the_requested_label() {
    let (mut app, caller, _active) = two_tabs();
    let (_new_id, ws_idx) = app
        .handle_spawn_tab(None, None, Some("workers".into()), None, None, Some(caller))
        .expect("spawn background tab");
    assert_eq!(app.workspaces[ws_idx].display_name(), "workers");
    app.shutdown();
}

/// Omitted cwd follows the **caller pane** — a spawn API places
/// workers relative to the orchestrator that asked, not relative to
/// wherever the renga server process happened to start.
#[test]
fn spawn_tab_inherits_the_callers_cwd_when_omitted() {
    let (mut app, caller, _active) = two_tabs();
    let base = std::env::temp_dir()
        .canonicalize()
        .expect("temp dir canonicalizes");
    let base = crate::app::layout_ops::strip_verbatim_prefix(base);
    if let Some(pane) = app.workspaces[0].panes.get_mut(&caller) {
        pane.cwd = base.clone();
    }

    let (_new_id, ws_idx) = app
        .handle_spawn_tab(None, None, None, None, None, Some(caller))
        .expect("spawn background tab");
    assert_eq!(
        app.workspaces[ws_idx].cwd, base,
        "background tab must inherit the caller pane's cwd"
    );
    app.shutdown();
}

/// Registering a name the addressing rules can never resolve (an
/// all-digit string parses as a numeric id) must fail up front — not
/// leave a successfully created tab behind with a dead alias.
#[test]
fn spawn_tab_with_invalid_name_refuses_before_mutation() {
    let (mut app, caller, _active) = two_tabs();
    let tabs_before = app.workspaces.len();
    for bad in ["123", "bad name", "wörker"] {
        let err = app
            .handle_spawn_tab(None, Some(bad.into()), None, None, None, Some(caller))
            .expect_err("invalid pane name");
        assert_eq!(err.code, Some(ipc::err_code::NAME_INVALID), "name={bad:?}");
    }
    assert_eq!(app.workspaces.len(), tabs_before, "no tab was created");
    app.shutdown();
}

/// #290 validated `spawn_tab`'s `name` but let `role` and `label`
/// through verbatim. Both reach another agent's context via
/// `list_peers` / `list_panes`, so a control character in either is the
/// same forgery primitive an invalid name was.
#[test]
fn spawn_tab_refuses_control_characters_in_role_and_label() {
    let (mut app, caller, _active) = two_tabs();
    let tabs_before = app.workspaces.len();

    let err = app
        .handle_spawn_tab(
            None,
            None,
            None,
            Some("a\nworker".into()),
            None,
            Some(caller),
        )
        .expect_err("role with a newline");
    assert_eq!(err.code, Some(ipc::err_code::NAME_INVALID));

    let err = app
        .handle_spawn_tab(
            None,
            None,
            Some("a\rworkers".into()),
            None,
            None,
            Some(caller),
        )
        .expect_err("label with a carriage return");
    assert_eq!(err.code, Some(ipc::err_code::NAME_INVALID));

    let err = app
        .handle_spawn_tab(
            None,
            None,
            None,
            Some("a\u{1b}[2Jb".into()),
            None,
            Some(caller),
        )
        .expect_err("role with an ANSI escape");
    assert_eq!(err.code, Some(ipc::err_code::NAME_INVALID));

    assert_eq!(app.workspaces.len(), tabs_before, "no tab was created");
    app.shutdown();
}

/// The charset restriction is deliberately *not* extended to the two
/// free-form fields: `role` is documented as "Optional free-form role
/// label" and a tab label defaults to a cwd-derived directory name, so
/// spaces and non-ASCII must keep working.
#[test]
fn spawn_tab_still_accepts_free_form_role_and_label() {
    let (mut app, caller, _active) = two_tabs();
    let (_id, ws_idx) = app
        .handle_spawn_tab(
            None,
            None,
            Some("リリース v2.0.0".into()),
            Some("code reviewer".into()),
            None,
            Some(caller),
        )
        .expect("free-form role and label stay legal");
    assert_eq!(
        app.workspaces[ws_idx].display_name(),
        "リリース v2.0.0",
        "a label with spaces and non-ASCII must survive"
    );
    app.shutdown();
}

#[test]
fn spawn_tab_with_invalid_cwd_refuses_before_mutation() {
    let (mut app, caller, _active) = two_tabs();
    let tabs_before = app.workspaces.len();
    let err = app
        .handle_spawn_tab(
            None,
            None,
            None,
            None,
            Some("/definitely/not/a/dir".into()),
            Some(caller),
        )
        .expect_err("bogus cwd");
    assert_eq!(err.code, Some(ipc::err_code::CWD_INVALID));
    assert_eq!(app.workspaces.len(), tabs_before, "no tab was created");
    app.shutdown();
}

#[test]
fn the_tab_cap_fails_with_tab_limit_reached_not_split_refused() {
    let (mut app, caller, _active) = two_tabs();
    while app.workspaces.len() < App::MAX_TABS {
        app.new_tab().expect("fill up to MAX_TABS");
    }

    let err = app
        .handle_spawn_tab(None, None, None, None, None, Some(caller))
        .expect_err("tab cap reached");
    assert_eq!(err.code, Some(ipc::err_code::TAB_LIMIT_REACHED));

    // The activating path hits the same cap with the same code.
    let err = app
        .handle_new_tab(None, None, None, None, None)
        .expect_err("tab cap reached for new_tab too");
    assert_eq!(err.code, Some(ipc::err_code::TAB_LIMIT_REACHED));
    app.shutdown();
}
