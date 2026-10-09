use super::*;

/// Why [`App::try_split_pane_in_workspace`] refused, with the numbers
/// the decision was made from (Issue #335).
///
/// The point of the split is the one distinction a caller has to act
/// on: `TargetTooSmall` is **target-local** — another target in the
/// same tab may still split — while `PaneLimitReached` is
/// **tab-global** and no target will help. Folding both into one
/// `split_refused` string made an orchestrator that read the refusal
/// as "the tab is full" give up on capacity that existed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SplitRefusal {
    /// The tab already holds `MAX_PANES` panes.
    PaneLimitReached { panes: usize, max: usize },
    /// Halving the target along the requested axis would leave panes
    /// under the configured minimum. `available` is the target's
    /// current extent on that axis, `required` the per-pane minimum
    /// (`min_pane_width` / `min_pane_height`).
    TargetTooSmall {
        target: usize,
        direction: SplitDirection,
        available: u16,
        required: u16,
        panes: usize,
        max: usize,
    },
    /// The terminal itself is below the layout threshold, so no
    /// workspace has geometry any target could be judged against.
    TerminalTooSmall { cols: u16, rows: u16 },
    /// The workspace disappeared between resolution and the split.
    WorkspaceMissing { ws_index: usize },
}

impl SplitRefusal {
    /// Wire code for this refusal. Only the two split-out causes get
    /// their own code; the rest keep the legacy `split_refused`,
    /// which callers must treat as "cause unknown".
    pub(crate) fn code(&self) -> &'static str {
        match self {
            SplitRefusal::PaneLimitReached { .. } => ipc::err_code::PANE_LIMIT_REACHED,
            SplitRefusal::TargetTooSmall { .. } => ipc::err_code::TARGET_TOO_SMALL,
            SplitRefusal::TerminalTooSmall { .. } | SplitRefusal::WorkspaceMissing { .. } => {
                ipc::err_code::SPLIT_REFUSED
            }
        }
    }

    /// Human-readable message carrying the observed values, so a
    /// caller can branch on the code and a human can see *why*
    /// without a second round-trip.
    pub(crate) fn message(&self) -> String {
        match *self {
            SplitRefusal::PaneLimitReached { panes, max } => format!(
                "pane limit reached: this tab already holds {panes} of {max} panes, so no target \
                 in it can be split. Close a pane, or place the new one in another tab \
                 (tab: {{\"new\": {{}}}})."
            ),
            SplitRefusal::TargetTooSmall {
                target,
                direction,
                available,
                required,
                panes,
                max,
            } => {
                let (axis, unit, knob) = match direction {
                    SplitDirection::Vertical => ("wide", "columns", "min_pane_width"),
                    SplitDirection::Horizontal => ("tall", "rows", "min_pane_height"),
                };
                format!(
                    "target pane {target} is too small to split: it is {available} {unit} {axis}, \
                     so each half would get {half} {unit}, below the {required}-{unit_singular} \
                     minimum ({knob}). This refusal is target-local — the tab holds {panes} of \
                     {max} panes, so a larger target (or the other direction) can still succeed.",
                    half = available / 2,
                    unit_singular = &unit[..unit.len() - 1],
                )
            }
            SplitRefusal::TerminalTooSmall { cols, rows } => format!(
                "terminal too small to lay out any split ({cols}x{rows}, minimum \
                 {MIN_LAYOUT_COLS}x{MIN_LAYOUT_ROWS}); no target in any tab can be judged \
                 against current geometry"
            ),
            SplitRefusal::WorkspaceMissing { ws_index } => {
                format!("workspace {ws_index} vanished before the split could be applied")
            }
        }
    }

    pub(crate) fn into_coded_error(self) -> ipc::CodedError {
        ipc::CodedError::new(self.code(), self.message())
    }
}

impl App {
    pub(crate) fn new_tab(&mut self) -> Result<usize> {
        self.new_tab_with_cwd(None)
    }

    pub(crate) fn new_tab_with_cwd(&mut self, cwd_override: Option<PathBuf>) -> Result<usize> {
        self.create_tab_with_cwd(cwd_override, true)
            .map(|(_, pane_id)| pane_id)
            // Callers of this legacy wrapper (Alt+T, layout TOML) only
            // surface the message; the coded form stays available via
            // `create_tab_with_cwd` for the IPC handlers.
            .map_err(|e| anyhow::anyhow!(e.to_string()))
    }

    /// Create a new single-pane tab. `activate: true` is the classic
    /// "open and focus" behavior (Alt+T, `new_tab`); `activate: false`
    /// creates the tab in the **background** (Issue #290, the `tab:
    /// {new: …}` spawn selector): the visible tab is untouched and the
    /// hidden workspace's geometry — rects *and* PTY size — is
    /// finalized before returning, so callers never observe the 10x40
    /// placeholder the pane is born with.
    ///
    /// Returns `(ws_idx, pane_id)` of the created tab.
    pub(crate) fn create_tab_with_cwd(
        &mut self,
        cwd_override: Option<PathBuf>,
        activate: bool,
    ) -> std::result::Result<(usize, usize), ipc::CodedError> {
        if self.workspaces.len() >= Self::MAX_TABS {
            return Err(ipc::CodedError::new(
                ipc::err_code::TAB_LIMIT_REACHED,
                format!(
                    "tab limit reached ({} of {} tabs)",
                    self.workspaces.len(),
                    Self::MAX_TABS
                ),
            ));
        }
        // A background tab must report real geometry in its success
        // reply — nothing else ever refreshes a hidden workspace —
        // and below the layout threshold there is no real geometry to
        // compute (`relayout_workspace` bails). Refuse up front,
        // mirroring `split_pane_in_workspace`. The activate path
        // keeps its historic tolerance: the next render lays the now
        // visible tab out anyway.
        if !activate && self.terminal_too_small_for_layout() {
            return Err(ipc::CodedError::new(
                ipc::err_code::SPLIT_REFUSED,
                "terminal too small to lay out a new background tab",
            ));
        }
        let cwd = cwd_override
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let name = dir_name(&cwd);
        let pane_id = self.next_pane_id;
        self.next_pane_id = self.next_pane_id.wrapping_add(1);

        let ws = Workspace::new(name, cwd, pane_id, 10, 40, self.event_tx.clone())
            .map_err(|e| ipc::CodedError::new(ipc::err_code::IO_ERROR, e.to_string()))?;
        self.workspaces.push(ws);
        let ws_idx = self.workspaces.len() - 1;
        if activate {
            self.active_tab = ws_idx;
            self.suspend_overlay();
        } else {
            // A hidden workspace never passes through
            // `ui::render_panes`, so nothing else would size it (see
            // the sibling branch in `split_pane_in_workspace`).
            // `suspend_overlay` is deliberately *not* called: the
            // visible tab did not change, and tearing down the IME
            // overlay the user is composing in because a background
            // tab appeared would be a regression.
            self.relayout_workspace(ws_idx);
            self.dirty = true;
        }
        // Sidebar rows are keyed by tab index, so the whole cache is
        // stale the moment the tab set changes.
        self.reset_org_sidebar_caches();
        Ok((ws_idx, pane_id))
    }

    pub(crate) fn close_tab(&mut self, index: usize) {
        if self.workspaces.len() <= 1 {
            return;
        }

        let pane_ids_in_tab: Vec<usize> = self.workspaces[index].panes.keys().copied().collect();

        let mut to_emit: Vec<(usize, Option<String>, Option<String>)> = Vec::new();
        {
            let ws = &mut self.workspaces[index];
            for pid in &pane_ids_in_tab {
                let name = ws
                    .pane_names
                    .iter()
                    .find(|(_, id)| **id == *pid)
                    .map(|(n, _)| n.clone());
                if let Some(pane) = ws.panes.get_mut(pid) {
                    if !pane.exit_event_emitted {
                        pane.exit_event_emitted = true;
                        to_emit.push((*pid, name, pane.role.clone()));
                    }
                }
            }
            for pid in &pane_ids_in_tab {
                self.saved_overlay_drafts.remove(pid);
                self.claude_monitor.remove(*pid);
                self.peer_client_kinds.remove(pid);
                self.peer_unread.remove(pid);
                self.pending_codex_peer_messages.remove(pid);
            }
        }
        if self
            .codex_peer_notification
            .as_ref()
            .is_some_and(|n| pane_ids_in_tab.contains(&n.target_pane))
        {
            self.codex_peer_notification = None;
        }

        let overlay_in_tab = self
            .overlay
            .as_ref()
            .is_some_and(|o| self.workspaces[index].panes.contains_key(&o.target_pane));
        if overlay_in_tab {
            self.overlay = None;
        }

        self.workspaces[index].shutdown();
        self.workspaces.remove(index);
        // `remove` shifts every later tab down one slot. Clamping only
        // when `active_tab` runs off the end (the pre-#291 behaviour)
        // silently retargets the active tab whenever an *earlier* tab
        // closes: with [A,B,C,D] and the user on C (index 2), closing B
        // leaves [A,C,D] and index 2 now points at D. Because the
        // numeric value never changed, the `prev_active != active_tab`
        // guard below also skipped `suspend_overlay`, stranding the IME
        // overlay on a pane in a different tab. Follow the shift
        // instead. Reachable from MCP `close_pane` / `close_tab` on a
        // background tab, and now from the org sidebar's cross-tab view.
        let prev_active = self.active_tab;
        if index < self.active_tab {
            self.active_tab -= 1;
        } else if self.active_tab >= self.workspaces.len() {
            self.active_tab = self.workspaces.len() - 1;
        }
        // Suspend only when the workspace the user is *looking at*
        // changed, which is exactly "we closed the active tab".
        // `prev_active != active_tab` is not that test any more: now
        // that an earlier close decrements the index, it also fires for
        // `index < prev_active`, where the visible workspace is
        // unchanged — and tearing down a half-composed IME overlay
        // because some other tab closed in the background would be a
        // regression of its own.
        if index == prev_active {
            self.suspend_overlay();
        }
        // Tab indices shift after removal; any pending outer-edge or
        // boundary double-click is keyed by the pre-removal index and
        // would now point at a different workspace.
        self.last_edge_click = None;
        self.last_boundary_click = None;
        self.last_tab_click = None;
        self.reset_org_sidebar_caches();
        self.mark_layout_change();
        // A tab that vanished under a pending prompt (MCP close of its
        // last pane, or the tab itself) can no longer be confirmed.
        self.revalidate_close_confirm();
        for (pid, name, role) in to_emit {
            self.emit_pane_exited(pid, name, role);
        }
    }

    pub(crate) const MAX_PANES: usize = 16;
    /// Cap on simultaneously open tabs. Exists for the same reason as
    /// `MAX_PANES`: automated orchestration (`spawn_*` with `tab:
    /// {new: …}`) can create tabs in a loop, and every tab carries a
    /// live PTY. Exceeding it fails with `tab_limit_reached` — never
    /// `split_refused`, which is about pane capacity *inside* a tab.
    pub(crate) const MAX_TABS: usize = 16;

    pub(crate) fn split_focused_pane(
        &mut self,
        direction: SplitDirection,
        cwd_override: Option<PathBuf>,
    ) -> Result<Option<usize>> {
        self.split_focused_pane_with_position(direction, false, cwd_override)
    }

    /// Like [`Self::split_focused_pane`] but lets the caller place the
    /// new pane on the first (top / left) side instead of the default
    /// second (bottom / right). Used by the outer-edge double-click
    /// path so a click on the top or left edge spawns the new pane on
    /// the clicked side. All min-size / MAX_PANES guards match
    /// `split_focused_pane`.
    pub(crate) fn split_focused_pane_with_position(
        &mut self,
        direction: SplitDirection,
        new_pane_first: bool,
        cwd_override: Option<PathBuf>,
    ) -> Result<Option<usize>> {
        let ws_index = self.active_tab;
        let target = self.ws().focused_pane_id;
        self.split_pane_in_workspace(ws_index, target, direction, new_pane_first, cwd_override)
    }

    /// Option-returning wrapper over [`Self::try_split_pane_in_workspace`]
    /// for the interactive callers (key chords, double-click, layout
    /// TOML apply) that only ever ask "did it split?" — none of them
    /// has anywhere to show a cause. The IPC path uses the detailed
    /// form so it can report *which* refusal it hit.
    pub(crate) fn split_pane_in_workspace(
        &mut self,
        ws_index: usize,
        target_pane_id: usize,
        direction: SplitDirection,
        new_pane_first: bool,
        cwd_override: Option<PathBuf>,
    ) -> Result<Option<usize>> {
        Ok(self
            .try_split_pane_in_workspace(
                ws_index,
                target_pane_id,
                direction,
                new_pane_first,
                cwd_override,
            )?
            .ok())
    }

    /// Split `target_pane_id` inside workspace `ws_index`, regardless of
    /// which tab is currently on screen.
    ///
    /// A refusal comes back as [`SplitRefusal`] rather than a bare
    /// `None` (Issue #335): the three reasons a split can be refused
    /// need different reactions from a caller, and only this function
    /// still has the numbers the decision was made from.
    ///
    /// Every piece of state this touches — the pane cap, the geometry
    /// used for the min-size guard and the new PTY's first-frame size,
    /// the layout tree, the inherited cwd, the post-split focus — is
    /// per-workspace, so all of it is indexed rather than read through
    /// `self.ws()`. Splitting a hidden tab must not disturb the visible
    /// one, which is why the relayout at the end is targeted too:
    /// `mark_layout_change` (with its repaint cooldown) is only right
    /// for the tab the user is actually watching.
    pub(crate) fn try_split_pane_in_workspace(
        &mut self,
        ws_index: usize,
        target_pane_id: usize,
        direction: SplitDirection,
        new_pane_first: bool,
        cwd_override: Option<PathBuf>,
    ) -> Result<std::result::Result<usize, SplitRefusal>> {
        if self.workspaces.get(ws_index).is_none() {
            return Ok(Err(SplitRefusal::WorkspaceMissing { ws_index }));
        }
        let panes = self.workspaces[ws_index].layout.pane_count();
        if panes >= Self::MAX_PANES {
            return Ok(Err(SplitRefusal::PaneLimitReached {
                panes,
                max: Self::MAX_PANES,
            }));
        }

        // Below the layout threshold no workspace has usable rects —
        // `relayout_workspace` bails, so `last_pane_rects` describes a
        // terminal that is gone. Refuse rather than judge "is there
        // room?" against geometry we know is stale. This is the one
        // refusal that is neither target-local nor a pane-cap hit, so
        // it keeps the legacy `split_refused` code.
        if self.terminal_too_small_for_layout() {
            let (cols, rows) = self.last_term_size;
            return Ok(Err(SplitRefusal::TerminalTooSmall { cols, rows }));
        }

        let focused_rect = self.workspaces[ws_index]
            .last_pane_rects
            .iter()
            .find(|(id, _)| *id == target_pane_id)
            .map(|&(_, rect)| rect);

        if let Some(rect) = focused_rect {
            let (available, required) = match direction {
                SplitDirection::Vertical => (rect.width, self.min_pane_width),
                SplitDirection::Horizontal => (rect.height, self.min_pane_height),
            };
            if available / 2 < required {
                return Ok(Err(SplitRefusal::TargetTooSmall {
                    target: target_pane_id,
                    direction,
                    available,
                    required,
                    panes,
                    max: Self::MAX_PANES,
                }));
            }
        }

        let new_id = self.next_pane_id;
        self.next_pane_id = self.next_pane_id.wrapping_add(1);

        let cwd = cwd_override.or_else(|| {
            self.workspaces[ws_index]
                .panes
                .get(&target_pane_id)
                .map(|p| p.cwd.clone())
        });

        // Seed the new PTY with the geometry it will actually get after the
        // split, minus the 1-cell border on each side. Falling back to a
        // fixed 10x40 (the old behavior) forced every fresh pane through a
        // startup resize/reflow once `render_panes` corrected the size, a
        // contributing factor to caret desync on plain PTY panes. The next
        // render still calls `pane.resize(...)` with the exact rect, so this
        // is purely a better first-frame estimate.
        let (init_rows, init_cols) = match focused_rect {
            Some(rect) => {
                let (w, h) = match direction {
                    SplitDirection::Vertical => (rect.width / 2, rect.height),
                    SplitDirection::Horizontal => (rect.width, rect.height / 2),
                };
                (h.saturating_sub(2).max(1), w.saturating_sub(2).max(1))
            }
            None => (10, 40),
        };
        let pane = Pane::new_with_cwd(new_id, init_rows, init_cols, self.event_tx.clone(), cwd)?;
        let ws = &mut self.workspaces[ws_index];
        ws.panes.insert(new_id, pane);
        ws.layout
            .split_pane_with_position(target_pane_id, new_id, direction, new_pane_first);
        // Focus follows the new pane *within its own tab*. For the
        // visible tab that is the long-standing behavior; for a hidden
        // tab it is what the user will find when they switch to it.
        ws.focused_pane_id = new_id;

        if ws_index == self.active_tab {
            self.mark_layout_change();
        } else {
            // A hidden workspace never passes through `ui::render_panes`,
            // so nothing else would refresh its `last_pane_rects`. Skip
            // this and the very next `list_panes` reports the new pane at
            // 0x0 with zero width/height — geometry the caller would take
            // at face value. Recompute for that workspace alone: no
            // repaint cooldown, no effect on the tab on screen.
            self.relayout_workspace(ws_index);
            // Only the mutated workspace's geometry moved, so only a
            // selection anchored *there* is stale. Clearing
            // unconditionally (what `mark_layout_change` does, correctly,
            // for the active tab) would make a background agent's spawn
            // drop the text the user was in the middle of selecting in
            // the tab they are looking at.
            if self.selection_belongs_to_workspace(ws_index) {
                self.selection = None;
            }
            self.dirty = true;
        }
        // A split during a single-pane tab-close prompt would widen the
        // blast radius past what the user agreed to.
        self.revalidate_close_confirm();
        Ok(Ok(new_id))
    }

    pub(crate) fn spawn_claude_in_selected_dir(&mut self, direction: SplitDirection) -> Result<()> {
        let raw_cwd = self.ws().file_tree.selected_launch_cwd();
        let canon = raw_cwd.canonicalize().unwrap_or_else(|_| raw_cwd.clone());
        let cwd = if canon.is_dir() {
            strip_verbatim_prefix(canon)
        } else {
            self.ws().file_tree.root_path.clone()
        };

        let Some(new_id) = self.split_focused_pane(direction, Some(cwd))? else {
            return Ok(());
        };

        if let Some(pane) = self.ws_mut().panes.get_mut(&new_id) {
            pane.queue_startup_text(&format!("{CLAUDE_PEER_LAUNCH_CMD} "));
        }
        self.emit_pane_started(new_id);
        Ok(())
    }

    pub fn apply_layout(&mut self, config: &LayoutConfig) -> Result<()> {
        let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        validate_layout_cwds(&config.root, &base)?;
        let initial_pane_id = self.ws().focused_pane_id;
        self.apply_layout_node(&config.root, initial_pane_id)?;
        Ok(())
    }

    fn apply_layout_node(&mut self, node: &LayoutNodeSpec, target_pane_id: usize) -> Result<()> {
        match node {
            LayoutNodeSpec::Pane {
                id,
                command,
                role,
                cwd: _,
            } => {
                if !id.is_empty() {
                    self.ws_mut().pane_names.insert(id.clone(), target_pane_id);
                }
                if let Some(pane) = self.ws_mut().panes.get_mut(&target_pane_id) {
                    if let Some(r) = role {
                        pane.role = Some(r.clone());
                    }
                    if let Some(cmd) = command {
                        let upgraded = crate::mcp_peer::upgrade_claude_command(cmd);
                        pane.queue_startup_command(&upgraded);
                    }
                }
                Ok(())
            }
            LayoutNodeSpec::Split {
                direction,
                ratio: _,
                first,
                second,
            } => {
                self.ws_mut().focused_pane_id = target_pane_id;
                let split_dir = match direction {
                    DirectionSpec::Vertical => SplitDirection::Vertical,
                    DirectionSpec::Horizontal => SplitDirection::Horizontal,
                };
                let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                let new_pane_cwd = subtree_root_cwd(second, &base);
                let new_pane_id = self
                    .split_focused_pane(split_dir, new_pane_cwd)?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "layout split refused (too small or MAX_PANES) while applying layout"
                        )
                    })?;
                self.apply_layout_node(first, target_pane_id)?;
                self.apply_layout_node(second, new_pane_id)?;
                self.emit_pane_started(new_pane_id);
                Ok(())
            }
        }
    }

    // ─── Ctrl+W close confirmation (Issue #285) ───────────
    //
    // Two clearly separated routes into the same destructive
    // primitives:
    //
    //   TUI  : `request_close_focused_*` → pending [`CloseConfirm`]
    //          → (user presses `y`) → `close_*_now`
    //   MCP  : `handle_close` → immediate close, no pending state
    //
    // The MCP contract is "close_pane closes the pane", so automation
    // never observes — let alone waits on — a human confirmation.

    /// Ctrl+W on a tab with more than one pane: remember the *focused
    /// pane id* and put up the confirmation. Nothing is destroyed here.
    pub(crate) fn request_close_focused_pane(&mut self) {
        let pane_id = self.ws().focused_pane_id;
        if self.ws().layout.pane_count() <= 1 {
            return;
        }
        self.close_confirm = Some(CloseConfirm::Pane { pane_id });
        self.dirty = true;
    }

    /// Ctrl+W on a single-pane tab: remember the tab by an anchor pane
    /// plus the full pane-id snapshot, so a concurrent split can be
    /// detected on confirm.
    pub(crate) fn request_close_focused_tab(&mut self) {
        if self.workspaces.len() <= 1 {
            return;
        }
        let mut expected_pane_ids = self.ws().layout.collect_pane_ids();
        expected_pane_ids.sort_unstable();
        let Some(&anchor_pane_id) = expected_pane_ids.first() else {
            return;
        };
        self.close_confirm = Some(CloseConfirm::Tab {
            anchor_pane_id,
            expected_pane_ids,
        });
        self.dirty = true;
    }

    /// `n` / `Esc` (and any other invalidation): drop the prompt.
    pub(crate) fn cancel_close_confirm(&mut self) {
        if self.close_confirm.take().is_some() {
            self.dirty = true;
        }
    }

    /// `y`: execute exactly what was pinned at request time, or
    /// nothing at all if the world moved underneath us.
    pub(crate) fn confirm_close_now(&mut self) {
        // Take first: the close primitives call
        // `revalidate_close_confirm`, which must not see the entry
        // it is in the middle of executing.
        let Some(pending) = self.close_confirm.take() else {
            return;
        };
        self.dirty = true;
        match pending {
            CloseConfirm::Pane { pane_id } => self.close_pane_now(pane_id),
            CloseConfirm::Tab {
                anchor_pane_id,
                expected_pane_ids,
            } => self.close_tab_now(anchor_pane_id, &expected_pane_ids),
        }
    }

    /// Close one confirmed pane. Re-resolves the workspace from the
    /// pane id (never `active_tab`) and refuses to escalate: if the
    /// pane became its tab's only pane while the prompt was up, the
    /// user's "close this pane" must not silently become "close this
    /// whole tab".
    ///
    /// A pane that exited naturally (`exited = true`) is still in the
    /// layout, so `y` simply removes it;
    /// [`Self::remove_pane_from_layout`]'s `exit_event_emitted` guard
    /// keeps `PaneExited` exactly-once.
    fn close_pane_now(&mut self, pane_id: usize) {
        let Some(ws_index) = self.workspace_index_of_pane(pane_id) else {
            return;
        };
        if self.workspaces[ws_index].layout.pane_count() <= 1 {
            return;
        }
        let _ = self.remove_pane_from_layout(ws_index, pane_id);
    }

    /// Close one confirmed tab, but only if it is still the same tab
    /// holding exactly the same panes as when the prompt went up.
    fn close_tab_now(&mut self, anchor_pane_id: usize, expected_pane_ids: &[usize]) {
        if self.workspaces.len() <= 1 {
            return;
        }
        let Some(ws_index) = self.workspace_index_of_pane(anchor_pane_id) else {
            return;
        };
        let mut current = self.workspaces[ws_index].layout.collect_pane_ids();
        current.sort_unstable();
        if current != expected_pane_ids {
            return;
        }
        self.close_tab(ws_index);
    }

    /// Whether the live text selection is anchored to something in
    /// workspace `ws_index`.
    ///
    /// A pane selection names its pane, so it can be attributed exactly.
    /// A preview selection belongs to whichever workspace's preview
    /// panel is open — the sidebar is per-workspace state, so it is the
    /// active tab's by construction.
    pub(crate) fn selection_belongs_to_workspace(&self, ws_index: usize) -> bool {
        match self.selection.as_ref().map(|s| &s.target) {
            None => false,
            Some(SelectionTarget::Pane(pane_id)) => self
                .workspaces
                .get(ws_index)
                .is_some_and(|ws| ws.panes.contains_key(pane_id)),
            Some(SelectionTarget::Preview) => ws_index == self.active_tab,
        }
    }

    pub(crate) fn workspace_index_of_pane(&self, pane_id: usize) -> Option<usize> {
        self.workspaces
            .iter()
            .position(|ws| ws.panes.contains_key(&pane_id))
    }

    /// Drop a pending confirmation whose target no longer matches
    /// reality. Called from every layout mutation (MCP close, MCP
    /// split, tab close) so the modal disappears the moment it stops
    /// describing something the user can still agree to.
    pub(crate) fn revalidate_close_confirm(&mut self) {
        let still_valid = match self.close_confirm.as_ref() {
            None => return,
            Some(CloseConfirm::Pane { pane_id }) => self
                .workspace_index_of_pane(*pane_id)
                // Not just "does the pane exist": once it is the tab's
                // only pane the confirmed action is no longer possible.
                .is_some_and(|i| self.workspaces[i].layout.pane_count() > 1),
            Some(CloseConfirm::Tab {
                anchor_pane_id,
                expected_pane_ids,
            }) => {
                self.workspaces.len() > 1
                    && self
                        .workspace_index_of_pane(*anchor_pane_id)
                        .is_some_and(|i| {
                            let mut current = self.workspaces[i].layout.collect_pane_ids();
                            current.sort_unstable();
                            current == *expected_pane_ids
                        })
            }
        };
        if !still_valid {
            self.close_confirm = None;
            self.dirty = true;
        }
    }

    pub(crate) fn remove_pane_from_layout(
        &mut self,
        ws_index: usize,
        pane_id: usize,
    ) -> std::result::Result<(), ipc::CodedError> {
        let ws = &mut self.workspaces[ws_index];
        if !ws.panes.contains_key(&pane_id) {
            return Err(ipc::CodedError::new(
                ipc::err_code::PANE_VANISHED,
                "pane vanished",
            ));
        }
        let pane_ids = ws.layout.collect_pane_ids();
        let current_idx = pane_ids.iter().position(|&id| id == pane_id);

        let exited_meta: Option<(Option<String>, Option<String>)> = {
            let name = ws
                .pane_names
                .iter()
                .find(|(_, id)| **id == pane_id)
                .map(|(n, _)| n.clone());
            match ws.panes.get_mut(&pane_id) {
                Some(pane) if !pane.exit_event_emitted => {
                    pane.exit_event_emitted = true;
                    Some((name, pane.role.clone()))
                }
                _ => None,
            }
        };

        ws.layout.remove_pane(pane_id);

        if let Some(mut pane) = ws.panes.remove(&pane_id) {
            pane.kill();
        }

        ws.pane_names.retain(|_, id| *id != pane_id);
        self.drop_overlay_for_pane(pane_id);
        self.claude_monitor.remove(pane_id);
        self.peer_client_kinds.remove(&pane_id);
        self.peer_unread.remove(&pane_id);
        self.pending_codex_peer_messages.remove(&pane_id);
        if self
            .codex_peer_notification
            .as_ref()
            .is_some_and(|n| n.target_pane == pane_id)
        {
            self.codex_peer_notification = None;
        }

        let ws = &mut self.workspaces[ws_index];
        let remaining_ids = ws.layout.collect_pane_ids();
        if ws.focused_pane_id == pane_id {
            if let Some(idx) = current_idx {
                let new_idx = if idx >= remaining_ids.len() {
                    remaining_ids.len().saturating_sub(1)
                } else {
                    idx
                };
                if let Some(&next) = remaining_ids.get(new_idx) {
                    ws.focused_pane_id = next;
                }
            } else if let Some(&first) = remaining_ids.first() {
                ws.focused_pane_id = first;
            }
        }

        if ws_index == self.active_tab {
            self.mark_layout_change();
        } else {
            // Same rule as a split into a hidden tab: the workspace whose
            // layout changed refreshes its own rects, because nothing
            // else will until it is next rendered.
            self.relayout_workspace(ws_index);
            self.dirty = true;
        }
        // An MCP close that removed the confirmation's target — or that
        // left a pane-close target as its tab's last pane — expires the
        // prompt rather than silently retargeting it.
        self.revalidate_close_confirm();
        if let Some((name, role)) = exited_meta {
            self.emit_pane_exited(pane_id, name, role);
        }
        Ok(())
    }

    pub(crate) fn handle_close(
        &mut self,
        target: &PaneRef,
        from_pane: Option<usize>,
    ) -> std::result::Result<usize, ipc::CodedError> {
        let (ws_index, pane_id) = self.resolve_target_with_global_fallback(from_pane, target)?;

        let is_only_pane = self.workspaces[ws_index].layout.pane_count() <= 1;
        if is_only_pane {
            if self.workspaces.len() <= 1 {
                return Err(ipc::CodedError::new(
                    ipc::err_code::LAST_PANE,
                    "cannot close the last pane of the only tab",
                ));
            }
            self.close_tab(ws_index);
            return Ok(pane_id);
        }

        self.remove_pane_from_layout(ws_index, pane_id)?;
        Ok(pane_id)
    }

    /// Which workspace an IPC request should be resolved against.
    ///
    /// `None` — no `from_pane` on the wire — is the legacy contract the
    /// `renga` CLI still relies on: "the tab the user is looking at".
    /// `Some(id)` is the caller's own pane, so the answer is the tab
    /// that *owns* that pane, which may well not be the visible one
    /// (Issue #288: a background agent's `send_keys` must not land in
    /// whatever tab the human happens to have switched to).
    ///
    /// A `from_pane` that no longer exists is an error, never a
    /// fallback: the caller's pane vanishing mid-call is precisely when
    /// guessing a workspace does the most damage.
    pub(crate) fn resolve_caller_workspace(
        &self,
        from_pane: Option<usize>,
    ) -> std::result::Result<usize, ipc::CodedError> {
        match from_pane {
            None => Ok(self.active_tab),
            Some(id) => self.workspace_index_of_pane(id).ok_or_else(|| {
                ipc::CodedError::new(
                    ipc::err_code::PANE_NOT_FOUND,
                    format!("caller pane {id} not found in any workspace"),
                )
            }),
        }
    }

    /// Resolve `target` for a caller sitting in workspace `ws_idx`.
    ///
    /// - `Focused` / `Name` stay inside `ws_idx`. Both are *relative*
    ///   addresses — "the focused pane", "the pane called worker" — and
    ///   a relative address that silently crosses into another tab is
    ///   the #288 bug in miniature. `Name` in particular is only unique
    ///   per tab, so a global search would be ambiguous by design.
    /// - `Id` searches every workspace. Numeric ids are globally unique
    ///   and unambiguous, so naming one is an explicit cross-tab
    ///   request — the same escape hatch [`Self::handle_close`] has
    ///   offered since it moved to `resolve_pane_across_workspaces`.
    pub(crate) fn resolve_target_from(
        &self,
        ws_idx: usize,
        target: &PaneRef,
    ) -> Option<(usize, usize)> {
        match target {
            PaneRef::Id(id) => self
                .workspaces
                .iter()
                .enumerate()
                .find(|(_, ws)| ws.panes.contains_key(id))
                .map(|(i, _)| (i, *id)),
            PaneRef::Focused | PaneRef::Name(_) => {
                let ws = self.workspaces.get(ws_idx)?;
                ws.resolve_pane_ref(target).map(|id| (ws_idx, id))
            }
        }
    }

    /// The one entry point every pane-targeting IPC handler uses, so
    /// the caller-scope rules cannot drift apart between `send_keys`,
    /// `inspect_pane`, `focus_pane` and the three `spawn_*` tools.
    ///
    /// Note the ordering: the caller is resolved *first* and its
    /// failure is fatal, even for an explicit `Id` target that would
    /// have resolved on its own. A request whose `from_pane` is bogus
    /// is a request we cannot attribute, and attributing it to the
    /// visible tab is the failure mode being fixed here.
    pub(crate) fn resolve_request_target(
        &self,
        from_pane: Option<usize>,
        target: &PaneRef,
    ) -> std::result::Result<(usize, usize), ipc::CodedError> {
        let ws_idx = self.resolve_caller_workspace(from_pane)?;
        let not_found = || {
            ipc::CodedError::new(
                ipc::err_code::PANE_NOT_FOUND,
                format!("pane not found: {target:?}"),
            )
        };
        match from_pane {
            // Legacy: everything — including `Id` — resolves inside the
            // active tab, exactly as before #288. Widening `Id` here
            // would be a silent semantic change for `renga send --id`.
            None => self.workspaces[ws_idx]
                .resolve_pane_ref(target)
                .map(|id| (ws_idx, id))
                .ok_or_else(not_found),
            Some(_) => self
                .resolve_target_from(ws_idx, target)
                .ok_or_else(not_found),
        }
    }

    /// The [`Self::resolve_request_target`] sibling for `Close` and
    /// `SetPaneIdentity` (Issue #296). Same caller-scoped rules once
    /// `from_pane` is present — `Focused` / `Name` stay in the caller's
    /// tab, `Id` still crosses tabs — but a *different legacy branch*.
    ///
    /// These two requests predate `from_pane` with an all-workspace
    /// search (`renga close --id 7` closes pane 7 wherever it lives,
    /// and `--name worker` falls back to other tabs when the visible
    /// one has no such pane). Narrowing that to the active tab would
    /// break the CLI, so `None` keeps
    /// [`Self::resolve_pane_across_workspaces`] verbatim. The five
    /// #288 requests cannot do this: their legacy contract was
    /// active-tab-only, and widening *that* would be the mirror-image
    /// silent change.
    ///
    /// What #296 actually removes is the case that made `close_pane`
    /// dangerous: `Focused` from a pane-bound agent used to mean "the
    /// pane the human is currently looking at".
    pub(crate) fn resolve_target_with_global_fallback(
        &self,
        from_pane: Option<usize>,
        target: &PaneRef,
    ) -> std::result::Result<(usize, usize), ipc::CodedError> {
        let not_found = || {
            ipc::CodedError::new(
                ipc::err_code::PANE_NOT_FOUND,
                format!("pane not found: {target:?}"),
            )
        };
        match from_pane {
            None => self
                .resolve_pane_across_workspaces(target)
                .ok_or_else(not_found),
            // Caller first, and its failure is fatal even for an `Id`
            // target — same ordering rationale as
            // `resolve_request_target`.
            Some(_) => {
                let ws_idx = self.resolve_caller_workspace(from_pane)?;
                self.resolve_target_from(ws_idx, target)
                    .ok_or_else(not_found)
            }
        }
    }

    /// Resolve an explicit `tab` selector (Issue #290) to a workspace
    /// index. The sibling of [`Self::resolve_caller_workspace`] for
    /// requests that name their tab instead of defaulting to the
    /// caller's.
    ///
    /// [`ipc::TabSelector::New`] is not resolvable here — creating a
    /// tab is [`Self::create_tab_with_cwd`]'s job and a `Split` cannot
    /// land in a tab that has no panes yet — so it fails with
    /// `protocol` (the MCP layer routes `{new: …}` to `SpawnTab`
    /// before a `Split` is ever built).
    pub(crate) fn resolve_tab_selector(
        &self,
        selector: &ipc::TabSelector,
    ) -> std::result::Result<usize, ipc::CodedError> {
        match selector {
            ipc::TabSelector::Name(name) => {
                // Scan all tabs, then judge the count. Labels are not
                // unique, and a first-match rule would silently pick a
                // tab the caller never meant — the wrong-tab class of
                // bug #288/#290 exist to prevent.
                let matches: Vec<usize> = self
                    .workspaces
                    .iter()
                    .enumerate()
                    .filter(|(_, ws)| ws.display_name() == name)
                    .map(|(i, _)| i)
                    .collect();
                match matches.as_slice() {
                    [] => Err(ipc::CodedError::new(
                        ipc::err_code::TAB_NOT_FOUND,
                        format!("no tab named {name:?}"),
                    )),
                    [only] => Ok(*only),
                    many => Err(ipc::CodedError::new(
                        ipc::err_code::TAB_AMBIGUOUS,
                        format!(
                            "{} tabs are named {name:?} (indices {many:?}); \
                             address one with {{index}} or {{pane_id}} instead",
                            many.len()
                        ),
                    )),
                }
            }
            ipc::TabSelector::Index(idx) => {
                if *idx < self.workspaces.len() {
                    Ok(*idx)
                } else {
                    Err(ipc::CodedError::new(
                        ipc::err_code::TAB_NOT_FOUND,
                        format!(
                            "tab index {idx} out of range ({} tabs, 0-based)",
                            self.workspaces.len()
                        ),
                    ))
                }
            }
            ipc::TabSelector::PaneId(pane_id) => {
                self.workspace_index_of_pane(*pane_id).ok_or_else(|| {
                    ipc::CodedError::new(
                        ipc::err_code::PANE_NOT_FOUND,
                        format!("tab anchor pane {pane_id} not found in any workspace"),
                    )
                })
            }
            ipc::TabSelector::New { .. } => Err(ipc::CodedError::new(
                ipc::err_code::PROTOCOL,
                "tab.new is not valid for a split request; use spawn_tab",
            )),
        }
    }

    /// Resolve `target` strictly inside tab `ws_idx` — the explicit
    /// `tab` selector path (Issue #290). Unlike
    /// [`Self::resolve_target_from`] there is no numeric-id cross-tab
    /// escape hatch: the caller already said which tab it means, so a
    /// numeric target living elsewhere is a contradiction between the
    /// two halves of the request (`target_tab_mismatch`), not an
    /// implicit redirect.
    pub(crate) fn resolve_target_in_tab(
        &self,
        ws_idx: usize,
        target: &PaneRef,
    ) -> std::result::Result<usize, ipc::CodedError> {
        let not_found = || {
            ipc::CodedError::new(
                ipc::err_code::PANE_NOT_FOUND,
                format!("pane not found: {target:?}"),
            )
        };
        match target {
            PaneRef::Id(id) => match self.workspace_index_of_pane(*id) {
                None => Err(not_found()),
                Some(owner) if owner != ws_idx => Err(ipc::CodedError::new(
                    ipc::err_code::TARGET_TAB_MISMATCH,
                    format!("target pane {id} lives in tab {owner}, not the selected tab {ws_idx}"),
                )),
                Some(_) => Ok(*id),
            },
            PaneRef::Focused | PaneRef::Name(_) => self
                .workspaces
                .get(ws_idx)
                .and_then(|ws| ws.resolve_pane_ref(target))
                .ok_or_else(not_found),
        }
    }

    pub(crate) fn resolve_pane_across_workspaces(
        &self,
        target: &PaneRef,
    ) -> Option<(usize, usize)> {
        match target {
            PaneRef::Focused => {
                let ws = self.ws();
                if ws.panes.contains_key(&ws.focused_pane_id) {
                    Some((self.active_tab, ws.focused_pane_id))
                } else {
                    None
                }
            }
            PaneRef::Id(id) => self
                .workspaces
                .iter()
                .enumerate()
                .find(|(_, ws)| ws.panes.contains_key(id))
                .map(|(i, _)| (i, *id)),
            PaneRef::Name(name) => {
                let ordered: Vec<usize> = std::iter::once(self.active_tab)
                    .chain((0..self.workspaces.len()).filter(|i| *i != self.active_tab))
                    .collect();
                for i in ordered {
                    let ws = &self.workspaces[i];
                    if let Some(&id) = ws.pane_names.get(name) {
                        if ws.panes.contains_key(&id) {
                            return Some((i, id));
                        }
                    }
                }
                None
            }
        }
    }

    /// The non-pane focus stops `Ctrl+Right` visits after it runs out
    /// of panes, in cycle order, filtered to the ones currently on
    /// screen.
    ///
    /// Extracted when the org sidebar added a fourth [`FocusTarget`]:
    /// the previous hand-written if-chains needed one more branch in
    /// each of six places, and the two directions had already drifted
    /// into subtly different shapes. Order is
    /// `panes → file tree → preview → org sidebar → panes`; the sidebar
    /// goes last so the pre-existing `Ctrl+Right` muscle memory
    /// (pane → tree → preview) is untouched.
    fn focus_cycle_targets(&self) -> Vec<FocusTarget> {
        // Membership is decided by the *resolved layout*, not by the
        // logical visibility flags. A panel can be flagged on and still
        // be nowhere on screen — `replace` mode takes the tree's slot,
        // and the degrade ladder drops the preview, then the tree, on a
        // narrow terminal. Cycling onto one of those swallows every
        // later keystroke, and for the file tree a bare `c` / `v` would
        // split the workspace into a Claude pane nobody asked for. The
        // sidebar ships on by default, so it eats columns the other two
        // used to have and makes that degrade case routine rather than
        // an edge case.
        let layout = self.main_area_layout();
        let mut targets = Vec::new();
        if layout.file_tree.is_some() {
            targets.push(FocusTarget::FileTree);
        }
        if layout.preview.is_some() {
            targets.push(FocusTarget::Preview);
        }
        if layout.org_sidebar.is_some() {
            targets.push(FocusTarget::OrgSidebar);
        }
        targets
    }

    pub(crate) fn focus_next_pane(&mut self) {
        let cycle = self.focus_cycle_targets();
        let ws = self.ws_mut();
        let ids = ws.layout.collect_pane_ids();

        if ws.focus_target == FocusTarget::Pane {
            let Some(idx) = ids.iter().position(|&id| id == ws.focused_pane_id) else {
                return;
            };
            if idx + 1 < ids.len() {
                ws.focused_pane_id = ids[idx + 1];
            } else if let Some(&first) = cycle.first() {
                ws.focus_target = first;
            } else {
                ws.focused_pane_id = ids[0];
            }
            return;
        }

        // A panel can vanish while it holds focus (toggled off, or
        // squeezed out by a narrow terminal), in which case it is no
        // longer in the cycle and focus falls back to the panes.
        match cycle.iter().position(|&t| t == ws.focus_target) {
            Some(pos) if pos + 1 < cycle.len() => ws.focus_target = cycle[pos + 1],
            _ => ws.focus_target = FocusTarget::Pane,
        }
    }

    pub(crate) fn focus_prev_pane(&mut self) {
        let cycle = self.focus_cycle_targets();
        let ws = self.ws_mut();
        let ids = ws.layout.collect_pane_ids();

        if ws.focus_target == FocusTarget::Pane {
            let Some(idx) = ids.iter().position(|&id| id == ws.focused_pane_id) else {
                return;
            };
            if idx > 0 {
                ws.focused_pane_id = ids[idx - 1];
            } else if let Some(&last) = cycle.last() {
                ws.focus_target = last;
            } else {
                ws.focused_pane_id = ids[ids.len() - 1];
            }
            return;
        }

        match cycle.iter().position(|&t| t == ws.focus_target) {
            Some(pos) if pos > 0 => ws.focus_target = cycle[pos - 1],
            _ => {
                ws.focus_target = FocusTarget::Pane;
                if let Some(&last) = ids.last() {
                    ws.focused_pane_id = last;
                }
            }
        }
    }

    /// The one supported way to change the active tab.
    ///
    /// Tab switching used to be `self.active_tab = n` copy-pasted
    /// across four call sites, each remembering a different subset of
    /// the bookkeeping: the keyboard paths suspended the IME overlay
    /// unconditionally (even when switching to the tab already active),
    /// the mouse path was the only one clearing the double-click
    /// caches, and none of them dropped the stale text selection. The
    /// org sidebar adds a fifth caller and a piece of state that
    /// *must* survive the switch, so the whole sequence lives here.
    ///
    /// Returns `true` when the active tab actually changed.
    pub(crate) fn switch_tab(&mut self, index: usize) -> bool {
        // `ws()` indexes `workspaces` directly and would panic. The
        // sidebar renders from a snapshot that an MCP `close_tab` can
        // invalidate between paint and click, so this is a live guard,
        // not a formality.
        if index >= self.workspaces.len() || index == self.active_tab {
            return false;
        }
        self.suspend_overlay();
        // Focus is per-workspace, so the incoming tab would otherwise
        // restore whatever it was focused on last — knocking the user
        // out of the sidebar they are currently driving.
        let keep_sidebar_focus = self.ws().focus_target == FocusTarget::OrgSidebar;
        self.active_tab = index;
        if keep_sidebar_focus {
            self.ws_mut().focus_target = FocusTarget::OrgSidebar;
        }
        // Every one of these is keyed by tab index or by a pane that
        // belongs to the tab we just left.
        self.last_tab_click = None;
        self.last_edge_click = None;
        self.last_boundary_click = None;
        self.selection = None;
        self.dirty = true;
        true
    }
}

/// Walk a layout subtree and return the cwd that should be used for
/// the pane hosting that subtree's root position. For a bare `Pane`
/// leaf that's simply its own cwd; for a `Split` we inherit from the
/// `first` child so the parent pane (already present before the split)
/// and the subtree agree. Relative paths are joined onto `base`. A
/// `None` result means "use default" (inherit from parent pane or
/// process cwd).
pub(crate) fn subtree_root_cwd(node: &LayoutNodeSpec, base: &std::path::Path) -> Option<PathBuf> {
    match node {
        LayoutNodeSpec::Pane { cwd, .. } => cwd.as_deref().and_then(|s| {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                let p = PathBuf::from(t);
                Some(if p.is_absolute() { p } else { base.join(p) })
            }
        }),
        LayoutNodeSpec::Split { first, .. } => subtree_root_cwd(first, base),
    }
}

/// Pre-flight check: walk the layout tree and fail fast if any `cwd`
/// field doesn't point at an existing directory. Keeps the mutation
/// semantics consistent with `Request::Split` / `Request::NewTab` —
/// bad cwd = no partial layout.
pub(crate) fn validate_layout_cwds(node: &LayoutNodeSpec, base: &std::path::Path) -> Result<()> {
    match node {
        LayoutNodeSpec::Pane { cwd, id, .. } => {
            if let Some(raw) = cwd {
                let t = raw.trim();
                if !t.is_empty() {
                    let p = PathBuf::from(t);
                    let joined = if p.is_absolute() { p } else { base.join(p) };
                    let meta = std::fs::metadata(&joined).map_err(|e| {
                        anyhow::anyhow!(
                            "layout pane '{id}' cwd {} is not accessible: {e}",
                            joined.display()
                        )
                    })?;
                    if !meta.is_dir() {
                        return Err(anyhow::anyhow!(
                            "layout pane '{id}' cwd {} is not a directory",
                            joined.display()
                        ));
                    }
                }
            }
            Ok(())
        }
        LayoutNodeSpec::Split { first, second, .. } => {
            validate_layout_cwds(first, base)?;
            validate_layout_cwds(second, base)?;
            Ok(())
        }
    }
}

/// Resolve an optional `cwd` string from an IPC Split / NewTab request
/// into an absolute `PathBuf`. Relative paths are joined onto `base`
/// (the target pane's cwd for Split, the server's process cwd for
/// NewTab). Missing / non-directory paths surface as
/// [`ipc::err_code::CWD_INVALID`] so the caller can distinguish them
/// from other failure codes. Returns `Ok(None)` when the caller did
/// not supply a cwd (including empty / whitespace) — preserving the
/// pre-cwd default-inheritance behavior.
pub(crate) fn resolve_optional_cwd(
    cwd: Option<&str>,
    base: &std::path::Path,
) -> std::result::Result<Option<PathBuf>, ipc::CodedError> {
    let raw = match cwd {
        Some(s) => s.trim(),
        None => return Ok(None),
    };
    if raw.is_empty() {
        return Ok(None);
    }
    let candidate = PathBuf::from(raw);
    let joined = if candidate.is_absolute() {
        candidate
    } else {
        base.join(candidate)
    };
    // Canonicalize so the stored cwd is stable and follows `..` /
    // symlinks deterministically. Canonicalize also implicitly
    // verifies existence — a missing path errors out here.
    let canon = std::fs::canonicalize(&joined).map_err(|e| {
        ipc::CodedError::new(
            ipc::err_code::CWD_INVALID,
            format!("cwd {} is not accessible: {e}", joined.display()),
        )
    })?;
    // Directory check goes against the canonical (verbatim-prefixed on
    // Windows) path, not the stripped one. Long paths / UNC shares can
    // fail `is_dir()` once the `\\?\` prefix is removed, so we verify
    // first and then strip purely for display/storage.
    if !canon.is_dir() {
        return Err(ipc::CodedError::new(
            ipc::err_code::CWD_INVALID,
            format!("cwd {} is not a directory", canon.display()),
        ));
    }
    // Windows' canonicalize returns a `\\?\C:\...` verbatim path,
    // which leaks into `PaneInfo.cwd` / MCP list output and looks
    // wrong in user-facing tooling. Strip the prefix for storage so
    // the PTY cwd string matches what a shell would show.
    Ok(Some(strip_verbatim_prefix(canon)))
}

/// Strip Windows `\\?\` (verbatim) and `\\?\UNC\` prefixes from a
/// canonicalized path. On non-Windows this is an identity function.
/// Kept as a free function so both the IPC cwd resolver and any
/// future code paths that serialize canonicalized paths can share one
/// definition. Prefers string manipulation over `dunce` so we don't
/// add a dependency for a few lines of path-prefix cleanup.
pub(crate) fn strip_verbatim_prefix(p: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.to_string_lossy().into_owned();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            // `\\?\UNC\server\share\...` → `\\server\share\...`
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
        PathBuf::from(s)
    }
    #[cfg(not(windows))]
    {
        p
    }
}

pub(crate) fn default_command_for_role(role: Option<&str>) -> Option<String> {
    match role {
        Some("claude") => Some(CLAUDE_PEER_LAUNCH_CMD.to_string()),
        _ => None,
    }
}

/// Extract directory name from a path for tab title.
pub(crate) fn dir_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}
