use super::*;

impl App {
    // ─── Key handling ─────────────────────────────────────

    pub fn handle_key_event(&mut self, key: KeyEvent) -> Result<bool> {
        // First-launch macOS tip: dismiss on any key, but fall through
        // so the key still performs its normal action. The banner is a
        // transient hint, not a modal — the user shouldn't have to
        // press a key twice (once to dismiss, once to do what they
        // wanted). Persists the marker file here so the banner never
        // reappears on the next launch, including when the next key
        // is Ctrl+Q — otherwise a quit-while-banner-up would leave
        // the marker unwritten and the tip would return next launch.
        if self.macos_tip_visible {
            self.dismiss_macos_tip();
        }

        // Emergency escape hatch: Ctrl+Q must always quit renga, even
        // while the IME composition overlay is holding input. Checked
        // before overlay routing so the user can never get trapped in
        // a wedged composition mode.
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('q') {
            self.should_quit = true;
            return Ok(true);
        }

        // Ctrl+W close confirmation (Issue #285) — a true modal.
        //
        // Position is load-bearing: *after* the Ctrl+Q escape hatch
        // above so a pending prompt can never trap the user, and
        // *before* the overlay / rename / regular handlers so no
        // keystroke reaches the PTY while the prompt is up. `y`/`Y`
        // execute, `n`/`N`/`Esc` cancel, and every other key is
        // swallowed with the prompt left standing — a stray keypress
        // must neither close a pane nor leak a character into the
        // shell underneath.
        if self.close_confirm.is_some() {
            // Ctrl+Y / Alt+Y are not "yes". Only an unmodified (or
            // merely shifted) y/n counts as an answer. Allowlist rather
            // than denylist: under crossterm's enhanced keyboard
            // protocol META and HYPER are reported independently of
            // ALT / SUPER, and naming the rejected flags one by one
            // would silently accept whatever the next protocol adds.
            let plain = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') if plain => self.confirm_close_now(),
                KeyCode::Char('n') | KeyCode::Char('N') if plain => self.cancel_close_confirm(),
                KeyCode::Esc => self.cancel_close_confirm(),
                _ => {}
            }
            return Ok(true);
        }

        // IME composition overlay — route every relevant key into the
        // buffer until the user commits or cancels. Takes precedence
        // over rename and every other handler so composition never
        // leaks into the layout / PTY unintentionally.
        if self.overlay.is_some() {
            return crate::input::overlay::handle_overlay_key(self, key);
        }

        if self.codex_peer_notification_is_visible() {
            if matches!(key.code, KeyCode::Esc)
                || (key.modifiers == KeyModifiers::CONTROL
                    && matches!(key.code, KeyCode::Char('c')))
            {
                self.dismiss_codex_peer_notification();
                return Ok(true);
            }
            if crate::input::overlay::is_overlay_commit_key(key) {
                return self
                    .accept_codex_peer_notification()
                    .map_err(|e| anyhow::anyhow!(e.to_string()));
            }
            // Typing past the overlay is not an "ignore": park it so
            // the request is still nudged once focus leaves (#197).
            self.snooze_codex_peer_notification();
        }

        // Rename mode — swallow all input until Enter/Esc.
        if self.rename_input.is_some() {
            return Ok(self.handle_rename_key(key));
        }

        // Open the IME composition overlay. Primary hotkey is
        // `Ctrl+;`, with `Alt+;` and `Alt+I` as fallbacks for
        // terminals that refuse to pass `Ctrl+;` through to
        // stdin. ASCII has no encoding for Ctrl+punctuation and
        // many terminals (Windows Terminal with WSL, VS Code
        // terminal on Linux, plain TTYs, some tmux configs) drop
        // the Ctrl modifier and deliver a bare `;` to the
        // application. The Alt-based fallbacks arrive as an
        // ESC-prefixed sequence that every tier-1 terminal
        // forwards reliably, so the overlay is always reachable.
        //
        // Originally gated to `is_claude_running()` panes, but
        // that proved flaky — Claude briefly retitles the pane
        // while running tools, so the detection would flicker
        // and the hotkey would "mysteriously stop working" mid-
        // session. The overlay opens unconditionally on any
        // focused pane; users who don't need IME just don't
        // press the hotkey.
        let is_semi = matches!(key.code, KeyCode::Char(';'));
        let is_alt_i = key.modifiers == KeyModifiers::ALT
            && matches!(key.code, KeyCode::Char('i') | KeyCode::Char('I'));
        let is_open_hotkey = ((key.modifiers == KeyModifiers::CONTROL
            || key.modifiers == KeyModifiers::ALT)
            && is_semi)
            || is_alt_i;
        if is_open_hotkey {
            match self.ime_mode {
                crate::config::ImeMode::Off => {
                    // User opted out of the overlay. Don't leak a bare
                    // ';' to the PTY either: terminals encode Ctrl+;
                    // inconsistently, and falling through to
                    // `key_event_to_bytes` strips the Ctrl modifier and
                    // injects a stray semicolon into the shell. Silent
                    // swallow matches the "off" intent — the hotkey
                    // simply does nothing.
                    return Ok(true);
                }
                crate::config::ImeMode::Hotkey => {
                    // Fall through to open the overlay deliberately.
                }
            }
            let focused_id = self.ws().focused_pane_id;
            let pane_focused = matches!(self.ws().focus_target, FocusTarget::Pane)
                && self.ws().panes.contains_key(&focused_id);
            if pane_focused {
                if let Some(saved) = self.take_overlay_draft(focused_id) {
                    self.overlay = Some(saved);
                    self.mark_layout_change();
                    return Ok(true);
                }

                // Visible-input bootstrap is Claude-specific. Codex
                // panes use a different composer layout, and trying to
                // "steal" their draft into the IME overlay corrupts the
                // handoff instead of preserving it.
                let snapshot = (!self
                    .pane_expects_codex_peer_delivery(self.active_tab, focused_id))
                .then(|| {
                    self.ws()
                        .panes
                        .get(&focused_id)
                        .and_then(crate::input::overlay::snapshot_visible_input)
                })
                .flatten();

                if snapshot.as_ref().is_some_and(|snapshot| {
                    crate::input::overlay::visible_input_contains_claude_paste_placeholder(
                        &snapshot.buffer,
                    )
                }) {
                    self.overlay = Some(OverlayState::new(focused_id));
                    self.mark_layout_change();
                    return Ok(true);
                }

                let mut overlay = OverlayState::new(focused_id);
                if let Some(snapshot) = snapshot.as_ref() {
                    overlay.buffer = snapshot.buffer.clone();
                    overlay.cursor = snapshot.cursor.min(overlay.buffer.chars().count());
                }
                self.overlay = Some(overlay);

                if let Some(snapshot) = snapshot.as_ref() {
                    let clear = crate::input::overlay::clear_visible_input_bytes(snapshot);
                    if !clear.is_empty() {
                        if let Some(pane) = self.ws_mut().panes.get_mut(&focused_id) {
                            let _ = pane.write_input(&clear);
                        }
                    }
                }
                self.mark_layout_change();
                return Ok(true);
            }
            // Fall through when focus is on the file tree / preview;
            // Ctrl+; in those contexts has no meaning and shouldn't
            // open an overlay attached to a hidden target.
        }

        // Ctrl+Q — quit
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('q') {
            self.should_quit = true;
            return Ok(true);
        }

        // Alt+R — rename active tab (session only)
        if key.modifiers == KeyModifiers::ALT
            && matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R'))
        {
            self.rename_input = Some(String::new());
            if !self.status_bar_visible {
                self.mark_layout_change();
            }
            return Ok(true);
        }

        // Ctrl+C — if text is selected, copy to clipboard instead of sending SIGINT
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
            if let Some(ref sel) = self.selection.clone() {
                let (sr, sc, er, ec) = sel.normalized();
                if sr != er || sc != ec {
                    let text = match sel.target {
                        SelectionTarget::Pane(pane_id) => self
                            .ws()
                            .panes
                            .get(&pane_id)
                            .map(|p| extract_selected_text(p, sr, sc, er, ec))
                            .unwrap_or_default(),
                        SelectionTarget::Preview => {
                            extract_preview_selected_text(&self.ws().preview, sr, sc, er, ec)
                        }
                    };
                    if !text.is_empty() {
                        self.copy_to_clipboard(&text);
                    }
                    self.selection = None;
                    return Ok(true);
                }
            }
            // No selection — fall through to forward Ctrl+C to PTY
        }

        // Ctrl+T / Alt+T — new tab (Alt+T groups with Alt-based tab nav)
        if (key.modifiers == KeyModifiers::CONTROL || key.modifiers == KeyModifiers::ALT)
            && matches!(key.code, KeyCode::Char('t') | KeyCode::Char('T'))
        {
            match self.create_tab_with_cwd(None, true) {
                Ok((_, new_id)) => self.emit_pane_started(new_id),
                // The MAX_TABS refusal must not bubble into the event
                // loop — `run_event_loop`'s `?` would tear down the
                // whole multiplexer over a full tab strip. Consume the
                // keypress instead, matching how a split at MAX_PANES
                // and the tab-bar `+` click already no-op at their
                // caps. Genuine failures (PTY spawn I/O) keep the
                // pre-#290 propagation.
                Err(e) if e.code == Some(ipc::err_code::TAB_LIMIT_REACHED) => {}
                Err(e) => return Err(anyhow::anyhow!(e.to_string())),
            }
            return Ok(true);
        }

        // Alt+Right — next tab
        if key.modifiers == KeyModifiers::ALT && key.code == KeyCode::Right {
            if !self.workspaces.is_empty() {
                self.switch_tab((self.active_tab + 1) % self.workspaces.len());
            }
            return Ok(true);
        }

        // Alt+Left — previous tab
        if key.modifiers == KeyModifiers::ALT && key.code == KeyCode::Left {
            if !self.workspaces.is_empty() {
                let target = if self.active_tab == 0 {
                    self.workspaces.len() - 1
                } else {
                    self.active_tab - 1
                };
                self.switch_tab(target);
            }
            return Ok(true);
        }

        // Alt+S — toggle status bar
        if key.modifiers == KeyModifiers::ALT
            && matches!(key.code, KeyCode::Char('s') | KeyCode::Char('S'))
        {
            self.status_bar_visible = !self.status_bar_visible;
            self.mark_layout_change();
            return Ok(true);
        }

        // Alt+P — insert the peer-enabled claude launch command into
        // the focused pane (trailing space, no Enter). The user reviews,
        // optionally edits, then presses Enter to actually run — a
        // conscious action, which is why we deliberately don't gate
        // this on "is renga-peers installed": pressing Alt+P already
        // means the user wants peer mode, and a missing MCP entry will
        // surface itself when Claude starts.
        //
        // Refuse when the pane is in alternate-screen mode (a TUI —
        // Claude Code itself, vim, less, lazygit — has captured the
        // terminal). Writing the command bytes there would land as
        // keystrokes inside that TUI instead of at a shell prompt,
        // which could accidentally send a prompt to a running Claude.
        if key.modifiers == KeyModifiers::ALT
            && matches!(key.code, KeyCode::Char('p') | KeyCode::Char('P'))
        {
            let ws = self.ws_mut();
            let focused_id = ws.focused_pane_id;
            if let Some(pane) = ws.panes.get_mut(&focused_id) {
                if pane.shell_accepts_command_injection() {
                    let cmd = format!("{CLAUDE_PEER_LAUNCH_CMD} ");
                    let _ = pane.write_input(cmd.as_bytes());
                    self.dirty = true;
                }
                // else: silently no-op; the pane is in an alt-screen
                // TUI. Users can switch to a shell pane and retry.
            }
            return Ok(true);
        }

        // Alt+1 .. Alt+9 — jump to tab N
        if key.modifiers == KeyModifiers::ALT {
            if let KeyCode::Char(c) = key.code {
                if let Some(digit) = c.to_digit(10) {
                    if digit >= 1 && (digit as usize) <= self.workspaces.len() {
                        self.switch_tab((digit as usize) - 1);
                        return Ok(true);
                    }
                }
            }
        }

        // Ctrl+Right — next pane
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Right {
            self.focus_next_pane();
            return Ok(true);
        }

        // Ctrl+Left — previous pane
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Left {
            self.focus_prev_pane();
            return Ok(true);
        }

        // Ctrl+B — toggle the org sidebar. Kept off Ctrl+F so the file
        // tree binding users already have in their fingers is untouched
        // (the two panels coexist by default).
        //
        // Checked *before* the per-panel dispatch below so it reaches
        // the sidebar from any focus, the way Ctrl+F already does from
        // the file tree. Gated on `org_sidebar_enabled` rather than
        // swallowed unconditionally: `[ui] org_sidebar = "off"` is the
        // documented escape hatch for users who need Ctrl+B in their
        // shell / tmux / readline, so with the feature off the key has
        // to fall through to the PTY untouched.
        if key.modifiers == KeyModifiers::CONTROL
            && key.code == KeyCode::Char('b')
            && self.org_sidebar_enabled()
        {
            self.toggle_org_sidebar();
            return Ok(true);
        }

        // Org sidebar mode.
        //
        // This dispatch chain is `if` / `==`, not an exhaustive `match`,
        // so the compiler will *not* flag a missing arm: leaving this
        // branch out would let sidebar-focused keys fall through into
        // the pane handlers below. Also gated on `org_sidebar_active`
        // so focus stranded on a panel that has since been toggled off
        // does not swallow input.
        if self.ws().focus_target == FocusTarget::OrgSidebar && self.org_sidebar_painted() {
            return self.handle_org_sidebar_key(key);
        }

        // Preview mode
        if self.ws().focus_target == FocusTarget::Preview && self.preview_painted() {
            return self.handle_preview_key(key);
        }

        // File tree mode. Gated on `file_tree_painted` for the same
        // reason as the sidebar above: a panel can hold focus while
        // being nowhere on screen — `replace` mode takes the tree's
        // slot, and the degrade ladder drops it on a narrow terminal —
        // and routing keys there swallows them (turning a bare `c` /
        // `v` into a pane split).
        if self.ws().focus_target == FocusTarget::FileTree && self.file_tree_painted() {
            if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('f') {
                self.toggle_file_tree();
                return Ok(true);
            }
            return self.handle_file_tree_key(key);
        }

        // Ctrl+F — toggle file tree
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('f') {
            self.toggle_file_tree();
            return Ok(true);
        }

        // Ctrl+P — swap preview and terminal positions
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('p') {
            self.layout_swapped = !self.layout_swapped;
            return Ok(true);
        }

        let multi_pane = self.ws().layout.pane_count() > 1;
        let multi_tab = self.workspaces.len() > 1;

        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
                if let Some(new_id) = self.split_focused_pane(SplitDirection::Vertical, None)? {
                    self.emit_pane_started(new_id);
                }
                Ok(true)
            }
            (KeyModifiers::CONTROL, KeyCode::Char('e')) => {
                if let Some(new_id) = self.split_focused_pane(SplitDirection::Horizontal, None)? {
                    self.emit_pane_started(new_id);
                }
                Ok(true)
            }
            (KeyModifiers::CONTROL, KeyCode::Char('w')) => {
                if self.ws().focus_target == FocusTarget::Preview {
                    // Close preview and return to pane
                    self.ws_mut().preview.close();
                    self.ws_mut().focus_target = FocusTarget::Pane;
                    Ok(true)
                } else if multi_pane {
                    // Ask first — the actual close happens in
                    // `confirm_close_now` once the user presses `y`.
                    // Closing the preview above stays unconfirmed: it
                    // destroys no process and reopens with one click.
                    self.request_close_focused_pane();
                    Ok(true)
                } else if multi_tab {
                    self.request_close_focused_tab();
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            _ => Ok(false),
        }
    }

    // ─── PTY forwarding ───────────────────────────────────

    /// Route a terminal-level paste payload (bracketed-paste from the
    /// host terminal — typically Ctrl+V on WSL2 / Windows Terminal /
    /// WezTerm / iTerm2) to the right destination. When the IME
    /// composition overlay is open, the paste belongs to the overlay
    /// buffer; otherwise it forwards to the focused pane's PTY via
    /// `forward_paste_to_pty`. Centralizing the routing here keeps
    /// `main.rs` from having to reach into overlay internals.
    pub fn handle_paste(&mut self, text: &str) -> Result<bool> {
        // Close confirmation is modal for pastes too. Drop the whole
        // payload: a pasted "y" must not read as consent, and letting
        // the rest through would type into the pane the user is being
        // asked about. Reported as "handled" so the caller skips the
        // post-paste render cooldown.
        if self.close_confirm.is_some() {
            return Ok(true);
        }
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.insert_str(text);
            self.dirty = true;
            return Ok(true);
        }
        self.forward_paste_to_pty(text)?;
        Ok(false)
    }

    /// Forward pasted text to PTY, wrapping in bracketed paste only if
    /// the PTY application has enabled the mode (e.g. Claude Code, modern
    /// readline). Sending bracketed paste to a shell that hasn't opted in
    /// causes the escape sequences to appear as literal text (issue #2).
    pub fn forward_paste_to_pty(&mut self, text: &str) -> Result<()> {
        let focused_id = self.ws().focused_pane_id;
        if let Some(pane) = self.ws_mut().panes.get_mut(&focused_id) {
            pane.scroll_reset();
            pane.clear_codex_transcript_overlay_hint();
            if pane.is_bracketed_paste_enabled() {
                let mut data = Vec::with_capacity(text.len() + 12);
                data.extend_from_slice(b"\x1b[200~");
                data.extend_from_slice(text.as_bytes());
                data.extend_from_slice(b"\x1b[201~");
                pane.write_input(&data)?;
            } else {
                pane.write_input(text.as_bytes())?;
            }
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn forward_key_to_pty(&mut self, key: KeyEvent) -> Result<()> {
        let focused_id = self.ws().focused_pane_id;
        if let Some(pane) = self.ws_mut().panes.get_mut(&focused_id) {
            pane.scroll_reset();
            pane.clear_codex_transcript_overlay_hint();
            if let Some(bytes) = key_event_to_bytes(&key) {
                pane.write_input(&bytes)?;
            }
        }
        Ok(())
    }
}

/// Extract text from a pane's vt100 screen within a selection range.
pub(crate) fn extract_selected_text(pane: &Pane, sr: u32, sc: u32, er: u32, ec: u32) -> String {
    let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
    let screen = parser.screen();
    let mut lines = Vec::new();

    for row in sr..=er {
        let mut line = String::new();
        let col_start = if row == sr { sc } else { 0 };
        let col_end = if row == er { ec } else { 999 };

        for col in col_start..=col_end {
            if let Some(cell) = screen.cell(row as u16, col as u16) {
                let contents = cell.contents();
                if contents.is_empty() {
                    line.push(' ');
                } else {
                    line.push_str(contents);
                }
            }
        }
        lines.push(line.trim_end().to_string());
    }

    // Remove trailing empty lines
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }

    lines.join("\n")
}

/// Extract text from the file preview within a selection range.
/// `sr`/`er` are absolute line indices; `sc`/`ec` are char offsets
/// within the line (selection is stored in source coordinates so it
/// survives scrolling). Trailing empty lines are stripped.
pub(crate) fn extract_preview_selected_text(
    preview: &crate::preview::Preview,
    sr: u32,
    sc: u32,
    er: u32,
    ec: u32,
) -> String {
    let lines = &preview.lines;
    let mut out: Vec<String> = Vec::new();

    for abs_row in sr..=er {
        let idx = abs_row as usize;
        if idx >= lines.len() {
            break;
        }
        let line = &lines[idx];
        let chars: Vec<char> = line.chars().collect();

        let col_start = if abs_row == sr { sc as usize } else { 0 };
        let col_end_inclusive = if abs_row == er {
            ec as usize
        } else {
            chars.len().saturating_sub(1)
        };

        let start = col_start.min(chars.len());
        let end = (col_end_inclusive.saturating_add(1)).min(chars.len());
        let slice: String = if start < end {
            chars[start..end].iter().collect()
        } else {
            String::new()
        };
        out.push(slice);
    }

    // Strip trailing empty lines only.
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }

    out.join("\n")
}

/// Public wrapper for key_event_to_bytes (used by main.rs paste detection).
pub(crate) fn key_event_to_bytes_pub(key: &KeyEvent) -> Option<Vec<u8>> {
    key_event_to_bytes(key)
}

/// Convert a crossterm KeyEvent into bytes suitable for PTY input.
fn key_event_to_bytes(key: &KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let ctrl_byte = (c.to_ascii_lowercase() as u8)
                    .wrapping_sub(b'a')
                    .wrapping_add(1);
                if ctrl_byte <= 26 {
                    if alt {
                        // Alt+Ctrl+Char → ESC + ctrl byte
                        Some(vec![0x1b, ctrl_byte])
                    } else {
                        Some(vec![ctrl_byte])
                    }
                } else {
                    Some(c.to_string().into_bytes())
                }
            } else if alt {
                // Alt+Char → ESC + char (standard xterm behavior)
                let mut bytes = vec![0x1b];
                bytes.extend_from_slice(c.to_string().as_bytes());
                Some(bytes)
            } else {
                Some(c.to_string().into_bytes())
            }
        }
        // Alt+Enter → send newline (\n) for multi-line input in Claude Code
        KeyCode::Enter if alt => Some(vec![b'\n']),
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Insert => Some(b"\x1b[2~".to_vec()),
        KeyCode::F(n) => {
            let seq = match n {
                1 => "\x1bOP",
                2 => "\x1bOQ",
                3 => "\x1bOR",
                4 => "\x1bOS",
                5 => "\x1b[15~",
                6 => "\x1b[17~",
                7 => "\x1b[18~",
                8 => "\x1b[19~",
                9 => "\x1b[20~",
                10 => "\x1b[21~",
                11 => "\x1b[23~",
                12 => "\x1b[24~",
                _ => return None,
            };
            Some(seq.as_bytes().to_vec())
        }
        _ => None,
    }
}
