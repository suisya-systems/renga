use super::*;

/// How often [`App::tick_claude_snapshots`] is allowed to walk every
/// tab. The per-pane monitors have their own 500 ms / 2 s throttles;
/// this one bounds the cost of the walk itself (a `collect_pane_ids`
/// per tab plus a mutex round-trip per pane) so it stays a few times a
/// second instead of once per event-loop turn (~30 Hz by default).
const SNAPSHOT_SWEEP_INTERVAL: Duration = Duration::from_millis(250);

/// Output silence after which a pane counts as waiting for input
/// (`pane_waiting_input`, Issue #72). Agents animate a spinner while
/// they work, so a few seconds of silence means they stopped.
const WAITING_INPUT_IDLE: Duration = Duration::from_secs(5);

impl App {
    #[allow(dead_code)] // retained as a test-ergonomic alias for new_with_cwd(None)
    pub fn new(rows: u16, cols: u16) -> Result<Self> {
        Self::new_with_cwd(rows, cols, None)
    }

    /// Like [`Self::new`] but lets the initial pane spawn in an
    /// explicit cwd. `None` preserves the historical process-cwd
    /// behavior. Used by `main` to honor a layout's root-leaf `cwd`
    /// before the TUI is handed the app state.
    pub fn new_with_cwd(rows: u16, cols: u16, initial_cwd: Option<PathBuf>) -> Result<Self> {
        let (event_tx, event_rx) = mpsc::channel();
        let (command_tx, command_rx) = mpsc::channel();

        let pane_rows = rows.saturating_sub(5); // title + tab bar + status + borders
        let pane_cols = cols.saturating_sub(2);

        let cwd = initial_cwd
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let name = dir_name(&cwd);

        let ws = Workspace::new(name, cwd, 1, pane_rows, pane_cols, event_tx.clone())?;

        let event_bus = crate::ipc::EventBus::new();
        // Initial pane already exists at this point; emit its
        // PaneStarted so subscribers joining immediately after App
        // construction see it.
        event_bus.emit(crate::ipc::Event::PaneStarted {
            id: 1,
            name: None,
            role: None,
            ts_ms: crate::ipc::events::now_ms(),
        });

        Ok(Self {
            workspaces: vec![ws],
            active_tab: 0,
            should_quit: false,
            event_tx,
            event_rx,
            command_tx,
            command_rx,
            next_pane_id: 2,
            dirty: true,
            paste_cooldown: 0,
            resize_cooldown: 0,
            last_term_size: (cols, rows),
            deferred_caret: None,
            file_tree_width: 20,
            preview_width: 40,
            layout_swapped: true,
            status_bar_visible: true,
            dragging: None,
            hover_border: None,
            last_tab_rects: Vec::new(),
            last_new_tab_rect: None,
            rename_input: None,
            overlay: None,
            close_confirm: None,
            saved_overlay_drafts: HashMap::new(),
            last_tab_click: None,
            last_edge_click: None,
            last_boundary_click: None,
            selection: None,
            version_info: {
                let info = crate::version_check::VersionInfo::new();
                crate::version_check::spawn_check(info.clone());
                info
            },
            claude_monitor: crate::claude_monitor::ClaudeMonitor::new(),
            peer_client_kinds: HashMap::new(),
            peer_unread: HashMap::new(),
            next_peer_msg_id: 1,
            pending_codex_peer_messages: HashMap::new(),
            codex_peer_notification: None,
            recent_peer_sends: HashMap::new(),
            pending_user_turns: Vec::new(),
            recent_user_turn_sends: HashMap::new(),
            #[cfg(test)]
            user_turn_writes: Vec::new(),
            clipboard: None,
            event_bus,
            ime_mode: crate::config::ImeMode::default(),
            lang: crate::i18n::Lang::default(),
            ime_freeze_panes_on_overlay: false,
            ime_overlay_catchup_ms: 0,
            last_overlay_repaint: None,
            min_pane_width: 20,
            min_pane_height: 5,
            image_picker: None,
            macos_tip_visible: false,
            macos_tip_shown_at: None,
            macos_tip_marker: None,
            // Placeholder mode; the real `[ui] org_sidebar` value lands
            // in `apply_config`, mirroring how `ime_mode` / `lang` are
            // seeded here and resolved there.
            org_sidebar_mode: crate::config::OrgSidebarMode::default(),
            org_sidebar_visible: false,
            org_sidebar_width: crate::app::layout_geometry::DEFAULT_ORG_SIDEBAR_WIDTH,
            last_org_sidebar_rect: None,
            org_sidebar_scroll: 0,
            org_sidebar_selection: None,
            org_sidebar_row_targets: Vec::new(),
            org_sidebar_follow_selection: true,
            claude_snapshots: HashMap::new(),
            last_claude_sweep: None,
            last_prompt_sweep: None,
        })
    }

    /// Surface the first-launch macOS Option-as-Meta banner for this
    /// session. Starts the 10-second auto-dismiss timer and remembers
    /// the marker path so a later key-press or timeout can persist
    /// "user saw it, never show again". Idempotent — repeated calls
    /// restart the timer rather than compounding.
    pub fn show_macos_tip(&mut self, marker: Option<PathBuf>) {
        self.macos_tip_visible = true;
        self.macos_tip_shown_at = Some(Instant::now());
        self.macos_tip_marker = marker;
        self.dirty = true;
    }

    /// Hide the banner and persist dismissal via the marker file if
    /// one is configured. Silent no-op when the banner isn't up.
    pub fn dismiss_macos_tip(&mut self) {
        if !self.macos_tip_visible {
            return;
        }
        self.macos_tip_visible = false;
        self.macos_tip_shown_at = None;
        if let Some(path) = self.macos_tip_marker.take() {
            crate::macos_tip::mark_dismissed(&path);
        }
        self.dirty = true;
    }

    /// Auto-dismiss when the banner has been visible longer than
    /// [`crate::macos_tip::AUTO_DISMISS`]. Called from the main loop
    /// alongside `maybe_tick_overlay_catchup`; cheap no-op in the
    /// common case (banner not visible).
    pub fn check_macos_tip_timeout(&mut self) {
        if !self.macos_tip_visible {
            return;
        }
        let Some(shown_at) = self.macos_tip_shown_at else {
            return;
        };
        if shown_at.elapsed() >= crate::macos_tip::AUTO_DISMISS {
            self.dismiss_macos_tip();
        }
    }

    pub(crate) fn suspend_overlay(&mut self) {
        if let Some(overlay) = self.overlay.take() {
            self.saved_overlay_drafts
                .insert(overlay.target_pane, overlay);
        }
    }

    pub(crate) fn clear_overlay_draft(&mut self, pane_id: usize) {
        self.saved_overlay_drafts.remove(&pane_id);
    }

    pub(crate) fn take_overlay_draft(&mut self, pane_id: usize) -> Option<OverlayState> {
        self.saved_overlay_drafts.remove(&pane_id)
    }

    pub(crate) fn drop_overlay_for_pane(&mut self, pane_id: usize) {
        self.clear_overlay_draft(pane_id);
        if self
            .overlay
            .as_ref()
            .is_some_and(|overlay| overlay.target_pane == pane_id)
        {
            self.overlay = None;
        }
    }

    /// Install a user-level config on top of the default App state.
    /// Called by `main` right after [`App::new`] so the CLI / config
    /// precedence in `config::Config::apply_cli_overrides` has already
    /// collapsed into a single resolved value.
    pub fn apply_config(&mut self, cfg: &crate::config::Config) {
        self.ime_mode = cfg.ime.mode;
        // Resolve `auto` against the live OS locale here rather than at
        // field-apply time so test harnesses can stub `current_os_locale`
        // by setting `cfg.ui.lang` to an explicit variant instead of
        // mutating environment state. Production callers hit the real
        // sys-locale path.
        self.lang = cfg
            .ui
            .lang
            .resolve(crate::i18n::current_os_locale().as_deref());
        self.ime_freeze_panes_on_overlay = cfg.ime.freeze_panes_on_overlay;
        // 0 means "catch-up disabled"; any non-zero value is floored
        // at MIN_OVERLAY_CATCHUP_MS so a fat-fingered `--…-catchup-ms 5`
        // can't turn freeze into a ~200 fps repaint storm.
        self.ime_overlay_catchup_ms = if cfg.ime.overlay_catchup_ms == 0 {
            0
        } else {
            cfg.ime
                .overlay_catchup_ms
                .max(crate::config::MIN_OVERLAY_CATCHUP_MS)
        };
        self.org_sidebar_mode = cfg.ui.org_sidebar;
        // `coexist` / `replace` both mean "the user wants this panel",
        // so it comes up with the app; `off` leaves it disabled and
        // makes Ctrl+B inert.
        self.org_sidebar_visible = self.org_sidebar_enabled();
    }

    /// Resolved message table for the current UI language. Prefer this
    /// over hand-rolled `Lang::messages()` calls so renderers never
    /// have to care about the enum → static-table indirection.
    pub fn messages(&self) -> &'static crate::i18n::Messages {
        self.lang.messages()
    }

    /// Time-based catch-up for the freeze-panes-on-overlay path.
    /// While the overlay is open AND freeze is on AND catch-up is
    /// configured to a non-zero interval, force a single repaint
    /// whenever the interval has elapsed. The user sees periodic
    /// body-content progress (Claude writing new lines, shell output
    /// scrolling) without the continuous flicker that plain
    /// freeze=off produces. No-op otherwise.
    pub fn maybe_tick_overlay_catchup(&mut self) {
        if self.overlay.is_none() {
            // Reset timer so the next open starts clean; otherwise a
            // catch-up could fire 0 ms after a fresh open if the
            // previous session left a stale Instant behind.
            self.last_overlay_repaint = None;
            return;
        }
        if !self.ime_freeze_panes_on_overlay || self.ime_overlay_catchup_ms == 0 {
            return;
        }
        let interval = std::time::Duration::from_millis(self.ime_overlay_catchup_ms);
        let now = Instant::now();
        match self.last_overlay_repaint {
            None => {
                // First tick of this overlay session — anchor the
                // timer at "now" without repainting, so the first
                // catch-up fires `interval` after open, not
                // immediately.
                self.last_overlay_repaint = Some(now);
            }
            Some(prev) if now.duration_since(prev) >= interval => {
                self.dirty = true;
                self.last_overlay_repaint = Some(now);
            }
            _ => {}
        }
    }

    /// Override the minimum per-child split dimensions. Values of `0`
    /// are clamped to `1` so `rect.width / 2 < min` stays meaningful
    /// (`0` would let splits succeed on a 1-column pane and produce
    /// zero-width children).
    pub fn set_min_pane_size(&mut self, width: u16, height: u16) {
        self.min_pane_width = width.max(1);
        self.min_pane_height = height.max(1);
    }

    /// Emit a [`PaneStarted`] event for a pane in the active workspace.
    pub(crate) fn emit_pane_started(&self, pane_id: usize) {
        self.emit_pane_started_in(self.active_tab, pane_id);
    }

    /// Emit a [`PaneStarted`] event for a pane in workspace `ws_index`,
    /// pulling the name/role that was just attached to it.
    ///
    /// The workspace has to be named explicitly: pane ids are unique
    /// App-wide, so looking one up in the *active* workspace after a
    /// cross-tab spawn silently finds nothing and emits `name: null,
    /// role: null` instead of erroring. `poll_events` is process-wide,
    /// so an orchestrator waiting on the worker it just named by
    /// `spawn_claude_pane(name = ...)` would never match the event.
    pub(crate) fn emit_pane_started_in(&self, ws_index: usize, pane_id: usize) {
        let Some(ws) = self.workspaces.get(ws_index) else {
            return;
        };
        let name = ws
            .pane_names
            .iter()
            .find(|(_, id)| **id == pane_id)
            .map(|(n, _)| n.clone());
        let role = ws.panes.get(&pane_id).and_then(|p| p.role.clone());
        self.event_bus.emit(crate::ipc::Event::PaneStarted {
            id: pane_id,
            name,
            role,
            ts_ms: crate::ipc::events::now_ms(),
        });
    }

    /// Emit a [`PaneExited`] event. Expects the caller to have already
    /// set `Pane.exit_event_emitted = true` (or to be about to remove
    /// the pane) so the event is exactly-once.
    pub(crate) fn emit_pane_exited(
        &self,
        pane_id: usize,
        name: Option<String>,
        role: Option<String>,
    ) {
        self.event_bus.emit(crate::ipc::Event::PaneExited {
            id: pane_id,
            name,
            role,
            ts_ms: crate::ipc::events::now_ms(),
        });
    }

    /// Copy text to clipboard, reusing the handle if available.
    pub(crate) fn copy_to_clipboard(&mut self, text: &str) {
        if let Some(ref mut cb) = self.clipboard {
            if cb.set_text(text).is_ok() {
                return;
            }
            self.clipboard = None;
        }

        self.clipboard = arboard::Clipboard::new().ok();
        if let Some(ref mut cb) = self.clipboard {
            if cb.set_text(text).is_ok() {
                return;
            }
        }

        if running_under_wsl() {
            let _ = copy_to_windows_clipboard(text);
        }
    }
}

fn running_under_wsl() -> bool {
    std::env::var_os("WSL_INTEROP").is_some()
        || std::env::var_os("WSL_DISTRO_NAME").is_some()
        || std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|release| release.to_ascii_lowercase().contains("microsoft"))
            .unwrap_or(false)
}

fn copy_to_windows_clipboard(text: &str) -> std::io::Result<()> {
    let mut child = Command::new("clip.exe")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }

    child.wait()?;
    Ok(())
}

/// Terminal dimensions below which the layout pass is skipped
/// entirely — there is not enough room to place a pane area, a tab bar
/// and borders.
pub(crate) const MIN_LAYOUT_COLS: u16 = 20;
pub(crate) const MIN_LAYOUT_ROWS: u16 = 5;

impl App {
    /// Recompute pane rectangles and apply sizes to every PTY in the
    /// active workspace. Returns `true` if any pane was actually
    /// resized (so callers can decide whether to enter the post-resize
    /// cooldown). Safe to call without a Frame — uses the cached
    /// `last_term_size`.
    pub fn relayout_panes(&mut self) -> bool {
        self.relayout_workspace(self.active_tab)
    }

    /// Recompute a workspace's cached rectangles **without touching its
    /// PTYs**.
    ///
    /// This is the half of the layout pass that is safe to run on a
    /// hidden tab. The other half — [`Self::relayout_workspace`] — calls
    /// [`Pane::resize`], which clears the vt100 buffer and leaves the
    /// child to repaint on SIGWINCH. A TUI child does repaint; a plain
    /// shell does not, so resizing a hidden pane destroys scrollback
    /// nobody asked to lose and that nothing regenerates. Anything that
    /// merely wants to *report* a hidden tab's geometry (`list_panes`)
    /// or *judge* it (the split min-size guard) wants this, not that.
    pub(crate) fn recompute_workspace_rects(&mut self, ws_index: usize) {
        if self.workspaces.get(ws_index).is_none() || self.terminal_too_small_for_layout() {
            return;
        }
        let rects = self.workspaces[ws_index]
            .layout
            .calculate_rects(self.main_area_layout_for(ws_index).panes);
        self.workspaces[ws_index].last_pane_rects = rects;
    }

    /// [`Self::relayout_panes`] for an arbitrary workspace: recompute the
    /// rectangles *and* push the new sizes into the PTYs.
    ///
    /// Reserve this for a workspace whose layout actually changed (a
    /// split, a pane close) or that is on screen. Resizing clears each
    /// pane's screen, so calling it speculatively on a hidden tab is a
    /// destructive read — see [`Self::recompute_workspace_rects`].
    pub fn relayout_workspace(&mut self, ws_index: usize) -> bool {
        if self.workspaces.get(ws_index).is_none() {
            return false;
        }
        if self.terminal_too_small_for_layout() {
            return false;
        }

        let rects = self.workspaces[ws_index]
            .layout
            .calculate_rects(self.main_area_layout_for(ws_index).panes);

        let mut any_changed = false;
        for (pane_id, rect) in &rects {
            if let Some(pane) = self.workspaces[ws_index].panes.get_mut(pane_id) {
                let inner_rows = rect.height.saturating_sub(2);
                let inner_cols = rect.width.saturating_sub(2);
                if pane.resize(inner_rows, inner_cols).unwrap_or(false) {
                    any_changed = true;
                }
            }
        }

        self.workspaces[ws_index].last_pane_rects = rects;
        any_changed
    }

    /// Resolve the main area's geometry from the cached terminal size,
    /// without needing a `Frame`.
    ///
    /// This is what lets non-render code ask "is the file tree actually
    /// on screen?" — a question the raw `file_tree_visible` flag cannot
    /// answer once `replace` mode and the narrow-terminal degrade
    /// ladder are in play, and one that focus and key routing have to
    /// get right or they hand the keyboard to an invisible panel.
    /// Reading `last_*_rect` instead would be wrong before the first
    /// paint and stale right after a resize.
    ///
    /// The vertical slots still mirror `ui::render` by hand (tab bar,
    /// main area, macOS tip, status bar); only the horizontal split is
    /// shared, via `layout_geometry::compute`.
    pub(crate) fn main_area_layout(&self) -> layout_geometry::MainAreaLayout {
        self.main_area_layout_for(self.active_tab)
    }

    /// [`Self::main_area_layout`] for an arbitrary workspace.
    ///
    /// Two of the horizontal inputs — whether the file tree is open and
    /// whether the preview is active — are per-workspace, so a hidden
    /// tab's pane area is genuinely not the visible tab's. The PTY
    /// resize path needs this to size a background tab's panes the way
    /// rendering *that* tab would (Issue #288).
    pub(crate) fn main_area_layout_for(&self, ws_index: usize) -> layout_geometry::MainAreaLayout {
        let (cols, rows) = self.last_term_size;
        let tab_h = 1u16;
        let status_h: u16 = if self.status_bar_visible || self.rename_input.is_some() {
            1
        } else {
            0
        };
        // The IME composition overlay is drawn as a centered floating
        // box on top of the pane area (see `ui::render_ime_overlay`),
        // so unlike the old single-row widget it does not claim a
        // layout slot — panes keep their full height whether the
        // overlay is open or not. The first-launch macOS tip *does*
        // claim two rows, so it has to come off the pane height here
        // as well or the PTYs spend the banner's lifetime believing
        // they are two rows taller than what gets painted.
        let macos_tip_h: u16 = if self.macos_tip_visible { 2 } else { 0 };
        let main_h = rows.saturating_sub(tab_h + status_h + macos_tip_h);
        layout_geometry::compute(
            self.main_area_input_for(ws_index, Rect::new(0, tab_h, cols, main_h)),
        )
    }

    /// Collect the horizontal-layout inputs for `area`.
    ///
    /// Both callers of [`layout_geometry::compute`] go through this so
    /// the renderer and the PTY-resize path cannot disagree about *what*
    /// they asked for, on top of already agreeing about how it resolves.
    ///
    /// [`layout_geometry::compute`]: crate::app::layout_geometry::compute
    pub(crate) fn main_area_input(&self, area: Rect) -> layout_geometry::MainAreaInput {
        self.main_area_input_for(self.active_tab, area)
    }

    /// [`Self::main_area_input`] for an arbitrary workspace. Falls back
    /// to the active tab's panel state if `ws_index` is out of range, so
    /// callers get the pre-#288 answer rather than a panic.
    pub(crate) fn main_area_input_for(
        &self,
        ws_index: usize,
        area: Rect,
    ) -> layout_geometry::MainAreaInput {
        let ws = self.workspaces.get(ws_index).unwrap_or_else(|| self.ws());
        layout_geometry::MainAreaInput {
            area,
            org_sidebar_mode: self.org_sidebar_mode,
            org_sidebar_visible: self.org_sidebar_visible,
            org_sidebar_width: self.org_sidebar_width,
            file_tree_visible: ws.file_tree_visible,
            file_tree_width: self.file_tree_width,
            preview_active: ws.preview.is_active(),
            preview_width: self.preview_width,
            layout_swapped: self.layout_swapped,
        }
    }

    /// Refresh the org sidebar's per-pane Claude snapshots.
    ///
    /// Called once per event-loop turn (alongside the other
    /// `maybe_*` / `check_*` tickers) rather than from the renderer,
    /// because the sidebar shows *every* tab and the renderer only ever
    /// walks the active one. Three separate throttles keep the cost
    /// bounded:
    ///
    /// * this sweep itself runs at most every [`SNAPSHOT_SWEEP_INTERVAL`],
    /// * panes in the visible tab are polled at the usual
    ///   `CHECK_INTERVAL` (unchanged from the pre-sidebar behaviour),
    /// * panes in background tabs are polled at
    ///   `BACKGROUND_CHECK_INTERVAL`, since nobody is watching them
    ///   closely enough to notice a two-second lag.
    ///
    /// Repaints are only requested when a snapshot actually differs
    /// from the cached one — without that the sweep would mark the UI
    /// dirty several times a second forever.
    pub(crate) fn tick_claude_snapshots(&mut self) {
        if !self.org_sidebar_active() {
            // Nothing reads the cache while the panel is down. The
            // active tab keeps being polled by `render_panes`, so the
            // pane borders and status bar are unaffected.
            return;
        }
        let now = Instant::now();
        if self
            .last_claude_sweep
            .is_some_and(|t| now.duration_since(t) < SNAPSHOT_SWEEP_INTERVAL)
        {
            return;
        }
        self.last_claude_sweep = Some(now);

        // Snapshot the (pane, cwd, interval) triples before touching the
        // monitor so the workspace borrow ends first.
        let active = self.active_tab;
        let targets: Vec<(usize, PathBuf, Duration)> = self
            .workspaces
            .iter()
            .enumerate()
            .flat_map(|(tab, ws)| {
                let interval = if tab == active {
                    crate::claude_monitor::CHECK_INTERVAL
                } else {
                    crate::claude_monitor::BACKGROUND_CHECK_INTERVAL
                };
                ws.layout
                    .collect_pane_ids()
                    .into_iter()
                    .filter_map(move |id| ws.panes.get(&id).map(|p| (id, p.cwd.clone(), interval)))
            })
            .collect();

        let mut changed = false;
        for (pane_id, cwd, interval) in &targets {
            self.claude_monitor
                .update_throttled(*pane_id, cwd, *interval);
            let snapshot = self.claude_monitor.snapshot(*pane_id);
            match self.claude_snapshots.get(pane_id) {
                Some(prev) if *prev == snapshot => {}
                _ => {
                    self.claude_snapshots.insert(*pane_id, snapshot);
                    changed = true;
                }
            }
        }

        // Drop entries for panes that have gone away, so a long session
        // that churns through panes doesn't grow the map forever.
        if self.claude_snapshots.len() > targets.len() {
            let live: HashSet<usize> = targets.iter().map(|(id, _, _)| *id).collect();
            self.claude_snapshots.retain(|id, _| live.contains(id));
            changed = true;
        }

        // Honour the IME overlay freeze (#37 / #82). `drain_pty_events`
        // suppresses PTY-driven repaints while the overlay is open so
        // composition doesn't flicker; marking dirty here on every
        // background-tab status change would punch straight through
        // that at up to 4 Hz. The cache is still refreshed above, so
        // the sidebar is correct at the next repaint — which the
        // overlay catch-up tick already schedules.
        if changed && !(self.overlay.is_some() && self.ime_freeze_panes_on_overlay) {
            self.dirty = true;
        }
    }

    /// Emit `pane_prompt_detected` / `pane_waiting_input` (Issue #72)
    /// and `pane_mode_changed` (Issue #49) for every live pane in every
    /// tab. Throttled like the snapshot
    /// sweep; only panes that produced output since the last pass get
    /// their screen rescanned.
    pub(crate) fn tick_prompt_events(&mut self) {
        let now = Instant::now();
        if self
            .last_prompt_sweep
            .is_some_and(|t| now.duration_since(t) < SNAPSHOT_SWEEP_INTERVAL)
        {
            return;
        }
        self.last_prompt_sweep = Some(now);

        let mut events = Vec::new();
        for ws in &mut self.workspaces {
            for (&id, pane) in ws.panes.iter_mut() {
                if pane.exited {
                    continue;
                }
                let mut found = None;
                let mut mode = None;
                if pane.output_seen {
                    let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
                    // A scrolled-back view shows history, not the live
                    // screen; leave `output_seen` set and rescan once
                    // the user is back at the bottom.
                    if parser.screen().scrollback() == 0 {
                        found = Some(crate::pane::detect_interactive_prompt(parser.screen()));
                        if pane.claude_ever_seen() {
                            mode = crate::pane::detect_claude_mode(parser.screen());
                        }
                    }
                }
                let name = || {
                    ws.pane_names
                        .iter()
                        .find(|(_, pid)| **pid == id)
                        .map(|(n, _)| n.clone())
                };
                match found {
                    Some(Some((kind, prompt, key))) => {
                        pane.output_seen = false;
                        if pane.reported_prompt.as_deref() != Some(key.as_str()) {
                            pane.reported_prompt = Some(key);
                            events.push(crate::ipc::Event::PanePromptDetected {
                                id,
                                name: name(),
                                role: pane.role.clone(),
                                kind: kind.to_string(),
                                prompt,
                                ts_ms: crate::ipc::events::now_ms(),
                            });
                        }
                    }
                    Some(None) => {
                        pane.output_seen = false;
                        pane.reported_prompt = None;
                    }
                    None => {}
                }
                if let Some(mode) = mode.filter(|m| pane.reported_mode != Some(*m)) {
                    events.push(crate::ipc::Event::PaneModeChanged {
                        id,
                        name: name(),
                        role: pane.role.clone(),
                        mode: mode.to_string(),
                        prev_mode: pane.reported_mode.replace(mode).map(str::to_string),
                        ts_ms: crate::ipc::events::now_ms(),
                    });
                }
                let idle = now.duration_since(pane.last_output_at);
                if !pane.waiting_input_reported && idle >= WAITING_INPUT_IDLE {
                    pane.waiting_input_reported = true;
                    events.push(crate::ipc::Event::PaneWaitingInput {
                        id,
                        name: name(),
                        role: pane.role.clone(),
                        idle_ms: idle.as_millis() as u64,
                        ts_ms: crate::ipc::events::now_ms(),
                    });
                }
            }
        }
        for ev in events {
            self.event_bus.emit(ev);
        }
    }

    /// Mark a layout change: apply resizes immediately and, if sizes
    /// actually changed, delay the next paint for a few frames so the
    /// PTY child can respond to SIGWINCH with a fresh redraw before
    /// we render. When no size changes happen (e.g. a sidebar toggle
    /// that fits in the same remaining width) we skip the cooldown so
    /// the UI stays responsive. Also drops any live selection, whose
    /// stored `content_rect` / `pane_id` could reference a layout that
    /// no longer exists.
    pub fn mark_layout_change(&mut self) {
        let changed = self.relayout_panes();
        if changed {
            // Take max so a freshly-triggered layout change on top of
            // an existing cooldown doesn't prematurely cut the wait.
            self.resize_cooldown = self.resize_cooldown.max(5);
        }
        // Any in-flight selection is bound to the old geometry.
        self.selection = None;
        self.dirty = true;
    }

    /// Whether the terminal is too small for the layout pass to run at
    /// all. Below this, [`Self::relayout_workspace`] bails and every
    /// workspace keeps whatever `last_pane_rects` it last had — so
    /// anything that would *act* on those rects has to treat them as
    /// unknown rather than current.
    pub(crate) fn terminal_too_small_for_layout(&self) -> bool {
        let (cols, rows) = self.last_term_size;
        cols < MIN_LAYOUT_COLS || rows < MIN_LAYOUT_ROWS
    }

    /// Called from main.rs on crossterm Resize events so we can update
    /// the cached terminal size and propagate the resize into panes.
    ///
    /// Hidden workspaces get their *rectangles* recomputed but their
    /// PTYs left alone. Pushing the resize into a hidden pane would
    /// clear its screen, and a plain shell — unlike a TUI — never
    /// repaints after SIGWINCH, so the output the user had scrolled to
    /// (and that a caller-scoped `inspect_pane` could still read) would
    /// simply be gone. The real resize happens when that tab is next
    /// rendered, which is when someone is actually looking at it.
    pub fn on_terminal_resize(&mut self, cols: u16, rows: u16) {
        self.last_term_size = (cols, rows);
        for i in 0..self.workspaces.len() {
            if i != self.active_tab {
                self.recompute_workspace_rects(i);
            }
        }
        // The active workspace goes through `mark_layout_change` so the
        // repaint cooldown and selection invalidation stay tied to the
        // tab actually being painted — and it is on screen, so resizing
        // its PTYs is exactly right.
        self.mark_layout_change();
    }

    /// Get the active workspace.
    pub fn ws(&self) -> &Workspace {
        &self.workspaces[self.active_tab]
    }

    /// Get the active workspace mutably.
    pub fn ws_mut(&mut self) -> &mut Workspace {
        &mut self.workspaces[self.active_tab]
    }
}
