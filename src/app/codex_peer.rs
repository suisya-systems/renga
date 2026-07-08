use super::*;

pub(crate) const CODEX_APPEND_ENTER_DELAY: Duration = Duration::from_millis(75);
pub(crate) const CODEX_PEER_NUDGE_SUBMIT_DELAY: Duration = Duration::from_millis(1000);
pub(crate) const CODEX_APPEND_ENTER_SNAPSHOT_LINES: usize = 8;

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PendingCodexPeerDelivery {
    Draft(PendingCodexPeerMessage),
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

fn pane_screen_tail_lines(pane: &Pane) -> Option<Vec<String>> {
    let parser = pane.parser.lock().ok()?;
    Some(screen_tail_lines(parser.screen()))
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

fn pane_screen_has_visible_text(pane: &Pane) -> bool {
    let Ok(parser) = pane.parser.lock() else {
        return false;
    };
    screen_has_visible_text(parser.screen())
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

fn codex_prompt_allows_peer_nudge(pane: &Pane) -> Option<bool> {
    let Ok(parser) = pane.parser.lock() else {
        return None;
    };
    codex_prompt_allows_peer_nudge_on_screen(parser.screen())
}

fn codex_peer_screen_tail(pane: &Pane) -> Option<String> {
    Some(
        pane_screen_tail_lines(pane)?
            .join("\n")
            .to_ascii_lowercase(),
    )
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
        header.push_str(&format!(" name={name}"));
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
    /// Route `body` from `from_pane` to `target` when both share a
    /// workspace. Cross-tab targets are silently dropped so the MCP
    /// server cannot enumerate panes in other tabs by probing ids.
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
        let (target_ws, target_id) = match self.resolve_pane_across_workspaces(target) {
            Some(pair) => pair,
            None => return Ok(()),
        };
        if sender_ws != target_ws {
            return Ok(());
        }
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
        if self.pane_expects_codex_peer_delivery(target_ws, target_id) {
            let message = PendingCodexPeerMessage {
                from_pane,
                from_name: from_name.clone(),
                from_kind,
            };
            let target_is_focused = self.active_tab == target_ws
                && self.workspaces[target_ws].focus_target == FocusTarget::Pane
                && self.workspaces[target_ws].focused_pane_id == target_id;
            if target_is_focused {
                self.pending_codex_peer_messages.remove(&target_id);
                if self
                    .codex_peer_notification
                    .as_ref()
                    .is_some_and(|notification| notification.target_pane == target_id)
                {
                    self.codex_peer_notification = None;
                }
                self.send_codex_peer_nudge_now(target_ws, target_id, &message)?;
            } else {
                self.push_pending_codex_peer_nudge(target_id, message);
            }
        }
        self.event_bus.emit(ipc::Event::PeerInbox {
            target_pane: target_id,
            from_pane,
            from_name,
            from_kind,
            body,
            ts_ms: ipc::events::now_ms(),
        });
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
        Ok(())
    }

    fn push_pending_codex_peer_nudge(&mut self, pane_id: usize, message: PendingCodexPeerMessage) {
        let queue = self.pending_codex_peer_messages.entry(pane_id).or_default();
        if queue.is_empty() {
            queue.push_back(PendingCodexPeerDelivery::Draft(message));
        }
    }

    fn send_codex_peer_nudge_now(
        &mut self,
        ws_index: usize,
        pane_id: usize,
        message: &PendingCodexPeerMessage,
    ) -> std::result::Result<(), ipc::CodedError> {
        let payload = crate::mcp_peer::build_send_keys_payload(
            &format_codex_peer_message(message),
            None,
            true,
        )
        .expect("codex peer focused nudge payload");
        let pane = self
            .workspaces
            .get_mut(ws_index)
            .and_then(|ws| ws.panes.get_mut(&pane_id))
            .ok_or_else(|| ipc::CodedError::new(ipc::err_code::PANE_VANISHED, "pane vanished"))?;
        write_input_to_pane(pane, payload.as_bytes(), false)?;
        self.dirty = true;
        Ok(())
    }

    pub(crate) fn codex_peer_notification_is_visible(&self) -> bool {
        if self.overlay.is_some() {
            return false;
        }
        let Some(notification) = self.codex_peer_notification.as_ref() else {
            return false;
        };
        self.ws().focus_target == FocusTarget::Pane
            && self.ws().focused_pane_id == notification.target_pane
            && self.ws().panes.contains_key(&notification.target_pane)
    }

    pub(crate) fn visible_codex_peer_notification(&self) -> Option<&CodexPeerNotificationState> {
        self.codex_peer_notification_is_visible()
            .then_some(self.codex_peer_notification.as_ref())
            .flatten()
    }

    pub(crate) fn dismiss_codex_peer_notification(&mut self) {
        if self.codex_peer_notification.take().is_some() {
            self.dirty = true;
        }
    }

    fn materialize_unfocused_codex_peer_notification(&mut self) {
        let Some(notification) = self.codex_peer_notification.clone() else {
            return;
        };
        if self.codex_peer_notification_is_visible() {
            return;
        }
        if self
            .resolve_pane_across_workspaces(&PaneRef::Id(notification.target_pane))
            .is_some()
        {
            self.push_pending_codex_peer_nudge(notification.target_pane, notification.message);
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
        Ok(true)
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
        let Some(tail) = codex_peer_screen_tail(pane) else {
            return false;
        };
        if tail.contains("esc to interrupt") || tail.contains("tab to queue message") {
            return false;
        }
        if !pane_screen_has_visible_text(pane) {
            return false;
        }
        if let Some(allowed) = codex_prompt_allows_peer_nudge(pane) {
            return allowed;
        }
        tail.contains("enter to send") || tail.contains("ready for input")
    }

    pub(crate) fn flush_pending_codex_peer_messages(&mut self) {
        self.materialize_unfocused_codex_peer_notification();
        let now = Instant::now();
        let mut empty_panes = Vec::new();
        for ws in &mut self.workspaces {
            let pane_ids: Vec<usize> = ws.panes.keys().copied().collect();
            for pane_id in pane_ids {
                if ws.focused_pane_id == pane_id {
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
                        PendingCodexPeerDelivery::Draft(message) => {
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
    }
}
