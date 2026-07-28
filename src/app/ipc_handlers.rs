use super::*;

impl App {
    pub(crate) fn handle_app_command(&mut self, cmd: AppCommand) {
        match cmd {
            AppCommand::List { reply } => {
                let ws = self.ws();
                let focused = ws.focused_pane_id;
                let mut name_by_id: HashMap<usize, String> = HashMap::new();
                for (name, id) in &ws.pane_names {
                    name_by_id.insert(*id, name.clone());
                }
                let rect_by_id: HashMap<usize, Rect> = ws.last_pane_rects.iter().copied().collect();
                let mut infos: Vec<PaneInfo> = Vec::new();
                for id in ws.layout.collect_pane_ids() {
                    let pane = ws.panes.get(&id);
                    let role = pane.and_then(|p| p.role.clone());
                    let cwd = pane.map(|p| p.cwd.to_string_lossy().to_string());
                    let kind = self.peer_client_kinds.get(&id).copied();
                    let rect = rect_by_id.get(&id).copied().unwrap_or_default();
                    let summary = pane.and_then(|p| p.summary.clone());
                    infos.push(PaneInfo {
                        id,
                        name: name_by_id.get(&id).cloned(),
                        role,
                        focused: id == focused,
                        x: rect.x,
                        y: rect.y,
                        width: rect.width,
                        height: rect.height,
                        cwd,
                        kind,
                        receive_mode: kind.map(|k| k.receive_mode()),
                        summary,
                    });
                }
                let _ = reply.send(infos);
            }
            AppCommand::Send {
                target,
                data,
                append_enter,
                reply,
            } => {
                let result = self.handle_send(&target, &data, append_enter);
                let _ = reply.send(result);
            }
            AppCommand::Focus { target, reply } => {
                let result = self.handle_focus(&target);
                let _ = reply.send(result);
            }
            AppCommand::Split {
                target,
                direction,
                command,
                name,
                role,
                cwd,
                reply,
            } => {
                let result = self.handle_split(&target, direction, command, name, role, cwd);
                let _ = reply.send(result);
            }
            AppCommand::NewTab {
                command,
                name,
                label,
                role,
                cwd,
                reply,
            } => {
                let result = self.handle_new_tab(command, name, label, role, cwd);
                let _ = reply.send(result);
            }
            AppCommand::Inspect {
                target,
                lines,
                include_cursor,
                reply,
            } => {
                let result = self.handle_inspect(&target, lines, include_cursor);
                let _ = reply.send(result);
            }
            AppCommand::Close { target, reply } => {
                let result = self.handle_close(&target);
                let _ = reply.send(result);
            }
            AppCommand::PeerList { from_pane, reply } => {
                let result = self.handle_peer_list(from_pane);
                let _ = reply.send(result);
            }
            AppCommand::PeerSend {
                from_pane,
                target,
                body,
                reply,
            } => {
                let result = self.handle_peer_send(from_pane, &target, body);
                let _ = reply.send(result);
            }
            AppCommand::PeerRegisterClient {
                pane_id,
                kind,
                reply,
            } => {
                let result = self.handle_peer_register_client(pane_id, kind);
                let _ = reply.send(result);
            }
            AppCommand::SetPaneIdentity {
                target,
                name,
                role,
                reply,
            } => {
                let result = self.handle_set_pane_identity(&target, name, role);
                let _ = reply.send(result);
            }
            AppCommand::SetSummary {
                pane_id,
                summary,
                reply,
            } => {
                let result = self.handle_set_summary(pane_id, summary);
                let _ = reply.send(result);
            }
        }
    }

    /// Resolve `from_pane` to its workspace, then return every other
    /// pane in that workspace as a [`PeerInfo`].
    pub(crate) fn handle_peer_list(
        &self,
        from_pane: usize,
    ) -> std::result::Result<Vec<PeerInfo>, ipc::CodedError> {
        let (ws_idx, _) = self
            .resolve_pane_across_workspaces(&PaneRef::Id(from_pane))
            .ok_or_else(|| {
                ipc::CodedError::new(
                    ipc::err_code::PANE_NOT_FOUND,
                    format!("caller pane {from_pane} not found in any workspace"),
                )
            })?;
        let ws = &self.workspaces[ws_idx];
        let name_by_id: HashMap<usize, String> = ws
            .pane_names
            .iter()
            .map(|(n, id)| (*id, n.clone()))
            .collect();
        let peers: Vec<PeerInfo> = ws
            .layout
            .collect_pane_ids()
            .into_iter()
            .filter(|id| *id != from_pane)
            .map(|id| {
                let pane = ws.panes.get(&id);
                PeerInfo {
                    id,
                    name: name_by_id.get(&id).cloned(),
                    role: pane.and_then(|p| p.role.clone()),
                    cwd: pane.map(|p| p.cwd.to_string_lossy().to_string()),
                    kind: self.peer_client_kinds.get(&id).copied(),
                    receive_mode: self
                        .peer_client_kinds
                        .get(&id)
                        .copied()
                        .map(|k| k.receive_mode()),
                    summary: pane.and_then(|p| p.summary.clone()),
                }
            })
            .collect();
        Ok(peers)
    }

    pub(crate) fn handle_set_pane_identity(
        &mut self,
        target: &PaneRef,
        name: Option<Option<String>>,
        role: Option<Option<String>>,
    ) -> std::result::Result<PaneInfo, ipc::CodedError> {
        let (ws_idx, pane_id) = self.resolve_pane_across_workspaces(target).ok_or_else(|| {
            ipc::CodedError::new(
                ipc::err_code::PANE_NOT_FOUND,
                format!("pane not found: {target:?}"),
            )
        })?;

        if let Some(Some(new_name)) = &name {
            let trimmed = new_name.trim();
            if trimmed.is_empty() {
                return Err(ipc::CodedError::new(
                    ipc::err_code::NAME_INVALID,
                    "name must not be empty — pass null to clear",
                ));
            }
            if trimmed.chars().all(|c| c.is_ascii_digit()) {
                return Err(ipc::CodedError::new(
                    ipc::err_code::NAME_INVALID,
                    format!("name {trimmed:?} is all-digits; would collide with numeric pane ids"),
                ));
            }
            if !trimmed
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                return Err(ipc::CodedError::new(
                    ipc::err_code::NAME_INVALID,
                    format!("name {trimmed:?} has invalid characters; allowed: [A-Za-z0-9_-]"),
                ));
            }
            let ws = &self.workspaces[ws_idx];
            if let Some(&holder) = ws.pane_names.get(trimmed) {
                if holder != pane_id {
                    return Err(ipc::CodedError::new(
                        ipc::err_code::NAME_IN_USE,
                        format!("name {trimmed:?} is already held by pane {holder} in this tab"),
                    ));
                }
            }
        }

        let ws = &mut self.workspaces[ws_idx];
        if let Some(name_change) = name {
            let keys_to_remove: Vec<String> = ws
                .pane_names
                .iter()
                .filter_map(|(k, &v)| (v == pane_id).then_some(k.clone()))
                .collect();
            for k in keys_to_remove {
                ws.pane_names.remove(&k);
            }
            if let Some(new_name) = name_change {
                ws.pane_names.insert(new_name.trim().to_string(), pane_id);
            }
        }
        if let Some(role_change) = role {
            if let Some(pane) = ws.panes.get_mut(&pane_id) {
                pane.role = role_change
                    .map(|r| r.trim().to_string())
                    .filter(|r| !r.is_empty());
            }
        }
        self.dirty = true;

        let ws = &self.workspaces[ws_idx];
        let name_for_pane = ws
            .pane_names
            .iter()
            .find(|(_, &id)| id == pane_id)
            .map(|(n, _)| n.clone());
        let pane = ws.panes.get(&pane_id).ok_or_else(|| {
            ipc::CodedError::new(ipc::err_code::PANE_VANISHED, "pane vanished mid-update")
        })?;
        let rect = ws
            .last_pane_rects
            .iter()
            .find(|(id, _)| *id == pane_id)
            .map(|(_, r)| *r)
            .unwrap_or_default();
        Ok(PaneInfo {
            id: pane_id,
            name: name_for_pane,
            role: pane.role.clone(),
            focused: ws.focused_pane_id == pane_id,
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
            cwd: Some(pane.cwd.to_string_lossy().to_string()),
            kind: self.peer_client_kinds.get(&pane_id).copied(),
            receive_mode: self
                .peer_client_kinds
                .get(&pane_id)
                .copied()
                .map(|k| k.receive_mode()),
            summary: pane.summary.clone(),
        })
    }

    /// Set or clear the per-pane summary published by the MCP
    /// `set_summary` tool. Empty input clears; >256-`chars` input is
    /// rejected before any mutation. Returns the updated [`PaneInfo`]
    /// so the caller can confirm.
    pub(crate) fn handle_set_summary(
        &mut self,
        pane_id: usize,
        summary: String,
    ) -> std::result::Result<PaneInfo, ipc::CodedError> {
        // Cap on `chars()` (Unicode scalar values), not bytes — gives
        // multi-byte scripts the same effective ceiling as ASCII.
        const MAX_SUMMARY_CHARS: usize = 256;
        if summary.chars().count() > MAX_SUMMARY_CHARS {
            return Err(ipc::CodedError::new(
                ipc::err_code::SUMMARY_TOO_LONG,
                format!(
                    "summary is {} chars; max is {MAX_SUMMARY_CHARS}",
                    summary.chars().count()
                ),
            ));
        }
        let (ws_idx, pane_id) = self
            .resolve_pane_across_workspaces(&PaneRef::Id(pane_id))
            .ok_or_else(|| {
                ipc::CodedError::new(
                    ipc::err_code::PANE_NOT_FOUND,
                    format!("caller pane {pane_id} not found in any workspace"),
                )
            })?;
        let ws = &mut self.workspaces[ws_idx];
        let pane = ws.panes.get_mut(&pane_id).ok_or_else(|| {
            ipc::CodedError::new(ipc::err_code::PANE_VANISHED, "pane vanished mid-update")
        })?;
        // Empty string clears the summary (round-trips to None on the
        // wire so callers see "no summary" via skip_serializing_if).
        pane.summary = if summary.is_empty() {
            None
        } else {
            Some(summary)
        };
        self.dirty = true;

        let ws = &self.workspaces[ws_idx];
        let name_for_pane = ws
            .pane_names
            .iter()
            .find(|(_, &id)| id == pane_id)
            .map(|(n, _)| n.clone());
        let pane = ws.panes.get(&pane_id).ok_or_else(|| {
            ipc::CodedError::new(ipc::err_code::PANE_VANISHED, "pane vanished mid-update")
        })?;
        let rect = ws
            .last_pane_rects
            .iter()
            .find(|(id, _)| *id == pane_id)
            .map(|(_, r)| *r)
            .unwrap_or_default();
        Ok(PaneInfo {
            id: pane_id,
            name: name_for_pane,
            role: pane.role.clone(),
            focused: ws.focused_pane_id == pane_id,
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
            cwd: Some(pane.cwd.to_string_lossy().to_string()),
            kind: self.peer_client_kinds.get(&pane_id).copied(),
            receive_mode: self
                .peer_client_kinds
                .get(&pane_id)
                .copied()
                .map(|k| k.receive_mode()),
            summary: pane.summary.clone(),
        })
    }

    pub(crate) fn handle_inspect(
        &self,
        target: &PaneRef,
        lines: Option<usize>,
        include_cursor: bool,
    ) -> std::result::Result<serde_json::Value, ipc::CodedError> {
        let ws = self.ws();
        let pane_id = ws.resolve_pane_ref(target).ok_or_else(|| {
            ipc::CodedError::new(
                ipc::err_code::PANE_NOT_FOUND,
                format!("pane not found: {target:?}"),
            )
        })?;
        let pane = ws
            .panes
            .get(&pane_id)
            .ok_or_else(|| ipc::CodedError::new(ipc::err_code::PANE_VANISHED, "pane vanished"))?;

        let pane_name = ws
            .pane_names
            .iter()
            .find(|(_, id)| **id == pane_id)
            .map(|(n, _)| n.clone());

        let (rows, cols, line_start, line_count, collected, cursor) = {
            let mut parser = pane.parser.lock().map_err(|_| {
                ipc::CodedError::new(ipc::err_code::INTERNAL, "vt100 parser lock poisoned")
            })?;

            // Pin the read to the live tail. A human may have scrolled
            // this pane back; inspect results must not depend on that,
            // and the user's scroll position must survive the call.
            // The whole save → read → restore dance happens under one
            // lock hold, so the renderer never sees an intermediate
            // offset.
            let saved_offset = parser.screen().scrollback();
            parser.screen_mut().set_scrollback(0);

            let (total_rows, total_cols) = {
                let size = parser.screen().size();
                (size.0 as usize, size.1 as usize)
            };

            let want: usize = lines.unwrap_or(total_rows).min(ipc::INSPECT_MAX_LINES);

            let read_row = |screen: &vt100::Screen, row: usize| -> String {
                let mut s = String::with_capacity(total_cols);
                for col in 0..total_cols {
                    if let Some(cell) = screen.cell(row as u16, col as u16) {
                        s.push_str(cell.contents());
                    }
                }
                s.trim_end().to_string()
            };

            // Coordinate system: row 0 is the top of the visible
            // screen at the live view; scrollback rows above it are
            // negative (-1 = the line just above the visible top).
            let mut collected: Vec<(isize, String)> = Vec::with_capacity(want);

            let visible_start = total_rows.saturating_sub(want);
            for row in visible_start..total_rows {
                collected.push((row as isize, read_row(parser.screen(), row)));
            }

            // Shortfall beyond the visible grid continues into
            // scrollback, walked one screenful per step. set_scrollback
            // clamps to the history that actually exists, so a clamped
            // response means the top has been reached.
            let mut hist_needed = want.saturating_sub(total_rows);
            let mut hist_read: usize = 0;
            while hist_needed > 0 {
                let step = hist_needed.min(total_rows);
                if step == 0 {
                    break;
                }
                let requested = hist_read + step;
                parser.screen_mut().set_scrollback(requested);
                let actual = parser.screen().scrollback();
                if actual <= hist_read {
                    break;
                }
                // With offset `actual`, screen row r shows the line at
                // coordinate r - actual; rows 0..(actual - hist_read)
                // are exactly the lines not yet collected.
                let new_rows = actual - hist_read;
                for row in 0..new_rows {
                    let coord = row as isize - actual as isize;
                    collected.push((coord, read_row(parser.screen(), row)));
                }
                hist_read = actual;
                hist_needed = hist_needed.saturating_sub(new_rows);
                if actual < requested {
                    break;
                }
            }

            parser.screen_mut().set_scrollback(saved_offset);

            collected.sort_by_key(|(coord, _)| *coord);

            let cursor = if include_cursor {
                let screen = parser.screen();
                let (crow, ccol) = screen.cursor_position();
                Some((!screen.hide_cursor(), crow as usize, ccol as usize))
            } else {
                None
            };

            let line_start = collected.first().map(|(coord, _)| *coord).unwrap_or(0);
            let line_count = collected.len();
            (
                total_rows, total_cols, line_start, line_count, collected, cursor,
            )
        };

        let text = collected
            .iter()
            .map(|(_, s)| s.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        let lines_json: Vec<serde_json::Value> = collected
            .into_iter()
            .map(|(row, text)| serde_json::json!({ "row": row, "text": text }))
            .collect();

        let mut pane_obj = serde_json::json!({ "id": pane_id });
        if let Some(n) = pane_name {
            pane_obj["name"] = serde_json::Value::String(n);
        }
        let mut payload = serde_json::json!({
            "pane": pane_obj,
            "screen": {
                "rows": rows,
                "cols": cols,
                "line_start": line_start,
                "line_count": line_count,
            },
            "lines": lines_json,
            "text": text,
        });

        if let Some((visible, crow, ccol)) = cursor {
            payload["cursor"] = serde_json::json!({
                "visible": visible,
                "row": crow,
                "col": ccol,
            });
        }

        Ok(payload)
    }

    pub(crate) fn handle_new_tab(
        &mut self,
        command: Option<String>,
        name: Option<String>,
        label: Option<String>,
        role: Option<String>,
        cwd: Option<String>,
    ) -> std::result::Result<usize, ipc::CodedError> {
        let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let cwd_override = resolve_optional_cwd(cwd.as_deref(), &base)?;
        let new_pane_id = self
            .new_tab_with_cwd(cwd_override)
            .map_err(|e| ipc::CodedError::new(ipc::err_code::IO_ERROR, e.to_string()))?;
        let effective_command = command.or_else(|| default_command_for_role(role.as_deref()));
        if let Some(pane) = self.ws_mut().panes.get_mut(&new_pane_id) {
            if let Some(cmd) = effective_command {
                pane.queue_startup_command(&cmd);
            }
            if let Some(r) = role {
                pane.role = Some(r);
            }
        }
        if let Some(name) = name {
            if !name.is_empty() {
                self.ws_mut().pane_names.insert(name, new_pane_id);
            }
        }
        if let Some(label) = label {
            if !label.is_empty() {
                self.ws_mut().custom_name = Some(label);
            }
        }
        self.dirty = true;
        self.emit_pane_started(new_pane_id);
        Ok(new_pane_id)
    }

    pub(crate) fn handle_send(
        &mut self,
        target: &PaneRef,
        data: &[u8],
        append_enter: bool,
    ) -> std::result::Result<(), ipc::CodedError> {
        let pane_id = self.ws().resolve_pane_ref(target).ok_or_else(|| {
            ipc::CodedError::new(
                ipc::err_code::PANE_NOT_FOUND,
                format!("pane not found: {target:?}"),
            )
        })?;
        let pane =
            self.ws_mut().panes.get_mut(&pane_id).ok_or_else(|| {
                ipc::CodedError::new(ipc::err_code::PANE_VANISHED, "pane vanished")
            })?;
        write_input_to_pane(pane, data, append_enter)?;
        self.dirty = true;
        Ok(())
    }

    pub(crate) fn handle_focus(
        &mut self,
        target: &PaneRef,
    ) -> std::result::Result<(), ipc::CodedError> {
        let pane_id = self.ws().resolve_pane_ref(target).ok_or_else(|| {
            ipc::CodedError::new(
                ipc::err_code::PANE_NOT_FOUND,
                format!("pane not found: {target:?}"),
            )
        })?;
        let ws = self.ws_mut();
        ws.focused_pane_id = pane_id;
        ws.focus_target = FocusTarget::Pane;
        self.dirty = true;
        Ok(())
    }

    pub(crate) fn handle_split(
        &mut self,
        target: &PaneRef,
        direction: ipc::Direction,
        command: Option<String>,
        name: Option<String>,
        role: Option<String>,
        cwd: Option<String>,
    ) -> std::result::Result<usize, ipc::CodedError> {
        let target_pane_id = self.ws().resolve_pane_ref(target).ok_or_else(|| {
            ipc::CodedError::new(
                ipc::err_code::PANE_NOT_FOUND,
                format!("pane not found: {target:?}"),
            )
        })?;
        let base = self
            .ws()
            .panes
            .get(&target_pane_id)
            .map(|p| p.cwd.clone())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let cwd_override = resolve_optional_cwd(cwd.as_deref(), &base)?;
        let prev_focus = self.ws().focused_pane_id;
        self.ws_mut().focused_pane_id = target_pane_id;
        let split_dir = match direction {
            ipc::Direction::Vertical => SplitDirection::Vertical,
            ipc::Direction::Horizontal => SplitDirection::Horizontal,
        };
        let new_pane_id = match self
            .split_focused_pane(split_dir, cwd_override)
            .map_err(|e| ipc::CodedError::new(ipc::err_code::IO_ERROR, e.to_string()))?
        {
            Some(id) => id,
            None => {
                self.ws_mut().focused_pane_id = prev_focus;
                return Err(ipc::CodedError::new(
                    ipc::err_code::SPLIT_REFUSED,
                    "split refused (max panes reached or pane too small)",
                ));
            }
        };
        let effective_command = command.or_else(|| default_command_for_role(role.as_deref()));
        if let Some(pane) = self.ws_mut().panes.get_mut(&new_pane_id) {
            if let Some(cmd) = effective_command {
                pane.queue_startup_command(&cmd);
            }
            if let Some(r) = role {
                pane.role = Some(r);
            }
        }
        if let Some(name) = name {
            if !name.is_empty() {
                self.ws_mut().pane_names.insert(name, new_pane_id);
            }
        }
        self.emit_pane_started(new_pane_id);
        Ok(new_pane_id)
    }
}
