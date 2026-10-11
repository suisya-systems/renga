use super::super::*;

fn make_pane_spec(id: &str) -> crate::layout_config::LayoutNodeSpec {
    crate::layout_config::LayoutNodeSpec::Pane {
        id: id.to_string(),
        command: None,
        role: None,
        cwd: None,
    }
}

#[test]
fn apply_layout_maps_split_first_to_target_and_second_to_new() {
    // Given a 2-pane Split spec, after apply_layout:
    // - pane_names["left"]  must point at the workspace's original
    //   pane (what we split off)
    // - pane_names["right"] must point at the freshly-created pane
    // - the LayoutNode tree must have Leaf(left) in `first` and
    //   Leaf(right) in `second`
    // Regression guard for the reviewer's concern that the
    // first/second recursion arms in apply_layout_node might drift.
    let cfg = crate::layout_config::LayoutConfig {
        version: 1,
        name: "test".into(),
        root: crate::layout_config::LayoutNodeSpec::Split {
            direction: crate::layout_config::DirectionSpec::Vertical,
            ratio: 0.5,
            first: Box::new(make_pane_spec("left")),
            second: Box::new(make_pane_spec("right")),
        },
    };

    let mut app = App::new(40, 80).expect("App::new");
    let initial_pane_id = app.ws().focused_pane_id;

    app.apply_layout(&cfg).expect("apply_layout");

    let ws = app.ws();
    let left_id = *ws
        .pane_names
        .get("left")
        .expect("pane_names[left] should be registered");
    let right_id = *ws
        .pane_names
        .get("right")
        .expect("pane_names[right] should be registered");

    assert_eq!(
        left_id, initial_pane_id,
        "`first` spec ('left') must map to the original/split-target pane"
    );
    assert_ne!(
        right_id, initial_pane_id,
        "`second` spec ('right') must map to a newly-spawned pane"
    );

    match &ws.layout {
        LayoutNode::Split { first, second, .. } => {
            match first.as_ref() {
                LayoutNode::Leaf { pane_id } => {
                    assert_eq!(*pane_id, left_id, "layout.first must be the 'left' pane")
                }
                other => panic!("expected Leaf in first, got {other:?}"),
            }
            match second.as_ref() {
                LayoutNode::Leaf { pane_id } => {
                    assert_eq!(*pane_id, right_id, "layout.second must be the 'right' pane")
                }
                other => panic!("expected Leaf in second, got {other:?}"),
            }
        }
        other => panic!("expected Split at root, got {other:?}"),
    }

    app.shutdown();
}

#[test]
fn apply_layout_nested_split_preserves_positions() {
    // A right-heavy tree: Split { first: "A", second: Split { "B", "C" } }.
    // After apply, the LayoutNode must mirror that shape exactly.
    let cfg = crate::layout_config::LayoutConfig {
        version: 1,
        name: "nested".into(),
        root: crate::layout_config::LayoutNodeSpec::Split {
            direction: crate::layout_config::DirectionSpec::Vertical,
            ratio: 0.5,
            first: Box::new(make_pane_spec("A")),
            second: Box::new(crate::layout_config::LayoutNodeSpec::Split {
                direction: crate::layout_config::DirectionSpec::Horizontal,
                ratio: 0.5,
                first: Box::new(make_pane_spec("B")),
                second: Box::new(make_pane_spec("C")),
            }),
        },
    };

    let mut app = App::new(40, 80).expect("App::new");
    app.apply_layout(&cfg).expect("apply_layout");

    let ws = app.ws();
    let a_id = *ws.pane_names.get("A").expect("A registered");
    let b_id = *ws.pane_names.get("B").expect("B registered");
    let c_id = *ws.pane_names.get("C").expect("C registered");

    match &ws.layout {
        LayoutNode::Split {
            first: outer_first,
            second: outer_second,
            ..
        } => {
            match outer_first.as_ref() {
                LayoutNode::Leaf { pane_id } => assert_eq!(*pane_id, a_id),
                other => panic!("outer.first must be Leaf(A), got {other:?}"),
            }
            match outer_second.as_ref() {
                LayoutNode::Split {
                    first: inner_first,
                    second: inner_second,
                    ..
                } => {
                    match inner_first.as_ref() {
                        LayoutNode::Leaf { pane_id } => assert_eq!(*pane_id, b_id),
                        other => panic!("inner.first must be Leaf(B), got {other:?}"),
                    }
                    match inner_second.as_ref() {
                        LayoutNode::Leaf { pane_id } => assert_eq!(*pane_id, c_id),
                        other => panic!("inner.second must be Leaf(C), got {other:?}"),
                    }
                }
                other => panic!("outer.second must be a Split, got {other:?}"),
            }
        }
        other => panic!("expected Split at root, got {other:?}"),
    }

    app.shutdown();
}

#[test]
fn handle_close_refuses_last_pane_of_only_tab() {
    // Fresh App has exactly one workspace with one pane; closing
    // that pane must fail with the `last_pane` code so subscribers
    // can distinguish "can't close" from "doesn't exist".
    let mut app = App::new(40, 80).expect("App::new");
    let only = app.ws().focused_pane_id;

    let err = app
        .handle_close(&ipc::PaneRef::Id(only), None)
        .expect_err("closing the last pane must fail");
    assert_eq!(err.code, Some(ipc::err_code::LAST_PANE));

    // Pane still alive.
    assert!(app.ws().panes.contains_key(&only));
    app.shutdown();
}

#[test]
fn handle_close_returns_pane_not_found_for_bogus_id() {
    let mut app = App::new(40, 80).expect("App::new");
    let err = app
        .handle_close(&ipc::PaneRef::Id(9_999), None)
        .expect_err("bogus id should fail");
    assert_eq!(err.code, Some(ipc::err_code::PANE_NOT_FOUND));
    app.shutdown();
}

#[test]
fn handle_close_removes_pane_and_returns_id() {
    // Build a 2-pane layout so `handle_close` has something to
    // remove without tripping the last-pane guard.
    let cfg = crate::layout_config::LayoutConfig {
        version: 1,
        name: "close-test".into(),
        root: crate::layout_config::LayoutNodeSpec::Split {
            direction: crate::layout_config::DirectionSpec::Vertical,
            ratio: 0.5,
            first: Box::new(make_pane_spec("left")),
            second: Box::new(make_pane_spec("right")),
        },
    };
    let mut app = App::new(40, 80).expect("App::new");
    app.apply_layout(&cfg).expect("apply_layout");

    let right_id = *app.ws().pane_names.get("right").expect("right registered");
    let left_id = *app.ws().pane_names.get("left").expect("left registered");

    let closed = app
        .handle_close(&ipc::PaneRef::Name("right".into()), None)
        .expect("close right pane");
    assert_eq!(closed, right_id);

    let ws = app.ws();
    assert!(!ws.panes.contains_key(&right_id), "pane must be removed");
    assert!(ws.panes.contains_key(&left_id), "left must survive");
    assert!(
        !ws.pane_names.contains_key("right"),
        "pane_names entry must be dropped"
    );
    assert_eq!(ws.layout.pane_count(), 1);
    assert_eq!(ws.focused_pane_id, left_id);

    app.shutdown();
}

#[test]
fn handle_close_in_background_tab_marks_dirty_and_updates_list() {
    // Cover the Codex review bug: closing a pane that lives in a
    // non-active workspace must still schedule a render and make
    // the pane disappear from subsequent `renga list` snapshots on
    // the freshly-touched tab. Prior to the fix the dirty flag
    // stayed low because `mark_layout_change` was gated on the
    // active tab.
    let cfg = crate::layout_config::LayoutConfig {
        version: 1,
        name: "bg-close".into(),
        root: crate::layout_config::LayoutNodeSpec::Split {
            direction: crate::layout_config::DirectionSpec::Vertical,
            ratio: 0.5,
            first: Box::new(make_pane_spec("bg-left")),
            second: Box::new(make_pane_spec("bg-right")),
        },
    };
    let mut app = App::new(40, 80).expect("App::new");
    app.apply_layout(&cfg).expect("apply_layout");

    // The 2-pane layout lives in workspace 0. Open a second tab so
    // workspace 0 becomes a background tab; the new tab (index 1)
    // becomes active with a single fresh pane.
    app.new_tab().expect("new_tab");
    assert_eq!(app.active_tab, 1);
    let active_focus_before = app.ws().focused_pane_id;

    // Clear the dirty flag set by new_tab so we can attribute the
    // next mutation to `handle_close` only.
    app.dirty = false;

    let bg_right_id = app.workspaces[0]
        .pane_names
        .get("bg-right")
        .copied()
        .expect("bg-right registered");

    let closed = app
        .handle_close(&ipc::PaneRef::Id(bg_right_id), None)
        .expect("close bg-right");
    assert_eq!(closed, bg_right_id);

    assert!(
        app.dirty,
        "close on a background workspace must schedule a repaint"
    );
    assert!(
        !app.workspaces[0].panes.contains_key(&bg_right_id),
        "bg-right must be gone from workspace 0"
    );
    // Active tab must be untouched.
    assert_eq!(app.active_tab, 1);
    assert_eq!(app.ws().focused_pane_id, active_focus_before);

    app.shutdown();
}

#[test]
fn close_releases_pane_name_for_reuse() {
    // After close, the stable name must be available again so a
    // subsequent `renga split --id same-name` doesn't collide with
    // a dangling entry.
    let cfg = crate::layout_config::LayoutConfig {
        version: 1,
        name: "reuse".into(),
        root: crate::layout_config::LayoutNodeSpec::Split {
            direction: crate::layout_config::DirectionSpec::Vertical,
            ratio: 0.5,
            first: Box::new(make_pane_spec("keeper")),
            second: Box::new(make_pane_spec("victim")),
        },
    };
    let mut app = App::new(40, 80).expect("App::new");
    app.apply_layout(&cfg).expect("apply_layout");

    let victim_id_before = *app.ws().pane_names.get("victim").expect("registered");
    app.handle_close(&ipc::PaneRef::Name("victim".into()), None)
        .expect("close victim");
    assert!(!app.ws().pane_names.contains_key("victim"));

    // Split again asking for the same name; handle_split must
    // succeed and the new pane id must be different from the old.
    let new_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            Some("victim".into()),
            None,
            None,
            None,
            None,
        )
        .expect("split with reused name");
    assert_ne!(new_id, victim_id_before);
    assert_eq!(
        app.ws().pane_names.get("victim").copied(),
        Some(new_id),
        "pane_names must point at the freshly-created pane, not the dead one"
    );

    app.shutdown();
}

#[test]
fn close_after_natural_exit_does_not_double_emit() {
    // The EOF detection path and the CLI close path both guard on
    // `Pane.exit_event_emitted`, so a subscriber must see at most
    // one `PaneExited` per pane id regardless of order. We can't
    // drive a real EOF in a unit test, but we can simulate the
    // race by flipping the flag manually before calling
    // `handle_close` — exercising the same guard the natural-exit
    // path would have used.
    let cfg = crate::layout_config::LayoutConfig {
        version: 1,
        name: "race".into(),
        root: crate::layout_config::LayoutNodeSpec::Split {
            direction: crate::layout_config::DirectionSpec::Vertical,
            ratio: 0.5,
            first: Box::new(make_pane_spec("a")),
            second: Box::new(make_pane_spec("b")),
        },
    };
    let mut app = App::new(40, 80).expect("App::new");
    app.apply_layout(&cfg).expect("apply_layout");

    let (_sub_id, rx) = app.event_bus.subscribe();
    let b_id = *app.ws().pane_names.get("b").expect("b registered");

    // Simulate PtyEof having beaten us to the emission.
    app.workspaces[0]
        .panes
        .get_mut(&b_id)
        .expect("pane b")
        .exit_event_emitted = true;

    let closed = app
        .handle_close(&ipc::PaneRef::Id(b_id), None)
        .expect("close pane b");
    assert_eq!(closed, b_id);
    assert!(!app.ws().panes.contains_key(&b_id));

    // Drain any events that did fire. The pre-flag means
    // handle_close must not emit PaneExited for b_id.
    let mut saw_b_exited = false;
    while let Ok(ev) = rx.try_recv() {
        if let ipc::Event::PaneExited { id, .. } = ev {
            if id == b_id {
                saw_b_exited = true;
            }
        }
    }
    assert!(
        !saw_b_exited,
        "PaneExited for b must be suppressed when exit_event_emitted was already set"
    );

    app.shutdown();
}

#[test]

fn default_command_for_role_returns_claude_launch_for_claude_role() {
    // Strategy C from #97: `renga split --role claude` must pre-fill
    // the peer-channel flag so the user doesn't need to know the
    // incantation. `default_command_for_role` is the single seam
    // mapping role names to preloaded commands — regress this and
    // new Claude panes silently skip the channel activation.
    let cmd = default_command_for_role(Some("claude")).expect("claude role preloads cmd");
    assert!(
        cmd.contains("--dangerously-load-development-channels server:renga-peers"),
        "launch cmd must carry the peer channel flag, got: {cmd}"
    );
    assert!(
        cmd.starts_with("claude "),
        "launch cmd must invoke claude: {cmd}"
    );
}

#[test]
fn default_command_for_role_returns_none_for_other_roles() {
    assert!(default_command_for_role(None).is_none());
    assert!(default_command_for_role(Some("worker")).is_none());
    assert!(default_command_for_role(Some("")).is_none());
}

#[test]
fn apply_layout_auto_upgrades_bare_claude_command() {
    // Issue #126: layout toml's `command = "claude"` should receive
    // the same peer-enabled launch-line upgrade as MCP spawn_pane /
    // Alt+P, so layout-declared panes join the renga-peers network.
    let cfg = crate::layout_config::LayoutConfig {
        version: 1,
        name: "upgrade-test".into(),
        root: crate::layout_config::LayoutNodeSpec::Pane {
            id: "secretary".into(),
            command: Some("claude".into()),
            role: None,
            cwd: None,
        },
    };
    let mut app = App::new(40, 80).expect("App::new");
    app.apply_layout(&cfg).expect("apply_layout");

    let pane_id = *app.ws().pane_names.get("secretary").expect("registered");
    let pane = app.ws().panes.get(&pane_id).expect("pane");
    let queued = pane
        .pending_startup
        .as_ref()
        .expect("startup command queued");
    let queued_str = std::str::from_utf8(queued).expect("utf8");
    assert!(
        queued_str.contains("--dangerously-load-development-channels server:renga-peers"),
        "layout `claude` should be upgraded to peer-enabled form; got: {queued_str:?}"
    );
    app.shutdown();
}

#[test]
fn apply_layout_preserves_non_claude_command() {
    // Non-`claude` commands must pass through untouched.
    let cfg = crate::layout_config::LayoutConfig {
        version: 1,
        name: "passthrough-test".into(),
        root: crate::layout_config::LayoutNodeSpec::Pane {
            id: "worker".into(),
            command: Some("echo hello".into()),
            role: None,
            cwd: None,
        },
    };
    let mut app = App::new(40, 80).expect("App::new");
    app.apply_layout(&cfg).expect("apply_layout");

    let pane_id = *app.ws().pane_names.get("worker").expect("registered");
    let pane = app.ws().panes.get(&pane_id).expect("pane");
    let queued_str =
        std::str::from_utf8(pane.pending_startup.as_ref().expect("queued")).expect("utf8");
    assert!(queued_str.starts_with("echo hello"));
    assert!(!queued_str.contains("--dangerously-load-development-channels"));
    app.shutdown();
}

#[test]
fn handle_split_explicit_command_wins_over_role_claude_default() {
    // Regression guard for #97 Stage 5a: `--role claude` pre-fills
    // the peer-flagged launch command only when no explicit
    // `--command` was given. A caller who wants a custom claude
    // invocation (say with `/some-workflow` args) must not have
    // their command silently stomped by the default.
    let mut app = App::new(40, 80).expect("App::new");
    let new_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            Some("echo custom".into()),
            None,
            Some("claude".into()),
            None,
            None,
            None,
        )
        .expect("split succeeds");
    let pane = app.ws().panes.get(&new_id).expect("pane exists");
    let queued = pane
        .pending_startup
        .as_ref()
        .expect("explicit --command queued");
    let queued_str = std::str::from_utf8(queued).expect("utf8");
    assert!(
        queued_str.starts_with("echo custom"),
        "explicit command should win; got: {queued_str:?}"
    );
    assert!(
        !queued_str.contains("--dangerously-load-development-channels"),
        "role default must not be appended when --command was explicit; got: {queued_str:?}"
    );
    app.shutdown();
}

#[test]
fn handle_split_emits_pane_started_with_attached_name_and_role() {
    // Regression: previously split_focused_pane emitted PaneStarted
    // before handle_split attached name / role, so subscribers saw
    // `name: None, role: None` and could never filter on the
    // stable identifier. Guard against that regression by
    // subscribing and verifying the emitted event carries both.
    let mut app = App::new(40, 80).expect("App::new");
    let (_sub_id, rx) = app.event_bus.subscribe();

    let id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            Some("worker-1".into()),
            Some("worker".into()),
            None,
            None,
            None,
        )
        .expect("split succeeds");

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
    let (name, role) = observed.expect("PaneStarted for new pane");
    assert_eq!(name.as_deref(), Some("worker-1"));
    assert_eq!(role.as_deref(), Some("worker"));

    app.shutdown();
}

#[test]
fn apply_layout_emits_pane_started_after_leaf_metadata_is_attached() {
    // Regression: apply_layout_node's Split arm used to emit
    // PaneStarted for the freshly-created pane before recursing
    // into the leaf that attaches its role, so subscribers saw
    // the new pane with `role: None`.
    let cfg = crate::layout_config::LayoutConfig {
        version: 1,
        name: "role-test".into(),
        root: crate::layout_config::LayoutNodeSpec::Split {
            direction: crate::layout_config::DirectionSpec::Vertical,
            ratio: 0.5,
            first: Box::new(crate::layout_config::LayoutNodeSpec::Pane {
                id: "keeper".into(),
                command: None,
                role: Some("keeper-role".into()),
                cwd: None,
            }),
            second: Box::new(crate::layout_config::LayoutNodeSpec::Pane {
                id: "new-leaf".into(),
                command: None,
                role: Some("leaf-role".into()),
                cwd: None,
            }),
        },
    };

    let mut app = App::new(40, 80).expect("App::new");
    let (_sub_id, rx) = app.event_bus.subscribe();

    app.apply_layout(&cfg).expect("apply_layout");

    let new_leaf_id = *app.ws().pane_names.get("new-leaf").expect("registered");
    let mut observed: Option<(Option<String>, Option<String>)> = None;
    while let Ok(ev) = rx.try_recv() {
        if let ipc::Event::PaneStarted {
            id: ev_id,
            name,
            role,
            ..
        } = ev
        {
            if ev_id == new_leaf_id {
                observed = Some((name, role));
                break;
            }
        }
    }
    let (name, role) = observed.expect("PaneStarted for freshly-split leaf");
    assert_eq!(name.as_deref(), Some("new-leaf"));
    assert_eq!(role.as_deref(), Some("leaf-role"));

    app.shutdown();
}

#[test]
fn split_refused_keeps_focus_and_emits_no_pane_started() {
    // Drive handle_split into its refused arm (pane too small
    // after halving, below `min_pane_width` — default 20) and
    // confirm:
    //   * TARGET_TOO_SMALL bubbles up (Issue #335: a target-local
    //     refusal is no longer folded into `split_refused`),
    //   * focus stays where it was,
    //   * the requested name is NOT registered,
    //   * no PaneStarted event leaks out for the nonexistent pane.
    let mut app = App::new(40, 80).expect("App::new");

    // First split succeeds: 80 cols minus file-tree (20) = 60 cols
    // of pane area → two panes of ~30 cols each.
    app.handle_split(
        &ipc::PaneRef::Focused,
        ipc::Direction::Vertical,
        None,
        Some("first".into()),
        None,
        None,
        None,
        None,
    )
    .expect("first split should succeed");

    let focus_before = app.ws().focused_pane_id;
    let (_sub_id, rx) = app.event_bus.subscribe();

    // Second vertical split on the now-focused ~30-col pane would
    // produce ~15-col children, below `min_pane_width` (default
    // 20) → refuse.
    let err = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            Some("overflow".into()),
            None,
            None,
            None,
            None,
        )
        .expect_err("too-narrow split must be refused");
    assert_eq!(err.code, Some(ipc::err_code::TARGET_TOO_SMALL));

    assert_eq!(
        app.ws().focused_pane_id,
        focus_before,
        "refused split must not move focus"
    );
    assert!(
        !app.ws().pane_names.contains_key("overflow"),
        "refused split must not register its requested name"
    );

    let any_started = rx
        .try_iter()
        .any(|ev| matches!(ev, ipc::Event::PaneStarted { .. }));
    assert!(!any_started, "refused split must not emit PaneStarted");

    app.shutdown();
}

/// Issue #335: the target-local refusal must say so, and carry the
/// numbers the decision was made from, so a caller can branch without
/// a second round-trip.
#[test]
fn a_too_small_target_reports_its_geometry_and_the_tab_headroom() {
    let mut app = App::new(40, 80).expect("App::new");
    app.handle_split(
        &ipc::PaneRef::Focused,
        ipc::Direction::Vertical,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("first split should succeed");

    let err = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect_err("too-narrow split must be refused");
    assert_eq!(err.code, Some(ipc::err_code::TARGET_TOO_SMALL));
    let msg = &err.message;
    assert!(msg.contains("columns wide"), "{msg}");
    assert!(msg.contains("min_pane_width"), "{msg}");
    assert!(msg.contains("target-local"), "{msg}");
    // The tab count is what tells the caller this is *not* a capacity
    // problem: two panes against a cap of sixteen.
    assert!(
        msg.contains(&format!("2 of {} panes", App::MAX_PANES)),
        "{msg}"
    );
    app.shutdown();
}

/// The sibling of the test above: the tab-global cause gets its own
/// code, so an orchestrator can tell "retry against another target"
/// from "this tab is full" (Issue #335).
#[test]
fn the_pane_cap_reports_pane_limit_reached_not_split_refused() {
    let mut app = App::new(60, 240).expect("App::new");
    // Split the roomiest pane each round so the tree stays balanced —
    // chaining splits off one pane runs out of geometry long before
    // the cap.
    while app.ws().layout.pane_count() < App::MAX_PANES {
        app.relayout_workspace(0);
        let (id, rect) = app
            .ws()
            .last_pane_rects
            .iter()
            .copied()
            .max_by_key(|(_, r)| u32::from(r.width) * u32::from(r.height))
            .expect("laid-out panes");
        let direction = if rect.width >= rect.height * 2 {
            ipc::Direction::Vertical
        } else {
            ipc::Direction::Horizontal
        };
        app.handle_split(
            &ipc::PaneRef::Id(id),
            direction,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("fill up to MAX_PANES");
    }
    app.relayout_workspace(0);

    let (id, rect) = app
        .ws()
        .last_pane_rects
        .iter()
        .copied()
        .max_by_key(|(_, r)| u32::from(r.width) * u32::from(r.height))
        .expect("laid-out panes");
    // The cap is checked before any geometry, so even the roomiest
    // pane — one that would otherwise split fine — is refused with the
    // tab-global code.
    assert!(
        rect.width / 2 >= app.min_pane_width,
        "{rect:?} is splittable"
    );
    let err = app
        .handle_split(
            &ipc::PaneRef::Id(id),
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect_err("pane cap reached");
    assert_eq!(err.code, Some(ipc::err_code::PANE_LIMIT_REACHED));
    assert!(
        err.message
            .contains(&format!("{} of {} panes", App::MAX_PANES, App::MAX_PANES)),
        "{}",
        err.message
    );
    app.shutdown();
}

#[test]
fn set_min_pane_size_lets_split_succeed_below_default_threshold() {
    // With defaults (20 / 5), a second vertical split on a
    // ~30-col pane refuses (same geometry as
    // `split_refused_keeps_focus_and_emits_no_pane_started`).
    // Lowering the threshold via `set_min_pane_size` must let the
    // same split succeed and emit exactly one PaneStarted with
    // the attached name. Exercises the runtime wiring that CLI
    // parse tests cannot cover.
    let mut app = App::new(40, 80).expect("App::new");

    // First split runs under defaults (20 / 5) so the cached rect
    // geometry feeding the second split is identical to the
    // refusal test's setup — only the threshold itself differs.
    app.handle_split(
        &ipc::PaneRef::Focused,
        ipc::Direction::Vertical,
        None,
        Some("first".into()),
        None,
        None,
        None,
        None,
    )
    .expect("first split should succeed");

    // Lower the threshold just before the split that would
    // otherwise refuse, so the causal contrast with the sibling
    // refusal test is explicit in the test body.
    app.set_min_pane_size(10, 3);

    let (_sub_id, rx) = app.event_bus.subscribe();

    let new_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            Some("narrow".into()),
            None,
            None,
            None,
            None,
        )
        .expect("split should succeed once min_pane_width is lowered");

    assert_eq!(app.ws().focused_pane_id, new_id, "focus moves to new pane");
    assert_eq!(
        app.ws().pane_names.get("narrow").copied(),
        Some(new_id),
        "requested name registers on success"
    );

    let started_ids: Vec<usize> = rx
        .try_iter()
        .filter_map(|ev| match ev {
            ipc::Event::PaneStarted { id, .. } => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(
        started_ids,
        vec![new_id],
        "exactly one PaneStarted for the freshly-created pane"
    );

    app.shutdown();
}

/// Same setup as the `set_min_pane_size_*` tests: after one default
/// split the focused pane is ~30 cols wide, so a second vertical split
/// is refused under the default 20. (A fresh `App` has no cached rects
/// yet, so the min-size guard needs this first split to have geometry.)
fn app_after_one_vertical_split() -> App {
    let mut app = App::new(40, 80).expect("App::new");
    app.handle_split(
        &ipc::PaneRef::Focused,
        ipc::Direction::Vertical,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("first split should succeed");
    app
}

fn split_sized(
    app: &mut App,
    direction: ipc::Direction,
    w: Option<u16>,
    h: Option<u16>,
) -> Result<usize, ipc::CodedError> {
    app.handle_split_sized(
        &ipc::PaneRef::Focused,
        direction,
        None,
        None,
        None,
        None,
        None,
        None,
        (w, h),
    )
}

#[test]
fn per_call_min_pane_width_relaxes_the_global_minimum() {
    // Issue #206: same geometry as
    // `set_min_pane_size_lets_split_succeed_below_default_threshold`,
    // but the threshold is lowered for this one call only.
    let mut app = app_after_one_vertical_split();
    let v = ipc::Direction::Vertical;
    split_sized(&mut app, v, None, None).expect_err("default 20 refuses ~15-col halves");
    split_sized(&mut app, v, Some(10), None).expect("override 10 lets the split succeed");
    assert_eq!(app.min_pane_width, 20, "global minimum is untouched");
    app.shutdown();
}

#[test]
fn per_call_min_pane_width_tightens_the_global_minimum() {
    let mut app = app_after_one_vertical_split();
    let panes = app.ws().panes.len();
    // The ~38-row pane halves to ~19 rows: fine under the default 5,
    // refused under 30 — and the refusal names the override.
    let h = ipc::Direction::Horizontal;
    let err = split_sized(&mut app, h, None, Some(30)).expect_err("override 30 must refuse");
    assert_eq!(err.code, Some(ipc::err_code::TARGET_TOO_SMALL));
    assert!(err.message.contains("30-row minimum"), "{}", err.message);
    assert_eq!(app.ws().panes.len(), panes);
    split_sized(&mut app, h, None, None).expect("default 5 still splits");
    app.shutdown();
}

#[test]
fn per_call_min_pane_size_below_hard_floor_is_refused_before_mutation() {
    let mut app = app_after_one_vertical_split();
    let panes = app.ws().panes.len();
    let v = ipc::Direction::Vertical;
    for (w, h) in [(Some(4), None), (None, Some(1)), (Some(0), Some(0))] {
        let err = split_sized(&mut app, v, w, h).expect_err("below floor must fail");
        assert_eq!(
            err.code,
            Some(ipc::err_code::MIN_PANE_SIZE_INVALID),
            "{w:?}/{h:?}"
        );
    }
    assert_eq!(app.ws().panes.len(), panes, "no pane created");
    // Exactly at the floor is accepted.
    split_sized(
        &mut app,
        v,
        Some(ipc::MIN_PANE_WIDTH_FLOOR),
        Some(ipc::MIN_PANE_HEIGHT_FLOOR),
    )
    .expect("floor values are legal");
    app.shutdown();
}

#[test]
fn set_min_pane_size_clamps_zero_to_one() {
    // `--min-pane-width 0` would make `rect.width / 2 < 0` always
    // false and let splits succeed on 1-col panes. The setter
    // must floor the value at 1.
    let mut app = App::new(40, 80).expect("App::new");
    app.set_min_pane_size(0, 0);
    assert_eq!(app.min_pane_width, 1);
    assert_eq!(app.min_pane_height, 1);
    app.shutdown();
}

#[test]
fn handle_new_tab_emits_pane_started_with_attached_name_and_role() {
    // Same race as handle_split, but for handle_new_tab: metadata
    // must be attached before emitting PaneStarted.
    let mut app = App::new(40, 80).expect("App::new");
    let (_sub_id, rx) = app.event_bus.subscribe();

    let id = app
        .handle_new_tab(
            None,
            Some("tab-pane".into()),
            Some("tab label".into()),
            Some("tab-role".into()),
            None,
        )
        .expect("new tab succeeds");

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
    let (name, role) = observed.expect("PaneStarted for new tab's pane");
    assert_eq!(name.as_deref(), Some("tab-pane"));
    assert_eq!(role.as_deref(), Some("tab-role"));

    app.shutdown();
}
