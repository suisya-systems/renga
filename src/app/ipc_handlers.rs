use super::*;

impl App {
    pub(crate) fn handle_app_command(&mut self, cmd: AppCommand) {
        match cmd {
            AppCommand::List {
                from_pane,
                tab,
                reply,
            } => {
                self.recompute_hidden_rects_for_ipc();
                let result = self.handle_list(from_pane, tab.as_ref());
                let _ = reply.send(result);
            }
            AppCommand::Send {
                target,
                data,
                append_enter,
                from_pane,
                reply,
            } => {
                let result = self.handle_send(&target, &data, append_enter, from_pane);
                let _ = reply.send(result);
            }
            AppCommand::Focus {
                target,
                from_pane,
                reply,
            } => {
                let result = self.handle_focus(&target, from_pane);
                let _ = reply.send(result);
            }
            AppCommand::Split {
                target,
                direction,
                command,
                name,
                role,
                cwd,
                from_pane,
                tab,
                reply,
            } => {
                self.recompute_hidden_rects_for_ipc();
                let result = self.handle_split(
                    &target,
                    direction,
                    command,
                    name,
                    role,
                    cwd,
                    from_pane,
                    tab.as_ref(),
                );
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
            AppCommand::SpawnTab {
                command,
                name,
                label,
                role,
                cwd,
                from_pane,
                reply,
            } => {
                let result = self.handle_spawn_tab(command, name, label, role, cwd, from_pane);
                let _ = reply.send(result);
            }
            AppCommand::Inspect {
                target,
                lines,
                include_cursor,
                from_pane,
                reply,
            } => {
                // No rect refresh: `inspect` reads the parser, not
                // `last_pane_rects`. The `rows`/`cols` it reports are the
                // PTY's real size and therefore always describe the
                // buffer being returned.
                let result = self.handle_inspect(&target, lines, include_cursor, from_pane);
                let _ = reply.send(result);
            }
            AppCommand::Close {
                target,
                from_pane,
                reply,
            } => {
                let result = self.handle_close(&target, from_pane);
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
            AppCommand::PeerSendUserTurn {
                from_pane,
                target,
                body,
                reply,
            } => {
                // Owns its own `reply`: a delivery that gets as far as
                // writing bytes is answered later, from
                // `flush_pending_user_turns`.
                self.handle_peer_send_user_turn(from_pane, &target, body, reply);
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
                from_pane,
                reply,
            } => {
                let result = self.handle_set_pane_identity(&target, name, role, from_pane);
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

    /// Bring every non-visible workspace's cached rectangles up to date,
    /// immediately before serving an IPC command whose answer depends on
    /// them: `List` reports rects, `Split` judges the min-size guard.
    /// `Send`, `Focus` and `Inspect` read no rects and skip it.
    ///
    /// This is the single chokepoint for the staleness, and being single
    /// is the point. Only the visible workspace is relaid out as the
    /// layout changes — a terminal resize, a status-bar toggle, a
    /// sidebar drag, a layout swap all move every workspace's pane area
    /// but only refresh the active one. Once caller-scoped tools could
    /// read a background tab, every such reader inherited stale rects;
    /// refreshing inside each reader meant four places to remember and,
    /// demonstrably, a fifth to forget. Doing it here means a new
    /// pane-control command cannot miss it.
    ///
    /// Deliberately [`App::recompute_workspace_rects`] and not
    /// [`App::relayout_workspace`]: this must stay a *read*. The latter
    /// resizes PTYs, which clears their screens, so a `list_panes` from
    /// one tab would wipe the inspectable contents of every other —
    /// including tabs that have nothing to do with the request. Rect
    /// computation alone is pure and cheap, so sweeping all hidden
    /// workspaces costs nothing worth narrowing.
    fn recompute_hidden_rects_for_ipc(&mut self) {
        for i in 0..self.workspaces.len() {
            if i != self.active_tab {
                self.recompute_workspace_rects(i);
            }
        }
    }

    /// `list_panes` / `renga list`: the panes of the caller's tab, or
    /// of the tab(s) a `tab` selector names (Issue #329).
    ///
    /// Before #288 this always read the active workspace, which made
    /// the tool's own description ("panes in the current tab") false for
    /// any agent whose tab was not the one on screen — and, worse, made
    /// the ids it returned unsafe to feed straight back into
    /// `send_keys`. #329 closes the other half of the same gap: a pane
    /// parked in a background tab was reachable by numeric id but
    /// invisible to every enumeration surface that carries geometry, so
    /// an orchestrator could neither monitor it nor count it.
    ///
    /// The caller is resolved **first and unconditionally**, even when
    /// an explicit selector — not the caller's tab — decides the scope,
    /// the same ordering `handle_split` enforces. A stale `from_pane`
    /// is an error on every path, never a silent fallback to the
    /// visible tab, and `same_tab` needs the caller's workspace anyway.
    ///
    /// Geometry caveat, inherited unchanged by the multi-tab paths: the
    /// rects come from each workspace's cached `last_pane_rects`, which
    /// [`Self::recompute_hidden_rects_for_ipc`] refreshes for every
    /// hidden tab before this runs. They are still all-zero before the
    /// first layout pass, and still stale while the terminal is too
    /// small to lay out — both are pre-existing and documented on
    /// [`PaneInfo`]'s geometry fields, now merely reachable for a tab
    /// other than the caller's.
    pub(crate) fn handle_list(
        &self,
        from_pane: Option<usize>,
        tab: Option<&ipc::ListTabSelector>,
    ) -> std::result::Result<Vec<PaneInfo>, ipc::CodedError> {
        let caller_ws = self.resolve_caller_workspace(from_pane)?;
        // `same_tab` is only meaningful when the answer may leave the
        // caller's tab *and* there is a caller pane to compare against.
        let caller_for_same_tab = match (tab, from_pane) {
            (Some(_), Some(_)) => Some(caller_ws),
            _ => None,
        };
        match tab {
            None => Ok(self.pane_infos_for_workspace(caller_ws, caller_for_same_tab)),
            Some(ipc::ListTabSelector::All) => {
                let ordered = std::iter::once(caller_ws)
                    .chain((0..self.workspaces.len()).filter(|i| *i != caller_ws));
                Ok(ordered
                    .flat_map(|ws_idx| self.pane_infos_for_workspace(ws_idx, caller_for_same_tab))
                    .collect())
            }
            Some(sel) => {
                let selector = sel
                    .as_tab_selector()
                    .expect("only All maps to None, handled above");
                let ws_idx = self.resolve_tab_selector(&selector)?;
                Ok(self.pane_infos_for_workspace(ws_idx, caller_for_same_tab))
            }
        }
    }

    /// Build the wire [`PaneInfo`] list for one workspace, in layout
    /// order. `caller_ws` is `Some` only when the enclosing response may
    /// span tabs; see [`PaneInfo::same_tab`].
    ///
    /// The single-pane replies of `SetPaneIdentity` / `SetSummary` build
    /// their own `PaneInfo` literal (they answer about one pane, not a
    /// workspace) — keep the three in step when the struct grows a
    /// field.
    pub(crate) fn pane_infos_for_workspace(
        &self,
        ws_idx: usize,
        caller_ws: Option<usize>,
    ) -> Vec<PaneInfo> {
        let Some(ws) = self.workspaces.get(ws_idx) else {
            return Vec::new();
        };
        let tab_name = ws.display_name().to_string();
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
                tab: Some(ws_idx),
                tab_name: Some(tab_name.clone()),
                same_tab: caller_ws.map(|c| c == ws_idx),
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
        infos
    }

    /// Resolve `from_pane` to its workspace, then return every other
    /// pane in **every** workspace as a [`PeerInfo`] (Issue #289). The
    /// caller's own tab is emitted first so same-tab siblings — the
    /// ones addressable by bare name — stay at the top; the remaining
    /// tabs follow in index order.
    pub(crate) fn handle_peer_list(
        &self,
        from_pane: usize,
    ) -> std::result::Result<Vec<PeerInfo>, ipc::CodedError> {
        let (caller_ws, _) = self
            .resolve_pane_across_workspaces(&PaneRef::Id(from_pane))
            .ok_or_else(|| {
                ipc::CodedError::new(
                    ipc::err_code::PANE_NOT_FOUND,
                    format!("caller pane {from_pane} not found in any workspace"),
                )
            })?;
        let ordered_ws = std::iter::once(caller_ws)
            .chain((0..self.workspaces.len()).filter(|i| *i != caller_ws));
        let mut peers = Vec::new();
        for ws_idx in ordered_ws {
            let ws = &self.workspaces[ws_idx];
            let name_by_id: HashMap<usize, String> = ws
                .pane_names
                .iter()
                .map(|(n, id)| (*id, n.clone()))
                .collect();
            peers.extend(
                ws.layout
                    .collect_pane_ids()
                    .into_iter()
                    .filter(|id| *id != from_pane)
                    .map(|id| {
                        let pane = ws.panes.get(&id);
                        PeerInfo {
                            id,
                            name: name_by_id.get(&id).cloned(),
                            role: pane.and_then(|p| p.role.clone()),
                            tab: Some(ws_idx),
                            tab_name: Some(ws.display_name().to_string()),
                            same_tab: Some(ws_idx == caller_ws),
                            cwd: pane.map(|p| p.cwd.to_string_lossy().to_string()),
                            kind: self.peer_client_kinds.get(&id).copied(),
                            receive_mode: self
                                .peer_client_kinds
                                .get(&id)
                                .copied()
                                .map(|k| k.receive_mode()),
                            summary: pane.and_then(|p| p.summary.clone()),
                        }
                    }),
            );
        }
        Ok(peers)
    }

    /// `from_pane` scopes `Focused` / `Name` to the caller's own tab
    /// (Issue #296) — renaming "the focused pane" from a background
    /// agent used to retarget whichever pane the human had in view.
    /// Numeric ids stay cross-tab, and the `name_in_use` check below
    /// keeps running against the *resolved* pane's workspace, so
    /// per-tab name uniqueness is unchanged.
    pub(crate) fn handle_set_pane_identity(
        &mut self,
        target: &PaneRef,
        name: Option<Option<String>>,
        role: Option<Option<String>>,
        from_pane: Option<usize>,
    ) -> std::result::Result<PaneInfo, ipc::CodedError> {
        let (ws_idx, pane_id) = self.resolve_target_with_global_fallback(from_pane, target)?;

        if let Some(Some(new_name)) = &name {
            let trimmed = validate_pane_name(new_name)?;
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
        // `role` was only ever trimmed here; validate it alongside the
        // name and before any mutation, so a rejected role cannot leave
        // a half-applied identity change behind.
        if let Some(Some(new_role)) = &role {
            validate_display_label(new_role, "role")?;
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
            tab: Some(ws_idx),
            tab_name: Some(ws.display_name().to_string()),
            // One-pane reply: there is no cross-tab set for
            // `same_tab` to discriminate within. See `PaneInfo`.
            same_tab: None,
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
            tab: Some(ws_idx),
            tab_name: Some(ws.display_name().to_string()),
            // One-pane reply: there is no cross-tab set for
            // `same_tab` to discriminate within. See `PaneInfo`.
            same_tab: None,
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
        from_pane: Option<usize>,
    ) -> std::result::Result<serde_json::Value, ipc::CodedError> {
        let (ws_idx, pane_id) = self.resolve_request_target(from_pane, target)?;
        let ws = &self.workspaces[ws_idx];
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
        // Validate before `create_tab_with_cwd`, so a rejected label
        // does not strand a newly created tab.
        let (name, role) = Self::validated_split_identity(name, role)?;
        let label = match label.as_deref() {
            None => None,
            Some(raw) => Some(validate_display_label(raw, "label")?.to_string()),
        };
        let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let cwd_override = resolve_optional_cwd(cwd.as_deref(), &base)?;
        // The coded creation path, not `new_tab_with_cwd`: a full tab
        // strip must surface as `tab_limit_reached`, not a generic
        // `io_error`.
        let (_ws_idx, new_pane_id) = self.create_tab_with_cwd(cwd_override, true)?;
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

    /// Spawn a fresh single-pane tab in the background (Issue #290, the
    /// `tab: {new: …}` selector of the MCP `spawn_*` tools). The mirror
    /// of [`Self::handle_new_tab`] with three deliberate differences:
    /// the active tab does not change, every post-create mutation is
    /// indexed by the new tab's `ws_idx` (using `ws_mut()` here would
    /// silently edit whatever tab the human is looking at), and a
    /// relative / omitted `cwd` follows the **caller pane**, not the
    /// server process — a spawn places workers relative to the
    /// orchestrator that asked.
    ///
    /// Returns `(new_pane_id, ws_idx)`.
    pub(crate) fn handle_spawn_tab(
        &mut self,
        command: Option<String>,
        name: Option<String>,
        label: Option<String>,
        role: Option<String>,
        cwd: Option<String>,
        from_pane: Option<usize>,
    ) -> std::result::Result<(usize, usize), ipc::CodedError> {
        let caller_ws = self.resolve_caller_workspace(from_pane)?;
        // Validate the pane name *before* creating anything: an
        // invalid name must not leave behind a successfully created
        // tab whose pane can never be addressed by the name the caller
        // thinks it registered. Empty means "no name", matching
        // `handle_split` / `handle_new_tab`.
        let name = match name.as_deref() {
            None => None,
            Some(raw) if raw.trim().is_empty() => None,
            Some(raw) => Some(validate_pane_name(raw)?.to_string()),
        };
        // #290 validated `name` here but left `role` and `label`
        // verbatim, so the two free-form fields kept the injection this
        // check exists to close.
        let role = match role.as_deref() {
            None => None,
            Some(raw) => Some(validate_display_label(raw, "role")?.to_string()),
        };
        let label = match label.as_deref() {
            None => None,
            Some(raw) => Some(validate_display_label(raw, "label")?.to_string()),
        };
        let base = from_pane
            .and_then(|id| self.workspaces[caller_ws].panes.get(&id))
            .map(|p| p.cwd.clone())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let cwd_override = resolve_optional_cwd(cwd.as_deref(), &base)?;
        // `Some(base)` rather than `None`: `create_tab_with_cwd`'s
        // `None` default is the server process cwd, and the omitted-cwd
        // contract here is "inherit the caller pane's cwd".
        let (ws_idx, new_pane_id) =
            self.create_tab_with_cwd(Some(cwd_override.unwrap_or(base)), false)?;
        let effective_command = command.or_else(|| default_command_for_role(role.as_deref()));
        if let Some(pane) = self.workspaces[ws_idx].panes.get_mut(&new_pane_id) {
            if let Some(cmd) = effective_command {
                pane.queue_startup_command(&cmd);
            }
            if let Some(r) = role {
                pane.role = Some(r);
            }
        }
        if let Some(name) = name {
            if !name.is_empty() {
                self.workspaces[ws_idx].pane_names.insert(name, new_pane_id);
            }
        }
        if let Some(label) = label {
            if !label.is_empty() {
                self.workspaces[ws_idx].custom_name = Some(label);
            }
        }
        self.dirty = true;
        // Indexed and last: geometry is already final
        // (`create_tab_with_cwd` relaid the hidden workspace out) and
        // name/role are set, so the single `pane_started` carries them.
        self.emit_pane_started_in(ws_idx, new_pane_id);
        Ok((new_pane_id, ws_idx))
    }

    pub(crate) fn handle_send(
        &mut self,
        target: &PaneRef,
        data: &[u8],
        append_enter: bool,
        from_pane: Option<usize>,
    ) -> std::result::Result<(), ipc::CodedError> {
        let (ws_idx, pane_id) = self.resolve_request_target(from_pane, target)?;
        let pane = self.workspaces[ws_idx]
            .panes
            .get_mut(&pane_id)
            .ok_or_else(|| ipc::CodedError::new(ipc::err_code::PANE_VANISHED, "pane vanished"))?;
        write_input_to_pane(pane, data, append_enter)?;
        self.dirty = true;
        Ok(())
    }

    /// Move keyboard focus. When the target lives in a tab that is not
    /// on screen, the visible tab switches too.
    ///
    /// That is deliberately disruptive, and the alternative is worse:
    /// setting `focused_pane_id` on a hidden workspace moves no
    /// keyboard focus at all — the user keeps typing into the tab they
    /// can see — so the tool would report success while doing nothing
    /// observable. "Focus this pane" has to mean the keystrokes land
    /// there. Both the MCP tool description and the docs say so.
    pub(crate) fn handle_focus(
        &mut self,
        target: &PaneRef,
        from_pane: Option<usize>,
    ) -> std::result::Result<(), ipc::CodedError> {
        let (ws_idx, pane_id) = self.resolve_request_target(from_pane, target)?;
        // Go through the shared tab switch rather than assigning
        // `active_tab`: it also drops the overlay and the selection and
        // double-click caches, every one of which is keyed to a pane or
        // tab index in the tab being left. A no-op when the target is
        // already the visible tab.
        self.switch_tab(ws_idx);
        // After the switch, not before: `switch_tab` carries org-sidebar
        // focus into the incoming tab, and this request is an explicit
        // instruction to put the keyboard on a *pane*.
        let ws = &mut self.workspaces[ws_idx];
        ws.focused_pane_id = pane_id;
        ws.focus_target = FocusTarget::Pane;
        self.dirty = true;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn handle_split(
        &mut self,
        target: &PaneRef,
        direction: ipc::Direction,
        command: Option<String>,
        name: Option<String>,
        role: Option<String>,
        cwd: Option<String>,
        from_pane: Option<usize>,
        tab: Option<&ipc::TabSelector>,
    ) -> std::result::Result<usize, ipc::CodedError> {
        // Before any placement work, so an invalid label cannot leave a
        // freshly split pane behind.
        let (name, role) = Self::validated_split_identity(name, role)?;
        let (ws_idx, target_pane_id) = match tab {
            None => self.resolve_request_target(from_pane, target)?,
            Some(selector) => {
                // Placement first (Issue #290): resolve the tab, then
                // resolve `target` strictly inside it. The caller must
                // still resolve even though the selector, not the
                // caller's tab, decides placement — an unattributable
                // request stays an error, the same rule
                // `resolve_request_target` enforces.
                self.resolve_caller_workspace(from_pane)?;
                let ws_idx = self.resolve_tab_selector(selector)?;
                let pane_id = self.resolve_target_in_tab(ws_idx, target)?;
                (ws_idx, pane_id)
            }
        };
        // A relative `cwd` is resolved against the *target* pane, not
        // the caller: that is the pre-#288 contract for this request and
        // `from_pane` only narrows which panes `target` may name. MCP
        // callers that want caller-relative paths absolutize before
        // sending (see `resolve_mcp_cwd`).
        let base = self.workspaces[ws_idx]
            .panes
            .get(&target_pane_id)
            .map(|p| p.cwd.clone())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let cwd_override = resolve_optional_cwd(cwd.as_deref(), &base)?;
        let split_dir = match direction {
            ipc::Direction::Vertical => SplitDirection::Vertical,
            ipc::Direction::Horizontal => SplitDirection::Horizontal,
        };
        // No focus round-trip any more: the indexed primitive takes the
        // target directly, so a refused split leaves every workspace's
        // focus exactly where it was instead of relying on a restore.
        let new_pane_id = match self
            .split_pane_in_workspace(ws_idx, target_pane_id, split_dir, false, cwd_override)
            .map_err(|e| ipc::CodedError::new(ipc::err_code::IO_ERROR, e.to_string()))?
        {
            Some(id) => id,
            None => {
                return Err(ipc::CodedError::new(
                    ipc::err_code::SPLIT_REFUSED,
                    "split refused (max panes reached or pane too small)",
                ));
            }
        };
        let effective_command = command.or_else(|| default_command_for_role(role.as_deref()));
        if let Some(pane) = self.workspaces[ws_idx].panes.get_mut(&new_pane_id) {
            if let Some(cmd) = effective_command {
                pane.queue_startup_command(&cmd);
            }
            if let Some(r) = role {
                pane.role = Some(r);
            }
        }
        if let Some(name) = name {
            if !name.is_empty() {
                self.workspaces[ws_idx].pane_names.insert(name, new_pane_id);
            }
        }
        // Indexed, not `emit_pane_started`: after a cross-tab split the
        // new pane is not in the active workspace, and the unindexed
        // form would emit the event with a null name and role.
        self.emit_pane_started_in(ws_idx, new_pane_id);
        Ok(new_pane_id)
    }

    /// Validate the `name` / `role` a `split` or `new_tab` wants to
    /// register, before either creates anything. Same ordering rule as
    /// [`Self::handle_spawn_tab`]: a rejected label must not leave
    /// behind a pane whose identity is not what the caller asked for.
    /// Empty means "no name", which both requests have always allowed.
    fn validated_split_identity(
        name: Option<String>,
        role: Option<String>,
    ) -> std::result::Result<(Option<String>, Option<String>), ipc::CodedError> {
        let name = match name.as_deref() {
            None => None,
            Some(raw) if raw.trim().is_empty() => None,
            Some(raw) => Some(validate_pane_name(raw)?.to_string()),
        };
        let role = match role.as_deref() {
            None => None,
            Some(raw) => Some(validate_display_label(raw, "role")?.to_string()),
        };
        Ok((name, role))
    }
}

/// Reject a control character in a free-form display label (`role`, tab
/// label). Unlike [`validate_pane_name`] this imposes **no charset** —
/// both fields are documented as free-form on the frozen v1.0 surface
/// (`role` is literally "Optional free-form role label", and a tab
/// label defaults to a cwd-derived directory name), so non-ASCII and
/// spaces stay legal. Only the `Cc` category is refused, because those
/// are the characters that stop being decoration once the label is
/// interpolated into another pane's context or written toward a PTY.
///
/// Input-side twin of [`ipc::strip_control_chars`]: rejecting here
/// keeps bad data out of the UI and out of `PaneInfo` / `PeerInfo`,
/// while the strip stays as the backstop for values that predate this
/// check (a layout file, or a name registered by an older build).
fn validate_display_label<'a>(
    label: &'a str,
    field: &str,
) -> std::result::Result<&'a str, ipc::CodedError> {
    if label.contains(char::is_control) {
        return Err(ipc::CodedError::new(
            ipc::err_code::NAME_INVALID,
            format!("{field} must not contain control characters"),
        ));
    }
    Ok(label)
}

/// Validate a stable pane name and return its trimmed form: non-empty,
/// not all-digits (digit strings parse as numeric pane ids, so an
/// all-digit name could never be addressed), charset `[A-Za-z0-9_-]`.
/// Shared by every path that registers a name — `set_pane_identity`,
/// `spawn_tab`, `split` and `new_tab` — so they cannot drift apart.
///
/// `split` / `new_tab` accepted names verbatim until v2.0.0. That gap
/// was reachable: a pane name flows into the Codex peer nudge, which
/// types it into another pane's PTY and presses Enter, and into the
/// channel banner a receiving agent reads. Closing it is a breaking
/// change to the frozen v1.0 surface, hence the major.
fn validate_pane_name(name: &str) -> std::result::Result<&str, ipc::CodedError> {
    let trimmed = name.trim();
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
    Ok(trimmed)
}
