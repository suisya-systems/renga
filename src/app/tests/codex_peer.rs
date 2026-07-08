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

#[test]
fn handle_peer_send_emits_peer_inbox_to_sibling_in_same_tab() {
    // Split pane A's workspace to create a sibling. Sending from
    // A to the sibling should emit Event::PeerInbox carrying the
    // sender id, the body, and a stable timestamp. This is the
    // core happy-path of #97: without this event the MCP peer
    // subprocess has nothing to push as a channel notification.
    let mut app = App::new(40, 80).expect("App::new");
    let (_sub_id, rx) = app.event_bus.subscribe();
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    // Drain PaneStarted events from the split so the assertion below
    // only sees the PeerInbox we care about.
    while let Ok(ev) = rx.try_recv() {
        if !matches!(ev, ipc::Event::PaneStarted { .. }) {
            panic!("unexpected event before peer send: {ev:?}");
        }
    }
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
    let (_sub_id, rx) = app.event_bus.subscribe();
    let sender_id = app.ws().focused_pane_id;
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
fn handle_peer_send_silently_drops_cross_tab_target() {
    // Cross-tab delivery is a silent no-op by design — callers
    // cannot enumerate panes in other tabs by probing ids. A
    // PeerInbox event would leak "pane X exists somewhere", so
    // the handler must emit nothing at all.
    let mut app = App::new(40, 80).expect("App::new");
    let (_sub_id, rx) = app.event_bus.subscribe();
    let sender_id = app.ws().focused_pane_id;
    // Open a fresh tab; its pane id is distinct from sender's.
    let other_tab_pane = app
        .handle_new_tab(None, None, None, None, None)
        .expect("new tab succeeds");
    assert_ne!(
        other_tab_pane, sender_id,
        "new_tab must allocate a fresh pane id"
    );
    // Drain PaneStarted / tab-switch events.
    while rx.try_recv().is_ok() {}

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(other_tab_pane),
        "should be silently dropped".to_string(),
    )
    .expect("cross-tab send reports success");
    let got_inbox = std::iter::from_fn(|| rx.try_recv().ok())
        .any(|ev| matches!(ev, ipc::Event::PeerInbox { .. }));
    assert!(
        !got_inbox,
        "cross-tab PeerSend must NOT emit a PeerInbox event"
    );
    app.shutdown();
}
#[test]
fn handle_peer_send_queues_codex_nudge_and_emits_peer_inbox() {
    let mut app = App::new(40, 80).expect("App::new");
    let (_sub_id, rx) = app.event_bus.subscribe();
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.handle_focus(&ipc::PaneRef::Id(sender_id))
        .expect("refocus sender");
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
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.handle_focus(&ipc::PaneRef::Id(sender_id))
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
fn handle_peer_send_nudges_focused_codex_directly_without_notification() {
    let mut app = App::new(40, 80).expect("App::new");
    let (_sub_id, rx) = app.event_bus.subscribe();
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.handle_focus(&ipc::PaneRef::Id(sibling_id))
        .expect("focus sibling");
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
    assert!(
        app.visible_codex_peer_notification().is_none(),
        "focused Codex target should not show a notification overlay"
    );
    assert_eq!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .map(|q| q.len()),
        None,
        "focused Codex target should not leave a queued PTY nudge"
    );

    app.flush_pending_codex_peer_messages();
    assert_eq!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .map(|q| q.len()),
        None,
        "direct focused nudge should not reappear in the queue"
    );
    app.shutdown();
}

#[test]
fn handle_peer_send_keeps_unfocused_codex_on_queued_nudge_path() {
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
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.handle_focus(&ipc::PaneRef::Id(sender_id))
        .expect("refocus sender");

    app.handle_peer_send(
        sender_id,
        &ipc::PaneRef::Id(sibling_id),
        "hello unfocused codex".to_string(),
    )
    .expect("peer send");

    assert!(app.visible_codex_peer_notification().is_none());
    assert_eq!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .map(|q| q.len()),
        Some(1),
        "unfocused Codex target should still use the ready-gated queue"
    );
    app.shutdown();
}

#[test]
fn handle_peer_send_repeated_focused_codex_messages_do_not_open_notification() {
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
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.handle_focus(&ipc::PaneRef::Id(sibling_id))
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

    assert!(
        app.visible_codex_peer_notification().is_none(),
        "focused Codex sends should be direct, not notification-backed"
    );
    assert_eq!(
        app.pending_codex_peer_messages
            .get(&sibling_id)
            .map(|q| q.len()),
        None,
        "focused Codex sends should not leak into the PTY nudge queue"
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
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.handle_focus(&ipc::PaneRef::Id(sender_id))
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
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.handle_focus(&ipc::PaneRef::Id(sender_id))
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
        )
        .expect("split succeeds");
    app.peer_client_kinds
        .insert(sibling_id, PeerClientKind::Codex);
    app.handle_focus(&ipc::PaneRef::Id(sender_id))
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
        )
        .expect("split succeeds");
    let peers = app.handle_peer_list(sender_id).expect("peer list");
    assert_eq!(peers.len(), 1, "expected one sibling, got {peers:?}");
    assert_eq!(peers[0].id, sibling_id);
    assert_eq!(peers[0].name.as_deref(), Some("sibling"));
    assert_eq!(peers[0].role.as_deref(), Some("worker"));
    // Caller must be excluded.
    assert!(
        peers.iter().all(|p| p.id != sender_id),
        "peer list must not include the caller"
    );
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
    let (_sub_id, rx) = app.event_bus.subscribe();
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
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
    let (_sub_id, rx) = app.event_bus.subscribe();
    let sender_id = app.ws().focused_pane_id;
    let sibling_id = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
            None,
            None,
            None,
            None,
        )
        .expect("split succeeds");
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
    let (_sub_id, rx) = app.event_bus.subscribe();
    let sender_a = app.ws().focused_pane_id;
    let sender_b = app
        .handle_split(
            &ipc::PaneRef::Focused,
            ipc::Direction::Vertical,
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
        )
        .expect("split succeeds (target)");
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
