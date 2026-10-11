//! IME composition overlay state and its modal key handler.
//!
//! Extracted from `src/app.rs` as part of Issue #66. The
//! module owns [`OverlayState`] and [`handle_overlay_key`]; `App` still
//! owns the `Option<OverlayState>` field and the open/close call sites.
//!
//! The overlay is a centered multi-line composition box. Enter inserts
//! a newline; Alt+Enter (the canonical commit on most terminals) or
//! Ctrl+Enter (Windows Terminal / wezterm / VS Code) commit the
//! buffer. The host terminal's IME candidate window anchors to the
//! caret inside the box.
//!
//! On WSL2 / Windows Terminal the host emulator binds Alt+Enter to
//! *Toggle Fullscreen* and consumes the chord before renga sees it
//! (Issue #226). The same hosts deliver the user's Ctrl+Enter as a
//! bare LF byte (0x0A) instead of a distinct CSI sequence, which
//! crossterm reports as Ctrl+J — so Ctrl+J is also accepted as a
//! commit key, giving a working submit path on those hosts without
//! requiring the user to enable extended-key reporting.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::App;

/// Multi-line IME composition buffer. The buffer stores `\n` as a
/// regular character; cursor is a char offset into the buffer, so
/// Japanese/Chinese multibyte text edits correctly. See
/// [`handle_overlay_key`] for the key-routing contract.
#[derive(Debug, Clone)]
pub struct OverlayState {
    /// Pane id the overlay will commit its buffer to. Stored at open
    /// time so focus changes during composition don't reroute the
    /// text; the overlay always commits to the pane that originated
    /// it.
    pub target_pane: usize,
    /// Composed characters so far, indexed by character (not byte),
    /// so inserting multibyte text and editing via arrow keys
    /// behaves intuitively for Japanese/Chinese composition.
    pub buffer: String,
    /// Cursor position in `buffer`, measured in characters. Valid
    /// range: `0..=buffer.chars().count()`.
    pub cursor: usize,
}

impl OverlayState {
    pub fn new(target_pane: usize) -> Self {
        Self {
            target_pane,
            buffer: String::new(),
            cursor: 0,
        }
    }

    /// Insert `ch` at the overlay cursor and advance. `'\n'` is a
    /// regular character — call `insert_char('\n')` to add a line
    /// break.
    pub fn insert_char(&mut self, ch: char) {
        let byte_idx = self
            .buffer
            .char_indices()
            .nth(self.cursor)
            .map(|(i, _)| i)
            .unwrap_or(self.buffer.len());
        self.buffer.insert(byte_idx, ch);
        self.cursor += 1;
    }

    /// Insert `s` at the overlay cursor and advance the cursor by the
    /// number of inserted chars. Used for routing terminal-level
    /// bracketed-paste payloads (Ctrl+V on WSL2 / Windows Terminal /
    /// WezTerm) into the composition buffer instead of the underlying
    /// pane. Honors the same 4096-char cap that `handle_overlay_key`
    /// applies to typed input, by truncating the tail rather than
    /// rejecting the whole paste — a dropped paste is harder to
    /// recover from than a clipped one.
    ///
    /// Line endings are normalized to `\n` so a Windows-clipboard
    /// paste (the WSL2 user's primary path) doesn't leave bare `\r`
    /// bytes in the buffer. The overlay's wrap/render path treats
    /// only `\n` as a hard newline, and `\r` is a width-zero control
    /// char — without normalization the host clipboard's CRLF would
    /// desync the rendered cursor from the buffer cursor.
    pub fn insert_str(&mut self, s: &str) {
        const MAX_BUFFER_CHARS: usize = 4096;

        if s.is_empty() {
            return;
        }
        let current_len = self.buffer.chars().count();
        if current_len >= MAX_BUFFER_CHARS {
            return;
        }
        let remaining = MAX_BUFFER_CHARS - current_len;
        // Stream the normalized chars and stop once we hit the cap so a
        // megabyte-class paste doesn't allocate megabytes upfront just
        // to throw most of it away. `take(remaining)` short-circuits
        // the `\r`/`\n` state machine the moment we've collected
        // enough.
        let to_insert: String = normalize_paste_line_endings(s).take(remaining).collect();
        if to_insert.is_empty() {
            return;
        }
        let inserted_chars = to_insert.chars().count();

        let byte_idx = self
            .buffer
            .char_indices()
            .nth(self.cursor)
            .map(|(i, _)| i)
            .unwrap_or(self.buffer.len());
        self.buffer.insert_str(byte_idx, &to_insert);
        self.cursor += inserted_chars;
    }

    /// Clear the entire composition buffer and reset the cursor.
    pub fn clear(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
    }

    /// Backspace: remove the char left of the cursor.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.cursor - 1;
        let (start, ch) = match self.buffer.char_indices().nth(prev) {
            Some(pair) => pair,
            None => return,
        };
        let end = start + ch.len_utf8();
        self.buffer.replace_range(start..end, "");
        self.cursor = prev;
    }

    pub fn cursor_left(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    pub fn cursor_right(&mut self) {
        let len = self.buffer.chars().count();
        if self.cursor < len {
            self.cursor += 1;
        }
    }

    /// Home in a multi-line buffer moves to the start of the current
    /// line, not the start of the whole buffer.
    pub fn cursor_home(&mut self) {
        let (_line, col) = self.line_col();
        self.cursor -= col;
    }

    /// End moves to the end of the current line (just before the
    /// next `\n`, or buffer end).
    pub fn cursor_end(&mut self) {
        // Walk the char iterator from the current cursor — avoid
        // materializing a Vec<char> for the whole buffer just to
        // scan a single line.
        let additional = self
            .buffer
            .chars()
            .skip(self.cursor)
            .take_while(|c| *c != '\n')
            .count();
        self.cursor += additional;
    }

    /// Absolute start of the buffer — multi-line equivalent of
    /// `Ctrl+Home` in a traditional editor.
    pub fn cursor_buffer_start(&mut self) {
        self.cursor = 0;
    }

    /// Absolute end of the buffer — multi-line equivalent of
    /// `Ctrl+End`.
    pub fn cursor_buffer_end(&mut self) {
        self.cursor = self.buffer.chars().count();
    }

    /// Arrow-up: move to the same column on the previous line. If
    /// already on the first line, collapse to column 0 (home of
    /// buffer) so a stuck user can always escape upward.
    pub fn cursor_up(&mut self) {
        let (line, col) = self.line_col();
        if line == 0 {
            self.cursor = 0;
            return;
        }
        self.cursor = self.line_col_to_cursor(line - 1, col);
    }

    /// Arrow-down: move to the same column on the next line. On the
    /// last line, jump to buffer end so Down is always meaningful.
    pub fn cursor_down(&mut self) {
        let (line, col) = self.line_col();
        let total_lines = self.total_lines();
        if line + 1 >= total_lines {
            self.cursor = self.buffer.chars().count();
            return;
        }
        self.cursor = self.line_col_to_cursor(line + 1, col);
    }

    /// Current cursor as (0-based line, 0-based column). Column is
    /// the number of chars on the current line before the cursor.
    pub fn line_col(&self) -> (usize, usize) {
        let mut line = 0usize;
        let mut col = 0usize;
        for (i, ch) in self.buffer.chars().enumerate() {
            if i >= self.cursor {
                break;
            }
            if ch == '\n' {
                line += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        (line, col)
    }

    /// Convert a (line, col) target back to a char offset, clamping
    /// col to the length of the requested line (so Up/Down over a
    /// shorter line lands at the line's end, matching editor norms).
    fn line_col_to_cursor(&self, target_line: usize, target_col: usize) -> usize {
        let mut line = 0usize;
        let mut col = 0usize;
        let mut last_in_target_line: Option<usize> = None;
        for (i, ch) in self.buffer.chars().enumerate() {
            if line == target_line {
                if col == target_col {
                    return i;
                }
                last_in_target_line = Some(i);
            }
            if ch == '\n' {
                if line == target_line {
                    // Reached end of target line before hitting the
                    // requested column — clamp to the line-end
                    // position (just before the newline).
                    return i;
                }
                line += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        // Ran off the end of the buffer. If we were ever on the
        // target line, clamp to buffer end; otherwise the target
        // line didn't exist (shouldn't happen — callers check
        // total_lines).
        if line == target_line || last_in_target_line.is_some() {
            self.buffer.chars().count()
        } else {
            self.cursor
        }
    }

    /// Total number of logical lines = `\n` count + 1. An empty
    /// buffer has 1 line.
    pub fn total_lines(&self) -> usize {
        self.buffer.chars().filter(|c| *c == '\n').count() + 1
    }
}

/// Visible Claude input reconstructed from the pane's current vt100
/// screen state. The buffer is best-effort: it preserves the
/// currently visible prompt contents and caret position so the IME
/// overlay can take over an in-progress draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VisibleInputSnapshot {
    pub buffer: String,
    pub cursor: usize,
}

const CLAUDE_PROMPT_GLYPHS: &[&str] = &[
    ">", "\u{276F}", // ❯
    "\u{203A}", // ›
    "\u{27E9}", // ⟩
    "\u{3009}", // 〉
    "\u{276D}", // ❭
    "\u{2771}", // ❱
];
const CLAUDE_PROMPT_SCAN_COLS: u16 = 8;
const CLAUDE_INPUT_WALK_MAX: u16 = 20;

/// Collapse `\r\n` and bare `\r` into `\n` for paste payloads. The
/// overlay buffer treats `\n` as the only line break (see
/// `wrap_overlay_buffer` in `ui.rs`), so leaving carriage returns
/// in-band would render as zero-width control glyphs and desync the
/// rendered cursor from the buffer cursor on Windows-clipboard
/// pastes via WSL2. Returns an iterator so the caller can `take()`
/// up to the buffer cap without allocating the full normalized
/// string for a paste that will be truncated anyway.
fn normalize_paste_line_endings(s: &str) -> impl Iterator<Item = char> + '_ {
    let mut prev_cr = false;
    s.chars().filter_map(move |ch| match ch {
        '\r' => {
            prev_cr = true;
            Some('\n')
        }
        '\n' if prev_cr => {
            prev_cr = false;
            None
        }
        _ => {
            prev_cr = false;
            Some(ch)
        }
    })
}

pub(crate) fn snapshot_visible_input(pane: &crate::pane::Pane) -> Option<VisibleInputSnapshot> {
    let parser = pane.parser.lock().ok()?;
    let screen = parser.screen();
    snapshot_visible_input_from_screen(screen)
}

pub(crate) fn clear_visible_input_bytes(snapshot: &VisibleInputSnapshot) -> Vec<u8> {
    let total = snapshot.buffer.chars().count();
    let mut bytes = Vec::with_capacity(total.saturating_mul(4));

    for _ in snapshot.cursor..total {
        bytes.extend_from_slice(b"\x1b[C");
    }
    bytes.extend(std::iter::repeat_n(0x7f, total));

    bytes
}

pub(crate) fn visible_input_contains_claude_paste_placeholder(visible: &str) -> bool {
    visible.contains("[Pasted text #")
}

fn snapshot_visible_input_from_screen(screen: &vt100::Screen) -> Option<VisibleInputSnapshot> {
    let prompt_row = find_prompt_row(screen)?;
    let prompt_col = find_prompt_glyph_col(screen, prompt_row)?;
    let last_row = resolve_input_row_last(screen, prompt_row);
    let (caret_row, caret_col) = resolve_claude_caret(screen)?;
    if caret_row < prompt_row || caret_row > last_row {
        return None;
    }

    let content_col = prompt_content_start_col(screen, prompt_row, prompt_col);
    let mut buffer = String::new();
    let mut cursor = 0usize;

    for row in prompt_row..=last_row {
        let row_text = extract_visible_row(screen, row, content_col);
        if row < caret_row {
            cursor += row_text.chars().count();
        } else if row == caret_row {
            cursor += count_chars_before_col(screen, row, content_col, caret_col);
        }
        buffer.push_str(&row_text);
    }

    Some(VisibleInputSnapshot { buffer, cursor })
}

fn cell_is_prompt_glyph(screen: &vt100::Screen, row: u16, col: u16) -> bool {
    screen
        .cell(row, col)
        .is_some_and(|c| CLAUDE_PROMPT_GLYPHS.iter().any(|g| *g == c.contents()))
}

fn find_prompt_glyph_col(screen: &vt100::Screen, row: u16) -> Option<u16> {
    let cols = screen.size().1.min(CLAUDE_PROMPT_SCAN_COLS);
    (0..cols).find(|&col| cell_is_prompt_glyph(screen, row, col))
}

fn row_has_non_blank(screen: &vt100::Screen, row: u16) -> bool {
    let cols = screen.size().1;
    for col in 0..cols {
        if let Some(c) = screen.cell(row, col) {
            let s = c.contents();
            if !s.is_empty() && s != " " {
                return true;
            }
        }
    }
    false
}

fn row_starts_with_prompt(screen: &vt100::Screen, row: u16) -> bool {
    find_prompt_glyph_col(screen, row).is_some()
}

fn row_col0_is_blank(screen: &vt100::Screen, row: u16) -> bool {
    screen
        .cell(row, 0)
        .map(|c| {
            let s = c.contents();
            s.is_empty() || s == " "
        })
        .unwrap_or(true)
}

fn is_continuation_candidate(screen: &vt100::Screen, row: u16) -> bool {
    row_col0_is_blank(screen, row) && row_has_non_blank(screen, row)
}

fn find_prompt_row(screen: &vt100::Screen) -> Option<u16> {
    let screen_rows = screen.size().0;
    (0..screen_rows)
        .rev()
        .find(|&row| row_starts_with_prompt(screen, row))
}

fn resolve_input_row_last(screen: &vt100::Screen, prompt_row: u16) -> u16 {
    let screen_rows = screen.size().0;
    let mut last = prompt_row;
    let mut blank_streak = 0u16;
    let max_row = prompt_row
        .saturating_add(CLAUDE_INPUT_WALK_MAX)
        .min(screen_rows.saturating_sub(1));
    let mut r = prompt_row.saturating_add(1);
    while r <= max_row {
        if row_starts_with_prompt(screen, r) {
            break;
        }
        if row_has_non_blank(screen, r) {
            if is_continuation_candidate(screen, r) {
                last = r;
                blank_streak = 0;
            } else {
                break;
            }
        } else {
            blank_streak += 1;
            if blank_streak >= 2 {
                break;
            }
        }
        r = r.saturating_add(1);
    }
    last
}

fn pick_caret_col_on_row(screen: &vt100::Screen, row: u16) -> u16 {
    let cols = screen.size().1;
    for col in (0..cols).rev() {
        if screen.cell(row, col).is_some_and(|c| c.inverse()) {
            return col;
        }
    }
    let mut last_nonblank: Option<u16> = None;
    for col in (0..cols).rev() {
        if let Some(c) = screen.cell(row, col) {
            let s = c.contents();
            if !s.is_empty() && s != " " {
                last_nonblank = Some(col);
                break;
            }
        }
    }
    last_nonblank
        .map(|c| c.saturating_add(1).min(cols.saturating_sub(1)))
        .unwrap_or(2)
}

fn resolve_claude_caret(screen: &vt100::Screen) -> Option<(u16, u16)> {
    let prompt_row = find_prompt_row(screen)?;
    let last = resolve_input_row_last(screen, prompt_row);
    let cols = screen.size().1;
    for row in (prompt_row..=last).rev() {
        for col in (0..cols).rev() {
            if screen.cell(row, col).is_some_and(|c| c.inverse()) {
                return Some((row, col));
            }
        }
    }
    // No inverse caret cell in the input block — modern Claude Code
    // (v2.1.x+) parks the visible hardware cursor on the edit cell
    // instead of painting an inverse cell. Trust it while it's inside
    // the block so the overlay bootstrap captures the real mid-line
    // cursor instead of snapping to end-of-input. Mirrors the same
    // tier in `ui.rs::resolve_claude_caret` — keep the two in sync.
    //
    // Unlike the ui.rs copy, do NOT clamp a pending-autowrap column
    // (vt100 reports col == cols after typing into the last cell):
    // `snapshot_visible_input_from_screen` consumes this column as the
    // EXCLUSIVE bound of `count_chars_before_col`, so the raw value is
    // the correct logical position — clamping would drop the caret one
    // character left of end-of-input. The drawing path clamps because
    // its contract is an on-screen cell; this path wants a text offset.
    if !screen.hide_cursor() {
        let (cur_row, cur_col) = screen.cursor_position();
        if (prompt_row..=last).contains(&cur_row) {
            return Some((cur_row, cur_col));
        }
    }
    Some((last, pick_caret_col_on_row(screen, last)))
}

fn prompt_content_start_col(screen: &vt100::Screen, prompt_row: u16, prompt_col: u16) -> u16 {
    let mut start = prompt_col.saturating_add(1);
    if screen
        .cell(prompt_row, start)
        .is_some_and(|c| c.contents() == " ")
    {
        start = start.saturating_add(1);
    }
    start
}

fn row_last_content_col(screen: &vt100::Screen, row: u16) -> Option<u16> {
    let cols = screen.size().1;
    (0..cols).rev().find(|&col| {
        screen.cell(row, col).is_some_and(|c| {
            let s = c.contents();
            !s.is_empty() && s != " "
        })
    })
}

fn extract_visible_row(screen: &vt100::Screen, row: u16, start_col: u16) -> String {
    let Some(last_col) = row_last_content_col(screen, row) else {
        return String::new();
    };
    if last_col < start_col {
        return String::new();
    }

    let mut out = String::new();
    for col in start_col..=last_col {
        if let Some(cell) = screen.cell(row, col) {
            let contents = cell.contents();
            if !contents.is_empty() {
                out.push_str(contents);
            }
        }
    }
    out
}

fn count_chars_before_col(
    screen: &vt100::Screen,
    row: u16,
    start_col: u16,
    caret_col: u16,
) -> usize {
    if caret_col <= start_col {
        return 0;
    }

    let mut count = 0usize;
    for col in start_col..caret_col {
        if let Some(cell) = screen.cell(row, col) {
            let contents = cell.contents();
            if !contents.is_empty() {
                count += contents.chars().count();
            }
        }
    }
    count
}

/// Modal key handling while the IME composition overlay is open.
///
/// Commits with **Alt+Enter** (the canonical commit on most
/// terminals, including macOS Option+Return) or **Ctrl+Enter**
/// (Windows Terminal / wezterm / VS Code / most Linux terminals).
/// On WSL2 / Windows Terminal where the host swallows Alt+Enter
/// for fullscreen and reports Ctrl+Enter as the LF byte (0x0A →
/// Ctrl+J in crossterm), **Ctrl+J** is treated as the same commit
/// key — see Issue #226.
/// Bare `Enter` inserts a newline into the composition buffer.
/// Cancels with Esc or Ctrl+C. Arrow keys navigate, Backspace
/// deletes, other printable characters are inserted; other Ctrl/Alt
/// chords are swallowed so renga's layout shortcuts can't leak
/// mid-composition. On commit, forwards the buffer to the original
/// target pane via the existing bracketed-paste path.
pub(crate) fn handle_overlay_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    let overlay = match app.overlay.as_mut() {
        Some(o) => o,
        None => return Ok(false),
    };

    // Cancel (Esc or Ctrl+C). Buffer is discarded; the overlay
    // closes and focus returns to the pane. The pre-removal
    // `Always` mode used to redirect the cancel key to the target
    // pane here so Claude's Esc-to-interrupt still worked, but the
    // Hotkey-only flow requires an explicit user action to open the
    // overlay in the first place, so we just cancel cleanly.
    if matches!(key.code, KeyCode::Esc)
        || (key.modifiers == KeyModifiers::CONTROL && matches!(key.code, KeyCode::Char('c')))
    {
        app.suspend_overlay();
        app.mark_layout_change();
        return Ok(true);
    }

    // Commit handling: Alt+Enter, Ctrl+Enter, Cmd+Enter (the latter
    // two only when the host enables extended-key reporting), or
    // Ctrl+J (the WSL / Windows Terminal fallback for Ctrl+Enter).
    // Bare Enter — and Shift+Enter — insert a newline into the
    // multi-line composition area, matching chat-app conventions
    // and keeping the "no commit modifier" rule intuitive.
    let is_commit = is_overlay_commit_key(key);
    if matches!(key.code, KeyCode::Enter) && !is_commit {
        if overlay.buffer.chars().count() < 4096 {
            overlay.insert_char('\n');
        }
        app.dirty = true;
        return Ok(true);
    }
    if is_commit {
        let target_pane = overlay.target_pane;
        let buffer = std::mem::take(&mut overlay.buffer);

        // Target sanity check — if the pane disappeared (close tab,
        // shell exit, …) while the user was composing, don't
        // silently discard their input. Keep the overlay open with
        // the buffer restored and fall out of this frame so the
        // user can recover. The buffer was `mem::take`'d above, so
        // put it back.
        let target_alive = app
            .ws()
            .panes
            .get(&target_pane)
            .map(|p| !p.exited)
            .unwrap_or(false);
        if !target_alive {
            if let Some(o) = app.overlay.as_mut() {
                o.buffer = buffer;
                o.cursor = o.buffer.chars().count();
            }
            app.dirty = true;
            return Ok(true);
        }

        // Target alive: close the overlay and deliver. If the
        // paste write fails (PTY closed mid-send, very rare), the
        // error propagates so the top-level render loop can log
        // or surface it instead of the text being dropped
        // silently.
        app.overlay = None;
        let mut commit_result: Result<()> = Ok(());
        if !buffer.is_empty() {
            let focused_before = app.ws().focused_pane_id;
            // forward_paste_to_pty writes to the currently-focused
            // pane, so temporarily refocus the overlay's target so
            // the paste reaches the right pane even if focus moved.
            app.ws_mut().focused_pane_id = target_pane;
            commit_result = app.forward_paste_to_pty(&buffer);
            app.ws_mut().focused_pane_id = focused_before;
        }
        app.clear_overlay_draft(target_pane);
        app.mark_layout_change();
        commit_result?;
        return Ok(true);
    }

    // Edit.
    match key.code {
        KeyCode::Backspace => overlay.backspace(),
        KeyCode::Left => overlay.cursor_left(),
        KeyCode::Right => overlay.cursor_right(),
        KeyCode::Up => overlay.cursor_up(),
        KeyCode::Down => overlay.cursor_down(),
        KeyCode::Home => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                overlay.cursor_buffer_start();
            } else {
                overlay.cursor_home();
            }
        }
        KeyCode::End => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                overlay.cursor_buffer_end();
            } else {
                overlay.cursor_end();
            }
        }
        KeyCode::Char(c) => {
            // Ctrl+U clears the entire draft (not just the current
            // line). The binding is borrowed from readline's
            // "discard" muscle memory but the semantics here are
            // whole-buffer. Handled before the generic Ctrl/Alt
            // swallow below so the chord actually reaches us.
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT)
                && (c == 'u' || c == 'U')
            {
                overlay.clear();
                app.dirty = true;
                return Ok(true);
            }
            if key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            {
                // Don't leak renga chord keys (Ctrl+D, Alt+T …)
                // into the buffer, but also don't let them
                // trigger renga's layout commands mid-composition
                // — the overlay is modal.
                return Ok(true);
            }
            // Cap at a generous but bounded size so a stuck key
            // can't grow the buffer without limit. Multi-line drafts
            // need more headroom than the old single-line overlay,
            // so the cap is bumped to 4096 chars.
            if overlay.buffer.chars().count() < 4096 {
                overlay.insert_char(c);
            }
        }
        _ => return Ok(true),
    }
    app.dirty = true;
    Ok(true)
}

/// Recognizes the IME overlay's commit chord. Accepts
/// `Alt+Enter` (the canonical binding on most terminals),
/// `Ctrl+Enter` and `Cmd+Enter` (only when the host terminal opts
/// into kitty keyboard protocol or xterm modifyOtherKeys), and
/// `Ctrl+J` as a WSL / Windows Terminal fallback for Ctrl+Enter —
/// those hosts deliver Ctrl+Enter as the raw LF byte (0x0A), which
/// crossterm's control-byte branch decodes into exactly
/// `KeyCode::Char('j') + CONTROL` (Issue #226). The Ctrl+J branch
/// is gated on the modifier set being *exactly* CONTROL: a literal
/// `Ctrl+Shift+J` or `Ctrl+Alt+J` typed on a host with extended-key
/// reporting is not what the WSL byte path delivers, and treating
/// those as commits would silently send the buffer when the user
/// just held an extra modifier mid-composition.
///
/// Treating Ctrl+J as a commit key is safe inside the modal
/// overlay: the overlay otherwise swallows every Ctrl-modified
/// character, so we aren't shadowing a binding the user could
/// already use here.
///
/// Reused by the codex peer notification accept path so the same
/// modifier rules apply consistently across modal prompts.
pub(crate) fn is_overlay_commit_key(key: KeyEvent) -> bool {
    if matches!(key.code, KeyCode::Enter)
        && key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL | KeyModifiers::SUPER)
    {
        return true;
    }
    matches!(key.code, KeyCode::Char('j')) && key.modifiers == KeyModifiers::CONTROL
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_screen(rows: u16, cols: u16, bytes: &[u8]) -> vt100::Parser {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(bytes);
        parser
    }

    const INV_ON: &[u8] = b"\x1b[7m";
    const INV_OFF: &[u8] = b"\x1b[27m";

    fn at(r: u16, c: u16) -> Vec<u8> {
        format!("\x1b[{};{}H", r + 1, c + 1).into_bytes()
    }

    #[test]
    fn insert_newline_is_regular_char() {
        let mut o = OverlayState::new(0);
        o.insert_char('a');
        o.insert_char('\n');
        o.insert_char('b');
        assert_eq!(o.buffer, "a\nb");
        assert_eq!(o.cursor, 3);
        assert_eq!(o.total_lines(), 2);
    }

    #[test]
    fn commit_key_accepts_alt_ctrl_super_enter_and_ctrl_j() {
        // Alt+Enter is the canonical commit binding; Ctrl+Enter and
        // Cmd+Enter only arrive when the host enables extended-key
        // reporting; Ctrl+J is the WSL / Windows Terminal fallback
        // that maps from a bare LF byte (Issue #226).
        for modifier in [
            KeyModifiers::ALT,
            KeyModifiers::CONTROL,
            KeyModifiers::SUPER,
        ] {
            let key = KeyEvent::new(KeyCode::Enter, modifier);
            assert!(
                is_overlay_commit_key(key),
                "Enter with {modifier:?} must commit"
            );
        }
        let ctrl_j = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL);
        assert!(
            is_overlay_commit_key(ctrl_j),
            "Ctrl+J must commit (WSL fallback)"
        );
    }

    #[test]
    fn commit_key_rejects_bare_enter_and_unmodified_or_overmodified_j() {
        // Bare Enter is "insert newline" inside the multi-line
        // composer, and bare 'j' is just a literal character —
        // neither should commit.
        assert!(!is_overlay_commit_key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE
        )));
        assert!(!is_overlay_commit_key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::SHIFT
        )));
        assert!(!is_overlay_commit_key(KeyEvent::new(
            KeyCode::Char('j'),
            KeyModifiers::NONE
        )));
        // The WSL fallback's premise is that the raw 0x0A byte
        // arrives as Char('j') + CONTROL exactly. A user who really
        // typed Ctrl+Shift+J or Ctrl+Alt+J on a host with extended
        // key reporting did not press Ctrl+Enter — committing on
        // those would silently send the buffer when an extra
        // modifier was held mid-composition.
        for extra in [KeyModifiers::SHIFT, KeyModifiers::ALT, KeyModifiers::SUPER] {
            let key = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL | extra);
            assert!(
                !is_overlay_commit_key(key),
                "Ctrl+J with extra {extra:?} must not commit"
            );
        }
        // Capital 'J' never originates from the WSL LF byte path
        // (crossterm always emits lowercase for control bytes); it
        // only appears when the user typed a literal Shift+J, which
        // is plainly not Ctrl+Enter.
        assert!(!is_overlay_commit_key(KeyEvent::new(
            KeyCode::Char('J'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_overlay_commit_key(KeyEvent::new(
            KeyCode::Char('J'),
            KeyModifiers::SHIFT
        )));
    }

    #[test]
    fn line_col_tracks_newlines() {
        let mut o = OverlayState::new(0);
        for ch in "abc\ndef\ngh".chars() {
            o.insert_char(ch);
        }
        // cursor at end: line 2, col 2
        assert_eq!(o.line_col(), (2, 2));

        o.cursor = 0;
        assert_eq!(o.line_col(), (0, 0));

        o.cursor = 4; // just after first \n
        assert_eq!(o.line_col(), (1, 0));
    }

    #[test]
    fn cursor_up_preserves_column_when_possible() {
        let mut o = OverlayState::new(0);
        for ch in "abcdef\nghi".chars() {
            o.insert_char(ch);
        }
        // cursor at end of "ghi" → (1, 3)
        assert_eq!(o.line_col(), (1, 3));
        o.cursor_up();
        // should land at (0, 3), i.e. inside "abcdef" at col 3
        assert_eq!(o.line_col(), (0, 3));
    }

    #[test]
    fn cursor_up_clamps_when_target_line_shorter() {
        let mut o = OverlayState::new(0);
        for ch in "ab\n12345".chars() {
            o.insert_char(ch);
        }
        // cursor at end of line 1 → (1, 5)
        assert_eq!(o.line_col(), (1, 5));
        o.cursor_up();
        // line 0 only has "ab" → clamp to line-end (1, 2)
        assert_eq!(o.line_col(), (0, 2));
    }

    #[test]
    fn cursor_up_on_first_line_goes_home() {
        let mut o = OverlayState::new(0);
        for ch in "abc".chars() {
            o.insert_char(ch);
        }
        o.cursor_up();
        assert_eq!(o.cursor, 0);
    }

    #[test]
    fn cursor_down_on_last_line_goes_end() {
        let mut o = OverlayState::new(0);
        for ch in "abc\nde".chars() {
            o.insert_char(ch);
        }
        o.cursor = 5; // inside "de" at col 1 (after 'd')
        o.cursor_down();
        assert_eq!(o.cursor, 6, "last-line Down should land at buffer end");
    }

    #[test]
    fn home_end_respect_current_line() {
        let mut o = OverlayState::new(0);
        for ch in "abc\ndef".chars() {
            o.insert_char(ch);
        }
        // cursor at end of line 1
        o.cursor_home();
        assert_eq!(o.line_col(), (1, 0), "Home should go to start of line");
        o.cursor_end();
        assert_eq!(o.line_col(), (1, 3), "End should go to end of line");

        // From start of line 1, Home stays put; End moves to end.
        o.cursor = 4;
        o.cursor_home();
        assert_eq!(o.cursor, 4);
    }

    #[test]
    fn buffer_start_end_navigate_whole_buffer() {
        let mut o = OverlayState::new(0);
        for ch in "abc\ndef\nghi".chars() {
            o.insert_char(ch);
        }
        o.cursor = 5; // middle of line 1
        o.cursor_buffer_start();
        assert_eq!(o.cursor, 0);
        o.cursor_buffer_end();
        assert_eq!(o.cursor, 11);
    }

    #[test]
    fn backspace_across_newline() {
        let mut o = OverlayState::new(0);
        for ch in "abc\n".chars() {
            o.insert_char(ch);
        }
        assert_eq!(o.total_lines(), 2);
        o.backspace();
        assert_eq!(o.buffer, "abc");
        assert_eq!(o.total_lines(), 1);
    }

    #[test]
    fn snapshot_visible_input_reads_single_line_prompt() {
        let mut bytes = at(2, 0);
        bytes.extend_from_slice(b"> hi");
        bytes.extend_from_slice(INV_ON);
        bytes.extend_from_slice(b" ");
        bytes.extend_from_slice(INV_OFF);
        let parser = make_screen(4, 20, &bytes);

        let snapshot = snapshot_visible_input_from_screen(parser.screen()).unwrap();
        assert_eq!(
            snapshot,
            VisibleInputSnapshot {
                buffer: "hi".into(),
                cursor: 2,
            }
        );
    }

    #[test]
    fn snapshot_visible_input_joins_wrapped_rows() {
        let mut bytes = at(2, 0);
        bytes.extend_from_slice(b"> aaaaaaaa");
        bytes.extend_from_slice(&at(3, 0));
        bytes.extend_from_slice(b"  bb");
        bytes.extend_from_slice(INV_ON);
        bytes.extend_from_slice(b" ");
        bytes.extend_from_slice(INV_OFF);
        let parser = make_screen(5, 10, &bytes);

        let snapshot = snapshot_visible_input_from_screen(parser.screen()).unwrap();
        assert_eq!(
            snapshot,
            VisibleInputSnapshot {
                buffer: "aaaaaaaabb".into(),
                cursor: 10,
            }
        );
    }

    #[test]
    fn snapshot_visible_input_preserves_mid_buffer_cursor() {
        let mut bytes = at(2, 0);
        bytes.extend_from_slice(b"> aa");
        bytes.extend_from_slice(INV_ON);
        bytes.extend_from_slice(b"a");
        bytes.extend_from_slice(INV_OFF);
        bytes.extend_from_slice(b"aaaaa");
        bytes.extend_from_slice(&at(3, 0));
        bytes.extend_from_slice(b"  bb");
        let parser = make_screen(5, 10, &bytes);

        let snapshot = snapshot_visible_input_from_screen(parser.screen()).unwrap();
        assert_eq!(
            snapshot,
            VisibleInputSnapshot {
                buffer: "aaaaaaaabb".into(),
                cursor: 2,
            }
        );
    }

    #[test]
    fn snapshot_visible_input_uses_visible_cursor_when_no_inverse_cell() {
        // Modern Claude Code (v2.1.x+) paints no inverse caret cell —
        // the visible hardware cursor marks the edit position. Opening
        // the overlay with the caret mid-line must capture that
        // position, not snap to end-of-input.
        let mut bytes = at(2, 0);
        bytes.extend_from_slice(b"> hello world");
        bytes.extend_from_slice(&at(2, 8)); // caret on the 'w' of "world"
        let parser = make_screen(4, 20, &bytes);

        let snapshot = snapshot_visible_input_from_screen(parser.screen()).unwrap();
        assert_eq!(
            snapshot,
            VisibleInputSnapshot {
                buffer: "hello world".into(),
                cursor: 6,
            }
        );
    }

    #[test]
    fn snapshot_visible_input_pending_wrap_cursor_keeps_end_of_input() {
        // Typing into the last cell leaves vt100 in pending-autowrap:
        // cursor_position() reports col == cols. As the exclusive bound
        // of count_chars_before_col that raw column means "after the
        // last char" — the snapshot must restore the caret at
        // end-of-input, not one character short (codex round-2 Major).
        let mut bytes = at(2, 0);
        bytes.extend_from_slice(b"> aaaaaaaa"); // exactly fills width 10
        let parser = make_screen(5, 10, &bytes);
        assert_eq!(
            parser.screen().cursor_position(),
            (2, 10),
            "pending-wrap premise"
        );

        let snapshot = snapshot_visible_input_from_screen(parser.screen()).unwrap();
        assert_eq!(
            snapshot,
            VisibleInputSnapshot {
                buffer: "aaaaaaaa".into(),
                cursor: 8,
            }
        );
    }

    #[test]
    fn clear_visible_input_bytes_walks_to_end_then_backspaces_all_chars() {
        let bytes = clear_visible_input_bytes(&VisibleInputSnapshot {
            buffer: "hello".into(),
            cursor: 2,
        });
        assert_eq!(bytes, b"\x1b[C\x1b[C\x1b[C\x7f\x7f\x7f\x7f\x7f");
    }

    #[test]
    fn detects_claude_paste_placeholder_inside_visible_input() {
        assert!(visible_input_contains_claude_paste_placeholder(
            "prefix [Pasted text #2 +16 lines] suffix"
        ));
        assert!(visible_input_contains_claude_paste_placeholder(
            "[Pasted text #1 +1 line]"
        ));
        assert!(visible_input_contains_claude_paste_placeholder(
            "[Pasted text #x +16 lines]"
        ));
        assert!(!visible_input_contains_claude_paste_placeholder(
            "plain input text"
        ));
    }
}
