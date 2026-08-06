use super::super::*;

#[test]
fn handle_set_pane_identity_sets_name_and_role() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;

    let info = app
        .handle_set_pane_identity(
            &ipc::PaneRef::Focused,
            Some(Some("secretary".into())),
            Some(Some("leader".into())),
            None,
        )
        .expect("set identity succeeds");
    assert_eq!(info.id, pane_id);
    assert_eq!(info.name.as_deref(), Some("secretary"));
    assert_eq!(info.role.as_deref(), Some("leader"));
    assert_eq!(app.ws().pane_names.get("secretary").copied(), Some(pane_id));
    assert_eq!(
        app.ws()
            .panes
            .get(&pane_id)
            .and_then(|p| p.role.clone())
            .as_deref(),
        Some("leader")
    );
    app.shutdown();
}

#[test]
fn handle_set_pane_identity_null_clears_existing_value() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    app.ws_mut().pane_names.insert("old".into(), pane_id);
    if let Some(pane) = app.ws_mut().panes.get_mut(&pane_id) {
        pane.role = Some("old-role".into());
    }

    let info = app
        .handle_set_pane_identity(&ipc::PaneRef::Focused, Some(None), Some(None), None)
        .expect("clear succeeds");
    assert!(info.name.is_none());
    assert!(info.role.is_none());
    assert!(!app.ws().pane_names.contains_key("old"));
    assert!(app.ws().panes.get(&pane_id).unwrap().role.is_none());
    app.shutdown();
}

#[test]
fn handle_set_pane_identity_keep_leaves_values_untouched() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    app.ws_mut().pane_names.insert("keeper".into(), pane_id);
    if let Some(pane) = app.ws_mut().panes.get_mut(&pane_id) {
        pane.role = Some("keeper-role".into());
    }

    // Both fields None → no-op call errors out? No — the handler
    // allows it (MCP layer guards against accidental no-op). Here
    // we exercise "update role only, keep name".
    let info = app
        .handle_set_pane_identity(
            &ipc::PaneRef::Focused,
            None,
            Some(Some("updated".into())),
            None,
        )
        .expect("role-only update");
    assert_eq!(info.name.as_deref(), Some("keeper"));
    assert_eq!(info.role.as_deref(), Some("updated"));
    app.shutdown();
}

#[test]
fn handle_set_pane_identity_rejects_name_collision() {
    // Split so we have two panes, name them distinctly, then try
    // to rename pane B to pane A's name. Must refuse with
    // NAME_IN_USE and leave both existing mappings intact.
    let mut app = App::new(40, 80).expect("App::new");
    let a_id = app.ws().focused_pane_id;
    app.ws_mut().pane_names.insert("alpha".into(), a_id);
    let b_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            Some("beta".into()),
            None,
            None,
            None,
            None,
        )
        .expect("split");

    let err = app
        .handle_set_pane_identity(
            &ipc::PaneRef::Id(b_id),
            Some(Some("alpha".into())),
            None,
            None,
        )
        .expect_err("colliding rename must fail");
    assert_eq!(err.code, Some(ipc::err_code::NAME_IN_USE));
    // Pre-collision state preserved.
    assert_eq!(app.ws().pane_names.get("alpha").copied(), Some(a_id));
    assert_eq!(app.ws().pane_names.get("beta").copied(), Some(b_id));
    app.shutdown();
}

#[test]
fn handle_set_pane_identity_idempotent_on_self_name() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    app.ws_mut().pane_names.insert("keeper".into(), pane_id);

    let info = app
        .handle_set_pane_identity(
            &ipc::PaneRef::Focused,
            Some(Some("keeper".into())),
            None,
            None,
        )
        .expect("self-name must not collide");
    assert_eq!(info.name.as_deref(), Some("keeper"));
    assert_eq!(app.ws().pane_names.get("keeper").copied(), Some(pane_id));
    app.shutdown();
}

#[test]
fn handle_set_pane_identity_rejects_all_digit_name() {
    let mut app = App::new(40, 80).expect("App::new");
    let err = app
        .handle_set_pane_identity(&ipc::PaneRef::Focused, Some(Some("123".into())), None, None)
        .expect_err("all-digit name must fail");
    assert_eq!(err.code, Some(ipc::err_code::NAME_INVALID));
    app.shutdown();
}

#[test]
fn handle_set_pane_identity_rejects_invalid_characters() {
    let mut app = App::new(40, 80).expect("App::new");
    let err = app
        .handle_set_pane_identity(
            &ipc::PaneRef::Focused,
            Some(Some("has space".into())),
            None,
            None,
        )
        .expect_err("space in name must fail");
    assert_eq!(err.code, Some(ipc::err_code::NAME_INVALID));
    app.shutdown();
}

/// BREAKING in v2.0.0: `split` accepted pane names verbatim, so a name
/// carrying `\r` reached the Codex peer nudge, which types it into
/// another pane's PTY and presses Enter. It now goes through the same
/// `validate_pane_name` as `set_pane_identity` / `spawn_tab`.
#[test]
fn handle_split_now_validates_the_pane_name() {
    let mut app = App::new(40, 80).expect("App::new");
    let panes_before = app.ws().panes.len();
    for bad in ["worker\rrm -rf ~", "has space", "123", "wörker"] {
        let err = app
            .handle_split(
                &ipc::PaneRef::Focused,
                ipc::Direction::Horizontal,
                None,
                Some(bad.into()),
                None,
                None,
                None,
                None,
            )
            .expect_err("invalid pane name must fail");
        assert_eq!(err.code, Some(ipc::err_code::NAME_INVALID), "name={bad:?}");
    }
    assert_eq!(
        app.ws().panes.len(),
        panes_before,
        "a rejected name must not leave a pane behind"
    );
    app.shutdown();
}

/// `role` keeps its documented free-form contract — only control
/// characters are refused, so the split path does not start rejecting
/// labels like `code reviewer`.
#[test]
fn handle_split_refuses_control_chars_in_role_but_keeps_it_free_form() {
    let mut app = App::new(40, 80).expect("App::new");
    let err = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Horizontal,
            None,
            None,
            Some("worker\n- id=99".into()),
            None,
            None,
            None,
        )
        .expect_err("control character in role must fail");
    assert_eq!(err.code, Some(ipc::err_code::NAME_INVALID));

    let new_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Horizontal,
            None,
            None,
            Some("コード レビュー".into()),
            None,
            None,
            None,
        )
        .expect("free-form role stays legal");
    assert_eq!(
        app.ws().panes.get(&new_id).and_then(|p| p.role.as_deref()),
        Some("コード レビュー")
    );
    app.shutdown();
}

#[test]
fn handle_set_pane_identity_removes_all_stale_name_entries() {
    // Defense-in-depth: even though normal flow keeps at most one
    // pane_names entry per pane, planting two manually must not
    // leave the stale one behind after a rename or clear. The
    // filter loop walks every key that maps to this pane; this
    // regression guard pins that behavior.
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    app.ws_mut().pane_names.insert("one".into(), pane_id);
    app.ws_mut().pane_names.insert("two".into(), pane_id);

    // Rename to a fresh name — both stale entries must vanish.
    let info = app
        .handle_set_pane_identity(
            &ipc::PaneRef::Focused,
            Some(Some("fresh".into())),
            None,
            None,
        )
        .expect("rename succeeds");
    assert_eq!(info.name.as_deref(), Some("fresh"));
    assert!(!app.ws().pane_names.contains_key("one"));
    assert!(!app.ws().pane_names.contains_key("two"));
    assert_eq!(app.ws().pane_names.get("fresh").copied(), Some(pane_id));

    // Plant two again and clear — both must be removed.
    app.ws_mut().pane_names.insert("alt".into(), pane_id);
    let info = app
        .handle_set_pane_identity(&ipc::PaneRef::Focused, Some(None), None, None)
        .expect("clear succeeds");
    assert!(info.name.is_none());
    assert!(!app.ws().pane_names.contains_key("fresh"));
    assert!(!app.ws().pane_names.contains_key("alt"));
    app.shutdown();
}

#[test]
fn handle_set_pane_identity_rejects_unknown_pane() {
    let mut app = App::new(40, 80).expect("App::new");
    let err = app
        .handle_set_pane_identity(
            &ipc::PaneRef::Id(9999),
            Some(Some("anything".into())),
            None,
            None,
        )
        .expect_err("unknown pane must fail");
    assert_eq!(err.code, Some(ipc::err_code::PANE_NOT_FOUND));
    app.shutdown();
}

#[test]
fn handle_split_with_cwd_spawns_pane_in_requested_dir() {
    // Split with an explicit absolute cwd and confirm the pane's
    // stored cwd reflects it (canonicalized). Validates the full
    // wire: AppCommand::Split cwd → handle_split → resolve →
    // split_focused_pane(cwd_override) → Pane::new_with_cwd.
    let tmp = std::env::temp_dir();
    let canon = std::fs::canonicalize(&tmp).unwrap_or(tmp);

    let mut app = App::new(40, 80).expect("App::new");
    let new_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            Some(canon.to_string_lossy().to_string()),
            None,
            None,
        )
        .expect("split with cwd succeeds");

    let pane_cwd = app
        .ws()
        .panes
        .get(&new_id)
        .map(|p| p.cwd.clone())
        .expect("new pane exists");
    // std::fs::canonicalize the caller-supplied path and compare;
    // both the handler and the test normalize the same way so the
    // pane cwd must match exactly.
    assert_eq!(
        std::fs::canonicalize(&pane_cwd).unwrap_or(pane_cwd.clone()),
        canon,
        "pane cwd must be the cwd passed to handle_split"
    );
    app.shutdown();
}

#[test]
fn handle_split_with_invalid_cwd_refuses_before_mutation() {
    // A missing cwd must return CWD_INVALID and must NOT allocate
    // a new pane / register a name.
    let mut app = App::new(40, 80).expect("App::new");
    let pane_count_before = app.ws().layout.pane_count();

    let err = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            Some("should-not-land".into()),
            None,
            Some("/this/path/definitely/does/not/exist/renga-test".into()),
            None,
            None,
        )
        .expect_err("invalid cwd must be refused");
    assert_eq!(err.code, Some(ipc::err_code::CWD_INVALID));
    assert_eq!(
        app.ws().layout.pane_count(),
        pane_count_before,
        "refused split must not grow the layout"
    );
    assert!(
        !app.ws().pane_names.contains_key("should-not-land"),
        "refused split must not register its requested name"
    );
    app.shutdown();
}

#[test]
fn handle_new_tab_with_invalid_cwd_refuses_before_mutation() {
    let mut app = App::new(40, 80).expect("App::new");
    let tab_count_before = app.workspaces.len();

    let err = app
        .handle_new_tab(
            None,
            Some("should-not-land".into()),
            None,
            None,
            Some("/this/path/definitely/does/not/exist/renga-test".into()),
        )
        .expect_err("invalid cwd must be refused");
    assert_eq!(err.code, Some(ipc::err_code::CWD_INVALID));
    assert_eq!(
        app.workspaces.len(),
        tab_count_before,
        "refused new_tab must not grow the tab list"
    );
    app.shutdown();
}

#[test]
fn handle_split_relative_cwd_resolves_against_target_pane_cwd() {
    // Plant a subdir under the target pane's cwd and split with a
    // relative cwd pointing at it. The resolved pane cwd must
    // equal the canonicalized subdir.
    let tmp = std::env::temp_dir().join("renga-cwd-test-target");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("mkdir tmp");
    let sub = tmp.join("child");
    std::fs::create_dir_all(&sub).expect("mkdir child");

    let mut app = App::new(40, 80).expect("App::new");
    // Rewrite the focused pane's cwd to the tmp dir so the
    // relative resolution has a known base. This mirrors how the
    // shell would report its cwd via OSC 7 at runtime.
    let focused = app.ws().focused_pane_id;
    if let Some(pane) = app.ws_mut().panes.get_mut(&focused) {
        pane.cwd = tmp.clone();
    }

    let new_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            Some("child".into()),
            None,
            None,
        )
        .expect("relative cwd resolves");

    let pane_cwd = app.ws().panes.get(&new_id).map(|p| p.cwd.clone()).unwrap();
    let expected = std::fs::canonicalize(&sub).unwrap_or(sub.clone());
    let got = std::fs::canonicalize(&pane_cwd).unwrap_or(pane_cwd);
    assert_eq!(got, expected);
    app.shutdown();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn resolve_optional_cwd_accepts_none_and_empty() {
    let base = std::env::temp_dir();
    assert!(resolve_optional_cwd(None, &base).unwrap().is_none());
    assert!(resolve_optional_cwd(Some(""), &base).unwrap().is_none());
    assert!(resolve_optional_cwd(Some("   "), &base).unwrap().is_none());
}

#[cfg(windows)]
#[test]
fn strip_verbatim_prefix_normalizes_windows_paths() {
    // `\\?\C:\foo` → `C:\foo`
    let drive = strip_verbatim_prefix(PathBuf::from(r"\\?\C:\foo\bar"));
    assert_eq!(drive, PathBuf::from(r"C:\foo\bar"));
    // `\\?\UNC\server\share\...` → `\\server\share\...`
    let unc = strip_verbatim_prefix(PathBuf::from(r"\\?\UNC\srv\share\x"));
    assert_eq!(unc, PathBuf::from(r"\\srv\share\x"));
    // Non-verbatim paths pass through unchanged.
    let plain = strip_verbatim_prefix(PathBuf::from(r"C:\already\plain"));
    assert_eq!(plain, PathBuf::from(r"C:\already\plain"));
}

#[cfg(windows)]
#[test]
fn resolve_optional_cwd_strips_verbatim_prefix_on_windows() {
    // Use the current exe's parent as a definitely-existing dir.
    let base = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|x| x.to_path_buf()))
        .unwrap_or_else(std::env::temp_dir);
    let resolved = resolve_optional_cwd(Some(&base.to_string_lossy()), &base)
        .expect("resolve should succeed")
        .expect("cwd should be Some");
    let s = resolved.to_string_lossy();
    assert!(
        !s.starts_with(r"\\?\"),
        "resolved cwd must not expose verbatim prefix, got {s}"
    );
}

#[test]
fn resolve_optional_cwd_rejects_nonexistent_path() {
    let base = std::env::temp_dir();
    let err = resolve_optional_cwd(Some("/definitely/not/a/real/path/renga-xyz-zzz"), &base)
        .expect_err("missing path must fail");
    assert_eq!(err.code, Some(ipc::err_code::CWD_INVALID));
}

#[test]
fn list_includes_pane_cwd() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;

    let (reply_tx, reply_rx) = oneshot::channel();
    app.handle_app_command(AppCommand::List {
        from_pane: None,
        reply: reply_tx,
    });
    let infos = reply_rx.recv().expect("list reply").expect("list ok");
    let info = infos.iter().find(|p| p.id == pane_id).unwrap();
    assert!(
        info.cwd.is_some(),
        "list must surface the pane's resolved cwd"
    );
}

#[test]
fn list_command_includes_rect_from_last_pane_rects() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    app.ws_mut().last_pane_rects = vec![(
        pane_id,
        Rect {
            x: 2,
            y: 3,
            width: 50,
            height: 20,
        },
    )];

    let (reply_tx, reply_rx) = oneshot::channel();
    app.handle_app_command(AppCommand::List {
        from_pane: None,
        reply: reply_tx,
    });
    let infos = reply_rx.recv().expect("list reply").expect("list ok");

    assert_eq!(infos.len(), 1);
    let info = &infos[0];
    assert_eq!(info.id, pane_id);
    assert!(info.focused);
    assert_eq!(info.x, 2);
    assert_eq!(info.y, 3);
    assert_eq!(info.width, 50);
    assert_eq!(info.height, 20);
}

#[test]
fn relayout_panes_caches_rect_origin_accounting_for_sidebar() {
    // Before #80, relayout_panes() used Rect::new(0, tab_h, ...)
    // because only width/height mattered for PTY sizing. Now that
    // `renga list` also exposes x/y from the same cache, the
    // origin must match ui::render_main_area's chunk order (tree
    // on the left, preview on the swapped side) — otherwise a
    // List call between a layout change and the next draw would
    // return x=0 for a pane that's actually rendered past the
    // file-tree sidebar.
    let mut app = App::new(40, 120).expect("App::new");
    app.last_term_size = (120, 40);
    // Workspace::new sets file_tree_visible = true; set it
    // explicitly here to make the test's precondition obvious.
    app.ws_mut().file_tree_visible = true;
    let tree_w = app.file_tree_width;
    assert!(tree_w > 0, "file tree width should be non-zero");

    app.relayout_panes();

    let pane_id = app.ws().focused_pane_id;
    let rect = app
        .ws()
        .last_pane_rects
        .iter()
        .find(|(id, _)| *id == pane_id)
        .map(|(_, r)| *r)
        .expect("relayout should populate rect for focused pane");
    assert_eq!(
        rect.x, tree_w,
        "pane origin must sit past the file-tree sidebar"
    );
    assert_eq!(rect.y, 1, "pane origin must sit below the tab strip");
}

#[test]
fn relayout_panes_rect_origin_follows_layout_swapped_preview() {
    // With `layout_swapped = true` the chunk order is
    // [tree] [preview] [panes] [...]. Pane origin must therefore
    // include the preview width too. With `layout_swapped = false`
    // preview sits to the right of the panes and does not offset
    // the origin.
    use std::path::PathBuf;

    for swapped in [true, false] {
        let mut app = App::new(40, 160).expect("App::new");
        app.last_term_size = (160, 40);
        app.ws_mut().file_tree_visible = true;
        // Activate preview without touching disk: is_active() just
        // checks Preview::file_path.is_some().
        app.ws_mut().preview.file_path = Some(PathBuf::from("dummy"));
        app.layout_swapped = swapped;

        let tree_w = app.file_tree_width;
        let preview_w = app.preview_width;

        app.relayout_panes();

        let pane_id = app.ws().focused_pane_id;
        let rect = app
            .ws()
            .last_pane_rects
            .iter()
            .find(|(id, _)| *id == pane_id)
            .map(|(_, r)| *r)
            .expect("relayout should populate rect for focused pane");
        let expected_x = tree_w + if swapped { preview_w } else { 0 };
        assert_eq!(
            rect.x, expected_x,
            "swapped={swapped}: pane x should be {expected_x} (tree_w={tree_w}, preview_w={preview_w})"
        );
    }
}

#[test]
fn list_command_ignores_stale_rect_entries_for_removed_panes() {
    // Entries in last_pane_rects for pane ids that are no longer
    // in the layout must not leak into the response. Output is
    // keyed off layout.collect_pane_ids(), so a stale rect for a
    // nonexistent id should simply be dropped.
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    let ghost_id = pane_id.wrapping_add(9999);
    app.ws_mut().last_pane_rects = vec![
        (
            pane_id,
            Rect {
                x: 1,
                y: 2,
                width: 10,
                height: 5,
            },
        ),
        (
            ghost_id,
            Rect {
                x: 100,
                y: 100,
                width: 100,
                height: 100,
            },
        ),
    ];

    let (reply_tx, reply_rx) = oneshot::channel();
    app.handle_app_command(AppCommand::List {
        from_pane: None,
        reply: reply_tx,
    });
    let infos = reply_rx.recv().expect("list reply").expect("list ok");

    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].id, pane_id);
    assert_eq!(infos[0].width, 10);
    assert!(
        infos.iter().all(|i| i.id != ghost_id),
        "stale rect for removed pane leaked into list"
    );
}

#[test]
fn list_command_zero_rect_when_pane_not_in_last_pane_rects() {
    let mut app = App::new(40, 80).expect("App::new");
    app.ws_mut().last_pane_rects.clear();

    let (reply_tx, reply_rx) = oneshot::channel();
    app.handle_app_command(AppCommand::List {
        from_pane: None,
        reply: reply_tx,
    });
    let infos = reply_rx.recv().expect("list reply").expect("list ok");

    assert_eq!(infos.len(), 1);
    let info = &infos[0];
    assert_eq!(info.x, 0);
    assert_eq!(info.y, 0);
    assert_eq!(info.width, 0);
    assert_eq!(info.height, 0);
}

#[test]
fn app_command_channel_sends_and_receives() {
    // Smoke-test that the AppCommand channel round-trips without
    // panicking. We can't exercise handle_* without spawning PTYs,
    // but confirming the types fit together catches breakage.
    let (tx, rx) = mpsc::channel::<AppCommand>();
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(AppCommand::List {
        from_pane: None,
        reply: reply_tx,
    })
    .unwrap();
    match rx.try_recv() {
        Ok(AppCommand::List { reply, .. }) => {
            reply.send(Ok(Vec::new())).unwrap();
            let list = reply_rx.recv().unwrap().expect("list ok");
            assert!(list.is_empty());
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[test]
fn handle_set_summary_sets_and_reads_back_via_list() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;

    let info = app
        .handle_set_summary(pane_id, "drafting design doc".into())
        .expect("set summary succeeds");
    assert_eq!(info.id, pane_id);
    assert_eq!(info.summary.as_deref(), Some("drafting design doc"));
    assert_eq!(
        app.ws()
            .panes
            .get(&pane_id)
            .and_then(|p| p.summary.clone())
            .as_deref(),
        Some("drafting design doc")
    );

    // List response must surface the summary so list_panes / list_peers
    // round-trips it to peers.
    let (reply_tx, reply_rx) = oneshot::channel();
    app.handle_app_command(AppCommand::List {
        from_pane: None,
        reply: reply_tx,
    });
    let infos = reply_rx.recv().expect("list reply").expect("list ok");
    let entry = infos
        .iter()
        .find(|p| p.id == pane_id)
        .expect("pane in list");
    assert_eq!(entry.summary.as_deref(), Some("drafting design doc"));
    app.shutdown();
}

#[test]
fn handle_set_summary_overwrites_previous_value() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;

    app.handle_set_summary(pane_id, "first".into()).unwrap();
    let info = app
        .handle_set_summary(pane_id, "second".into())
        .expect("overwrite succeeds");
    assert_eq!(info.summary.as_deref(), Some("second"));
    app.shutdown();
}

#[test]
fn handle_set_summary_empty_clears() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;

    app.handle_set_summary(pane_id, "before".into()).unwrap();
    let info = app
        .handle_set_summary(pane_id, String::new())
        .expect("clear succeeds");
    assert!(info.summary.is_none());
    assert!(app.ws().panes.get(&pane_id).unwrap().summary.is_none());
    app.shutdown();
}

#[test]
fn handle_set_summary_rejects_oversized_input() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;

    // 257 ASCII chars (one over the cap).
    let too_long = "x".repeat(257);
    let err = app
        .handle_set_summary(pane_id, too_long)
        .expect_err("oversized summary must be rejected");
    assert_eq!(err.code, Some(ipc::err_code::SUMMARY_TOO_LONG));
    // Pre-call state is preserved (no partial write).
    assert!(app.ws().panes.get(&pane_id).unwrap().summary.is_none());
    app.shutdown();
}

#[test]
fn handle_set_summary_accepts_exactly_max_length() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;

    // 256 chars is on the boundary — must succeed.
    let at_cap = "x".repeat(256);
    let info = app
        .handle_set_summary(pane_id, at_cap.clone())
        .expect("exactly-cap summary must be accepted");
    assert_eq!(info.summary.as_deref(), Some(at_cap.as_str()));
    app.shutdown();
}

#[test]
fn handle_set_summary_caps_on_chars_not_bytes() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;

    // 256 multi-byte Japanese chars is well over 256 bytes (UTF-8 = 3
    // bytes per char). Must still be accepted because the cap is on
    // chars(), not byte length.
    let multibyte = "あ".repeat(256);
    assert!(multibyte.len() > 256, "precondition: more bytes than chars");
    let info = app
        .handle_set_summary(pane_id, multibyte.clone())
        .expect("256 multi-byte chars must be accepted");
    assert_eq!(info.summary.as_deref(), Some(multibyte.as_str()));
    app.shutdown();
}

#[test]
fn handle_set_summary_repeated_calls_are_stable() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;

    // Hammer the handler back-to-back to check that nothing leaks
    // (mutation order, lock state, etc.).
    for i in 0..5 {
        let info = app
            .handle_set_summary(pane_id, format!("iter-{i}"))
            .expect("set succeeds");
        assert_eq!(info.summary.as_deref(), Some(format!("iter-{i}").as_str()));
    }
    app.shutdown();
}

#[test]
fn handle_set_summary_unknown_pane_is_rejected() {
    let mut app = App::new(40, 80).expect("App::new");
    let err = app
        .handle_set_summary(9999, "x".into())
        .expect_err("unknown pane must fail");
    assert_eq!(err.code, Some(ipc::err_code::PANE_NOT_FOUND));
    app.shutdown();
}

#[test]
fn handle_peer_list_surfaces_summary() {
    // Two panes; only the second has a summary set. handle_peer_list
    // (called from the *first* pane's perspective) must include the
    // second pane's summary on its PeerInfo entry.
    let mut app = App::new(40, 80).expect("App::new");
    let a_id = app.ws().focused_pane_id;
    let b_id = app
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
        .expect("split");
    app.handle_set_summary(b_id, "running tests".into())
        .expect("set on b");

    let peers = app.handle_peer_list(a_id).expect("peer list from a");
    let b_entry = peers.iter().find(|p| p.id == b_id).expect("b in peers");
    assert_eq!(b_entry.summary.as_deref(), Some("running tests"));
    // a is not in its own peer list, but spot-check that no spurious
    // summary appears on the empty side either.
    assert!(peers.iter().all(|p| p.id != a_id));
    app.shutdown();
}

// ── handle_inspect: scrollback continuation (#278) ───────────

/// Feed `count` numbered lines (`SBTEST-0000`, `SBTEST-0001`, …) into
/// the focused pane's vt100 parser in a single `process()` call so
/// the block stays contiguous regardless of PTY reader timing.
fn seed_numbered_lines(app: &mut App, count: usize) {
    let mut blob = String::with_capacity(count * 14);
    for i in 0..count {
        blob.push_str(&format!("SBTEST-{i:04}\r\n"));
    }
    let pane_id = app.ws().focused_pane_id;
    let pane = app.ws().panes.get(&pane_id).expect("focused pane exists");
    let mut parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
    parser.process(blob.as_bytes());
}

fn focused_grid_height(app: &App) -> usize {
    let pane_id = app.ws().focused_pane_id;
    let pane = app.ws().panes.get(&pane_id).expect("focused pane exists");
    let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
    parser.screen().size().0 as usize
}

fn inspect_texts(payload: &serde_json::Value) -> Vec<String> {
    payload["lines"]
        .as_array()
        .expect("lines array")
        .iter()
        .map(|l| l["text"].as_str().expect("text").to_string())
        .collect()
}

#[test]
fn handle_inspect_lines_beyond_height_reaches_scrollback() {
    let mut app = App::new(40, 80).expect("App::new");
    let height = focused_grid_height(&app);
    seed_numbered_lines(&mut app, height + 40);

    let want = height + 20;
    let payload = app
        .handle_inspect(&ipc::PaneRef::Focused, Some(want), false, None)
        .expect("inspect succeeds");

    assert_eq!(
        payload["screen"]["line_count"].as_u64(),
        Some(want as u64),
        "history exists, so the full request must be honored"
    );
    assert_eq!(
        payload["screen"]["line_start"].as_i64(),
        Some(-20),
        "20 of the lines must come from scrollback (negative rows)"
    );

    // The seeded lines must appear in order with consecutive numbering.
    // Shell prompt noise may surround the block but must not
    // interleave it. Threshold is the 20 scrollback lines only: the
    // history side is immune to late repaints (conpty clear/redraw
    // can overwrite the visible grid after seeding, but scrolled-off
    // lines are frozen), so this stays deterministic on all CI OSes.
    let nums: Vec<u64> = inspect_texts(&payload)
        .iter()
        .filter_map(|t| t.strip_prefix("SBTEST-").and_then(|n| n.parse().ok()))
        .collect();
    assert!(
        nums.len() >= 20,
        "expected at least the 20 scrollback-side seeded lines, got {}",
        nums.len()
    );
    assert!(
        nums.windows(2).all(|w| w[1] == w[0] + 1),
        "seeded lines must be contiguous and ordered: {nums:?}"
    );
    app.shutdown();
}

#[test]
fn handle_inspect_small_n_stays_on_screen_grid() {
    let mut app = App::new(40, 80).expect("App::new");
    let height = focused_grid_height(&app);
    seed_numbered_lines(&mut app, height + 40);

    let payload = app
        .handle_inspect(&ipc::PaneRef::Focused, Some(3), false, None)
        .expect("inspect succeeds");

    assert_eq!(payload["screen"]["line_count"].as_u64(), Some(3));
    assert_eq!(
        payload["screen"]["line_start"].as_i64(),
        Some((height - 3) as i64),
        "small N keeps the pre-#278 bottom-of-grid semantics"
    );
    let rows: Vec<i64> = payload["lines"]
        .as_array()
        .expect("lines array")
        .iter()
        .map(|l| l["row"].as_i64().expect("row"))
        .collect();
    assert!(
        rows.iter().all(|r| *r >= 0),
        "no scrollback rows for small N: {rows:?}"
    );
    app.shutdown();
}

#[test]
fn handle_inspect_is_pinned_to_live_tail_and_preserves_scroll() {
    let mut app = App::new(40, 80).expect("App::new");
    let height = focused_grid_height(&app);
    seed_numbered_lines(&mut app, height + 30);

    let pane_id = app.ws().focused_pane_id;
    app.ws().panes.get(&pane_id).expect("pane").scroll_up(10);

    let payload = app
        .handle_inspect(&ipc::PaneRef::Focused, None, false, None)
        .expect("inspect succeeds");

    // The last seeded line sits at the live bottom. A view-anchored
    // read (pre-#278 behavior) scrolled up by 10 would not contain it.
    let last = format!("SBTEST-{:04}", height + 30 - 1);
    assert!(
        inspect_texts(&payload).iter().any(|t| t == &last),
        "inspect must read the live tail even while the pane is scrolled"
    );

    // ≥ 10, not == 10: vt100 auto-increments the offset whenever new
    // lines enter scrollback while the view is scrolled back, and the
    // live shell can still emit output between scroll_up and here. A
    // restore bug would reset the offset to 0, which this still
    // catches (lines=None never walks history, so no walk residue can
    // fake a non-zero offset).
    let pane = app.ws().panes.get(&pane_id).expect("pane");
    let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
    assert!(
        parser.screen().scrollback() >= 10,
        "the user's scroll position must survive the inspect, got {}",
        parser.screen().scrollback()
    );
    drop(parser);
    app.shutdown();
}

#[test]
fn handle_inspect_clamps_at_inspect_max_lines() {
    let mut app = App::new(40, 80).expect("App::new");
    let height = focused_grid_height(&app);
    seed_numbered_lines(&mut app, ipc::INSPECT_MAX_LINES + 200);

    let payload = app
        .handle_inspect(
            &ipc::PaneRef::Focused,
            Some(ipc::INSPECT_MAX_LINES + 3000),
            false,
            None,
        )
        .expect("inspect succeeds");

    assert_eq!(
        payload["screen"]["line_count"].as_u64(),
        Some(ipc::INSPECT_MAX_LINES as u64),
        "requests beyond the cap are clamped to INSPECT_MAX_LINES"
    );
    assert_eq!(
        payload["screen"]["line_start"].as_i64(),
        Some(-((ipc::INSPECT_MAX_LINES - height) as i64)),
    );
    app.shutdown();
}
