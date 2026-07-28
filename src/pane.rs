use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

use crate::app::AppEvent;

const MOUSE_PROTOCOL_CACHE_TTL: Duration = Duration::from_secs(2);

#[derive(Copy, Clone)]
struct CachedMouseProtocol {
    mode: vt100::MouseProtocolMode,
    encoding: vt100::MouseProtocolEncoding,
    seen_at: Instant,
}

/// A terminal pane wrapping a PTY and vt100 parser.
pub struct Pane {
    pub id: usize,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    pub parser: Arc<Mutex<vt100::Parser>>,
    child: Box<dyn Child + Send + Sync>,
    _reader_handle: thread::JoinHandle<()>,
    last_rows: u16,
    last_cols: u16,
    pub exited: bool,
    pub title: Arc<Mutex<String>>,
    pub cwd: PathBuf,
    pub total_scrollback: Arc<std::sync::atomic::AtomicUsize>,
    /// Bytes to write into the PTY once the shell prompt is ready.
    /// `None` means no command queued (or already flushed).
    pub pending_startup: Option<Vec<u8>>,
    /// Set to `true` by the reader thread once a shell prompt has been
    /// observed. Used to gate `pending_startup` flushing so the command
    /// is not eaten by an initializing shell.
    pub prompt_seen: Arc<AtomicBool>,
    /// Latches to `true` the first time the OSC window title contains
    /// "claude". Never reset. Consumed only by `claude_ever_seen()` —
    /// **not** by `is_claude_running()` — because the latch must not
    /// leak into call sites that genuinely care whether Claude is the
    /// current foreground app (e.g. `shell_accepts_command_injection`
    /// gating `Alt+P`).
    pub claude_seen: Arc<AtomicBool>,
    /// Codex equivalent of `claude_seen`. Latches on the first OSC
    /// title that mentions "codex" and never resets, so cosmetic
    /// indicators (border accent, pane label, tab title decoration)
    /// keep identifying the pane as Codex even when Codex rewrites
    /// its title to a task-specific summary that drops the literal
    /// substring. Foreground-app gating (mouse protocol resolution,
    /// codex_peer detection) still uses the live `is_codex_running()`
    /// signal — see issue #209 for the cosmetic-vs-foreground split.
    pub codex_seen: Arc<AtomicBool>,
    /// Cache of the most recently *detected* Claude caret cell on
    /// this pane: `(host_row, host_col)` in vt100 screen coords —
    /// already shifted to land on Claude's inverse-video marker.
    /// Used as the host-caret position whenever the renderer cannot
    /// detect an inverse cell near the live vt100 cursor (Claude is
    /// painting elsewhere on the screen, blink is in its OFF phase,
    /// etc.). Sticky: only refreshed by detection, never expired
    /// or auto-cleared. Default `None` until the first detection.
    pub claude_caret_cache: Mutex<Option<(u16, u16)>>,
    /// Cache the last non-`None` mouse reporting mode we actually saw
    /// from the child PTY. Codex appears to transiently redraw without
    /// the live vt100 state always surfacing the mode on every frame,
    /// so mouse forwarding reuses this cache for a short TTL rather
    /// than guessing a protocol from scratch.
    mouse_protocol_cache: Arc<Mutex<Option<CachedMouseProtocol>>>,
    /// DECSET 1007 ("alternate scroll mode") is not tracked by vt100
    /// 0.16, but terminals still use it to map wheel events to
    /// Up/Down arrow keys even on the main screen. Track the latest
    /// value from the raw PTY stream so Codex can get the same
    /// fallback behavior it gets outside renga.
    alternate_scroll_mode: Arc<AtomicBool>,
    /// Best-effort local latch for Codex's transcript overlay
    /// (`Ctrl+T`). Wheel fallback opens it once, then keeps using
    /// transcript navigation keys until normal typing resumes.
    codex_transcript_overlay_hint: Arc<AtomicBool>,
    /// Free-form label for tools/humans. Unlike the name (registered in
    /// `Workspace.pane_names` as the unique IPC key), `role` may repeat
    /// and may be absent. Surfaced via `renga list`.
    pub role: Option<String>,
    /// Optional 1-2 sentence per-pane summary set by the pane's MCP
    /// `set_summary` tool. In-memory only; cleared when the pane exits.
    /// Surfaced via `list_panes` / `list_peers` so peer agents can see
    /// what other panes are working on.
    pub summary: Option<String>,
    /// Set once the App has published a `PaneExited` event for this
    /// pane. Guards the multiple exit pathways (explicit close, tab
    /// close, natural shell exit) so subscribers see exactly one event.
    pub exit_event_emitted: bool,
    /// Kill-on-close Job Object holding the pane shell and every
    /// descendant the kernel added since spawn. `None` when job
    /// creation/assignment failed at spawn time — `kill()` then falls
    /// back to the legacy `taskkill /F /T` tree walk.
    #[cfg(windows)]
    job: Option<crate::win_job::PaneJob>,
}

impl Pane {
    /// Create a new pane with a PTY shell.
    #[allow(dead_code)] // retained for tests / external callers that don't care about cwd
    pub fn new(id: usize, rows: u16, cols: u16, event_tx: Sender<AppEvent>) -> Result<Self> {
        Self::new_with_cwd(id, rows, cols, event_tx, None)
    }

    pub fn new_with_cwd(
        id: usize,
        rows: u16,
        cols: u16,
        event_tx: Sender<AppEvent>,
        cwd: Option<PathBuf>,
    ) -> Result<Self> {
        let pty_system = native_pty_system();

        let pty_size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };

        let pair = pty_system.openpty(pty_size).context("Failed to open PTY")?;

        let shell = detect_shell();
        let mut cmd = CommandBuilder::new(&shell);

        let shell_name = shell
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();

        if shell_name.contains("bash") || shell_name.contains("zsh") {
            cmd.arg("--login");
        }

        let work_dir =
            cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        cmd.cwd(&work_dir);
        cmd.env("TERM", "xterm-256color");
        cmd.env("RENGA", "1"); // marker to detect nested renga
                               // Per-pane identity for the MCP peer subprocess (see #97). The
                               // subprocess is spawned by Claude Code, which inherits env
                               // from this PTY, so reading `RENGA_PANE_ID` at startup is how
                               // the subprocess tells renga's IPC server which pane it is.
        cmd.env("RENGA_PANE_ID", id.to_string());

        let child = pair
            .slave
            .spawn_command(cmd)
            .context("Failed to spawn shell")?;

        // Windows: capture the shell (and, via kernel-side inheritance,
        // every future descendant) in a kill-on-close Job Object so
        // pane close can reap the whole tree even after intermediate
        // parents exit — `taskkill /T` can't reach those. Assignment
        // failure is non-fatal: `kill()` falls back to taskkill.
        #[cfg(windows)]
        let job = child.process_id().and_then(crate::win_job::PaneJob::assign);

        // Drop the slave side — we only use master
        drop(pair.slave);

        let writer = pair
            .master
            .take_writer()
            .context("Failed to take PTY writer")?;

        // Scrollback buffer: 10000 lines of history
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 10000)));
        let pane_title = Arc::new(Mutex::new(String::new()));

        let reader = pair
            .master
            .try_clone_reader()
            .context("Failed to clone PTY reader")?;

        let parser_clone = Arc::clone(&parser);
        let title_clone = Arc::clone(&pane_title);
        let scrollback_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let scrollback_clone = Arc::clone(&scrollback_counter);
        let prompt_seen = Arc::new(AtomicBool::new(false));
        let prompt_seen_clone = Arc::clone(&prompt_seen);
        let claude_seen = Arc::new(AtomicBool::new(false));
        let claude_seen_clone = Arc::clone(&claude_seen);
        let codex_seen = Arc::new(AtomicBool::new(false));
        let codex_seen_clone = Arc::clone(&codex_seen);
        let mouse_protocol_cache = Arc::new(Mutex::new(None));
        let mouse_protocol_cache_clone = Arc::clone(&mouse_protocol_cache);
        let alternate_scroll_mode = Arc::new(AtomicBool::new(false));
        let alternate_scroll_mode_clone = Arc::clone(&alternate_scroll_mode);
        let codex_transcript_overlay_hint = Arc::new(AtomicBool::new(false));
        let reader_handle = thread::spawn(move || {
            pty_reader_thread(
                reader,
                parser_clone,
                title_clone,
                scrollback_clone,
                prompt_seen_clone,
                claude_seen_clone,
                codex_seen_clone,
                mouse_protocol_cache_clone,
                alternate_scroll_mode_clone,
                id,
                event_tx,
            );
        });

        let mut pane = Self {
            id,
            master: pair.master,
            writer,
            parser,
            child,
            _reader_handle: reader_handle,
            last_rows: rows,
            last_cols: cols,
            exited: false,
            title: pane_title,
            cwd: work_dir,
            total_scrollback: scrollback_counter,
            pending_startup: None,
            prompt_seen,
            claude_seen,
            codex_seen,
            claude_caret_cache: Mutex::new(None),
            mouse_protocol_cache,
            alternate_scroll_mode,
            codex_transcript_overlay_hint,
            role: None,
            summary: None,
            exit_event_emitted: false,
            #[cfg(windows)]
            job,
        };

        // Inject OSC 7 hook after shell starts
        // Leading space prevents it from appearing in bash history
        if shell_name.contains("bash") {
            let setup = concat!(
                " __renga_osc7() { printf '\\033]7;file://%s%s\\007' \"$HOSTNAME\" \"$PWD\"; };",
                " PROMPT_COMMAND=\"__renga_osc7;${PROMPT_COMMAND}\";",
                " clear\n",
            );
            let _ = pane.write_input(setup.as_bytes());
        } else if shell_name.contains("zsh") {
            let setup = concat!(
                " __renga_osc7() { printf '\\033]7;file://%s%s\\007' \"$HOST\" \"$PWD\"; };",
                " precmd_functions+=(__renga_osc7);",
                " clear\n",
            );
            let _ = pane.write_input(setup.as_bytes());
        }

        Ok(pane)
    }

    /// Write input bytes to the PTY (keyboard input from user).
    pub fn write_input(&mut self, data: &[u8]) -> Result<()> {
        if self.exited {
            return Ok(());
        }
        if self.writer.write_all(data).is_err() || self.writer.flush().is_err() {
            self.exited = true;
        }
        Ok(())
    }

    /// Resize the PTY and vt100 parser. Returns `true` if the size
    /// actually changed (useful for callers that want to know whether
    /// a SIGWINCH was sent to the child). No-op and returns `false`
    /// when the size hasn't changed.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<bool> {
        if rows == 0 || cols == 0 {
            return Ok(false);
        }

        // Skip if size hasn't changed
        if rows == self.last_rows && cols == self.last_cols {
            return Ok(false);
        }

        self.last_rows = rows;
        self.last_cols = cols;

        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("Failed to resize PTY")?;

        let mut parser = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        parser.screen_mut().set_size(rows, cols);
        // Clear the screen buffer to avoid rendering stale content at the new size.
        // The TUI app (e.g. Claude Code) receives SIGWINCH and will redraw.
        // A brief blank frame is preferable to overlapping garbled output.
        parser.process(b"\x1b[2J\x1b[H");
        Ok(true)
    }

    /// Scroll the terminal view up (into scrollback history).
    pub fn scroll_up(&self, lines: usize) {
        let mut parser = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        let current = parser.screen().scrollback();
        parser.screen_mut().set_scrollback(current + lines);
    }

    /// Get scrollbar info: (current_offset, max_offset).
    /// max_offset is estimated by trying to scroll to a large value and checking.
    pub fn scrollbar_info(&self) -> (usize, usize) {
        let parser = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        let screen = parser.screen();
        let current = screen.scrollback();
        // Estimate max by checking: set_scrollback clamps to actual scrollback length
        // We can't query it directly, so use the stored total_scrollback as estimate
        let total = self
            .total_scrollback
            .load(std::sync::atomic::Ordering::Relaxed);
        (current, total)
    }

    /// Scroll the terminal view down (towards current output).
    pub fn scroll_down(&self, lines: usize) {
        let mut parser = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        let current = parser.screen().scrollback();
        parser
            .screen_mut()
            .set_scrollback(current.saturating_sub(lines));
    }

    /// Reset scroll to the bottom (live view).
    pub fn scroll_reset(&self) {
        let mut parser = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        parser.screen_mut().set_scrollback(0);
    }

    /// Check if the terminal is scrolled back.
    pub fn is_scrolled_back(&self) -> bool {
        let parser = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        parser.screen().scrollback() > 0
    }

    /// Check if the PTY application has enabled bracketed paste mode.
    pub fn is_bracketed_paste_enabled(&self) -> bool {
        let parser = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        parser.screen().bracketed_paste()
    }

    /// Decide how a mouse-wheel event at `(local_col, local_row)` — pane
    /// content-area coordinates, 0-origin — should be handled. Returns:
    ///
    /// * `Some(bytes)` when the caller should forward those bytes to
    ///   the PTY instead of scrolling the vt100 scrollback. Two sub-
    ///   cases:
    ///   - **Mouse reporting enabled** (any `MouseProtocolMode` other
    ///     than `None`), regardless of whether the app is in the
    ///     alternate screen buffer: the bytes are an xterm mouse
    ///     report encoded in the protocol the app selected (SGR /
    ///     UTF-8 / Default). Claude Code `/tui fullscreen` lives
    ///     here — it enables DECSET 1003 on the *main* screen.
    ///   - **Alt screen but no mouse reporting** (e.g. `less`): the
    ///     bytes are an arrow-key escape so the wheel still moves
    ///     the cursor, mirroring xterm / WezTerm behavior.
    /// * `None` for a plain shell on the main screen with no mouse
    ///   reporting — the caller falls back to `scroll_up` /
    ///   `scroll_down` and walks the vt100 scrollback.
    pub fn wheel_forward_bytes(
        &self,
        codex_hint: bool,
        scroll_down: bool,
        local_col: u16,
        local_row: u16,
    ) -> Option<Vec<u8>> {
        let parser = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        let screen = parser.screen();
        let alt = screen.alternate_screen();
        let scrollback = screen.scrollback();
        let is_codex = codex_hint || self.is_codex_running();
        let mouse = self.effective_mouse_protocol(
            screen.mouse_protocol_mode(),
            screen.mouse_protocol_encoding(),
            codex_hint,
        );

        // Decision order matters: an app that enabled mouse reporting
        // expects the wheel even if it hasn't entered the alt screen.
        // Claude Code's `/tui fullscreen` is exactly this case — it
        // sets MouseProtocolMode::AnyMotion (DECSET 1003) without
        // switching to the alternate screen buffer, so gating on
        // `alternate_screen()` alone silently drops the event.
        //
        // - mouse reporting on  → encode wheel report in the app's
        //   chosen protocol (works for both in-place TUIs like Claude
        //   /tui and classic alt-screen TUIs like vim).
        // - Codex with a recently-observed mouse mode but a transient
        //   live `None` state → reuse that cached mode for a short TTL
        //   (same "sticky for UI stability" idea as Claude's caret
        //   tracking, but bounded so an intentional mouse-off toggle
        //   still wins quickly).
        // - mouse reporting off + alt screen → xterm-style arrow
        //   fallback so `less` and friends still move their cursor.
        // - Codex on the main screen with zero host scrollback →
        //   transcript-overlay fallback. First wheel opens the
        //   transcript (`Ctrl+T`), later wheels use overlay-native
        //   arrow scrolling until normal typing resumes.
        // - mouse reporting off + normal screen → None, let the caller
        //   scroll vt100 scrollback (normal shell history).
        match mouse {
            Some((_, encoding)) => {
                let button: u8 = if scroll_down { 65 } else { 64 };
                Some(encode_mouse_wheel_report(
                    button, local_col, local_row, encoding,
                ))
            }
            None => {
                if should_use_arrow_wheel_fallback(
                    alt || self.alternate_scroll_mode.load(Ordering::Relaxed),
                    is_codex,
                ) {
                    Some(encode_arrow_wheel_fallback(scroll_down))
                } else if should_use_codex_main_screen_wheel_fallback(
                    is_codex,
                    alt,
                    self.alternate_scroll_mode.load(Ordering::Relaxed),
                    scrollback,
                ) {
                    Some(encode_codex_transcript_wheel_fallback(
                        scroll_down,
                        self.mark_codex_transcript_overlay_hint(),
                    ))
                } else {
                    None
                }
            }
        }
    }

    /// Decide how a mouse button press/release/drag at `(local_col,
    /// local_row)` — pane content-area coordinates, 0-origin — should
    /// be handled. Mirrors [`Pane::wheel_forward_bytes`] (Issue #52 /
    /// PR #53) for non-wheel events: the same click that lands in a
    /// plain shell is a renga concern (focus, scrollbar, drag-select)
    /// while a click on a pane running Claude Code `/tui fullscreen`,
    /// vim, lazygit, etc. needs to reach the PTY as an xterm mouse
    /// report so the app can handle it.
    ///
    /// Returns `Some(bytes)` when the caller should forward the report
    /// to the PTY (and skip the renga-side handlers for this event).
    /// Returns `None` when mouse reporting is disabled, or when the
    /// active [`MouseProtocolMode`] doesn't cover this event type —
    /// e.g. plain `Press` mode never emits release events, so
    /// forwarding one would be protocol noise.
    ///
    /// Mode → action gating follows the xterm ladder:
    /// * `None` → nothing forwards.
    /// * `Press` (DECSET 9) → only button presses.
    /// * `PressRelease` (DECSET 1000) → presses + releases, no drag.
    /// * `ButtonMotion` (DECSET 1002) → press + release + held-button drag.
    /// * `AnyMotion` (DECSET 1003) → same as `ButtonMotion` for this
    ///   call site; plain hover motion (no button held) goes through a
    ///   different path that we haven't wired yet.
    pub fn click_forward_bytes(
        &self,
        codex_hint: bool,
        button: PointerButton,
        action: PointerAction,
        local_col: u16,
        local_row: u16,
    ) -> Option<Vec<u8>> {
        let parser = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        let screen = parser.screen();
        let mouse = self.effective_mouse_protocol(
            screen.mouse_protocol_mode(),
            screen.mouse_protocol_encoding(),
            codex_hint,
        );
        let (mode, encoding) = mouse?;

        let allowed = mouse_action_allowed(mode, action);

        if !allowed {
            return None;
        }

        Some(encode_mouse_button_report(
            button, action, local_col, local_row, encoding,
        ))
    }

    /// Check if Claude Code is running in this pane (by current window
    /// title). This is the live signal — it flips back to `false` the
    /// moment Claude exits or rewrites the title to something that
    /// doesn't contain "claude". Use this for foreground-app gating
    /// (e.g. `shell_accepts_command_injection`); use
    /// `claude_ever_seen` for cursor-rendering purposes that must
    /// survive Claude's transient task-name title rewrites.
    pub fn is_claude_running(&self) -> bool {
        if let Ok(t) = self.title.lock() {
            title_mentions_client(&t, "claude")
        } else {
            false
        }
    }

    /// Check if Codex is running in this pane (by current window
    /// title). This is the live signal — it flips back to `false`
    /// the moment Codex exits or rewrites the title to something
    /// that doesn't contain "codex". Use this for foreground-app
    /// gating (mouse protocol resolution, codex_peer fallback) and
    /// `codex_ever_seen()` for cosmetic indicators that must
    /// survive Codex's transient task-name title rewrites (#209).
    pub fn is_codex_running(&self) -> bool {
        if let Ok(t) = self.title.lock() {
            title_mentions_client(&t, "codex")
        } else {
            false
        }
    }

    fn effective_mouse_protocol(
        &self,
        mode: vt100::MouseProtocolMode,
        encoding: vt100::MouseProtocolEncoding,
        codex_hint: bool,
    ) -> Option<(vt100::MouseProtocolMode, vt100::MouseProtocolEncoding)> {
        resolve_mouse_protocol(
            mode,
            encoding,
            codex_hint || self.is_codex_running(),
            self.cached_mouse_protocol(),
        )
    }

    fn cached_mouse_protocol(
        &self,
    ) -> Option<(vt100::MouseProtocolMode, vt100::MouseProtocolEncoding)> {
        let cache = self
            .mouse_protocol_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let cached = (*cache)?;
        (cached.seen_at.elapsed() <= MOUSE_PROTOCOL_CACHE_TTL)
            .then_some((cached.mode, cached.encoding))
    }

    pub(crate) fn clear_codex_transcript_overlay_hint(&self) {
        self.codex_transcript_overlay_hint
            .store(false, Ordering::Relaxed);
    }

    fn mark_codex_transcript_overlay_hint(&self) -> bool {
        self.codex_transcript_overlay_hint
            .swap(true, Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn set_codex_transcript_overlay_hint_for_test(&self, active: bool) {
        self.codex_transcript_overlay_hint
            .store(active, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn codex_transcript_overlay_hint_for_test(&self) -> bool {
        self.codex_transcript_overlay_hint.load(Ordering::Relaxed)
    }

    /// Sticky check: has Claude ever been observed running in this
    /// pane (by OSC title)? Latches on first match and never resets.
    ///
    /// Needed because Claude rewrites its window title to reflect the
    /// in-flight task (e.g. `✶ Write a 5000-character novel`), and
    /// those rewrites frequently drop the literal "claude" string. A
    /// non-latched check would flip to `false` mid-task and the
    /// renderer would stop showing the hardware caret — Claude keeps
    /// the PTY cursor hidden via DECTCEM and relies on the host
    /// terminal cursor being placed over its own block glyph.
    ///
    /// Scoped narrowly to the cursor-rendering path so call sites
    /// that need an honest "is Claude the current foreground app?"
    /// signal still get one via `is_claude_running()`.
    pub fn claude_ever_seen(&self) -> bool {
        self.claude_seen.load(Ordering::Relaxed)
    }

    /// Sticky check: has Codex ever been observed running in this
    /// pane (by OSC title)? Latches on first match and never resets.
    /// Mirrors `claude_ever_seen()` and exists for the same reason —
    /// Codex CLI rewrites its window title to reflect the in-flight
    /// task, frequently dropping the literal "codex" substring, which
    /// would otherwise flip the cosmetic indicators (border accent,
    /// pane label, tab title decoration) off mid-session. See #209.
    ///
    /// Foreground-app gating (mouse protocol resolution,
    /// `pane_expects_codex_peer_delivery` fallback) still calls
    /// `is_codex_running()` so it sees the honest current state.
    pub fn codex_ever_seen(&self) -> bool {
        self.codex_seen.load(Ordering::Relaxed)
    }

    /// Whether it is safe to synthesize a shell command line into this
    /// pane's PTY. Returns `false` when any other foreground process
    /// has captured the terminal — `alternate_screen()` catches TUIs
    /// like vim / less / lazygit; `is_claude_running()` catches Claude
    /// Code's `/tui fullscreen` mode, which enables mouse reporting
    /// without entering the alt screen (see the mouse-forwarding path
    /// in `map_wheel_for_pane_buffer` for the same distinction).
    /// Callers that want to inject a command (`Alt+P`, orchestrator
    /// scripts) should gate on this.
    pub fn shell_accepts_command_injection(&self) -> bool {
        let alt_screen = {
            let parser = self.parser.lock().unwrap_or_else(|e| e.into_inner());
            parser.screen().alternate_screen()
        };
        !alt_screen && !self.is_claude_running()
    }

    /// Kill the PTY child process.
    ///
    /// On Windows, `portable-pty`'s `Child::kill` is a bare
    /// `TerminateProcess` against the immediate shell only — any
    /// grandchildren (e.g. `claude`/`node.exe` launched from the shell
    /// via `pending_startup`) survive and keep open handles on the
    /// pane's working directory. That blocks `git worktree remove` /
    /// `rmdir` until the renga process itself exits (#214). The pane's
    /// Job Object (assigned at spawn) terminates every descendant in
    /// one call, independent of the parent/child links still being
    /// intact; `taskkill /F /T` remains only as the fallback for the
    /// rare spawn where job assignment failed, with its known holes
    /// (can't reach children of already-dead intermediates).
    pub fn kill(&mut self) {
        // `try_wait` distinguishes "child still alive, needs killing"
        // from "child already exited, just needs reaping" — important
        // because `pane.exited` only signals PTY EOF was observed, not
        // that the child has been waited on, so naive short-circuiting
        // on `exited` would zombie the shell on Unix Drop. The taskkill
        // / `child.kill()` path is skipped when the process is already
        // gone so the close+Drop pair doesn't double-spawn taskkill on
        // Windows (#214 review), but `wait()` always runs to reap.
        let alive = !matches!(self.child.try_wait(), Ok(Some(_)));
        // Terminate the job even when the shell itself already exited:
        // orphaned grandchildren (dev servers, `run_in_background`
        // jobs, mcp-peer, …) stay in the job after their parents die,
        // and this is the only close path that can still reach them.
        // `take()` keeps the close+Drop pair single-shot. A job that
        // refuses to terminate reports `false`, so the taskkill
        // fallback below still runs instead of being skipped on the
        // strength of a call that did nothing.
        #[cfg(windows)]
        let job_terminated = match self.job.take() {
            Some(job) => job.terminate(),
            None => false,
        };
        if alive {
            #[cfg(windows)]
            if !job_terminated {
                if let Some(pid) = self.child.process_id() {
                    let _ = std::process::Command::new("taskkill")
                        .args(["/F", "/T", "/PID", &pid.to_string()])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .stdin(std::process::Stdio::null())
                        .status();
                }
            }
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        self.exited = true;
    }

    /// Queue a command to be written into the PTY once the shell prompt
    /// is ready. A trailing newline is appended automatically so the
    /// command is executed as soon as the shell sees it.
    pub fn queue_startup_command(&mut self, cmd: &str) {
        let mut data = cmd.as_bytes().to_vec();
        if !data.ends_with(b"\n") {
            data.push(b'\n');
        }
        self.pending_startup = Some(data);
    }

    /// Queue raw text to be inserted at the shell prompt without an
    /// automatic newline. Mirrors `Alt+P`'s "insert but don't submit"
    /// semantics so the user can review / edit before pressing Enter.
    /// Use [`queue_startup_command`] when the command should auto-run.
    pub fn queue_startup_text(&mut self, text: &str) {
        self.pending_startup = Some(text.as_bytes().to_vec());
    }

    /// Whether the shell child process has exited, for tests that need
    /// to distinguish "shell dead" from "PTY closed" — on ConPTY the
    /// PTY read only EOFs when the last attached client detaches, so
    /// `exited` lags the shell's death while grandchildren are alive.
    /// Gated on `windows` as well as `test`: the only caller is the
    /// `#[cfg(windows)]` job-reaping test, so `#[cfg(test)]` alone
    /// makes this dead code everywhere else and fails CI's clippy job,
    /// which runs `-D warnings` on Linux.
    #[cfg(all(test, windows))]
    pub(crate) fn child_exited_for_test(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    /// If a startup command is queued and the shell prompt has been
    /// observed, write the command into the PTY and clear the queue.
    /// Returns `Ok(true)` if a flush happened, `Ok(false)` otherwise.
    /// Acquire ordering pairs with the reader thread's `Release` store.
    pub fn try_flush_startup(&mut self) -> std::io::Result<bool> {
        if self.pending_startup.is_none() {
            return Ok(false);
        }
        if !self.prompt_seen.load(Ordering::Acquire) {
            return Ok(false);
        }
        if let Some(data) = self.pending_startup.take() {
            // Mirror `write_input`: any write OR flush failure marks the
            // pane as exited and is reported as a no-op flush so callers
            // do not see partial-write panics.
            if self.writer.write_all(&data).is_err() || self.writer.flush().is_err() {
                self.exited = true;
                return Ok(false);
            }
            return Ok(true);
        }
        Ok(false)
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Which mouse button the report encodes. Only the three physical
/// buttons renga actually receives from crossterm — extra buttons
/// (4/5/wheel, side buttons) are handled by their own paths and
/// don't round-trip through this enum.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PointerButton {
    Left,
    Middle,
    Right,
}

impl PointerButton {
    /// Low 2 bits of the xterm button code: 0 = left, 1 = middle, 2 = right.
    fn code(self) -> u8 {
        match self {
            PointerButton::Left => 0,
            PointerButton::Middle => 1,
            PointerButton::Right => 2,
        }
    }
}

/// Which part of a button interaction the event represents. `Drag` is
/// a motion event with a button still held; plain hover (no button) is
/// a separate path not handled here.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PointerAction {
    Press,
    Release,
    Drag,
}

/// Encode an xterm mouse button report (press / release / drag) for
/// the given protocol encoding. Separate from
/// [`encode_mouse_wheel_report`] because the release encoding for the
/// legacy `Default` / `Utf8` forms uses a different button field (`3`
/// instead of the physical button code) — merging the two would have
/// required every wheel call site to also thread a "this is a release"
/// flag through for no gain.
///
/// `col` / `row` are pane-local content-area coordinates, **0-origin**;
/// the encoder converts to the 1-origin wire form. The `Default`
/// encoding truncates past 223 for the same reason `encode_mouse_wheel_report`
/// does (single-byte cell + 32 offset).
pub fn encode_mouse_button_report(
    button: PointerButton,
    action: PointerAction,
    col: u16,
    row: u16,
    encoding: vt100::MouseProtocolEncoding,
) -> Vec<u8> {
    let c1 = col.saturating_add(1);
    let r1 = row.saturating_add(1);
    let btn = button.code();

    match encoding {
        vt100::MouseProtocolEncoding::Sgr => {
            // SGR: `CSI < Cb ; Cx ; Cy ; {M|m}`. `M` ends press and
            // drag, `m` ends release. `Cb` keeps the physical button
            // code for press / release; drag sets the +32 motion bit.
            let cb = match action {
                PointerAction::Press | PointerAction::Release => u32::from(btn),
                PointerAction::Drag => u32::from(btn) + 32,
            };
            let final_byte = match action {
                PointerAction::Press | PointerAction::Drag => 'M',
                PointerAction::Release => 'm',
            };
            format!("\x1b[<{cb};{c1};{r1}{final_byte}").into_bytes()
        }
        vt100::MouseProtocolEncoding::Utf8 => {
            let cb = mouse_button_legacy_cb(button, action);
            let mut v: Vec<u8> = vec![0x1b, b'[', b'M', cb];
            encode_utf8_coord(&mut v, c1);
            encode_utf8_coord(&mut v, r1);
            v
        }
        vt100::MouseProtocolEncoding::Default => {
            let cb = mouse_button_legacy_cb(button, action);
            let col_byte = c1.saturating_add(32).min(255) as u8;
            let row_byte = r1.saturating_add(32).min(255) as u8;
            vec![0x1b, b'[', b'M', cb, col_byte, row_byte]
        }
    }
}

/// Legacy `Default` / `Utf8` button byte: `button_code + 32`, with
/// release flattened to `3 + 32` (the legacy encoding has no per-button
/// release signal) and drag marked with the `+32` motion flag on top
/// of the press code.
fn mouse_button_legacy_cb(button: PointerButton, action: PointerAction) -> u8 {
    let base: u8 = match action {
        PointerAction::Press => button.code(),
        // Legacy release encodes as `3` regardless of which physical
        // button was let go — the app keys off the earlier press.
        PointerAction::Release => 3,
        // Drag = press button code + motion bit.
        PointerAction::Drag => button.code() + 32,
    };
    base.saturating_add(32)
}

fn resolve_mouse_protocol(
    mode: vt100::MouseProtocolMode,
    encoding: vt100::MouseProtocolEncoding,
    allow_cached_fallback: bool,
    cached: Option<(vt100::MouseProtocolMode, vt100::MouseProtocolEncoding)>,
) -> Option<(vt100::MouseProtocolMode, vt100::MouseProtocolEncoding)> {
    match mode {
        vt100::MouseProtocolMode::None if allow_cached_fallback => cached,
        vt100::MouseProtocolMode::None => None,
        _ => Some((mode, encoding)),
    }
}

fn should_use_arrow_wheel_fallback(alt_like: bool, is_codex: bool) -> bool {
    alt_like && !is_codex
}

fn should_use_codex_main_screen_wheel_fallback(
    is_codex: bool,
    alt_screen: bool,
    alt_scroll_mode: bool,
    scrollback: usize,
) -> bool {
    is_codex && !alt_screen && !alt_scroll_mode && scrollback == 0
}

fn encode_arrow_wheel_fallback(scroll_down: bool) -> Vec<u8> {
    let seq = if scroll_down { b"\x1b[B" } else { b"\x1b[A" };
    let mut out = Vec::with_capacity(seq.len() * 3);
    for _ in 0..3 {
        out.extend_from_slice(seq);
    }
    out
}

fn encode_codex_transcript_wheel_fallback(scroll_down: bool, transcript_active: bool) -> Vec<u8> {
    if transcript_active {
        encode_arrow_wheel_fallback(scroll_down)
    } else {
        vec![0x14]
    }
}

fn mouse_action_allowed(mode: vt100::MouseProtocolMode, action: PointerAction) -> bool {
    match (mode, action) {
        (vt100::MouseProtocolMode::None, _) => false,
        (vt100::MouseProtocolMode::Press, PointerAction::Press) => true,
        (vt100::MouseProtocolMode::Press, _) => false,
        (vt100::MouseProtocolMode::PressRelease, PointerAction::Drag) => false,
        (vt100::MouseProtocolMode::PressRelease, _) => true,
        (vt100::MouseProtocolMode::ButtonMotion, _) => true,
        (vt100::MouseProtocolMode::AnyMotion, _) => true,
    }
}

/// Encode a mouse-wheel report for the given xterm protocol encoding.
///
/// `button` is the xterm button code (64 = wheel up, 65 = wheel down).
/// `col` / `row` are pane-local content-area coordinates, **0-origin**
/// — the encoder converts to the 1-origin form on the wire.
///
/// Supports SGR (recommended, CSI < ... M), UTF-8-based, and the
/// legacy "Default" encoding. The Default form truncates coordinates
/// past 223 because each cell is transmitted as `coord + 32` in a
/// single byte — this is an xterm-era limitation and mirrors
/// upstream terminals (WezTerm, Alacritty) behavior.
pub fn encode_mouse_wheel_report(
    button: u8,
    col: u16,
    row: u16,
    encoding: vt100::MouseProtocolEncoding,
) -> Vec<u8> {
    let c1 = col.saturating_add(1);
    let r1 = row.saturating_add(1);
    match encoding {
        vt100::MouseProtocolEncoding::Sgr => format!("\x1b[<{button};{c1};{r1}M").into_bytes(),
        vt100::MouseProtocolEncoding::Utf8 => {
            let mut v: Vec<u8> = vec![0x1b, b'[', b'M', button.saturating_add(32)];
            encode_utf8_coord(&mut v, c1);
            encode_utf8_coord(&mut v, r1);
            v
        }
        vt100::MouseProtocolEncoding::Default => {
            let col_byte = c1.saturating_add(32).min(255) as u8;
            let row_byte = r1.saturating_add(32).min(255) as u8;
            vec![
                0x1b,
                b'[',
                b'M',
                button.saturating_add(32),
                col_byte,
                row_byte,
            ]
        }
    }
}

fn encode_utf8_coord(out: &mut Vec<u8>, coord: u16) {
    // xterm UTF-8 mouse reporting: emit the coordinate + 32 as a
    // UTF-8-encoded code point. Values up to 2015 fit.
    let code = coord.saturating_add(32) as u32;
    if code < 0x80 {
        out.push(code as u8);
    } else {
        // Two-byte UTF-8 for values in [0x80, 0x7FF].
        let c = code.min(0x7FF);
        out.push(0xC0 | ((c >> 6) as u8));
        out.push(0x80 | ((c & 0x3F) as u8));
    }
}

fn detect_alternate_scroll_toggle(data: &[u8]) -> Option<bool> {
    let enable = b"\x1b[?1007h";
    let disable = b"\x1b[?1007l";
    let mut last = None;
    for i in 0..data.len() {
        if data[i..].starts_with(enable) {
            last = Some(true);
        } else if data[i..].starts_with(disable) {
            last = Some(false);
        }
    }
    last
}

/// Background thread that reads PTY output and feeds it to vt100 parser.
#[allow(clippy::too_many_arguments)]
fn pty_reader_thread(
    mut reader: Box<dyn Read + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    title: Arc<Mutex<String>>,
    scrollback_count: Arc<std::sync::atomic::AtomicUsize>,
    prompt_seen: Arc<AtomicBool>,
    claude_seen: Arc<AtomicBool>,
    codex_seen: Arc<AtomicBool>,
    mouse_protocol_cache: Arc<Mutex<Option<CachedMouseProtocol>>>,
    alternate_scroll_mode: Arc<AtomicBool>,
    pane_id: usize,
    event_tx: Sender<AppEvent>,
) {
    // Rolling tail of the most recent bytes read from the PTY. Used to
    // detect a shell prompt that may straddle two reader chunks. Capped
    // so the buffer cannot grow without bound.
    const TAIL_CAP: usize = 256;
    let mut tail: Vec<u8> = Vec::with_capacity(TAIL_CAP * 2);
    let mut control_tail: Vec<u8> = Vec::with_capacity(64);
    let mut osc52_tail: Vec<u8> = Vec::with_capacity(4096);

    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                let _ = event_tx.send(AppEvent::PtyEof(pane_id));
                break;
            }
            Ok(n) => {
                let data = &buf[..n];

                // Track scrollback lines (count newlines)
                let newlines = data.iter().filter(|&&b| b == b'\n').count();
                if newlines > 0 {
                    scrollback_count.fetch_add(newlines, std::sync::atomic::Ordering::Relaxed);
                }

                // Detect OSC 7 (cwd notification). Bash/zsh emit this on
                // every prompt thanks to the hook injected in `Pane::new`,
                // so its presence is also a strong "prompt is up" signal.
                // Release ordering pairs with the Acquire load in
                // `Pane::try_flush_startup` so the queued startup command
                // is published to the main thread atomically.
                if let Some(path) = extract_osc7(data) {
                    prompt_seen.store(true, Ordering::Release);
                    // Drop the rolling tail once the latch is set so we
                    // do not retain memory for the rest of the session.
                    tail = Vec::new();
                    let _ = event_tx.send(AppEvent::CwdChanged(pane_id, path));
                }

                // Detect OSC 0/2 (window title) — used to detect Claude Code
                if let Some(new_title) = extract_osc_title(data) {
                    // Latch: once Claude has been seen in this pane,
                    // remember it forever so transient title rewrites
                    // (Claude reflects the in-flight task in the title
                    // and the literal "claude" frequently drops out)
                    // do not flip `is_claude_running()` to false and
                    // hide the hardware caret. See `Pane::claude_seen`.
                    let lower = new_title.to_lowercase();
                    if lower.contains("claude") {
                        claude_seen.store(true, Ordering::Relaxed);
                    }
                    if lower.contains("codex") {
                        codex_seen.store(true, Ordering::Relaxed);
                    }
                    if let Ok(mut t) = title.lock() {
                        *t = new_title;
                    }
                }

                // Heuristic prompt detection over a rolling tail so prompts
                // that straddle two reads are still picked up.
                if !prompt_seen.load(Ordering::Acquire) {
                    tail.extend_from_slice(data);
                    if tail.len() > TAIL_CAP * 2 {
                        let drop = tail.len() - TAIL_CAP;
                        tail.drain(..drop);
                    }
                    if is_prompt_ready(&tail) {
                        prompt_seen.store(true, Ordering::Release);
                        // Tail no longer needed once the flag latches on.
                        tail = Vec::new();
                    }
                }

                control_tail.extend_from_slice(data);
                if control_tail.len() > 64 {
                    let drop = control_tail.len() - 64;
                    control_tail.drain(..drop);
                }
                if let Some(enabled) = detect_alternate_scroll_toggle(&control_tail) {
                    alternate_scroll_mode.store(enabled, Ordering::Relaxed);
                }
                osc52_tail.extend_from_slice(data);
                for text in drain_osc52_copies(&mut osc52_tail) {
                    let _ = event_tx.send(AppEvent::ClipboardCopy(text));
                }
                if osc52_tail.len() > 1_048_576 {
                    osc52_tail.clear();
                }

                let mut parser = parser.lock().unwrap_or_else(|e| e.into_inner());
                parser.process(data);
                let screen = parser.screen();
                let mode = screen.mouse_protocol_mode();
                if !matches!(mode, vt100::MouseProtocolMode::None) {
                    let mut cache = mouse_protocol_cache
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    *cache = Some(CachedMouseProtocol {
                        mode,
                        encoding: screen.mouse_protocol_encoding(),
                        seen_at: Instant::now(),
                    });
                }
                drop(parser);
                let _ = event_tx.send(AppEvent::PtyOutput(pane_id));
            }
            Err(_) => {
                break;
            }
        }
    }
}

fn drain_osc52_copies(buf: &mut Vec<u8>) -> Vec<String> {
    const PREFIX: &[u8] = b"\x1b]52;";
    let mut copies = Vec::new();
    let mut search_from = 0;

    loop {
        let Some(start_rel) = find_subslice(&buf[search_from..], PREFIX) else {
            keep_possible_prefix_suffix(buf, PREFIX);
            break;
        };
        let start = search_from + start_rel;
        let payload_start = start + PREFIX.len();
        let Some((term_start, term_end)) = find_osc_terminator(buf, payload_start) else {
            if start > 0 {
                buf.drain(..start);
            }
            break;
        };

        if let Some(text) = decode_osc52_body(&buf[payload_start..term_start]) {
            copies.push(text);
        }
        buf.drain(..term_end);
        search_from = 0;
    }

    copies
}

fn decode_osc52_body(body: &[u8]) -> Option<String> {
    let sep = body.iter().position(|&b| b == b';')?;
    let payload = &body[sep + 1..];
    if payload == b"?" {
        return None;
    }
    let bytes = decode_base64(payload)?;
    String::from_utf8(bytes).ok()
}

fn find_osc_terminator(buf: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i < buf.len() {
        if buf[i] == b'\x07' {
            return Some((i, i + 1));
        }
        if buf[i] == b'\x1b' && buf.get(i + 1) == Some(&b'\\') {
            return Some((i, i + 2));
        }
        i += 1;
    }
    None
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn keep_possible_prefix_suffix(buf: &mut Vec<u8>, prefix: &[u8]) {
    let keep = prefix.len().saturating_sub(1);
    if buf.len() <= keep {
        return;
    }
    let start = buf.len() - keep;
    let suffix = buf[start..].to_vec();
    buf.clear();
    buf.extend_from_slice(&suffix);
}

fn decode_base64(input: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut quartet = [0u8; 4];
    let mut n = 0;

    for &b in input {
        if b.is_ascii_whitespace() {
            continue;
        }
        quartet[n] = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => 64,
            _ => return None,
        };
        n += 1;
        if n == 4 {
            push_base64_quartet(&mut out, quartet)?;
            n = 0;
        }
    }

    if n > 0 {
        for slot in quartet.iter_mut().skip(n) {
            *slot = 64;
        }
        push_base64_quartet(&mut out, quartet)?;
    }

    Some(out)
}

fn push_base64_quartet(out: &mut Vec<u8>, q: [u8; 4]) -> Option<()> {
    if q[0] == 64 || q[1] == 64 {
        return None;
    }
    out.push((q[0] << 2) | (q[1] >> 4));
    if q[2] != 64 {
        out.push((q[1] << 4) | (q[2] >> 2));
    }
    if q[3] != 64 {
        out.push((q[2] << 6) | q[3]);
    }
    Some(())
}

/// Extract path from OSC 7 escape sequence: \x1b]7;file://HOST/PATH(\x07|\x1b\\)
fn extract_osc7(data: &[u8]) -> Option<PathBuf> {
    let s = std::str::from_utf8(data).ok()?;

    // Look for OSC 7 pattern
    let marker = "\x1b]7;";
    let start = s.find(marker)?;
    let rest = &s[start + marker.len()..];

    // Find the terminator: BEL (\x07) or ST (\x1b\\)
    let end = rest.find('\x07').or_else(|| rest.find("\x1b\\"));

    let uri = &rest[..end?];

    // Parse file:// URI → extract path
    // Formats: file://hostname/path, file:///path, file:///c/Users/...
    if let Some(path_str) = uri.strip_prefix("file://") {
        // Skip hostname part: find the path starting with /
        // file://hostname/path → skip "hostname", take "/path"
        // file:///path → hostname is empty, take "/path"
        let path = if path_str.starts_with('/') {
            // No hostname (file:///path)
            path_str
        } else {
            // Has hostname (file://host/path)
            let slash_pos = path_str.find('/')?;
            &path_str[slash_pos..]
        };

        // On Windows/MSYS2, convert /c/Users/... to C:\Users\...
        #[cfg(windows)]
        {
            let path_bytes = path.as_bytes();
            if path_bytes.len() >= 3
                && path_bytes[0] == b'/'
                && path_bytes[1].is_ascii_alphabetic()
                && path_bytes[2] == b'/'
            {
                let drive = path_bytes[1].to_ascii_uppercase() as char;
                let rest = &path[2..];
                let win_path = format!("{}:{}", drive, rest.replace('/', "\\"));
                return Some(PathBuf::from(win_path));
            }
        }
        return Some(PathBuf::from(path));
    }

    None
}

/// Extract window title from OSC 0 or OSC 2: \x1b]0;TITLE\x07 or \x1b]2;TITLE\x07
fn extract_osc_title(data: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(data).ok()?;
    // Look for OSC 0 or OSC 2
    for marker in &["\x1b]0;", "\x1b]2;"] {
        if let Some(start) = s.find(marker) {
            let rest = &s[start + marker.len()..];
            let end = rest.find('\x07').or_else(|| rest.find("\x1b\\"));
            if let Some(end) = end {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}

/// Returns `true` if `buf` looks like the recently-emitted bytes end with
/// a shell prompt (`$`, `>`, `%`, or `#`), optionally followed by trailing
/// whitespace and CSI/ANSI escape sequences such as color resets.
///
/// This is intentionally conservative: it strips only ANSI CSI sequences
/// (`ESC [ ... <final-byte>`) and trailing ASCII whitespace. False
/// negatives (e.g. exotic prompt styles) only delay startup-command flush
/// by one PTY read cycle. False positives risk firing the startup command
/// against a still-initializing shell.
pub fn is_prompt_ready(buf: &[u8]) -> bool {
    let stripped = strip_csi_escapes(buf);
    let trimmed = trim_ascii_whitespace_end(&stripped);
    let Some(&last) = trimmed.last() else {
        return false;
    };
    if !matches!(last, b'$' | b'>' | b'%' | b'#') {
        return false;
    }
    // Guard against common non-prompt endings that happen to finish
    // with a prompt-like character:
    // - PowerShell / npm-style progress bars: `[====>]` redrawing can
    //   leave `====>` visible mid-frame before the closing bracket.
    // - Percentage readouts: `50%` ends in `%` (zsh's prompt marker).
    // Each guard rejects a specific combination of (last, prev) that is
    // overwhelmingly output, not a prompt.
    if let Some(&prev) = trimmed.get(trimmed.len().saturating_sub(2)) {
        if last == b'>' && matches!(prev, b'=' | b'-' | b'~' | b'.' | b'*') {
            return false;
        }
        if last == b'%' && prev.is_ascii_digit() {
            return false;
        }
    }
    true
}

fn strip_csi_escapes(buf: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(buf.len());
    let mut i = 0;
    while i < buf.len() {
        if buf[i] == 0x1b && i + 1 < buf.len() && buf[i + 1] == b'[' {
            i += 2;
            while i < buf.len() {
                let c = buf[i];
                i += 1;
                if (0x40..=0x7E).contains(&c) {
                    break;
                }
            }
        } else {
            out.push(buf[i]);
            i += 1;
        }
    }
    out
}

fn trim_ascii_whitespace_end(buf: &[u8]) -> &[u8] {
    let mut end = buf.len();
    while end > 0 && matches!(buf[end - 1], b' ' | b'\t' | b'\r' | b'\n') {
        end -= 1;
    }
    &buf[..end]
}

fn title_mentions_client(title: &str, needle: &str) -> bool {
    title.to_ascii_lowercase().contains(needle)
}

/// Detect the appropriate shell to launch.
pub fn detect_shell() -> PathBuf {
    #[cfg(windows)]
    {
        detect_shell_windows()
    }
    #[cfg(not(windows))]
    {
        detect_shell_unix()
    }
}

#[cfg(windows)]
fn detect_shell_windows() -> PathBuf {
    // Try Git Bash first
    let git_bash_paths = [
        r"C:\Program Files\Git\bin\bash.exe",
        r"C:\Program Files (x86)\Git\bin\bash.exe",
    ];

    for path in &git_bash_paths {
        let p = PathBuf::from(path);
        if p.exists() {
            return p;
        }
    }

    // Try bash in PATH
    if let Ok(output) = std::process::Command::new("where").arg("bash").output() {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(line) = stdout.lines().next() {
                let p = PathBuf::from(line.trim());
                if p.exists() {
                    return p;
                }
            }
        }
    }

    // Fallback to PowerShell
    PathBuf::from("powershell.exe")
}

#[cfg(not(windows))]
fn detect_shell_unix() -> PathBuf {
    if let Ok(shell) = std::env::var("SHELL") {
        let p = PathBuf::from(&shell);
        if p.exists() {
            return p;
        }
    }
    PathBuf::from("/bin/sh")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `file:///path` — empty hostname, the path is taken verbatim.
    #[test]
    fn extract_osc7_reads_empty_hostname_form() {
        assert_eq!(
            extract_osc7(b"\x1b]7;file:///tmp/work\x07"),
            Some(PathBuf::from("/tmp/work"))
        );
    }

    /// `file://host/path` — the hostname is skipped from the first
    /// slash on. Also covers the ST (`ESC \`) terminator.
    #[test]
    fn extract_osc7_skips_hostname_before_path() {
        assert_eq!(
            extract_osc7(b"\x1b]7;file://myhost/tmp/work\x1b\\"),
            Some(PathBuf::from("/tmp/work"))
        );
    }

    /// A hostname with no path separator at all has no path to
    /// extract, so the whole sequence is rejected. Pins the branch the
    /// `?` rewrite replaced (clippy::question_mark under Rust 1.97).
    #[test]
    fn extract_osc7_rejects_hostname_without_path() {
        assert_eq!(extract_osc7(b"\x1b]7;file://myhost\x07"), None);
    }

    #[test]
    fn drain_osc52_copies_decodes_bel_terminated_payload() {
        let mut buf = b"\x1b]52;c;aGVsbG8=\x07".to_vec();
        assert_eq!(drain_osc52_copies(&mut buf), vec!["hello"]);
        assert!(buf.is_empty());
    }

    #[test]
    fn drain_osc52_copies_decodes_st_terminated_payload() {
        let mut buf = b"\x1b]52;c;44GT44KT44Gr44Gh44Gv\x1b\\".to_vec();
        assert_eq!(drain_osc52_copies(&mut buf), vec!["こんにちは"]);
        assert!(buf.is_empty());
    }

    #[test]
    fn drain_osc52_copies_handles_split_sequence() {
        let mut buf = b"\x1b]52;c;aGVs".to_vec();
        assert!(drain_osc52_copies(&mut buf).is_empty());
        buf.extend_from_slice(b"bG8=\x07");
        assert_eq!(drain_osc52_copies(&mut buf), vec!["hello"]);
        assert!(buf.is_empty());
    }

    /// End-to-end acceptance for the pane Job Object (renga-trx): a
    /// grandchild that outlives its shell — the shell spawns it
    /// detached (`disown`) and then exits — must still die when the
    /// pane is killed. The legacy `taskkill /F /T` path provably
    /// leaked this shape: the shell was already gone, so the taskkill
    /// branch was skipped and nothing reaped the orphan.
    ///
    /// Liveness is probed through a kernel-enforced exclusive file
    /// lock held by the grandchild (see `win_job::tests` for why
    /// signal-based probes don't work in sandboxed environments).
    #[cfg(windows)]
    #[test]
    fn kill_reaps_grandchild_after_shell_natural_exit() {
        use std::time::{Duration, Instant};

        // The startup command below is bash syntax (`& disown; exit`).
        // On a machine where detect_shell() falls back to PowerShell
        // the command would fail for shell-language reasons, not
        // product reasons — skip rather than report a false negative.
        let shell_name = detect_shell()
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if !shell_name.contains("bash") {
            eprintln!("skipping: test requires a bash pane shell, got {shell_name}");
            return;
        }

        fn wait_for(mut cond: impl FnMut() -> bool, budget: Duration) -> bool {
            let deadline = Instant::now() + budget;
            while Instant::now() < deadline {
                if cond() {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            false
        }

        /// Removes the listed files on drop, so temp artifacts are
        /// cleaned up even when an assertion panics mid-test.
        struct TempFiles(Vec<std::path::PathBuf>);
        impl Drop for TempFiles {
            fn drop(&mut self) {
                for p in &self.0 {
                    let _ = std::fs::remove_file(p);
                }
            }
        }

        let tag = format!("renga-trx-e2e-{}", std::process::id());
        let temp = std::env::temp_dir();
        let lock_path = temp.join(format!("{tag}.lock"));
        let script_path = temp.join(format!("{tag}.ps1"));
        let _cleanup = TempFiles(vec![lock_path.clone(), script_path.clone()]);
        std::fs::write(&lock_path, b"x").expect("create lock file");
        // Forward slashes keep the path inert through bash quoting.
        let lock_fwd = lock_path.display().to_string().replace('\\', "/");
        std::fs::write(
            &script_path,
            format!("$f=[IO.File]::Open('{lock_fwd}','Open','ReadWrite','None'); Start-Sleep 60"),
        )
        .expect("write locker script");
        let script_fwd = script_path.display().to_string().replace('\\', "/");

        let lock_is_held =
            |path: &std::path::Path| std::fs::OpenOptions::new().write(true).open(path).is_err();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut pane = Pane::new(9901, 24, 80, tx).expect("spawn pane");
        // Detach the locker from the shell, then end the shell — the
        // exact "natural exit leaves an orphan" scenario.
        pane.queue_startup_command(&format!(
            "powershell -NoProfile -ExecutionPolicy Bypass -File '{script_fwd}' & disown; exit"
        ));
        assert!(
            wait_for(
                || pane.try_flush_startup().unwrap_or(false),
                Duration::from_secs(30)
            ),
            "shell prompt should be detected and startup command flushed"
        );
        assert!(
            wait_for(|| lock_is_held(&lock_path), Duration::from_secs(30)),
            "grandchild should start and hold the lock"
        );
        // Wait for the shell itself to exit so kill() runs down the
        // already-exited path. Can't use PtyEof here: ConPTY only EOFs
        // the read side once the LAST attached client detaches, and
        // the orphaned powershell keeps the session open by design.
        assert!(
            wait_for(|| pane.child_exited_for_test(), Duration::from_secs(30)),
            "shell should exit after the startup command"
        );

        pane.kill();

        assert!(
            wait_for(|| !lock_is_held(&lock_path), Duration::from_secs(10)),
            "orphaned grandchild should be dead after pane kill"
        );
    }

    #[test]
    fn wheel_report_sgr_up_matches_xterm_format() {
        // xterm SGR wheel up: CSI < 64 ; col ; row M (1-origin coords)
        let bytes = encode_mouse_wheel_report(64, 9, 4, vt100::MouseProtocolEncoding::Sgr);
        assert_eq!(bytes, b"\x1b[<64;10;5M");
    }

    #[test]
    fn wheel_report_sgr_down_matches_xterm_format() {
        let bytes = encode_mouse_wheel_report(65, 0, 0, vt100::MouseProtocolEncoding::Sgr);
        assert_eq!(bytes, b"\x1b[<65;1;1M");
    }

    #[test]
    fn wheel_report_default_encoding_uses_single_byte_plus_32() {
        // Legacy xterm: ESC [ M button+32 col+33 row+33 (1-origin + 32
        // offset = col 0 -> 33, row 0 -> 33).
        let bytes = encode_mouse_wheel_report(64, 0, 0, vt100::MouseProtocolEncoding::Default);
        assert_eq!(bytes, vec![0x1b, b'[', b'M', 96, 33, 33]);
    }

    #[test]
    fn wheel_report_default_truncates_past_223() {
        // coord 300 + 32 offset = 332, clamped to 255 so the legacy
        // byte doesn't wrap. This preserves xterm's well-known cap.
        let bytes = encode_mouse_wheel_report(65, 300, 300, vt100::MouseProtocolEncoding::Default);
        assert_eq!(bytes[0..4], [0x1b, b'[', b'M', 97]);
        assert_eq!(bytes[4], 255);
        assert_eq!(bytes[5], 255);
    }

    #[test]
    fn wheel_report_utf8_multi_byte_for_wide_cols() {
        // col=100 -> 1-origin 101, +32 = 133 (0x85) which must be
        // encoded as 2-byte UTF-8, not a raw 0x85 byte.
        let bytes = encode_mouse_wheel_report(64, 100, 0, vt100::MouseProtocolEncoding::Utf8);
        assert_eq!(bytes[0..4], [0x1b, b'[', b'M', 96]);
        // 133 as UTF-8: 0xC2 0x85
        assert_eq!(bytes[4], 0xC2);
        assert_eq!(bytes[5], 0x85);
        // row=0 -> 1-origin 1, +32 = 33 (0x21), single byte
        assert_eq!(bytes[6], 33);
    }

    // -- encode_mouse_button_report (Issue #52 follow-up: clicks) ----

    #[test]
    fn button_report_sgr_press_terminator_is_capital_m() {
        // SGR press of left button at (col=9, row=4) — the `M`
        // terminator is what distinguishes press/drag from release
        // in the SGR encoding. Button code 0 = left.
        let bytes = encode_mouse_button_report(
            PointerButton::Left,
            PointerAction::Press,
            9,
            4,
            vt100::MouseProtocolEncoding::Sgr,
        );
        assert_eq!(bytes, b"\x1b[<0;10;5M");
    }

    #[test]
    fn button_report_sgr_release_terminator_is_lowercase_m() {
        // SGR release: same button code as press, but lowercase `m`.
        let bytes = encode_mouse_button_report(
            PointerButton::Left,
            PointerAction::Release,
            9,
            4,
            vt100::MouseProtocolEncoding::Sgr,
        );
        assert_eq!(bytes, b"\x1b[<0;10;5m");
    }

    #[test]
    fn button_report_sgr_drag_sets_motion_bit() {
        // SGR drag: button_code + 32 = 32 for left, `M` terminator.
        let bytes = encode_mouse_button_report(
            PointerButton::Left,
            PointerAction::Drag,
            9,
            4,
            vt100::MouseProtocolEncoding::Sgr,
        );
        assert_eq!(bytes, b"\x1b[<32;10;5M");
    }

    #[test]
    fn button_report_sgr_middle_and_right_press() {
        let middle = encode_mouse_button_report(
            PointerButton::Middle,
            PointerAction::Press,
            0,
            0,
            vt100::MouseProtocolEncoding::Sgr,
        );
        assert_eq!(middle, b"\x1b[<1;1;1M");
        let right = encode_mouse_button_report(
            PointerButton::Right,
            PointerAction::Press,
            0,
            0,
            vt100::MouseProtocolEncoding::Sgr,
        );
        assert_eq!(right, b"\x1b[<2;1;1M");
    }

    #[test]
    fn button_report_default_release_collapses_to_button_three() {
        // Legacy encoding: a release of any button is reported as
        // `3` (the xterm-era "no button held" sentinel) + 32 = 35.
        // This is intentionally lossy — the app pairs it with the
        // most recent press to know which physical button lifted.
        let bytes = encode_mouse_button_report(
            PointerButton::Left,
            PointerAction::Release,
            0,
            0,
            vt100::MouseProtocolEncoding::Default,
        );
        assert_eq!(bytes, vec![0x1b, b'[', b'M', 35, 33, 33]);

        let right_release = encode_mouse_button_report(
            PointerButton::Right,
            PointerAction::Release,
            0,
            0,
            vt100::MouseProtocolEncoding::Default,
        );
        assert_eq!(
            right_release, bytes,
            "legacy release must be button-agnostic — right release encodes identically to left"
        );
    }

    #[test]
    fn button_report_default_drag_adds_motion_offset() {
        // Legacy drag: button_code + 32 (motion) + 32 (base offset)
        // = 0 + 32 + 32 = 64 for left-button drag.
        let bytes = encode_mouse_button_report(
            PointerButton::Left,
            PointerAction::Drag,
            0,
            0,
            vt100::MouseProtocolEncoding::Default,
        );
        assert_eq!(bytes, vec![0x1b, b'[', b'M', 64, 33, 33]);
    }

    #[test]
    fn button_report_utf8_wide_coords() {
        // Same UTF-8 boundary case as the wheel test: row coord
        // crossing 0x80 must multi-byte encode.
        let bytes = encode_mouse_button_report(
            PointerButton::Left,
            PointerAction::Press,
            0,
            100,
            vt100::MouseProtocolEncoding::Utf8,
        );
        // Cb = 0 + 32 = 32 for left press
        assert_eq!(bytes[0..4], [0x1b, b'[', b'M', 32]);
        // col=0 -> 1-origin 1, +32 = 33, single byte
        assert_eq!(bytes[4], 33);
        // row=100 -> 1-origin 101, +32 = 133 (0x85), 2-byte UTF-8
        assert_eq!(bytes[5], 0xC2);
        assert_eq!(bytes[6], 0x85);
    }

    #[test]
    fn missing_mouse_mode_can_reuse_recent_codex_cache() {
        assert_eq!(
            resolve_mouse_protocol(
                vt100::MouseProtocolMode::None,
                vt100::MouseProtocolEncoding::Default,
                true,
                Some((
                    vt100::MouseProtocolMode::PressRelease,
                    vt100::MouseProtocolEncoding::Sgr,
                ))
            ),
            Some((
                vt100::MouseProtocolMode::PressRelease,
                vt100::MouseProtocolEncoding::Sgr,
            ))
        );
        assert!(mouse_action_allowed(
            vt100::MouseProtocolMode::PressRelease,
            PointerAction::Press,
        ));
        assert!(mouse_action_allowed(
            vt100::MouseProtocolMode::PressRelease,
            PointerAction::Release,
        ));
    }

    #[test]
    fn missing_mouse_mode_stays_disabled_without_recent_cache() {
        assert_eq!(
            resolve_mouse_protocol(
                vt100::MouseProtocolMode::None,
                vt100::MouseProtocolEncoding::Sgr,
                false,
                Some((
                    vt100::MouseProtocolMode::PressRelease,
                    vt100::MouseProtocolEncoding::Sgr,
                ))
            ),
            None
        );
        assert!(!mouse_action_allowed(
            vt100::MouseProtocolMode::None,
            PointerAction::Press,
        ));
    }

    #[test]
    fn detects_alternate_scroll_enable_and_disable() {
        assert_eq!(detect_alternate_scroll_toggle(b"\x1b[?1007h"), Some(true));
        assert_eq!(detect_alternate_scroll_toggle(b"\x1b[?1007l"), Some(false));
    }

    #[test]
    fn detects_last_alternate_scroll_toggle_in_mixed_stream() {
        assert_eq!(
            detect_alternate_scroll_toggle(b"abc\x1b[?1007hdef\x1b[?1007lghi"),
            Some(false)
        );
    }

    #[test]
    fn codex_skips_arrow_wheel_fallback_even_in_alt_scroll_context() {
        assert!(!should_use_arrow_wheel_fallback(true, true));
        assert!(should_use_arrow_wheel_fallback(true, false));
        assert!(!should_use_arrow_wheel_fallback(false, false));
    }

    #[test]
    fn codex_main_screen_without_scrollback_uses_transcript_fallback() {
        assert!(should_use_codex_main_screen_wheel_fallback(
            true, false, false, 0
        ));
        assert_eq!(
            encode_codex_transcript_wheel_fallback(false, false),
            b"\x14"
        );
        assert_eq!(
            encode_codex_transcript_wheel_fallback(false, true),
            b"\x1b[A\x1b[A\x1b[A"
        );
        assert_eq!(
            encode_codex_transcript_wheel_fallback(true, true),
            b"\x1b[B\x1b[B\x1b[B"
        );
    }

    #[test]
    fn generic_arrow_wheel_fallback_stays_line_oriented() {
        assert_eq!(encode_arrow_wheel_fallback(false), b"\x1b[A\x1b[A\x1b[A");
        assert_eq!(encode_arrow_wheel_fallback(true), b"\x1b[B\x1b[B\x1b[B");
    }

    #[test]
    fn codex_main_screen_with_scrollback_stays_on_host_path() {
        assert!(!should_use_codex_main_screen_wheel_fallback(
            true, false, false, 2
        ));
        assert!(!should_use_codex_main_screen_wheel_fallback(
            false, false, false, 0
        ));
        assert!(!should_use_codex_main_screen_wheel_fallback(
            true, true, false, 0
        ));
        assert!(!should_use_codex_main_screen_wheel_fallback(
            true, false, true, 0
        ));
    }

    #[test]
    fn test_detect_shell_returns_valid_path() {
        let shell = detect_shell();
        assert!(
            !shell.as_os_str().is_empty(),
            "Shell path should not be empty"
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_detect_shell_windows_returns_exe() {
        let shell = detect_shell();
        let ext = shell
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase());
        assert_eq!(ext.as_deref(), Some("exe"), "Windows shell should be .exe");
    }

    #[cfg(not(windows))]
    #[test]
    fn test_detect_shell_unix_uses_shell_env() {
        let shell = detect_shell();
        if let Ok(env_shell) = std::env::var("SHELL") {
            assert_eq!(
                shell,
                PathBuf::from(&env_shell),
                "Should use $SHELL env var"
            );
        }
    }

    // -- is_prompt_ready -------------------------------------------------

    #[test]
    fn prompt_ready_dollar_with_space() {
        assert!(is_prompt_ready(b"user@host:~$ "));
    }

    #[test]
    fn prompt_ready_powershell_chevron() {
        assert!(is_prompt_ready(b"PS C:\\> "));
    }

    #[test]
    fn prompt_ready_zsh_percent() {
        assert!(is_prompt_ready(b"% "));
    }

    #[test]
    fn prompt_ready_root_hash() {
        assert!(is_prompt_ready(b"root@host:/# "));
    }

    #[test]
    fn prompt_not_ready_when_loading() {
        assert!(!is_prompt_ready(b"loading dependencies..."));
    }

    #[test]
    fn prompt_ready_strips_trailing_ansi_color() {
        // Common: prompt char then color reset
        assert!(is_prompt_ready(b"user@host:~$ \x1b[0m"));
    }

    #[test]
    fn prompt_not_ready_for_empty_input() {
        assert!(!is_prompt_ready(b""));
    }

    #[test]
    fn prompt_not_ready_when_only_motd_text() {
        assert!(!is_prompt_ready(b"Welcome to Ubuntu 22.04 LTS"));
    }

    // ─── progress-bar / output misfire guards ────────────────

    #[test]
    fn prompt_not_ready_for_progress_bar_equals_chevron() {
        // Mid-redraw progress bar: `[====>   ]` truncated to `====>`
        // before the closing bracket comes through. Must not trigger.
        assert!(!is_prompt_ready(b"loading [====>"));
    }

    #[test]
    fn prompt_not_ready_for_dashed_progress_chevron() {
        // `--->` style progress marker (common in make-style output).
        assert!(!is_prompt_ready(b"step 3 --->"));
    }

    #[test]
    fn prompt_not_ready_for_asterisk_chevron() {
        assert!(!is_prompt_ready(b"***>"));
    }

    #[test]
    fn prompt_not_ready_for_percentage_readout() {
        // `50%` at end of a progress line should NOT look like a zsh
        // prompt.
        assert!(!is_prompt_ready(b"Downloading... 50%"));
    }

    #[test]
    fn prompt_not_ready_for_hundred_percent() {
        assert!(!is_prompt_ready(b"Done: 100%"));
    }

    #[test]
    fn prompt_ready_powershell_with_real_path_before_chevron() {
        // Regression guard: the previous char in a PowerShell prompt is
        // a letter or path separator (`>` after `e` or `\`), not an
        // ASCII-art character — must still trigger.
        assert!(is_prompt_ready(b"PS C:\\Users\\me>"));
        assert!(is_prompt_ready(b"PS C:\\Users\\me> "));
    }

    #[test]
    fn prompt_ready_zsh_percent_after_space() {
        // Bare `%` preceded by whitespace stays a valid zsh prompt.
        assert!(is_prompt_ready(b"user ~/dir % "));
    }

    #[test]
    fn title_mentions_client_matches_case_insensitively() {
        assert!(title_mentions_client("Codex - review mode", "codex"));
        assert!(title_mentions_client("CLAUDE /company", "claude"));
        assert!(!title_mentions_client("bash", "codex"));
    }
}
