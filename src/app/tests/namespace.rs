//! Issue #390 — a pane namespace widens name resolution beyond one
//! tab, and the CLI can create / fill a background tab without
//! switching the visible one.
//!
//! Shape: workspace 0 holds the "secretary", workspace 1 is the tab
//! the human is watching (active). Background tabs are appended after.

use super::super::*;

/// Two tabs, tab 1 visible. Pane in tab 0 becomes `secretary` in
/// namespace `org`. Returns `(app, secretary, human_pane)`.
fn org_with_secretary() -> (App, usize, usize) {
    let mut app = App::new(40, 120).expect("App::new");
    let secretary = app.ws().focused_pane_id;
    app.new_tab().expect("new_tab");
    let human = app.ws().focused_pane_id;
    app.handle_set_pane_identity_ns(
        &ipc::PaneRef::Id(secretary),
        Some(Some("secretary".into())),
        None,
        Some(Some("org".into())),
        Some(secretary),
    )
    .expect("secretary joins org");
    (app, secretary, human)
}

fn spawn_bg(
    app: &mut App,
    name: &str,
    from_pane: Option<usize>,
    namespace: Option<&str>,
) -> std::result::Result<(usize, usize), ipc::CodedError> {
    app.handle_spawn_tab_ns(
        None,
        Some(name.into()),
        None,
        None,
        None,
        from_pane,
        namespace.map(str::to_string),
    )
}

/// What `send_message` resolves for `sender` (Issue #390 fallback).
fn peer(app: &App, sender: usize, target: &ipc::PaneRef) -> Result<(usize, usize), ()> {
    let ws = app.workspace_index_of_pane(sender).ok_or(())?;
    app.resolve_peer_target(sender, ws, target).ok_or(())
}

fn pane_ns(app: &App, id: usize) -> Option<String> {
    app.workspaces
        .iter()
        .find_map(|ws| ws.panes.get(&id))
        .and_then(|p| p.namespace.clone())
}

#[test]
fn background_pane_spawned_by_a_namespaced_caller_reaches_it_by_name() {
    let (mut app, secretary, _human) = org_with_secretary();
    let (watcher, ws) = spawn_bg(&mut app, "ci-watch", Some(secretary), None).expect("spawn");
    assert_eq!(app.active_tab, 1, "visible tab unchanged");
    assert_eq!(pane_ns(&app, watcher).as_deref(), Some("org"), "inherited");

    let target = ipc::PaneRef::Name("secretary".into());
    assert_eq!(peer(&app, watcher, &target).unwrap(), (0, secretary));
    app.handle_peer_send(watcher, &target, "hello".into())
        .expect("send by name across tabs");
    // Pane-control tools keep the strict #288 rule: a name never leaves
    // the caller's tab, namespace or not.
    let err = app
        .resolve_request_target(Some(watcher), &target)
        .unwrap_err();
    assert_eq!(err.code, Some(ipc::err_code::PANE_NOT_FOUND));
    let err = app.handle_close(&target, Some(watcher)).unwrap_err();
    assert_eq!(err.code, Some(ipc::err_code::PANE_NOT_FOUND));
    // And the other way round.
    assert_eq!(
        peer(&app, secretary, &ipc::PaneRef::Name("ci-watch".into())).unwrap(),
        (ws, watcher)
    );
    app.shutdown();
}

#[test]
fn panes_without_a_namespace_keep_per_tab_names() {
    let (mut app, secretary, human) = org_with_secretary();
    let target = ipc::PaneRef::Name("secretary".into());
    // A plain caller never leaves its tab, even toward a namespaced pane.
    let err = app
        .handle_peer_send(human, &target, "x".into())
        .unwrap_err();
    assert_eq!(err.code, Some(ipc::err_code::PANE_NOT_FOUND));
    // An opted-out spawn from the secretary is plain too.
    let (plain, _) = spawn_bg(&mut app, "plain", Some(secretary), Some("")).expect("spawn");
    assert_eq!(pane_ns(&app, plain), None);
    assert!(peer(&app, plain, &target).is_err());
    // And a namespaced caller cannot reach an un-namespaced pane in
    // another tab by name.
    assert!(peer(&app, secretary, &ipc::PaneRef::Name("plain".into())).is_err());
    app.shutdown();
}

#[test]
fn another_namespace_does_not_resolve() {
    let (mut app, _secretary, _human) = org_with_secretary();
    let (other, _) = spawn_bg(&mut app, "w", None, Some("other-org")).expect("spawn");
    assert!(peer(&app, other, &ipc::PaneRef::Name("secretary".into())).is_err());
    // The same name in another namespace is not a collision.
    spawn_bg(&mut app, "secretary", None, Some("other-org")).expect("same name, other ns");
    app.shutdown();
}

#[test]
fn own_tab_name_wins_over_the_namespace() {
    let (mut app, secretary, human) = org_with_secretary();
    // The human's tab has its own plain `secretary`; a namespaced pane
    // split into that tab resolves the local one, exactly as pre-#390.
    app.handle_set_pane_identity_ns(
        &ipc::PaneRef::Id(human),
        Some(Some("secretary".into())),
        None,
        None,
        None,
    )
    .expect("local plain secretary");
    let split = app
        .handle_split_sized(
            &ipc::PaneRef::Id(human),
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
            Some(secretary),
            Some(&ipc::TabSelector::Index(1)),
            (None, None),
            None,
        )
        .expect("split into tab 1");
    assert_eq!(pane_ns(&app, split).as_deref(), Some("org"));
    assert_eq!(
        peer(&app, split, &ipc::PaneRef::Name("secretary".into())).unwrap(),
        (1, human)
    );
    app.shutdown();
}

#[test]
fn a_name_is_unique_across_a_namespaces_tabs() {
    let (mut app, secretary, human) = org_with_secretary();
    let err = spawn_bg(&mut app, "secretary", Some(secretary), None).unwrap_err();
    assert_eq!(err.code, Some(ipc::err_code::NAME_IN_USE));
    assert_eq!(app.workspaces.len(), 2, "refused before creating a tab");

    // Joining the namespace with a name it already holds elsewhere.
    app.handle_set_pane_identity_ns(
        &ipc::PaneRef::Id(human),
        Some(Some("secretary".into())),
        None,
        None,
        None,
    )
    .expect("plain local name is fine");
    let err = app
        .handle_set_pane_identity_ns(
            &ipc::PaneRef::Id(human),
            None,
            None,
            Some(Some("org".into())),
            None,
        )
        .unwrap_err();
    assert_eq!(err.code, Some(ipc::err_code::NAME_IN_USE));
    assert_eq!(pane_ns(&app, human), None, "nothing applied");

    // Leaving is always allowed; `""` clears like null.
    app.handle_set_pane_identity_ns(
        &ipc::PaneRef::Id(secretary),
        None,
        None,
        Some(Some(String::new())),
        None,
    )
    .expect("leave");
    assert_eq!(pane_ns(&app, secretary), None);
    app.shutdown();
}

#[test]
fn an_invalid_namespace_is_refused() {
    let (mut app, secretary, _human) = org_with_secretary();
    let err = spawn_bg(&mut app, "w", Some(secretary), Some("a b")).unwrap_err();
    assert_eq!(err.code, Some(ipc::err_code::NAME_INVALID));
    app.shutdown();
}

/// What `renga new-tab --background --label watchers` followed by
/// `renga split --tab watchers` sends: no `from_pane`, and the visible
/// tab never changes.
#[test]
fn cli_can_build_a_labelled_background_tab_without_switching() {
    let (mut app, secretary, _human) = org_with_secretary();
    let (first, ws) = app
        .handle_spawn_tab_ns(
            None,
            Some("w1".into()),
            Some("watchers".into()),
            None,
            None,
            None,
            Some("org".into()),
        )
        .expect("background tab");
    assert_eq!(app.active_tab, 1);
    let second = app
        .handle_split_sized(
            &ipc::PaneRef::Focused,
            ipc::Direction::Horizontal,
            None,
            Some("w2".into()),
            None,
            None,
            None,
            Some(&ipc::TabSelector::Name("watchers".into())),
            (None, None),
            Some("org".into()),
        )
        .expect("split into the background tab");
    assert_eq!(app.active_tab, 1, "visible tab unchanged");
    assert!(app.workspaces[ws].panes.contains_key(&first));
    assert!(app.workspaces[ws].panes.contains_key(&second));
    assert_eq!(
        peer(&app, second, &ipc::PaneRef::Name("secretary".into()))
            .unwrap()
            .1,
        secretary
    );
    app.shutdown();
}
