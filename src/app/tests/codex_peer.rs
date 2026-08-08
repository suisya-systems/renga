use super::super::*;

fn seed_focused_pane_screen(app: &mut App, bytes: &[u8]) -> usize {
    let pane_id = app.ws().focused_pane_id;
    let pane = app
        .ws_mut()
        .panes
        .get_mut(&pane_id)
        .expect("focused pane exists");
    let mut parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
    parser.process(bytes);
    drop(parser);
    pane_id
}

#[test]
fn codex_peer_delivery_ready_accepts_ready_for_input_fallback() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = seed_focused_pane_screen(&mut app, b"\x1b[2J\x1b[Hready for input");
    let pane = app.ws().panes.get(&pane_id).expect("pane");

    assert!(App::codex_peer_delivery_ready(true, pane));

    app.shutdown();
}

#[test]
fn codex_peer_delivery_ready_rejects_queue_banner() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = seed_focused_pane_screen(&mut app, b"\x1b[2J\x1b[HTab to queue message");
    let pane = app.ws().panes.get(&pane_id).expect("pane");

    assert!(!App::codex_peer_delivery_ready(true, pane));

    app.shutdown();
}

#[test]
fn codex_peer_delivery_ready_rejects_busy_codex_prompt() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id =
        seed_focused_pane_screen(&mut app, b"\x1b[2J\x1b[H\xE2\x80\xBA typed draft\x1b[1;14H");
    let pane = app.ws().panes.get(&pane_id).expect("pane");

    assert!(!App::codex_peer_delivery_ready(true, pane));

    app.shutdown();
}

#[test]
fn codex_peer_delivery_ready_requires_codex_registration_or_process() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = seed_focused_pane_screen(&mut app, b"\x1b[2J\x1b[Hready for input");
    let pane = app.ws().panes.get(&pane_id).expect("pane");

    assert!(!App::codex_peer_delivery_ready(false, pane));

    app.shutdown();
}

#[test]
fn format_codex_peer_message_includes_sender_and_check_messages_guidance() {
    let formatted = format_codex_peer_message(&PendingCodexPeerMessage {
        from_pane: 7,
        from_name: Some("planner".to_string()),
        from_kind: Some(PeerClientKind::Claude),
    });
    assert!(!formatted.contains('\n'), "{formatted:?}");
    assert!(
        formatted.contains("Peer request from id=7 name=planner kind=claude."),
        "{formatted:?}"
    );
    assert!(
        formatted.contains("Run check_messages now."),
        "{formatted:?}"
    );
    assert!(
        formatted.contains("use send_message only when a reply or status update is needed."),
        "{formatted:?}"
    );
}

/// The nudge is typed into the target pane's PTY and followed by
/// Enter, so a `\r` in the sender's name would submit everything before
/// it as a prompt in another agent's composer — and an `\x1b` would
/// reach the terminal as a live escape sequence. Names registered
/// through `split` / `new_tab` were unvalidated before v2.0.0, and
/// #289 widened delivery to every tab, so this is the last line of
/// defense for a name that predates the input check.
#[test]
fn format_codex_peer_message_strips_control_characters_from_the_sender_name() {
    let formatted = format_codex_peer_message(&PendingCodexPeerMessage {
        from_pane: 7,
        from_name: Some("planner\r/quit\rrm -rf ~\u{1b}[2J".to_string()),
        from_kind: Some(PeerClientKind::Claude),
    });
    assert!(
        !formatted.contains('\r') && !formatted.contains('\n') && !formatted.contains('\u{1b}'),
        "no control character may survive into the PTY payload: {formatted:?}"
    );
    assert!(
        formatted.contains("name=planner/quitrm -rf ~[2J"),
        "the printable remainder is kept so the sender is still identifiable: {formatted:?}"
    );
}

#[test]
fn handle_peer_send_emits_peer_inbox_to_sibling_in_same_tab() {
    // Split pane A's workspace to create a sibling. Sending from
    // A to the sibling should emit Event::PeerInbox carrying the
    // sender id, the body, and a stable timestamp. This is the
    // core happy-path of #97: without this event the MCP peer
    // subprocess has nothing to push as a channel notification.
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
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
        .expect("split succeeds");
    // Bind the receiver to the sibling. Since #306 that makes it a
    // precise instrument: the only `PeerInbox` it can ever hold is one
    // addressed to this pane, so a misroute reads as an empty stream
    // rather than hiding among other panes' traffic. (An unscoped
    // subscription would see this delivery too — it would just see
    // everything else with it.) Subscribing after the split also means
    // the split's own `PaneStarted` events are already behind us and
    // the stream starts empty.
    let (_sub_id, rx) = app
        .event_bus
        .subscribe_scoped(ipc::EventScope::PaneInbox(sibling_id));
    assert!(
        rx.try_recv().is_err(),
        "no events expected before the peer send"
    );
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "hello sibling".to_string(),
    )
    .expect("peer send");
    let mut found = false;
    while let Ok(ev) = rx.try_recv() {
        if let ipc::Event::PeerInbox {
            target_pane,
            from_pane,
            body,
            ..
        } = ev
        {
            assert_eq!(target_pane, sibling_id);
            assert_eq!(from_pane, sender_id);
            assert_eq!(body, "hello sibling");
            found = true;
            break;
        }
    }
    assert!(found, "expected PeerInbox event after handle_peer_send");
    app.shutdown();
}

#[test]
fn handle_peer_send_loops_back_to_sender_pane() {
    // Regression for renga#215: when the resolved target is the
    // sender pane itself (e.g. claude-org-ja's peer_notify resolving
    // "secretary" from a shell inside the secretary pane), the
    // handler must still emit PeerInbox so the local check_messages
    // loop picks it up. Prior to the fix the self-send was silently
    // dropped while JSON-RPC reported Delivered.
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    // A self-send targets the sender pane, so that is the pane to bind
    // to: the receiver then holds nothing but mail addressed back to
    // the sender, which is the whole claim under test.
    let (_sub_id, rx) = app
        .event_bus
        .subscribe_scoped(ipc::EventScope::PaneInbox(sender_id));
    while rx.try_recv().is_ok() {}

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sender_id),
        "self ping".to_string(),
    )
    .expect("self send");

    let mut found = false;
    while let Ok(ev) = rx.try_recv() {
        if let ipc::Event::PeerInbox {
            target_pane,
            from_pane,
            body,
            ..
        } = ev
        {
            assert_eq!(target_pane, sender_id);
            assert_eq!(from_pane, sender_id);
            assert_eq!(body, "self ping");
            found = true;
            break;
        }
    }
    assert!(found, "expected PeerInbox event for self-send (renga#215)");
    app.shutdown();
}

#[test]
fn handle_peer_send_delivers_to_cross_tab_target_by_id() {
    // Issue #289 inverted the old silent-drop guard: a numeric id in
    // another tab is a legitimate cross-tab address and must produce
    // exactly one PeerInbox event, same as a same-tab send.
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    // Open a fresh tab; its pane id is distinct from sender's.
    let other_tab_pane = app
        .handle_new_tab(None, None, None, None, None)
        .expect("new tab succeeds");
    assert_ne!(
        other_tab_pane, sender_id,
        "new_tab must allocate a fresh pane id"
    );
    // Pane ids are globally unique, so #306 routing is tab-agnostic:
    // binding to the other tab's pane id is all a subscriber over
    // there has to do.
    let (_sub_id, rx) = app
        .event_bus
        .subscribe_scoped(ipc::EventScope::PaneInbox(other_tab_pane));
    // Drain PaneStarted / tab-switch events.
    while rx.try_recv().is_ok() {}

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(other_tab_pane),
        "hello other tab".to_string(),
    )
    .expect("cross-tab send succeeds");
    let inboxes: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|ev| match ev {
            ipc::Event::PeerInbox {
                target_pane,
                from_pane,
                body,
                ..
            } => Some((target_pane, from_pane, body)),
            _ => None,
        })
        .collect();
    assert_eq!(
        inboxes,
        vec![(other_tab_pane, sender_id, "hello other tab".to_string())],
        "cross-tab PeerSend must emit exactly one PeerInbox for the target"
    );
    app.shutdown();
}

#[test]
fn handle_peer_send_resolves_name_in_sender_workspace_not_active_tab() {
    // Pane names are only unique per tab. The sender sits in a
    // background tab while the human views a tab holding a pane with
    // the SAME name — resolution must prefer the sender's workspace,
    // not the visible one, or background-tab orchestrators misroute
    // (Issue #289 design review, Major 2).
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let same_tab_worker = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            Some("worker".into()),
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    // A second tab with an identically named pane, left as the active
    // tab so the old active-tab-first resolution would pick it.
    let other_tab_worker = app
        .handle_new_tab(None, Some("worker".into()), None, None, None)
        .expect("new tab succeeds");
    assert_eq!(app.active_tab, 1, "new tab should be the visible one");
    // One subscriber per candidate. Under #306 the misroute would show
    // up as an empty `mine` *and* a non-empty `theirs`, so watching
    // both keeps the failure diagnosable instead of just "nothing
    // arrived".
    let (_mine_id, mine) = app
        .event_bus
        .subscribe_scoped(ipc::EventScope::PaneInbox(same_tab_worker));
    let (_theirs_id, theirs) = app
        .event_bus
        .subscribe_scoped(ipc::EventScope::PaneInbox(other_tab_worker));
    while mine.try_recv().is_ok() {}
    while theirs.try_recv().is_ok() {}

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Name("worker".into()),
        "task for MY worker".to_string(),
    )
    .expect("name send succeeds");
    let inbox_targets = |rx: &std::sync::mpsc::Receiver<ipc::Event>| -> Vec<usize> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|ev| match ev {
                ipc::Event::PeerInbox { target_pane, .. } => Some(target_pane),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        inbox_targets(&mine),
        vec![same_tab_worker],
        "name must resolve to the sender's own tab, not the visible tab \
         (other-tab worker is {other_tab_worker})"
    );
    assert!(
        inbox_targets(&theirs).is_empty(),
        "the identically named pane in the visible tab must receive nothing"
    );
    app.shutdown();
}

#[test]
fn handle_peer_send_rejects_a_name_that_only_exists_in_another_tab() {
    // Cross-tab sends require the numeric pane id. An unqualified
    // name never leaves the sender's workspace, and an unresolvable
    // target errors instead of faking "Delivered".
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let far_worker = app
        .handle_new_tab(None, Some("far-worker".into()), None, None, None)
        .expect("new tab succeeds");
    // Bind to the only pane the name could have wrongly resolved to.
    // An unscoped subscription carries every pane's `PeerInbox`, so the
    // assertion below would have to re-check `target_pane` to mean
    // anything; binding states it structurally — this receiver holds
    // far-worker's mail or it holds nothing.
    let (_sub_id, rx) = app
        .event_bus
        .subscribe_scoped(ipc::EventScope::PaneInbox(far_worker));
    while rx.try_recv().is_ok() {}

    let err = app
        .handle_peer_send(
            sender_id,
            &ipc::PaneRef::Name("far-worker".into()),
            "won't arrive".to_string(),
        )
        .expect_err("other-tab name must not resolve");
    assert_eq!(err.code, Some(ipc::err_code::PANE_NOT_FOUND));
    let got_inbox = std::iter::from_fn(|| rx.try_recv().ok())
        .any(|ev| matches!(ev, ipc::Event::PeerInbox { .. }));
    assert!(!got_inbox, "a failed resolution must not deliver anything");
    app.shutdown();
}

#[test]
fn flush_delivers_codex_nudge_to_background_tab_focused_pane() {
    // A single-pane background tab's only pane is always its
    // workspace-focused pane. The flush skip must therefore be limited
    // to the pane the human actually sees (active tab), or cross-tab
    // Codex nudges queue forever (Issue #289 design review, Major 1).
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let codex_pane = app
        .handle_new_tab(None, None, None, None, None)
        .expect("new tab succeeds");
    app.peer_client_kinds
        .insert(codex_pane, PeerClientKind::Codex);
    {
        let pane = app.workspaces[1]
            .panes
            .get_mut(&codex_pane)
            .expect("codex pane");
        let mut parser = pane.parser.lock().unwrap();
        parser.process(b"\x1b[?25h\x1b[2J\x1b[Hready for input\n\nenter to send");
    }
    // Send the human back to tab 0; the Codex tab is now background.
    assert!(app.switch_tab(0), "switch back to the sender tab");
    assert_eq!(app.active_tab, 0, "sender tab should be visible again");

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_pane),
        "cross-tab codex ping".to_string(),
    )
    .expect("peer send");
    assert_eq!(
        app.pending_codex_peer_messages
            .get(&codex_pane)
            .map(|q| q.len()),
        Some(1),
        "background-tab Codex target should queue a nudge"
    );

    app.flush_pending_codex_peer_messages();
    assert!(
        matches!(
            app.pending_codex_peer_messages
                .get(&codex_pane)
                .and_then(|q| q.front()),
            Some(PendingCodexPeerDelivery::SubmitAt(_))
        ),
        "first flush must type the draft into the background tab's \
         workspace-focused pane instead of skipping it"
    );
    if let Some(queue) = app.pending_codex_peer_messages.get_mut(&codex_pane) {
        queue[0] = PendingCodexPeerDelivery::SubmitAt(Instant::now());
    }
    app.flush_pending_codex_peer_messages();
    assert!(
        !app.pending_codex_peer_messages.contains_key(&codex_pane),
        "second flush should submit the queued nudge"
    );
    app.shutdown();
}

#[test]
fn flush_promotes_queued_draft_to_notification_when_target_tab_activates() {
    // A nudge queued while its tab was hidden must not stall when the
    // human switches onto the pane before delivery: the flush promotes
    // the still-undelivered draft into the focused-pane notification
    // overlay instead of skipping it forever (Codex review of #289).
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let codex_pane = app
        .handle_new_tab(None, None, None, None, None)
        .expect("new tab succeeds");
    app.peer_client_kinds
        .insert(codex_pane, PeerClientKind::Codex);
    assert!(app.switch_tab(0), "back to the sender tab");

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_pane),
        "queued while hidden".to_string(),
    )
    .expect("peer send");
    assert!(app.pending_codex_peer_messages.contains_key(&codex_pane));

    // The human switches onto the Codex tab before any flush delivered
    // the draft (its screen was never ready).
    assert!(app.switch_tab(1), "activate the codex tab");
    app.flush_pending_codex_peer_messages();

    assert!(
        !app.pending_codex_peer_messages.contains_key(&codex_pane),
        "promotion must consume the queued draft"
    );
    let notification = app
        .visible_codex_peer_notification()
        .expect("draft should surface as a visible notification");
    assert_eq!(notification.target_pane, codex_pane);
    app.shutdown();
}

#[test]
fn flush_cancels_half_delivered_nudge_once_target_pane_is_watched() {
    // Once the draft text has been typed into the composer (SubmitAt
    // stage), activating the tab hands ownership to the human: the
    // typed nudge is on screen for them to submit or edit, so the
    // deferred Enter is cancelled rather than resumed later — a
    // resumed submit could fire into content the user rewrote in the
    // meantime (Codex review of #289).
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let codex_pane = app
        .handle_new_tab(None, None, None, None, None)
        .expect("new tab succeeds");
    app.peer_client_kinds
        .insert(codex_pane, PeerClientKind::Codex);
    {
        let pane = app.workspaces[1]
            .panes
            .get_mut(&codex_pane)
            .expect("codex pane");
        let mut parser = pane.parser.lock().unwrap();
        parser.process(b"\x1b[?25h\x1b[2J\x1b[Hready for input\n\nenter to send");
    }
    assert!(app.switch_tab(0), "back to the sender tab");
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(codex_pane),
        "half delivered".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();
    assert!(
        matches!(
            app.pending_codex_peer_messages
                .get(&codex_pane)
                .and_then(|q| q.front()),
            Some(PendingCodexPeerDelivery::SubmitAt(_))
        ),
        "draft should have been typed into the background pane"
    );
    if let Some(queue) = app.pending_codex_peer_messages.get_mut(&codex_pane) {
        queue[0] = PendingCodexPeerDelivery::SubmitAt(Instant::now());
    }

    assert!(app.switch_tab(1), "activate the codex tab mid-delivery");
    app.flush_pending_codex_peer_messages();
    assert!(
        !app.pending_codex_peer_messages.contains_key(&codex_pane),
        "watching the pane must cancel the deferred submit outright"
    );
    assert!(
        app.visible_codex_peer_notification().is_none(),
        "half-delivered nudge must not double-surface as a notification"
    );

    // Leaving again must not resurrect the submit — the composer may
    // no longer hold our text.
    assert!(app.switch_tab(0), "leave the codex tab again");
    app.flush_pending_codex_peer_messages();
    assert!(
        !app.pending_codex_peer_messages.contains_key(&codex_pane),
        "a cancelled submit stays cancelled after the pane is unwatched"
    );
    app.shutdown();
}
#[test]
fn handle_peer_send_queues_codex_nudge_and_emits_peer_inbox() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
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
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.handle_focus(&ipc::PaneRef::Id(sender_id), None)
        .expect("refocus sender");
    // #306: bound to the target pane. A Codex target is no exception —
    // the nudge is an extra delivery path, not a replacement for the
    // event, so this receiver must still see one.
    let (_sub_id, rx) = app
        .event_bus
        .subscribe_scoped(ipc::EventScope::PaneInbox(sibling_id));
    while rx.try_recv().is_ok() {}

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "hello codex".to_string(),
    )
    .expect("peer send");

    let peer_inbox = rx
        .try_iter()
        .find(|event| matches!(event, ipc::Event::PeerInbox { .. }))
        .expect("Codex delivery should still emit PeerInbox");
    match peer_inbox {
        ipc::Event::PeerInbox {
            target_pane,
            from_pane,
            from_name,
            from_kind,
            body,
            ..
        } => {
            assert_eq!(target_pane, sibling_id);
            assert_eq!(from_pane, sender_id);
            assert_eq!(from_name.as_deref(), None);
            assert_eq!(from_kind, None);
            assert_eq!(body, "hello codex");
        }
        other => panic!("unexpected event: {other:?}"),
    }
    let queued = app
        .pending_codex_peer_messages
        .get(&sibling_id)
        .expect("queued codex peer message");
    assert_eq!(queued.len(), 1);
    match &queued[0] {
        PendingCodexPeerDelivery::Draft(msg) => {
            assert_eq!(msg.from_pane, sender_id);
            assert_eq!(msg.from_name.as_deref(), None);
            assert_eq!(msg.from_kind, None);
        }
        other => panic!("unexpected queued delivery: {other:?}"),
    }
    app.shutdown();
}

#[test]
fn handle_peer_send_coalesces_codex_nudges_per_pane() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
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
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.handle_focus(&ipc::PaneRef::Id(sender_id), None)
        .expect("refocus sender");

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "hello codex".to_string(),
    )
    .expect("first peer send");
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "hello again codex".to_string(),
    )
    .expect("second peer send");

    assert_eq!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .map(|q| q.len()),
        Some(1),
        "multiple queued inbox messages should share a single pane-local nudge"
    );
    app.shutdown();
}
#[test]
fn pane_expects_codex_peer_delivery_accepts_registered_codex_without_title() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    if let Some(pane) = app.ws_mut().panes.get_mut(&pane_id) {
        *pane.title.lock().unwrap() = String::new();
    }
    assert!(
        !app.pane_expects_codex_peer_delivery(app.active_tab, pane_id),
        "blank title with no registration should not look like Codex"
    );

    app.peer_client_kinds.insert(pane_id, PeerClientKind::Codex);
    assert!(
        app.pane_expects_codex_peer_delivery(app.active_tab, pane_id),
        "registered Codex peer must count even when OSC title detection never fired"
    );
    app.shutdown();
}

#[test]
fn pane_expects_codex_peer_delivery_accepts_pending_codex_startup() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    if let Some(pane) = app.ws_mut().panes.get_mut(&pane_id) {
        *pane.title.lock().unwrap() = String::new();
        pane.pending_startup = Some(b"codex --model gpt-5\n".to_vec());
    }

    assert!(
        app.pane_expects_codex_peer_delivery(app.active_tab, pane_id),
        "queued codex startup should count before MCP registration lands"
    );
    app.shutdown();
}

#[test]
fn forward_key_to_pty_clears_codex_transcript_overlay_hint() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    app.ws_mut()
        .panes
        .get_mut(&pane_id)
        .expect("focused pane exists")
        .set_codex_transcript_overlay_hint_for_test(true);

    app.forward_key_to_pty(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
        .expect("forward key");

    assert!(
        !app.ws()
            .panes
            .get(&pane_id)
            .expect("focused pane exists")
            .codex_transcript_overlay_hint_for_test(),
        "direct PTY key forwarding must clear transcript fallback state"
    );
    app.shutdown();
}

#[test]
fn forward_paste_to_pty_clears_codex_transcript_overlay_hint() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    app.ws_mut()
        .panes
        .get_mut(&pane_id)
        .expect("focused pane exists")
        .set_codex_transcript_overlay_hint_for_test(true);

    app.forward_paste_to_pty("hello").expect("forward paste");

    assert!(
        !app.ws()
            .panes
            .get(&pane_id)
            .expect("focused pane exists")
            .codex_transcript_overlay_hint_for_test(),
        "direct PTY paste forwarding must clear transcript fallback state"
    );
    app.shutdown();
}

#[test]
fn handle_peer_send_defers_codex_nudge_while_target_is_focused() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
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
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.handle_focus(&ipc::PaneRef::Id(sibling_id), None)
        .expect("focus sibling");
    // #306: bound to the target pane, the same one the deferred nudge
    // is queued for.
    let (_sub_id, rx) = app
        .event_bus
        .subscribe_scoped(ipc::EventScope::PaneInbox(sibling_id));
    while rx.try_recv().is_ok() {}

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "hello focused codex".to_string(),
    )
    .expect("peer send");

    let peer_inbox = rx
        .try_iter()
        .find(|event| matches!(event, ipc::Event::PeerInbox { .. }))
        .expect("focused Codex should still receive PeerInbox");
    match peer_inbox {
        ipc::Event::PeerInbox {
            target_pane, body, ..
        } => {
            assert_eq!(target_pane, sibling_id);
            assert_eq!(body, "hello focused codex");
        }
        other => panic!("unexpected event: {other:?}"),
    }
    let notification = app
        .visible_codex_peer_notification()
        .expect("focused Codex target should show a notification overlay");
    assert_eq!(notification.target_pane, sibling_id);
    assert_eq!(notification.pending_count, 1);
    assert_eq!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .map(|q| q.len()),
        None,
        "focused Codex target should not queue an immediate PTY nudge"
    );

    app.flush_pending_codex_peer_messages();
    assert_eq!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .map(|q| q.len()),
        None,
        "focused Codex target should stay notification-only while it remains focused"
    );

    app.handle_focus(&ipc::PaneRef::Id(sender_id), None)
        .expect("refocus sender");
    app.flush_pending_codex_peer_messages();
    assert!(
        app.visible_codex_peer_notification().is_none(),
        "moving focus away should hand the notification back to the worker queue"
    );
    assert_eq!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .map(|q| q.len()),
        Some(1),
        "unfocused Codex target should regain a queued nudge"
    );
    {
        let pane = app.ws_mut().panes.get_mut(&sibling_id).expect("pane");
        let mut parser = pane.parser.lock().unwrap();
        parser.process(b"\x1b[?25h\x1b[2J\x1b[Hready for input\n\nenter to send");
    }
    app.flush_pending_codex_peer_messages();
    assert_eq!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .map(|q| q.len()),
        Some(1),
        "first unfocused flush should advance the deferred nudge to submit stage"
    );
    if let Some(queue) = app.pending_codex_peer_messages.get_mut(&sibling_id) {
        queue[0] = PendingCodexPeerDelivery::SubmitAt(Instant::now());
    }
    app.flush_pending_codex_peer_messages();
    assert!(
        !app.pending_codex_peer_messages.contains_key(&sibling_id),
        "second unfocused flush should submit the deferred nudge"
    );
    app.shutdown();
}

#[test]
fn handle_peer_send_coalesces_focused_codex_notifications() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
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
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.handle_focus(&ipc::PaneRef::Id(sibling_id), None)
        .expect("focus sibling");

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "hello focused codex".to_string(),
    )
    .expect("first peer send");
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "hello again focused codex".to_string(),
    )
    .expect("second peer send");

    let notification = app
        .visible_codex_peer_notification()
        .expect("focused Codex target should still show one notification");
    assert_eq!(notification.pending_count, 2);
    assert_eq!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .map(|q| q.len()),
        None,
        "focused notifications should not leak into the PTY nudge queue"
    );
    app.shutdown();
}

#[test]
fn focused_codex_notification_esc_dismisses_without_queueing_nudge() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
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
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.handle_focus(&ipc::PaneRef::Id(sibling_id), None)
        .expect("focus sibling");
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "hello focused codex".to_string(),
    )
    .expect("peer send");

    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    let consumed = app.handle_key_event(esc).expect("dismiss notification");
    assert!(consumed);
    assert!(app.visible_codex_peer_notification().is_none());
    assert!(
        !app.pending_codex_peer_messages.contains_key(&sibling_id),
        "dismissing the notification should not silently queue a PTY nudge"
    );
    app.shutdown();
}

#[test]
fn focused_codex_notification_commit_clears_notification() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
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
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.handle_focus(&ipc::PaneRef::Id(sibling_id), None)
        .expect("focus sibling");
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "hello focused codex".to_string(),
    )
    .expect("peer send");

    let commit = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
    let consumed = app.handle_key_event(commit).expect("commit notification");
    assert!(consumed);
    assert!(app.visible_codex_peer_notification().is_none());
    assert!(
        !app.pending_codex_peer_messages.contains_key(&sibling_id),
        "manual insert should consume the focused notification without requeueing"
    );
    app.shutdown();
}

#[test]
fn flush_pending_codex_peer_messages_requires_ready_screen() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
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
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.handle_focus(&ipc::PaneRef::Id(sender_id), None)
        .expect("refocus sender");
    {
        let pane = app.ws_mut().panes.get_mut(&sibling_id).expect("pane");
        *pane.title.lock().unwrap() = "Codex".to_string();
        let mut parser = pane.parser.lock().unwrap();
        parser.process(b"\x1b[?25lworking");
    }
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "hello codex".to_string(),
    )
    .expect("peer send");
    app.flush_pending_codex_peer_messages();
    assert_eq!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .map(|q| q.len()),
        Some(1),
        "busy Codex pane should keep the message queued"
    );

    {
        let pane = app.ws_mut().panes.get_mut(&sibling_id).expect("pane");
        let mut parser = pane.parser.lock().unwrap();
        parser.process(b"\x1b[?25h\x1b[2J\x1b[Hready for input\n\nenter to send");
    }
    app.flush_pending_codex_peer_messages();
    assert_eq!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .map(|q| q.len()),
        Some(1),
        "first ready flush should advance to submit stage"
    );
    if let Some(queue) = app.pending_codex_peer_messages.get_mut(&sibling_id) {
        queue[0] = PendingCodexPeerDelivery::SubmitAt(Instant::now());
    }
    app.flush_pending_codex_peer_messages();
    assert!(
        !app.pending_codex_peer_messages.contains_key(&sibling_id),
        "second flush should submit the queued nudge"
    );
    app.shutdown();
}

#[test]
fn flush_pending_codex_peer_messages_waits_for_non_blank_codex_screen() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
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
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.handle_focus(&ipc::PaneRef::Id(sender_id), None)
        .expect("refocus sender");
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "hello codex".to_string(),
    )
    .expect("peer send");

    app.flush_pending_codex_peer_messages();
    assert_eq!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .map(|q| q.len()),
        Some(1),
        "blank Codex screen should keep the nudge queued"
    );

    {
        let pane = app.ws_mut().panes.get_mut(&sibling_id).expect("pane");
        let mut parser = pane.parser.lock().unwrap();
        parser
            .process(b"\x1b[?25h\x1b[2J\x1b[H\xE2\x80\xBA Explain this codebase\n\n  gpt-5.4 high");
    }
    app.flush_pending_codex_peer_messages();
    assert_eq!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .map(|q| q.len()),
        Some(1),
        "non-blank Codex screen should advance to submit stage first"
    );
    if let Some(queue) = app.pending_codex_peer_messages.get_mut(&sibling_id) {
        queue[0] = PendingCodexPeerDelivery::SubmitAt(Instant::now());
    }
    app.flush_pending_codex_peer_messages();
    assert!(
        !app.pending_codex_peer_messages.contains_key(&sibling_id),
        "second flush should submit the queued nudge"
    );
    app.shutdown();
}

#[test]
fn codex_prompt_allows_peer_nudge_uses_recent_content_on_tall_screens() {
    let mut parser = vt100::Parser::new(120, 120, 0);
    parser.process(
        b"\x1b[?25h\x1b[2J\x1b[H\
          Tip: NEW: JavaScript REPL is now available in /experimental.\n\
          \n\
          \n\
          \xE2\x80\xBA Summarize recent commits\n\
          \n\
            gpt-5.4 high\x1b[4;3H",
    );

    let screen = parser.screen();
    let tail = screen_tail_lines(screen).join("\n").to_ascii_lowercase();
    assert!(
        tail.contains("summarize recent commits"),
        "tail snapshot should stay anchored to the recent Codex prompt"
    );
    assert_eq!(
        codex_prompt_allows_peer_nudge_on_screen(screen),
        Some(true),
        "recent Codex prompt on a tall screen should allow the peer nudge"
    );
}

#[test]
fn flush_pending_codex_peer_messages_does_not_interrupt_existing_codex_draft() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
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
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.handle_focus(&ipc::PaneRef::Id(sender_id), None)
        .expect("refocus sender");
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "hello codex".to_string(),
    )
    .expect("peer send");

    {
        let pane = app.ws_mut().panes.get_mut(&sibling_id).expect("pane");
        let mut parser = pane.parser.lock().unwrap();
        parser.process(b"\x1b[?25h\x1b[2J\x1b[H\xE2\x80\xBA typed draft\n\n  gpt-5.4 high");
    }
    app.flush_pending_codex_peer_messages();
    assert_eq!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .map(|q| q.len()),
        Some(1),
        "Codex pane with an existing draft should keep the nudge queued"
    );

    {
        let pane = app.ws_mut().panes.get_mut(&sibling_id).expect("pane");
        let mut parser = pane.parser.lock().unwrap();
        parser.process(
            b"\x1b[?25h\x1b[2J\x1b[H\xE2\x80\xBA Run /review on my current changes\n\n  gpt-5.4 high\x1b[1;3H",
        );
    }
    app.flush_pending_codex_peer_messages();
    assert_eq!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .map(|q| q.len()),
        Some(1),
        "placeholder prompt should advance to submit stage once the pane is clean"
    );
    if let Some(queue) = app.pending_codex_peer_messages.get_mut(&sibling_id) {
        queue[0] = PendingCodexPeerDelivery::SubmitAt(Instant::now());
    }
    app.flush_pending_codex_peer_messages();
    assert!(
        !app.pending_codex_peer_messages.contains_key(&sibling_id),
        "clean Codex prompt should eventually submit the queued nudge"
    );
    app.shutdown();
}

#[test]
fn handle_peer_list_excludes_caller_and_lists_siblings() {
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            Some("sibling".into()),
            Some("worker".into()),
            None,
            None,
            None,
        )
        .expect("split succeeds");
    let peers = app.handle_peer_list(sender_id).expect("peer list");
    assert_eq!(peers.len(), 1, "expected one sibling, got {peers:?}");
    assert_eq!(peers[0].id, sibling_id);
    assert_eq!(peers[0].name.as_deref(), Some("sibling"));
    assert_eq!(peers[0].role.as_deref(), Some("worker"));
    assert_eq!(peers[0].tab, Some(0));
    assert_eq!(peers[0].same_tab, Some(true));
    // Caller must be excluded.
    assert!(
        peers.iter().all(|p| p.id != sender_id),
        "peer list must not include the caller"
    );
    app.shutdown();
}

#[test]
fn handle_peer_list_spans_all_tabs_with_caller_tab_first() {
    // Issue #289: list_peers enumerates every workspace, still
    // excluding only the caller. The caller's own tab leads so
    // same-tab siblings (addressable by bare name) stay on top, and
    // every entry carries tab metadata plus the same_tab flag.
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
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
        .expect("split succeeds");
    let other_tab_pane = app
        .handle_new_tab(None, None, None, Some("scout".into()), None)
        .expect("new tab succeeds");
    // The new tab is now active — listing from the tab-0 sender must
    // still put the sender's own (background) tab first.
    assert_eq!(app.active_tab, 1);

    let peers = app.handle_peer_list(sender_id).expect("peer list");
    let ids: Vec<usize> = peers.iter().map(|p| p.id).collect();
    assert_eq!(
        ids,
        vec![sibling_id, other_tab_pane],
        "caller's tab first, then remaining tabs in index order"
    );
    assert!(
        peers.iter().all(|p| p.id != sender_id),
        "peer list must not include the caller"
    );

    let sibling = &peers[0];
    assert_eq!(sibling.tab, Some(0));
    assert_eq!(sibling.same_tab, Some(true));
    assert!(sibling.tab_name.is_some(), "tab label should be surfaced");

    let other = &peers[1];
    assert_eq!(other.tab, Some(1));
    assert_eq!(other.same_tab, Some(false));
    assert_eq!(other.role.as_deref(), Some("scout"));
    app.shutdown();
}

#[test]
fn handle_peer_list_reorders_middle_tab_caller_ahead_of_lower_tabs() {
    // With the caller in tab 0 the caller-first order is
    // indistinguishable from plain index order, so pin the reorder
    // from a MIDDLE tab: the caller's tab-1 sibling must outrank the
    // tab-0 pane even though tab 0 comes first by index.
    let mut app = App::new(40, 80).expect("App::new");
    let tab0_pane = app.ws().focused_pane_id;
    let caller_id = app
        .handle_new_tab(None, None, None, None, None)
        .expect("second tab");
    assert_eq!(app.active_tab, 1);
    let caller_sibling = app
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
        .expect("split caller tab");
    let tab2_pane = app
        .handle_new_tab(None, None, None, None, None)
        .expect("third tab");

    let peers = app.handle_peer_list(caller_id).expect("peer list");
    let ids: Vec<usize> = peers.iter().map(|p| p.id).collect();
    assert_eq!(
        ids,
        vec![caller_sibling, tab0_pane, tab2_pane],
        "caller's own tab leads even when a lower-indexed tab exists"
    );
    assert_eq!(peers[0].same_tab, Some(true));
    assert_eq!(peers[1].tab, Some(0));
    assert_eq!(peers[2].tab, Some(2));
    app.shutdown();
}

#[test]
fn handle_peer_send_dedupes_identical_payload_within_window() {
    // renga#221 acceptance criterion #2: re-sending the exact same
    // payload from the same peer within the dedupe window must not
    // produce two PeerInbox events. Otherwise a chatty dispatcher /
    // worker can paper the receiver's transcript with phantom
    // Human: turns.
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
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
        .expect("split succeeds");
    // #306: bound to the target pane, so the "how many arrived" count
    // is read off a receiver that can hold nothing else.
    let (_sub_id, rx) = app
        .event_bus
        .subscribe_scoped(ipc::EventScope::PaneInbox(sibling_id));
    while rx.try_recv().is_ok() {}

    app.handle_peer_send(sender_id, &ipc::PaneRef::Id(sibling_id), "ack".to_string())
        .expect("first send");
    app.handle_peer_send(sender_id, &ipc::PaneRef::Id(sibling_id), "ack".to_string())
        .expect("second send (duplicate)");

    let mut peer_inboxes = 0usize;
    while let Ok(ev) = rx.try_recv() {
        if let ipc::Event::PeerInbox { body, .. } = ev {
            assert_eq!(body, "ack");
            peer_inboxes += 1;
        }
    }
    assert_eq!(
        peer_inboxes, 1,
        "duplicate identical payload should collapse to a single PeerInbox"
    );
    app.shutdown();
}

#[test]
fn handle_peer_send_distinct_bodies_are_not_deduped() {
    // Sanity check: dedupe is keyed on body, so two genuinely
    // distinct messages must both go through. Without this, the
    // "every reply gets the same prefix" pattern would silently
    // swallow follow-ups.
    let mut app = App::new(40, 80).expect("App::new");
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
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
        .expect("split succeeds");
    // #306: bound to the target pane.
    let (_sub_id, rx) = app
        .event_bus
        .subscribe_scoped(ipc::EventScope::PaneInbox(sibling_id));
    while rx.try_recv().is_ok() {}

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "first".to_string(),
    )
    .expect("first send");
    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "second".to_string(),
    )
    .expect("second send");

    let mut bodies = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let ipc::Event::PeerInbox { body, .. } = ev {
            bodies.push(body);
        }
    }
    assert_eq!(bodies, vec!["first", "second"]);
    app.shutdown();
}

#[test]
fn handle_peer_send_dedupe_does_not_collapse_distinct_senders() {
    // Dedupe key is (target, from, body). Two different peers
    // sending the same text must both deliver, since they really
    // are independent messages in the human sense.
    let mut app = App::new(40, 80).expect("App::new");
    let sender_a = app.ws().focused_pane_id;
    let sender_b = app
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
        .expect("split succeeds (sender_b)");
    let target = app
        .handle_split(
            &ipc::PaneRef::Id(sender_a),
            ipc::Direction::Horizontal,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds (target)");
    // #306: both sends address the same pane, so one bound receiver
    // sees both deliveries — which is exactly the fan-in this test is
    // about.
    let (_sub_id, rx) = app
        .event_bus
        .subscribe_scoped(ipc::EventScope::PaneInbox(target));
    while rx.try_recv().is_ok() {}

    app.handle_peer_send(sender_a, &ipc::PaneRef::Id(target), "ping".to_string())
        .expect("a -> target");
    app.handle_peer_send(sender_b, &ipc::PaneRef::Id(target), "ping".to_string())
        .expect("b -> target");

    let mut count = 0usize;
    while let Ok(ev) = rx.try_recv() {
        if let ipc::Event::PeerInbox { .. } = ev {
            count += 1;
        }
    }
    assert_eq!(
        count, 2,
        "same body from distinct senders must not collapse into one delivery"
    );
    app.shutdown();
}
