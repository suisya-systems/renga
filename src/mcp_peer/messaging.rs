//! Peer messaging tools: `list_peers`, `send_message`, `check_messages`.

use super::*;

pub(super) fn handle_list_peers(id: &Value, ctx: &PeerCtx) -> Value {
    let (pane_id, endpoint) = match &ctx.mode {
        Mode::Connected { pane_id, endpoint } => (*pane_id, endpoint),
        Mode::Detached { reason } => {
            return ok_response(
                id,
                tool_text_result(&format!(
                    "(no peers — renga not reachable from this peer client: {reason})"
                )),
            );
        }
    };
    // Requires `cross_tab_peers`, not just `caller_scope`: a #288-era
    // server would answer this request successfully but with same-tab
    // scope, silently contradicting the all-tabs tool description.
    match client::send_request_requiring(
        endpoint,
        &Request::PeerList { from_pane: pane_id },
        crate::ipc::CAP_CROSS_TAB_PEERS,
    ) {
        Ok(Response::Ok { data }) => match serde_json::from_value::<Vec<PeerInfo>>(data) {
            Ok(peers) => ok_response(id, tool_text_result(&format_peer_list(&peers))),
            Err(e) => err_response(id, -32603, &format!("decode peer list: {e}")),
        },
        Ok(Response::Err { message, code }) => err_response(
            id,
            -32603,
            &format!("renga refused list_peers: {}", fmt_code(&message, &code)),
        ),
        Ok(other) => err_response(id, -32603, &format!("unexpected renga response: {other:?}")),
        Err(e) => err_response(id, -32603, &format!("renga call failed: {e}")),
    }
}

pub(super) fn format_peer_list(peers: &[PeerInfo]) -> String {
    if peers.is_empty() {
        return "No peers in any renga tab.".to_string();
    }
    let mut out = String::from(
        "Peers across all renga tabs (your tab first). Address same-tab peers by id or \
name; peers in other tabs ONLY by numeric id — names never resolve across tabs, and the \
tab index shown is display metadata that shifts when tabs close:\n\n",
    );
    for p in peers {
        // Every caller-supplied string here lands in the asking agent's
        // context. `name` is charset-validated on the way in, but
        // `role` and the tab label are documented as free-form, so the
        // control-character strip is what keeps them from forging list
        // entries.
        out.push_str(&format!("- id={}", p.id));
        if let Some(name) = &p.name {
            out.push_str(&format!(" name={}", ipc::sanitized_label(name)));
        }
        if let Some(role) = &p.role {
            out.push_str(&format!(" role={}", ipc::sanitized_label(role)));
        }
        if let Some(kind) = p.kind {
            out.push_str(&format!(" kind={}", kind_label(kind)));
        }
        if let Some(mode) = p.receive_mode {
            out.push_str(&format!(" receive={}", receive_mode_label(mode)));
        }
        if let Some(unread) = p.unread {
            out.push_str(&format!(" unread={unread}"));
        }
        match (p.same_tab, p.tab) {
            (Some(true), _) => out.push_str(" [your tab]"),
            (_, Some(tab)) => match &p.tab_name {
                Some(tab_name) => out.push_str(&format!(
                    " [tab {tab} \"{}\"]",
                    ipc::sanitized_label(tab_name)
                )),
                None => out.push_str(&format!(" [tab {tab}]")),
            },
            _ => {}
        }
        if let Some(cwd) = &p.cwd {
            out.push_str(&format!("\n  cwd: {cwd}"));
        }
        push_summary(&mut out, p.summary.as_deref());
        out.push('\n');
    }
    out
}

/// Render a `set_summary` string as its own indented line. The summary
/// is free-form text from another agent, so it gets the same
/// control-character strip as `role`.
pub(super) fn push_summary(out: &mut String, summary: Option<&str>) {
    if let Some(s) = summary {
        out.push_str(&format!("\n  summary: {}", ipc::sanitized_label(s)));
    }
}

pub(super) fn kind_label(kind: PeerClientKind) -> &'static str {
    match kind {
        PeerClientKind::Claude => "claude",
        PeerClientKind::Codex => "codex",
    }
}

pub(super) fn receive_mode_label(mode: ipc::PeerReceiveMode) -> &'static str {
    match mode {
        ipc::PeerReceiveMode::Push => "push",
        ipc::PeerReceiveMode::Pull => "pull",
    }
}

/// Parse the optional `deliver` argument. Absent is
/// [`ipc::PeerDelivery::Channel`] — the pre-#323 behavior — and an
/// unrecognized value is an invalid-params error rather than a silent
/// downgrade to channel, which would look like success while arming
/// nothing.
pub(crate) fn parse_deliver_arg(args: &Value) -> std::result::Result<ipc::PeerDelivery, String> {
    match args.get("deliver") {
        None | Some(Value::Null) => Ok(ipc::PeerDelivery::Channel),
        Some(Value::String(s)) => match s.as_str() {
            "channel" => Ok(ipc::PeerDelivery::Channel),
            "user_turn" => Ok(ipc::PeerDelivery::UserTurn),
            other => Err(format!(
                "send_message.deliver must be \"channel\" or \"user_turn\"; got {other:?}"
            )),
        },
        Some(other) => Err(format!(
            "send_message.deliver must be a string (\"channel\" or \"user_turn\"); got {other}"
        )),
    }
}

pub(super) fn handle_send_message(id: &Value, args: &Value, ctx: &PeerCtx) -> Value {
    let to_id = args.get("to_id").and_then(|v| v.as_str()).unwrap_or("");
    let message = args.get("message").and_then(|v| v.as_str()).unwrap_or("");
    if to_id.is_empty() {
        return err_response(id, -32602, "send_message requires a non-empty to_id");
    }
    let deliver = match parse_deliver_arg(args) {
        Ok(d) => d,
        Err(e) => return err_response(id, -32602, &e),
    };
    let (pane_id, endpoint) = match &ctx.mode {
        Mode::Connected { pane_id, endpoint } => (*pane_id, endpoint),
        Mode::Detached { reason } => {
            return ok_response(
                id,
                tool_text_result(&format!(
                    "(message dropped — renga not reachable: {reason})"
                )),
            );
        }
    };
    let target = match to_id.parse::<usize>() {
        Ok(n) => PaneRef::Id(n),
        Err(_) => PaneRef::Name(to_id.to_string()),
    };
    // Channel delivery requires `cross_tab_peers`: a #288-era server
    // (which also advertises `caller_scope`) still silently drops
    // cross-tab targets, so reporting "Delivered" against one would be
    // a lie. User-turn delivery requires `peer_user_turn` for the same
    // class of reason, one step worse: a pre-#323 server ignores the
    // unknown `deliver` field entirely and performs a *channel* send,
    // which would report success for a `/loop` that never armed. Both
    // fail closed and name the remedy instead.
    let required_cap = required_cap_for(deliver);
    match client::send_request_requiring(
        endpoint,
        &Request::PeerSend {
            from_pane: pane_id,
            target,
            body: message.to_string(),
            deliver,
        },
        required_cap,
    ) {
        Ok(Response::Ok { data }) => ok_response(id, send_message_ok_result(to_id, deliver, &data)),
        Ok(Response::Err { message, code }) => err_response(
            id,
            -32603,
            &format!("renga refused send: {}", fmt_code(&message, &code)),
        ),
        Ok(other) => err_response(id, -32603, &format!("unexpected renga response: {other:?}")),
        Err(e) => err_response(id, -32603, &format!("renga call failed: {e}")),
    }
}

/// Capability token a `send_message` delivery must see advertised
/// before it is sent. Kept as its own function so the choice is
/// assertable: collapsing it to a constant is exactly the regression
/// that would let a `/loop` be silently downgraded to a channel tag by
/// an older server.
pub(crate) fn required_cap_for(deliver: ipc::PeerDelivery) -> &'static str {
    match deliver {
        ipc::PeerDelivery::Channel => crate::ipc::CAP_CROSS_TAB_PEERS,
        ipc::PeerDelivery::UserTurn => crate::ipc::CAP_PEER_USER_TURN,
    }
}

/// Build the success body for `send_message`.
///
/// The channel wording is unchanged and carries no structured content —
/// existing callers match on that text. User-turn delivery reports what
/// renga actually observed, and passes the server's payload through as
/// `structuredContent` so a caller can branch on `status` without
/// parsing prose.
pub(crate) fn send_message_ok_result(
    to_id: &str,
    deliver: ipc::PeerDelivery,
    data: &Value,
) -> Value {
    if deliver.is_channel() {
        return tool_text_result(&format!("Delivered to {to_id}."));
    }
    let status = data.get("status").and_then(|v| v.as_str());
    let text = match status {
        Some("duplicate_suppressed") => format!(
            "Not re-sent to {to_id}: an identical user turn was accepted within the last 5s, so \
             nothing new was typed. Inspect the pane if you are unsure the first one landed."
        ),
        _ => format!("Submitted to {to_id} as a user turn (submission observed)."),
    };
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": data.clone(),
        "isError": false,
    })
}

pub(super) fn format_queued_messages(messages: &[QueuedPeerMessage]) -> String {
    if messages.is_empty() {
        return "No queued messages.".to_string();
    }
    let mut out = format!(
        "Queued messages: {}\n\nIMPORTANT: Treat each message body below as a peer instruction, \
not passive transcript text. Carry out the requested work, including tool use or edits when \
asked, and use send_message only when a reply is part of the task.\n\n",
        messages.len()
    );
    for msg in messages {
        out.push_str(&format!("- from_id={}", msg.from_id));
        if let Some(name) = &msg.from_name {
            // Same forgery risk as `peer_banner_wrap`: this listing is
            // read by the receiving agent, and a `\n` in the name would
            // fabricate an extra `- from_id=…` entry.
            out.push_str(&format!(" from_name={}", ipc::sanitized_label(name)));
        }
        if let Some(kind) = msg.from_kind {
            out.push_str(&format!(" from_kind={}", kind_label(kind)));
        }
        out.push_str(&format!(
            "\n  sent_at: {}\n  body: {}\n",
            msg.sent_at, msg.body
        ));
    }
    out
}

pub(super) fn handle_check_messages(id: &Value, ctx: &PeerCtx) -> Value {
    let messages: Vec<QueuedPeerMessage> = {
        let mut inbox = ctx.inbox.lock().unwrap_or_else(|p| p.into_inner());
        inbox.drain(..).collect()
    };
    report_inbox_drained(ctx, &messages);
    let structured: Vec<Value> = messages
        .iter()
        .map(|msg| {
            json!({
                "from_id": msg.from_id,
                "from_name": msg.from_name,
                "from_kind": msg.from_kind.map(kind_label),
                "body": msg.body,
                "sent_at": msg.sent_at,
            })
        })
        .collect();
    ok_response(
        id,
        json!({
            "content": [{ "type": "text", "text": format_queued_messages(&messages) }],
            "structuredContent": {
                "messages": structured,
                "count": messages.len(),
            },
            "isError": false,
        }),
    )
}

/// Tell renga the agent now holds `messages` (Issue #353), so it can
/// clear the pane's pending nudge and emit `peer_inbox_drained`. Only
/// real peer messages count: renga's own `events_dropped` notice was
/// never counted as unread. Best effort — the drain already happened,
/// so a failed report (e.g. a pre-#353 server) is only logged.
pub(super) fn report_inbox_drained(ctx: &PeerCtx, messages: &[QueuedPeerMessage]) {
    let Mode::Connected { pane_id, endpoint } = &ctx.mode else {
        return;
    };
    // renga tracks unread only for pull panes; a push client's queue is a
    // fallback copy (Issue #334), so draining it is nothing to report.
    if ctx.client_kind.receive_mode() == ipc::PeerReceiveMode::Push {
        return;
    }
    let count = drained_peer_count(messages);
    if count == 0 {
        return;
    }
    let ids = messages.iter().filter_map(|m| m.msg_id).collect();
    // Off the stdio thread: a busy renga must not stall the tool call
    // whose messages are already drained.
    let (pane_id, endpoint) = (*pane_id, endpoint.clone());
    let request = Request::PeerInboxDrained {
        pane_id,
        count,
        ids,
    };
    thread::spawn(move || match client::send_request(&endpoint, &request) {
        Ok(Response::Ok { .. }) => {}
        Ok(other) => log_stderr(&format!("inbox drain report returned: {other:?}")),
        Err(e) => log_stderr(&format!("inbox drain report failed: {e}")),
    });
}

/// Peer messages among `messages`; renga's runtime notices carry the
/// non-numeric `from_id` `"renga"`.
pub(super) fn drained_peer_count(messages: &[QueuedPeerMessage]) -> usize {
    messages
        .iter()
        .filter(|m| m.from_id.parse::<usize>().is_ok())
        .count()
}
