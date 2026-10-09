use super::*;

pub(crate) const CODEX_APPEND_ENTER_DELAY: Duration = Duration::from_millis(75);
pub(crate) const CODEX_PEER_NUDGE_SUBMIT_DELAY: Duration = Duration::from_millis(1000);
pub(crate) const CODEX_APPEND_ENTER_SNAPSHOT_LINES: usize = 8;
/// How long a nudge may sit undelivered as a draft before the pane is
/// reported as stalled (`peer_nudge_stalled` + title badge). A Codex UI
/// change that the readiness heuristic no longer recognizes would
/// otherwise keep the nudge queued forever without any signal (#354).
pub(crate) const CODEX_PEER_NUDGE_STALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Smallest terminal (cols, rows) the focused-pane notification box
/// can be drawn in. Below it the overlay must not count as visible,
/// or Esc / Alt+Enter would be swallowed by a box nobody can see.
pub(crate) const CODEX_PEER_NOTIFICATION_MIN_SIZE: (u16, u16) = (44, 5);

/// Window during which a `(target, from, body)` triple is treated as
/// a re-send and dropped before reaching `Event::PeerInbox`. Set to a
/// small handful of seconds so legitimate retries after the
/// receiver's reply still get through, but a dispatcher / worker
/// that fires the exact same payload twice in quick succession
/// can't double-paper the transcript with phantom user turns. See
/// renga#221 acceptance criterion #2.
pub(crate) const PEER_SEND_DEDUPE_TTL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingCodexPeerMessage {
    pub(crate) from_pane: usize,
    pub(crate) from_name: Option<String>,
    pub(crate) from_kind: Option<PeerClientKind>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexPeerNotificationState {
    pub(crate) target_pane: usize,
    pub(crate) message: PendingCodexPeerMessage,
    pub(crate) pending_count: usize,
    /// Hidden because the human kept typing into the pane. The
    /// notification is parked, not dropped: it hands off to the
    /// deferred nudge once focus leaves, and a newer message shows it
    /// again. Before this, any stray keystroke discarded it and the
    /// queued request was never nudged at all (Issue #197).
    pub(crate) snoozed: bool,
    /// When the oldest message this notification covers was first
    /// queued. Carried across the overlay <-> deferred-nudge handoffs
    /// so a focus round trip does not restart the stall clock (#354).
    pub(crate) queued_at: Instant,
}

impl CodexPeerNotificationState {
    fn register_messages(
        &mut self,
        message: PendingCodexPeerMessage,
        count: usize,
        queued_at: Instant,
    ) {
        self.message = message;
        self.pending_count = self.pending_count.saturating_add(count);
        self.snoozed = false;
        self.queued_at = self.queued_at.min(queued_at);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PendingCodexPeerDelivery {
    /// Latest message plus how many arrived since the last nudge, so a
    /// notification parked in the queue keeps its count when promoted
    /// back to the overlay. The `Instant` is when the oldest of them
    /// was queued, which is what the stall timeout measures.
    Draft(PendingCodexPeerMessage, usize, Instant),
    SubmitAt(Instant),
}

pub(crate) fn screen_tail_lines(screen: &vt100::Screen) -> Vec<String> {
    let (rows, cols) = screen.size();
    let (cursor_row, _) = screen.cursor_position();
    let mut last_content_row = None;
    for row in 0..rows {
        let mut has_text = false;
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col) {
                if !cell.contents().trim().is_empty() {
                    has_text = true;
                    break;
                }
            }
        }
        if has_text {
            last_content_row = Some(row);
        }
    }
    let end_row = last_content_row.unwrap_or(cursor_row).max(cursor_row);
    let start_row = end_row
        .saturating_add(1)
        .saturating_sub(CODEX_APPEND_ENTER_SNAPSHOT_LINES as u16);
    let mut lines =
        Vec::with_capacity(end_row.saturating_sub(start_row).saturating_add(1) as usize);
    for row in start_row..=end_row {
        let mut line = String::with_capacity(cols as usize);
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col) {
                line.push_str(cell.contents());
            }
        }
        lines.push(line.trim_end().to_string());
    }
    lines
}

pub(crate) fn screen_has_visible_text(screen: &vt100::Screen) -> bool {
    let (rows, cols) = screen.size();
    for row in 0..rows {
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col) {
                if !cell.contents().trim().is_empty() {
                    return true;
                }
            }
        }
    }
    false
}

pub(crate) fn codex_prompt_allows_peer_nudge_on_screen(screen: &vt100::Screen) -> Option<bool> {
    if screen.hide_cursor() {
        return Some(false);
    }
    let (rows, cols) = screen.size();
    let mut last_content_row = None;
    for row in 0..rows {
        let mut has_text = false;
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col) {
                if !cell.contents().trim().is_empty() {
                    has_text = true;
                    break;
                }
            }
        }
        if has_text {
            last_content_row = Some(row);
        }
    }
    let mut prompt_row = None;
    let (cursor_row, cursor_col) = screen.cursor_position();
    let end_row = last_content_row.unwrap_or(cursor_row).max(cursor_row);
    let start_row = end_row
        .saturating_add(1)
        .saturating_sub(CODEX_APPEND_ENTER_SNAPSHOT_LINES as u16);
    for row in (start_row..=end_row).rev() {
        let mut line = String::with_capacity(cols as usize);
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col) {
                line.push_str(cell.contents());
            }
        }
        if line.trim_start().starts_with('›') {
            prompt_row = Some(row);
            break;
        }
    }
    let prompt_row = prompt_row?;
    if cursor_row > prompt_row {
        return Some(false);
    }
    if cursor_row == prompt_row && cursor_col > 2 {
        return Some(false);
    }
    Some(true)
}

/// Codex is mid-turn: its working line or queue hint is on screen.
fn codex_screen_busy(screen: &vt100::Screen) -> bool {
    let tail = screen_tail_lines(screen).join("\n").to_ascii_lowercase();
    tail.contains("esc to interrupt") || tail.contains("tab to queue message")
}

/// Whether a Codex screen may receive a typed peer nudge right now.
/// Pinned against real captured Codex screens in
/// `src/app/tests/fixtures/codex/`.
pub(crate) fn codex_peer_screen_ready(screen: &vt100::Screen) -> bool {
    if codex_screen_busy(screen) {
        return false;
    }
    let tail = screen_tail_lines(screen).join("\n").to_ascii_lowercase();
    if !screen_has_visible_text(screen) {
        return false;
    }
    if let Some(allowed) = codex_prompt_allows_peer_nudge_on_screen(screen) {
        return allowed;
    }
    // No `›` composer, so only a banner vouches for readiness. Demand
    // what the composer path proves with the caret as well — a visible
    // cursor at the edit position of an otherwise blank row — or the
    // nudge lands appended to whatever draft the human left (#354).
    if !(tail.contains("enter to send") || tail.contains("ready for input")) {
        return false;
    }
    if screen.hide_cursor() {
        return false;
    }
    let (cursor_row, cursor_col) = screen.cursor_position();
    let cols = screen.size().1;
    let line: String = (0..cols)
        .filter_map(|col| screen.cell(cursor_row, col))
        .map(|cell| cell.contents())
        .collect();
    // An optional prompt glyph (`>`, `▌`, ...) and nothing else.
    let rest = line.trim_start();
    let rest = rest
        .strip_prefix(|c: char| !c.is_alphanumeric())
        .unwrap_or(rest);
    cursor_col <= 2 && rest.trim().is_empty()
}

fn pending_startup_looks_like_codex(pane: &Pane) -> bool {
    pane.pending_startup
        .as_ref()
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .is_some_and(|text| text.trim_start().starts_with("codex"))
}

pub(crate) fn format_codex_peer_message(msg: &PendingCodexPeerMessage) -> String {
    let mut header = format!("Peer request from id={}", msg.from_pane);
    if let Some(name) = &msg.from_name {
        // This string is typed into the target pane's PTY and followed
        // by Enter, so a control character in the sender's name is a
        // prompt injection into someone else's composer, not a display
        // glitch. `split` / `new_tab` accepted names verbatim before
        // #289 widened delivery to every tab.
        header.push_str(&format!(" name={}", ipc::sanitized_label(name)));
    }
    if let Some(kind) = msg.from_kind {
        let kind = match kind {
            PeerClientKind::Claude => "claude",
            PeerClientKind::Codex => "codex",
        };
        header.push_str(&format!(" kind={kind}"));
    }
    let guidance = "Run check_messages now. Treat each returned message as a direct coworker request: do the requested work, and use send_message only when a reply or status update is needed.";
    format!("{header}. {guidance}")
}

pub(crate) fn write_input_to_pane(
    pane: &mut Pane,
    data: &[u8],
    append_enter: bool,
) -> std::result::Result<(), ipc::CodedError> {
    pane.write_input(data)
        .map_err(|e| ipc::CodedError::new(ipc::err_code::IO_ERROR, e.to_string()))?;
    if append_enter {
        if !data.is_empty() && (pane.is_codex_running() || pending_startup_looks_like_codex(pane)) {
            std::thread::sleep(CODEX_APPEND_ENTER_DELAY);
        }
        pane.write_input(b"\r")
            .map_err(|e| ipc::CodedError::new(ipc::err_code::IO_ERROR, e.to_string()))?;
    }
    Ok(())
}

impl App {
    /// Route `body` from `from_pane` to `target` — in any tab since
    /// Issue #289 dropped the same-tab restriction. Target resolution
    /// is caller-scoped ([`Self::resolve_target_from`]): a numeric id
    /// reaches every tab, while a name or `focused` stays inside the
    /// *sender's* workspace — names are only unique per tab, so
    /// resolving them against the tab the human happens to be viewing
    /// would misroute background-tab senders. An unresolvable target
    /// fails with `pane_not_found` instead of pretending to deliver.
    /// Self-sends loop back to the sender pane: tooling like
    /// claude-org-ja's peer_notify resolves "secretary" from a shell
    /// running inside the secretary pane, and a silent drop there
    /// breaks the notification round-trip (see renga#215).
    pub(crate) fn handle_peer_send(
        &mut self,
        from_pane: usize,
        target: &PaneRef,
        body: String,
    ) -> std::result::Result<(), ipc::CodedError> {
        let (sender_ws, _) = self
            .resolve_pane_across_workspaces(&PaneRef::Id(from_pane))
            .ok_or_else(|| {
                ipc::CodedError::new(
                    ipc::err_code::PANE_NOT_FOUND,
                    format!("sender pane {from_pane} not found"),
                )
            })?;
        let (target_ws, target_id) =
            self.resolve_target_from(sender_ws, target).ok_or_else(|| {
                ipc::CodedError::new(
                    ipc::err_code::PANE_NOT_FOUND,
                    format!(
                        "peer target not found: {target:?} (names only resolve inside the \
                         sender's tab; use the numeric pane id from list_peers for other tabs)"
                    ),
                )
            })?;
        if self.is_duplicate_peer_send(target_id, from_pane, &body) {
            // Same (target, from, body) within the dedupe window —
            // treat as a no-op so duplicate dispatcher acks /
            // worker false-fires don't paper the receiver's
            // transcript with phantom Human: turns. The sender
            // gets a successful Ok() reply so it can't probe the
            // dedupe state. (renga#221)
            return Ok(());
        }
        self.materialize_unfocused_codex_peer_notification();
        let from_name = self.workspaces[sender_ws]
            .pane_names
            .iter()
            .find(|(_, id)| **id == from_pane)
            .map(|(n, _)| n.clone());
        let from_kind = self.peer_client_kinds.get(&from_pane).copied();
        let pull_mode = self.pane_expects_codex_peer_delivery(target_ws, target_id);
        if pull_mode {
            let message = PendingCodexPeerMessage {
                from_pane,
                from_name: from_name.clone(),
                from_kind,
            };
            let target_is_focused = self.active_tab == target_ws
                && self.workspaces[target_ws].focus_target == FocusTarget::Pane
                && self.workspaces[target_ws].focused_pane_id == target_id;
            if target_is_focused {
                // A draft still queued here is the same undelivered
                // nudge; its age carries into the overlay (#354).
                let queued_at = match self
                    .pending_codex_peer_messages
                    .remove(&target_id)
                    .and_then(|mut q| q.pop_front())
                {
                    Some(PendingCodexPeerDelivery::Draft(_, _, queued_at)) => queued_at,
                    _ => Instant::now(),
                };
                match self.codex_peer_notification.as_mut() {
                    Some(notification) if notification.target_pane == target_id => {
                        notification.register_messages(message, 1, queued_at);
                    }
                    _ => {
                        self.codex_peer_notification = Some(CodexPeerNotificationState {
                            target_pane: target_id,
                            message,
                            pending_count: 1,
                            snoozed: false,
                            queued_at,
                        });
                    }
                }
                self.dirty = true;
            } else {
                self.push_pending_codex_peer_nudge(target_id, message, 1, Instant::now());
            }
        }
        let msg_id = self.next_peer_msg_id;
        self.next_peer_msg_id += 1;
        let reached_inbox = self.event_bus.emit(ipc::Event::PeerInbox {
            target_pane: target_id,
            from_pane,
            from_name,
            from_kind,
            body,
            ts_ms: ipc::events::now_ms(),
            msg_id: Some(msg_id),
        });
        // Count only what actually entered the pane's MCP inbox: a
        // message sent before its subprocess subscribed, or dropped on
        // a full queue, can never be drained (Issue #353).
        if pull_mode && reached_inbox {
            self.peer_unread
                .entry(target_id)
                .or_default()
                .insert(msg_id);
        }
        self.sync_peer_delivery();
        Ok(())
    }

    /// Return true when an identical (target, from, body) peer send
    /// arrived within [`PEER_SEND_DEDUPE_TTL`]. A side effect
    /// records the new send so future calls compare against it,
    /// and stale entries (older than the TTL) are evicted on every
    /// call so the map can't grow unbounded under heavy traffic.
    fn is_duplicate_peer_send(&mut self, target: usize, from: usize, body: &str) -> bool {
        let now = Instant::now();
        self.recent_peer_sends
            .retain(|_, ts| now.duration_since(*ts) < PEER_SEND_DEDUPE_TTL);
        let key = (target, from, body.to_string());
        match self.recent_peer_sends.get(&key).copied() {
            Some(prev) if now.duration_since(prev) < PEER_SEND_DEDUPE_TTL => {
                // Refresh the timestamp so a chatty sender keeps
                // getting its retries collapsed instead of slipping
                // a duplicate through right at the TTL boundary.
                self.recent_peer_sends.insert(key, now);
                true
            }
            _ => {
                self.recent_peer_sends.insert(key, now);
                false
            }
        }
    }

    pub(crate) fn handle_peer_register_client(
        &mut self,
        pane_id: usize,
        kind: PeerClientKind,
    ) -> std::result::Result<(), ipc::CodedError> {
        self.resolve_pane_across_workspaces(&PaneRef::Id(pane_id))
            .ok_or_else(|| {
                ipc::CodedError::new(
                    ipc::err_code::PANE_NOT_FOUND,
                    format!("pane {pane_id} not found for peer registration"),
                )
            })?;
        self.peer_client_kinds.insert(pane_id, kind);
        // A (re)started MCP subprocess begins with an empty inbox, so
        // whatever was counted against the previous one can never be
        // drained now.
        self.peer_unread.remove(&pane_id);
        self.sync_peer_delivery();
        Ok(())
    }

    pub(crate) fn peer_unread_count(&self, pane_id: usize) -> usize {
        self.peer_unread.get(&pane_id).map_or(0, BTreeSet::len)
    }

    /// The pane's agent drained `count` peer messages (Issue #353),
    /// named by `ids` (Issue #369). Pending nudge state is dropped only
    /// once nothing is left unread: a message sent between the drain
    /// and this report is still in the inbox and still needs its nudge.
    /// Acking by id makes a second report of the same messages (two
    /// subscribers bound to one pane) a no-op. Without ids (a pre-#369
    /// client) the oldest `count` messages are cleared, as before.
    ///
    /// Messages still queued when the MCP subprocess dies stay unread
    /// until its successor registers.
    pub(crate) fn handle_peer_inbox_drained(
        &mut self,
        pane_id: usize,
        count: usize,
        ids: &[u64],
    ) -> std::result::Result<(), ipc::CodedError> {
        self.resolve_pane_across_workspaces(&PaneRef::Id(pane_id))
            .ok_or_else(|| {
                ipc::CodedError::new(
                    ipc::err_code::PANE_NOT_FOUND,
                    format!("pane {pane_id} not found for inbox drain"),
                )
            })?;
        let mut unread = self.peer_unread.remove(&pane_id).unwrap_or_default();
        if ids.is_empty() {
            for _ in 0..count {
                unread.pop_first();
            }
        } else {
            for id in ids {
                unread.remove(id);
            }
        }
        let left = unread.len();
        if left > 0 {
            self.peer_unread.insert(pane_id, unread);
            if let Some(n) = self
                .codex_peer_notification
                .as_mut()
                .filter(|n| n.target_pane == pane_id)
            {
                n.pending_count = n.pending_count.min(left);
            }
        } else {
            // An already-typed nudge (`SubmitAt`) is left to finish:
            // dropping it would strand the half-written draft in the
            // composer.
            if matches!(
                self.pending_codex_peer_messages
                    .get(&pane_id)
                    .and_then(|q| q.front()),
                Some(PendingCodexPeerDelivery::Draft(..))
            ) {
                self.pending_codex_peer_messages.remove(&pane_id);
            }
            if self
                .codex_peer_notification
                .as_ref()
                .is_some_and(|n| n.target_pane == pane_id)
            {
                self.codex_peer_notification = None;
                self.dirty = true;
            }
        }
        self.event_bus.emit(ipc::Event::PeerInboxDrained {
            pane: pane_id,
            count,
            ts_ms: ipc::events::now_ms(),
        });
        self.sync_peer_delivery();
        Ok(())
    }

    fn push_pending_codex_peer_nudge(
        &mut self,
        pane_id: usize,
        message: PendingCodexPeerMessage,
        count: usize,
        queued_at: Instant,
    ) {
        let queue = self.pending_codex_peer_messages.entry(pane_id).or_default();
        match queue.front_mut() {
            None => queue.push_back(PendingCodexPeerDelivery::Draft(message, count, queued_at)),
            Some(PendingCodexPeerDelivery::Draft(latest, pending, oldest)) => {
                *latest = message;
                *pending = pending.saturating_add(count);
                *oldest = (*oldest).min(queued_at);
            }
            // The nudge is already typed; check_messages drains this one too.
            Some(PendingCodexPeerDelivery::SubmitAt(_)) => {}
        }
    }

    fn codex_peer_notification_fits(&self) -> bool {
        let (cols, rows) = self.last_term_size;
        let (min_cols, min_rows) = CODEX_PEER_NOTIFICATION_MIN_SIZE;
        // Below the UI's own minimum nothing but "too small" is drawn.
        cols >= min_cols.max(crate::ui::MIN_TERMINAL_WIDTH)
            && rows >= min_rows.max(crate::ui::MIN_TERMINAL_HEIGHT)
    }

    /// A live notification for the watched pane that the terminal is
    /// too small to draw. The status bar says so instead.
    pub(crate) fn codex_peer_notification_needs_hint(&self) -> Option<usize> {
        let n = self.codex_peer_notification.as_ref()?;
        (!n.snoozed
            && self.overlay.is_none()
            && !self.codex_peer_notification_fits()
            && self.codex_peer_notification_target_is_watched())
        .then_some(n.pending_count)
    }

    fn codex_peer_notification_target_is_watched(&self) -> bool {
        let Some(notification) = self.codex_peer_notification.as_ref() else {
            return false;
        };
        self.ws().focus_target == FocusTarget::Pane
            && self.ws().focused_pane_id == notification.target_pane
            && self.ws().panes.contains_key(&notification.target_pane)
    }

    pub(crate) fn codex_peer_notification_is_visible(&self) -> bool {
        self.overlay.is_none()
            && self
                .codex_peer_notification
                .as_ref()
                .is_some_and(|n| !n.snoozed)
            && self.codex_peer_notification_fits()
            && self.codex_peer_notification_target_is_watched()
    }

    pub(crate) fn visible_codex_peer_notification(&self) -> Option<&CodexPeerNotificationState> {
        self.codex_peer_notification_is_visible()
            .then_some(self.codex_peer_notification.as_ref())
            .flatten()
    }

    pub(crate) fn dismiss_codex_peer_notification(&mut self) {
        if let Some(n) = self.codex_peer_notification.take() {
            // Nothing was handed to the pane, so its unread messages
            // must not read as `nudged` (see `sync_peer_delivery`).
            if let Some((ws_idx, _)) =
                self.resolve_pane_across_workspaces(&PaneRef::Id(n.target_pane))
            {
                if let Some(pane) = self.workspaces[ws_idx].panes.get_mut(&n.target_pane) {
                    pane.peer_delivery = None;
                }
            }
            self.dirty = true;
        }
    }

    pub(crate) fn snooze_codex_peer_notification(&mut self) {
        if let Some(notification) = self.codex_peer_notification.as_mut() {
            notification.snoozed = true;
            self.dirty = true;
        }
    }

    fn materialize_unfocused_codex_peer_notification(&mut self) {
        let Some(notification) = self.codex_peer_notification.clone() else {
            return;
        };
        // A snoozed or too-small-to-draw notification stays parked
        // while the human is still on the pane; it only becomes a PTY
        // nudge once they leave.
        if self.codex_peer_notification_is_visible()
            || ((notification.snoozed || !self.codex_peer_notification_fits())
                && self.codex_peer_notification_target_is_watched())
        {
            return;
        }
        if self
            .resolve_pane_across_workspaces(&PaneRef::Id(notification.target_pane))
            .is_some()
        {
            self.push_pending_codex_peer_nudge(
                notification.target_pane,
                notification.message,
                notification.pending_count,
                notification.queued_at,
            );
        }
        self.codex_peer_notification = None;
        self.dirty = true;
    }

    pub(crate) fn accept_codex_peer_notification(
        &mut self,
    ) -> std::result::Result<bool, ipc::CodedError> {
        let Some(notification) = self.codex_peer_notification.clone() else {
            return Ok(false);
        };
        if !self.codex_peer_notification_is_visible() {
            return Ok(false);
        }
        let payload = crate::mcp_peer::build_send_keys_payload(
            &format_codex_peer_message(&notification.message),
            None,
            false,
        )
        .expect("codex peer notification payload");
        let pane = self
            .ws_mut()
            .panes
            .get_mut(&notification.target_pane)
            .ok_or_else(|| ipc::CodedError::new(ipc::err_code::PANE_VANISHED, "pane vanished"))?;
        write_input_to_pane(pane, payload.as_bytes(), false)?;
        self.pending_codex_peer_messages
            .remove(&notification.target_pane);
        self.codex_peer_notification = None;
        self.dirty = true;
        self.emit_peer_nudge_submitted(notification.target_pane);
        self.sync_peer_delivery();
        Ok(true)
    }

    fn pane_name_and_role(&self, pane_id: usize) -> (Option<String>, Option<String>) {
        let Some((ws_idx, _)) = self.resolve_pane_across_workspaces(&PaneRef::Id(pane_id)) else {
            return (None, None);
        };
        let ws = &self.workspaces[ws_idx];
        (
            ws.pane_names
                .iter()
                .find(|(_, id)| **id == pane_id)
                .map(|(n, _)| n.clone()),
            ws.panes.get(&pane_id).and_then(|p| p.role.clone()),
        )
    }

    /// Only for a pane tracked as `queued`, so the event always pairs
    /// with a `peer_nudge_queued` (an Enter after a full drain, or for
    /// a message that never reached an inbox, owes nothing).
    fn emit_peer_nudge_submitted(&self, pane_id: usize) {
        let Some((ws_idx, _)) = self.resolve_pane_across_workspaces(&PaneRef::Id(pane_id)) else {
            return;
        };
        let tracked = self.workspaces[ws_idx]
            .panes
            .get(&pane_id)
            .and_then(|p| p.peer_delivery)
            .is_some_and(|d| d.state == ipc::PeerDeliveryState::Queued);
        if !tracked {
            return;
        }
        let (name, role) = self.pane_name_and_role(pane_id);
        self.event_bus.emit(ipc::Event::PeerNudgeSubmitted {
            id: pane_id,
            name,
            role,
            pending: self.peer_unread_count(pane_id),
            ts_ms: ipc::events::now_ms(),
        });
    }

    /// What is still owed to `pane_id` (Issue #352): `Queued` while
    /// renga holds the nudge (draft, overlay, or typed and awaiting its
    /// Enter), `Nudged` once it is in the pane but `check_messages` has
    /// not drained the messages. `pending` is the unread inbox count;
    /// messages that never reached an inbox (or were wiped by a
    /// re-registration) cannot be read, so they are not owed.
    pub(crate) fn derive_peer_delivery(
        &self,
        pane_id: usize,
    ) -> Option<(ipc::PeerDeliveryState, usize)> {
        let unread = self.peer_unread_count(pane_id);
        if unread == 0 {
            return None;
        }
        let holding = self
            .pending_codex_peer_messages
            .get(&pane_id)
            .is_some_and(|q| !q.is_empty())
            || self
                .codex_peer_notification
                .as_ref()
                .is_some_and(|n| n.target_pane == pane_id);
        let state = if holding {
            ipc::PeerDeliveryState::Queued
        } else {
            ipc::PeerDeliveryState::Nudged
        };
        Some((state, unread))
    }

    /// Refresh every pane's `peer_delivery` badge state and emit
    /// `peer_nudge_queued` when a pane starts holding a nudge.
    fn sync_peer_delivery(&mut self) {
        let now_ms = ipc::events::now_ms();
        for ws_idx in 0..self.workspaces.len() {
            let pane_ids: Vec<usize> = self.workspaces[ws_idx].panes.keys().copied().collect();
            for pane_id in pane_ids {
                let derived = self.derive_peer_delivery(pane_id);
                let Some(pane) = self.workspaces[ws_idx].panes.get_mut(&pane_id) else {
                    continue;
                };
                let prev = pane.peer_delivery;
                // `Nudged` only continues a tracked delivery: unread
                // messages whose nudge was dismissed were never handed
                // to the pane.
                let derived = derived.filter(|(state, _)| {
                    *state == ipc::PeerDeliveryState::Queued || prev.is_some()
                });
                let next = derived.map(|(state, pending)| ipc::PeerDeliveryStatus {
                    state,
                    pending,
                    since_ms: prev
                        .filter(|p| p.state == state)
                        .map_or(now_ms, |p| p.since_ms),
                });
                if prev == next {
                    continue;
                }
                pane.peer_delivery = next;
                self.dirty = true;
                let queued = |d: Option<ipc::PeerDeliveryStatus>| {
                    d.is_some_and(|d| d.state == ipc::PeerDeliveryState::Queued)
                };
                if queued(next) && !queued(prev) {
                    let (name, role) = self.pane_name_and_role(pane_id);
                    self.event_bus.emit(ipc::Event::PeerNudgeQueued {
                        id: pane_id,
                        name,
                        role,
                        pending: next.map_or(0, |d| d.pending),
                        ts_ms: now_ms,
                    });
                }
            }
        }
    }

    pub(crate) fn pane_expects_codex_peer_delivery(&self, ws_index: usize, pane_id: usize) -> bool {
        // Registration is authoritative when present. Without this
        // short-circuit a Claude-registered pane whose current OSC
        // title transiently contains the substring "codex" (very
        // common for orchestration workers debugging Codex-related
        // issues) would fall through to the title heuristic and be
        // mis-classified as a Codex recipient — see issue #209's
        // discussion of the related #208 regression.
        match self.peer_client_kinds.get(&pane_id) {
            Some(PeerClientKind::Codex) => return true,
            Some(PeerClientKind::Claude) => return false,
            None => {}
        }
        self.workspaces[ws_index]
            .panes
            .get(&pane_id)
            .is_some_and(|pane| pane.is_codex_running() || pending_startup_looks_like_codex(pane))
    }

    pub(crate) fn codex_peer_delivery_ready(registered_codex: bool, pane: &Pane) -> bool {
        if !registered_codex && !pane.is_codex_running() {
            return false;
        }
        let Ok(parser) = pane.parser.lock() else {
            return false;
        };
        codex_peer_screen_ready(parser.screen())
    }

    pub(crate) fn flush_pending_codex_peer_messages(&mut self) {
        self.materialize_unfocused_codex_peer_notification();
        let now = Instant::now();
        let active_tab = self.active_tab;
        // A user-turn delivery owns that composer until it finishes.
        // This flush runs first in the frame, so typing a nudge into a
        // composer that already holds a body would make the submitted
        // turn the concatenation of the two (Issue #323).
        let user_turn_panes = self.panes_with_user_turn_in_flight();
        let mut empty_panes = Vec::new();
        let mut submitted = Vec::new();
        for (ws_idx, ws) in self.workspaces.iter_mut().enumerate() {
            let pane_ids: Vec<usize> = ws.panes.keys().copied().collect();
            for pane_id in pane_ids {
                // Report a nudge that has waited out the stall timeout,
                // whatever is holding it back (unrecognized screen,
                // overlay unanswered, user turn in flight). The queued
                // draft and the overlay share one clock, so a focus
                // round trip neither clears nor re-fires it. Recomputed
                // every flush, so any path that delivers or drops the
                // nudge clears it. A busy Codex is expected to hold the
                // nudge, so the clock does not run while it is busy: it
                // is held at `now` and only counts once the turn ends.
                let clock = match self
                    .pending_codex_peer_messages
                    .get_mut(&pane_id)
                    .and_then(|q| q.front_mut())
                {
                    Some(PendingCodexPeerDelivery::Draft(_, _, queued_at)) => Some(queued_at),
                    _ => None,
                }
                .or_else(|| {
                    self.codex_peer_notification
                        .as_mut()
                        .filter(|n| n.target_pane == pane_id)
                        .map(|n| &mut n.queued_at)
                });
                let stalled_for = clock
                    .map(|queued_at| {
                        let busy = ws.panes.get(&pane_id).is_some_and(|pane| {
                            pane.parser
                                .lock()
                                .is_ok_and(|parser| codex_screen_busy(parser.screen()))
                        });
                        if busy {
                            *queued_at = now;
                        }
                        now.saturating_duration_since(*queued_at)
                    })
                    .filter(|waited| *waited >= CODEX_PEER_NUDGE_STALL_TIMEOUT);
                if let Some(pane) = ws.panes.get_mut(&pane_id) {
                    if pane.peer_nudge_stalled != stalled_for.is_some() {
                        pane.peer_nudge_stalled = stalled_for.is_some();
                        self.dirty = true;
                        if let Some(waited) = stalled_for {
                            self.event_bus.emit(ipc::Event::PeerNudgeStalled {
                                id: pane_id,
                                name: ws
                                    .pane_names
                                    .iter()
                                    .find(|(_, id)| **id == pane_id)
                                    .map(|(n, _)| n.clone()),
                                role: pane.role.clone(),
                                queued_ms: waited.as_millis() as u64,
                                ts_ms: ipc::events::now_ms(),
                            });
                        }
                    }
                }
                if user_turn_panes.contains(&pane_id) {
                    continue;
                }
                // Only the pane the human is actually looking at is
                // exempt from PTY nudges (the focused-pane overlay
                // covers it). A background tab's `focused_pane_id` is
                // just a bookmark — skipping it too would strand
                // cross-tab nudges forever on single-pane tabs, where
                // the only pane is always the workspace-focused one
                // (Issue #289). Focus on the file tree / preview of
                // the same tab is "away" too: the human cannot type
                // into the pane, so the nudge goes out (Issue #355).
                if ws_idx == active_tab
                    && ws.focus_target == FocusTarget::Pane
                    && ws.focused_pane_id == pane_id
                {
                    // A nudge that was queued while the pane was hidden
                    // would otherwise stall for as long as the human
                    // stays on it — no overlay exists because
                    // `handle_peer_send` only creates one when the
                    // target was focused *at send time*.
                    match self
                        .pending_codex_peer_messages
                        .get(&pane_id)
                        .and_then(|q| q.front())
                        .cloned()
                    {
                        // Promote a still-undelivered draft into the
                        // notification overlay (the designed UX for a
                        // focused target); the inverse of
                        // `materialize_unfocused_...`, and only when
                        // the overlay would be immediately visible so
                        // the two conversions cannot fight. If the
                        // overlay is busy elsewhere, stay queued and
                        // retry on a later flush.
                        Some(PendingCodexPeerDelivery::Draft(message, count, queued_at))
                            if self.overlay.is_none() =>
                        {
                            match self.codex_peer_notification.as_mut() {
                                Some(n) if n.target_pane == pane_id => {
                                    n.register_messages(message, count, queued_at);
                                    self.pending_codex_peer_messages.remove(&pane_id);
                                    self.dirty = true;
                                }
                                None => {
                                    self.pending_codex_peer_messages.remove(&pane_id);
                                    self.codex_peer_notification =
                                        Some(CodexPeerNotificationState {
                                            target_pane: pane_id,
                                            message,
                                            pending_count: count,
                                            snoozed: false,
                                            queued_at,
                                        });
                                    self.dirty = true;
                                }
                                Some(_) => {}
                            }
                        }
                        // A half-delivered nudge loses its owner the
                        // moment the human watches the pane: the typed
                        // draft is on screen for them to submit or
                        // edit, and once they can touch the composer a
                        // deferred Enter could submit *their* content,
                        // not ours. Cancel the pending submit instead
                        // of resuming it later (Codex review of #289).
                        // The typed nudge is the human's to submit now.
                        Some(PendingCodexPeerDelivery::SubmitAt(_)) => {
                            self.pending_codex_peer_messages.remove(&pane_id);
                            self.dirty = true;
                            submitted.push(pane_id);
                        }
                        _ => {}
                    }
                    continue;
                }
                let Some(queue) = self.pending_codex_peer_messages.get_mut(&pane_id) else {
                    continue;
                };
                let Some(delivery) = queue.front().cloned() else {
                    empty_panes.push(pane_id);
                    continue;
                };
                if let Some(pane) = ws.panes.get_mut(&pane_id) {
                    match delivery {
                        PendingCodexPeerDelivery::Draft(message, _, _) => {
                            let registered_codex = self.peer_client_kinds.get(&pane_id)
                                == Some(&PeerClientKind::Codex);
                            if !Self::codex_peer_delivery_ready(registered_codex, pane) {
                                continue;
                            }
                            let payload = crate::mcp_peer::build_send_keys_payload(
                                &format_codex_peer_message(&message),
                                None,
                                false,
                            )
                            .expect("codex peer draft payload");
                            if write_input_to_pane(pane, payload.as_bytes(), false).is_ok() {
                                queue.pop_front();
                                queue.push_front(PendingCodexPeerDelivery::SubmitAt(
                                    now + CODEX_PEER_NUDGE_SUBMIT_DELAY,
                                ));
                                self.dirty = true;
                            }
                        }
                        PendingCodexPeerDelivery::SubmitAt(ready_at) => {
                            if now < ready_at {
                                continue;
                            }
                            let payload = crate::mcp_peer::build_send_keys_payload("", None, true)
                                .expect("codex peer submit payload");
                            if write_input_to_pane(pane, payload.as_bytes(), false).is_ok() {
                                queue.pop_front();
                                self.dirty = true;
                                submitted.push(pane_id);
                            }
                        }
                    }
                }
                if queue.is_empty() {
                    empty_panes.push(pane_id);
                }
            }
        }
        for pane_id in empty_panes {
            self.pending_codex_peer_messages.remove(&pane_id);
        }
        for pane_id in submitted {
            self.emit_peer_nudge_submitted(pane_id);
        }
        self.sync_peer_delivery();
    }
}
