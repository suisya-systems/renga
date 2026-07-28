use super::*;

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
            pending_codex_peer_messages: HashMap::new(),
            codex_peer_notification: None,
            recent_peer_sends: HashMap::new(),
            clipboard: None,
            event_bus,
            ime_mode: crate::config::ImeMode::default(),
            lang: crate::i18n::Lang::default(),
            ime_freeze_panes_on_overlay: false,
            ime_overlay_catchup_ms: 0,
            ime_anchor_row_offset: crate::config::DEFAULT_IME_ANCHOR_ROW_OFFSET,
            last_overlay_repaint: None,
            min_pane_width: 20,
            min_pane_height: 5,
            image_picker: None,
            macos_tip_visible: false,
            macos_tip_shown_at: None,
            macos_tip_marker: None,
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
        // `apply_cli_overrides` already clamps this, but re-clamp here so
        // a caller that hand-builds a `Config` (tests, future embedders)
        // can't hand the renderer an offset that walks the caret off the
        // input line it is supposed to be anchoring near.
        self.ime_anchor_row_offset = cfg
            .ime
            .anchor_row_offset
            .min(crate::config::MAX_IME_ANCHOR_ROW_OFFSET);
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

    /// Emit a [`PaneStarted`] event for the given pane id. Pulls the
    /// current name/role from the active workspace so subscribers
    /// receive the metadata that was just attached.
    pub(crate) fn emit_pane_started(&self, pane_id: usize) {
        let ws = self.ws();
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

impl App {
    /// Recompute pane rectangles and apply sizes to every PTY in the
    /// active workspace. Returns `true` if any pane was actually
    /// resized (so callers can decide whether to enter the post-resize
    /// cooldown). Safe to call without a Frame — uses the cached
    /// `last_term_size`.
    pub fn relayout_panes(&mut self) -> bool {
        let (cols, rows) = self.last_term_size;
        if cols < 20 || rows < 5 {
            return false;
        }

        // Mirror the area math in ui::render / render_main_area,
        // including the fallback where tree / preview are hidden when
        // the terminal is too narrow. Keeping these in sync prevents
        // PTY size drift from the actually-painted pane size.
        const MIN_PANE_AREA_WIDTH: u16 = 20;
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
        // overlay is open or not.
        let main_h = rows.saturating_sub(tab_h + status_h);

        let mut has_tree = self.ws().file_tree_visible;
        let mut has_preview = self.ws().preview.is_active();
        let tree_w_nom = self.file_tree_width;
        let preview_w_nom = self.preview_width;

        let needed = MIN_PANE_AREA_WIDTH
            + if has_tree { tree_w_nom } else { 0 }
            + if has_preview { preview_w_nom } else { 0 };
        if cols < needed && has_preview {
            has_preview = false;
        }
        let needed = MIN_PANE_AREA_WIDTH + if has_tree { tree_w_nom } else { 0 };
        if cols < needed && has_tree {
            has_tree = false;
        }

        let tree_w = if has_tree { tree_w_nom } else { 0 };
        let preview_w = if has_preview { preview_w_nom } else { 0 };
        let pane_w = cols.saturating_sub(tree_w).saturating_sub(preview_w);

        // Mirror ui::render_main_area's chunk ordering so the cached
        // rects reflect actual on-screen positions (not just sizes).
        // The IPC `list` response and mouse hit-testing both read x/y
        // from last_pane_rects, so getting the origin right matters
        // here even between renders. Chunk order there is:
        //   [tree?] [preview(if swapped)?] [panes] [preview(if !swapped)?]
        let pane_x = tree_w + if self.layout_swapped { preview_w } else { 0 };
        let pane_area = Rect::new(pane_x, tab_h, pane_w, main_h);
        let rects = self.ws().layout.calculate_rects(pane_area);

        let mut any_changed = false;
        for (pane_id, rect) in &rects {
            if let Some(pane) = self.ws_mut().panes.get_mut(pane_id) {
                let inner_rows = rect.height.saturating_sub(2);
                let inner_cols = rect.width.saturating_sub(2);
                if pane.resize(inner_rows, inner_cols).unwrap_or(false) {
                    any_changed = true;
                }
            }
        }

        self.ws_mut().last_pane_rects = rects;
        any_changed
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

    /// Called from main.rs on crossterm Resize events so we can update
    /// the cached terminal size and propagate the resize into panes.
    pub fn on_terminal_resize(&mut self, cols: u16, rows: u16) {
        self.last_term_size = (cols, rows);
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
