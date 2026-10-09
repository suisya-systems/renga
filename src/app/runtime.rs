use super::*;

impl App {
    pub fn drain_pty_events(&mut self) -> bool {
        let mut had_events = false;
        // Track state-changing events (pane exit, cwd update) separately
        // from raw PTY output. When `ime_freeze_panes_on_overlay` is on
        // and the overlay is open we suppress repaints caused by pure
        // PTY output so Claude's thinking spinner can't flicker the
        // overlay, but state-changing events still need to repaint
        // because they affect non-pane UI (tab labels, sidebar cwd).
        let mut had_state_change = false;
        while let Ok(event) = self.event_rx.try_recv() {
            had_events = true;
            match event {
                AppEvent::PtyEof(pane_id) => {
                    had_state_change = true;
                    let mut exit_meta: Option<(Option<String>, Option<String>)> = None;
                    for ws in &mut self.workspaces {
                        if let Some(pane) = ws.panes.get_mut(&pane_id) {
                            pane.exited = true;
                            if !pane.exit_event_emitted {
                                pane.exit_event_emitted = true;
                                let name = ws
                                    .pane_names
                                    .iter()
                                    .find(|(_, id)| **id == pane_id)
                                    .map(|(n, _)| n.clone());
                                exit_meta = Some((name, pane.role.clone()));
                            }
                            break;
                        }
                    }
                    if let Some((name, role)) = exit_meta {
                        self.emit_pane_exited(pane_id, name, role);
                    }
                }
                AppEvent::CwdChanged(pane_id, new_cwd) => {
                    had_state_change = true;
                    // Security: resolve symlinks and relative components.
                    // Reject paths that don't resolve to a real directory
                    // (prevents OSC 7 escape sequence path injection).
                    // `is_dir` runs on the canonical (verbatim on
                    // Windows) path; `strip_verbatim_prefix` then
                    // normalizes for storage so `pane.cwd` never
                    // carries `\\?\` — PaneInfo / MCP output rely on
                    // the stripped form.
                    let new_cwd = match new_cwd.canonicalize() {
                        Ok(p) if p.is_dir() => strip_verbatim_prefix(p),
                        _ => continue,
                    };
                    for ws in &mut self.workspaces {
                        if ws.panes.contains_key(&pane_id) {
                            // Update pane's cwd
                            if let Some(pane) = ws.panes.get_mut(&pane_id) {
                                pane.cwd = new_cwd.clone();
                            }
                            if ws.focused_pane_id == pane_id {
                                let prev_show_hidden = ws.file_tree.show_hidden;
                                ws.file_tree = FileTree::new(new_cwd.clone());
                                // FileTree::new defaults to show_hidden=true
                                // Only toggle if the previous state was different
                                if ws.file_tree.show_hidden != prev_show_hidden {
                                    ws.file_tree.toggle_hidden();
                                }
                                ws.cwd = new_cwd;
                                ws.name = dir_name(&ws.cwd);
                                ws.preview.close();
                            }
                            break;
                        }
                    }
                }
                AppEvent::PtyOutput(pane_id) => {
                    if let Some(pane) = self
                        .workspaces
                        .iter_mut()
                        .find_map(|ws| ws.panes.get_mut(&pane_id))
                    {
                        pane.last_output_at = Instant::now();
                        pane.output_seen = true;
                        pane.waiting_input_reported = false;
                    }
                }
                AppEvent::ClipboardCopy(text) => {
                    self.copy_to_clipboard(&text);
                }
                AppEvent::PtyReply(pane_id, bytes) => {
                    if let Some(pane) = self
                        .workspaces
                        .iter_mut()
                        .find_map(|ws| ws.panes.get_mut(&pane_id))
                    {
                        let _ = pane.write_input(&bytes);
                    }
                }
            }
        }
        if had_events {
            let freeze_output = self.ime_freeze_panes_on_overlay && self.overlay.is_some();
            if had_state_change || !freeze_output {
                self.dirty = true;
            }
        }
        had_events
    }

    pub fn shutdown(&mut self) {
        // Surface PaneExited for every still-live pane before we tear
        // down the workspaces, so an event-stream subscriber observes
        // the final state consistently with the exactly-once contract.
        // `exit_event_emitted` guards against re-emitting any pane that
        // already exited via Ctrl+W, close_tab, or a natural shell exit.
        let mut pending: Vec<(usize, Option<String>, Option<String>)> = Vec::new();
        for ws in &mut self.workspaces {
            let pane_ids: Vec<usize> = ws.panes.keys().copied().collect();
            for pid in pane_ids {
                let name = ws
                    .pane_names
                    .iter()
                    .find(|(_, id)| **id == pid)
                    .map(|(n, _)| n.clone());
                if let Some(pane) = ws.panes.get_mut(&pid) {
                    if !pane.exit_event_emitted {
                        pane.exit_event_emitted = true;
                        pending.push((pid, name, pane.role.clone()));
                    }
                }
            }
        }
        for (pid, name, role) in pending {
            self.emit_pane_exited(pid, name, role);
        }
        for ws in &mut self.workspaces {
            ws.shutdown();
        }
        self.peer_client_kinds.clear();
        self.peer_unread.clear();
        self.pending_codex_peer_messages.clear();
    }

    /// Drain any pending IPC commands and dispatch them. Safe to call
    /// every frame — it's a no-op when the channel is empty. Commands
    /// always target the active workspace.
    pub fn drain_app_commands(&mut self) {
        // Pop into a local Vec first so we can borrow `self` mutably
        // inside `handle_app_command` without fighting the receiver
        // borrow.
        let mut cmds: Vec<AppCommand> = Vec::new();
        while let Ok(cmd) = self.command_rx.try_recv() {
            cmds.push(cmd);
        }
        for cmd in cmds {
            self.handle_app_command(cmd);
        }
    }
}
