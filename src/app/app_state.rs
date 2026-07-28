use super::*;

/// Commands that flow from the IPC server thread into the App's event
/// loop. Each variant carries a `oneshot::Sender` so the server thread
/// can block-wait for the App to finish processing.
#[allow(dead_code)] // constructed by the IPC server (wired in Step 3.3)
#[derive(Debug)]
pub enum AppCommand {
    /// Snapshot the pane list of the active workspace.
    List {
        reply: oneshot::Sender<Vec<PaneInfo>>,
    },
    /// Write `data` to the target pane's PTY.
    Send {
        target: PaneRef,
        data: Vec<u8>,
        append_enter: bool,
        reply: oneshot::Sender<std::result::Result<(), ipc::CodedError>>,
    },
    /// Move keyboard focus to the target pane in the active workspace.
    Focus {
        target: PaneRef,
        reply: oneshot::Sender<std::result::Result<(), ipc::CodedError>>,
    },
    /// Split the target pane. If `command` is given, it's queued on the
    /// new pane and flushed when its shell prompt appears. If `name` is
    /// given, it's registered so later IPC calls can address the pane by
    /// name. Returns the new pane's id on success.
    Split {
        target: PaneRef,
        direction: ipc::Direction,
        command: Option<String>,
        name: Option<String>,
        role: Option<String>,
        cwd: Option<String>,
        reply: oneshot::Sender<std::result::Result<usize, ipc::CodedError>>,
    },
    /// Open a new tab with a fresh single pane. Focus switches to the
    /// new tab (mirrors the Alt+T keybinding). Returns the new pane's
    /// id on success.
    NewTab {
        command: Option<String>,
        name: Option<String>,
        label: Option<String>,
        role: Option<String>,
        cwd: Option<String>,
        reply: oneshot::Sender<std::result::Result<usize, ipc::CodedError>>,
    },
    /// Snapshot the visible screen of the target pane. See
    /// [`ipc::Request::Inspect`] for the response shape.
    Inspect {
        target: PaneRef,
        lines: Option<usize>,
        include_cursor: bool,
        reply: oneshot::Sender<std::result::Result<serde_json::Value, ipc::CodedError>>,
    },
    /// Close the target pane. Returns the id of the pane that was
    /// closed, so the caller can confirm which pane was resolved.
    Close {
        target: PaneRef,
        reply: oneshot::Sender<std::result::Result<usize, ipc::CodedError>>,
    },
    /// List peers visible to `from_pane` — every other pane in the
    /// same workspace. Drives the MCP peer subprocess's `list_peers`
    /// tool.
    PeerList {
        from_pane: usize,
        reply: oneshot::Sender<std::result::Result<Vec<PeerInfo>, ipc::CodedError>>,
    },
    /// Route a peer message from `from_pane` to `target`, provided
    /// both live in the same workspace. Emits `Event::PeerInbox` on
    /// the event bus so a subscribed MCP subprocess can push it out
    /// as a `notifications/claude/channel` frame. Cross-tab targets
    /// are silently accepted and dropped (no-op success) — v1 does
    /// not expose cross-tab routing; callers cannot distinguish
    /// "dropped" from "unknown peer" on purpose.
    PeerSend {
        from_pane: usize,
        target: PaneRef,
        body: String,
        reply: oneshot::Sender<std::result::Result<(), ipc::CodedError>>,
    },
    /// Publish the MCP client kind currently attached to a pane so
    /// peer/pane listings can surface push-vs-pull receive behavior.
    PeerRegisterClient {
        pane_id: usize,
        kind: PeerClientKind,
        reply: oneshot::Sender<std::result::Result<(), ipc::CodedError>>,
    },
    /// Rename or clear the `name` / `role` of an existing pane. See
    /// [`ipc::Request::SetPaneIdentity`] for the three-state semantics
    /// of each field. Success returns the pane's updated [`PaneInfo`]
    /// so callers can confirm the new identity without a separate
    /// `List` round-trip.
    SetPaneIdentity {
        target: PaneRef,
        name: Option<Option<String>>,
        role: Option<Option<String>>,
        reply: oneshot::Sender<std::result::Result<PaneInfo, ipc::CodedError>>,
    },
    /// Set or clear the summary string of a specific pane. Used by the
    /// MCP `set_summary` tool — `pane_id` is the caller pane resolved
    /// from `RENGA_PANE_ID`. Returns the updated [`PaneInfo`] so the
    /// caller can confirm without a separate `List` round-trip.
    SetSummary {
        pane_id: usize,
        summary: String,
        reply: oneshot::Sender<std::result::Result<PaneInfo, ipc::CodedError>>,
    },
}

/// Events dispatched within the app.
pub enum AppEvent {
    /// PTY output received for a pane.
    PtyOutput(#[allow(dead_code)] usize),
    /// A pane emitted OSC 52 with clipboard text.
    ClipboardCopy(String),
    /// PTY process exited for a pane.
    PtyEof(usize),
    /// Shell changed working directory (pane_id, new path).
    CwdChanged(usize, PathBuf),
}

/// Flag-preloaded launch command for `renga split --role claude` and
/// Alt+P. Also consumed by `crate::mcp_peer` so `spawn_pane` /
/// `new_tab` upgrade a bare `claude` invocation to the peer-enabled
/// form, mirroring what Alt+P types into the focused pane.
///
/// Kept as a string (not a shell-escaped arg vector) because the pane
/// startup-command path feeds it through the shell, which handles the
/// `--dangerously-load-development-channels` spelling uniformly across
/// bash / zsh / pwsh.
pub(crate) const CLAUDE_PEER_LAUNCH_CMD: &str =
    "claude --dangerously-load-development-channels server:renga-peers";

pub struct App {
    pub workspaces: Vec<Workspace>,
    pub active_tab: usize,
    pub should_quit: bool,
    pub event_tx: Sender<AppEvent>,
    pub event_rx: Receiver<AppEvent>,
    /// Clonable sender for the IPC server thread. Drop the server thread
    /// to stop producing commands; the receiver lives on the App side.
    pub command_tx: Sender<AppCommand>,
    pub(crate) command_rx: Receiver<AppCommand>,
    pub(crate) next_pane_id: usize,
    pub dirty: bool,
    pub paste_cooldown: u8, // frames to skip rendering after paste
    /// Frames to skip rendering after a layout change (split, close,
    /// sidebar toggle, terminal resize). Gives Claude Code / bash time
    /// to process SIGWINCH and send a fresh redraw before we paint,
    /// avoiding the brief "old buffer at new size" garbled frame.
    pub resize_cooldown: u8,
    /// Last known terminal size (cols, rows). Updated from main.rs on
    /// Event::Resize and from ui::render on every frame. Used by
    /// `relayout_panes()` so layout-change handlers can resize PTYs
    /// without needing a Frame reference.
    pub last_term_size: (u16, u16),
    /// Final hardware-caret position deferred for the main loop to apply
    /// AFTER `terminal.draw`, used only on conpty (Windows / WSL). ratatui's
    /// frame-end cursor handling re-shows the cursor at its stale post-paint
    /// position before moving it, leaking a one-frame caret flicker onto the
    /// Claude spinner row through conpty (#260). On those targets `ui::render`
    /// stashes the caret here instead of calling `frame.set_cursor_position`,
    /// so the loop can `MoveTo` then `Show` while the cursor is still hidden.
    /// Always `None` on non-conpty targets (the original `set_cursor_position`
    /// path is preserved there — see #253). Refreshed every render.
    pub deferred_caret: Option<(u16, u16)>,
    // Shared settings
    pub file_tree_width: u16,
    pub preview_width: u16,
    // Layout: swap preview and terminal positions
    pub layout_swapped: bool,
    // Toggle status bar visibility (Alt+S)
    pub status_bar_visible: bool,
    // Drag/hover state
    pub dragging: Option<DragTarget>,
    pub hover_border: Option<DragTarget>,
    // Tab bar rects for mouse click
    pub last_tab_rects: Vec<(usize, Rect)>,
    pub last_new_tab_rect: Option<Rect>,
    /// Active tab rename input buffer. When `Some`, key input is
    /// routed to this buffer instead of the focused PTY; Enter commits
    /// to the active workspace's `custom_name`, Esc cancels.
    pub rename_input: Option<String>,
    /// IME composition overlay. When `Some`, key input is routed into
    /// this buffer instead of the focused PTY; the overlay draws a
    /// centered multi-line composition box on top of the pane area
    /// so the host terminal's IME candidate window has a concrete
    /// text-input widget to anchor to (Issue #25 / Phase 4b). `Enter`
    /// inserts a newline; `Alt+Enter` / `Ctrl+Enter` commits the
    /// composed text to the target pane via the existing
    /// bracketed-paste path; `Esc` / `Ctrl+C` cancels.
    pub overlay: Option<OverlayState>,
    /// Saved IME overlay drafts keyed by target pane. Closing the
    /// overlay temporarily stashes the draft here so reopening on the
    /// same pane can resume composition.
    pub(crate) saved_overlay_drafts: HashMap<usize, OverlayState>,
    /// (tab index, timestamp) of the last left-click on a tab label.
    /// Used to detect a double-click → enter rename mode.
    pub(crate) last_tab_click: Option<(usize, Instant)>,
    /// (tab index, column, row, timestamp) of the last left-click on
    /// a pane outer edge cell. Used to detect an outer-edge
    /// double-click → split the underlying pane in that direction.
    /// `active_tab` is part of the key so a tab switch within the
    /// 500 ms window doesn't promote the first click on the new tab's
    /// matching cell into a phantom double-click. Cleared on any
    /// other left-click path (tab/new-tab/file-tree/preview/inner
    /// pane cell) so unrelated clicks reset the timer.
    pub(crate) last_edge_click: Option<(usize, u16, u16, Instant)>,
    /// (tab index, column, row, timestamp) of the last left-click that
    /// landed on a shared internal split boundary. Mirrors
    /// [`Self::last_edge_click`] but for the divider between sibling
    /// panes: a second click on the same divider cell within 500 ms
    /// double-clicks it and splits the adjacent pane (Issue #247),
    /// while a single click still falls through to the resize-drag.
    /// Cleared on the same unrelated click paths as `last_edge_click`.
    pub(crate) last_boundary_click: Option<(usize, u16, u16, Instant)>,
    // Text selection
    pub selection: Option<TextSelection>,
    // Version check (background)
    pub version_info: crate::version_check::VersionInfo,
    // Claude Code JSONL monitoring
    pub claude_monitor: crate::claude_monitor::ClaudeMonitor,
    /// Runtime metadata published by connected MCP peer subprocesses.
    /// Keyed by pane id so `list_peers` / `list_panes` can surface
    /// whether a pane is using Claude-style push or Codex-style poll.
    pub(crate) peer_client_kinds: HashMap<usize, PeerClientKind>,
    /// One-shot nudges waiting to be injected into Codex panes so the
    /// pane runs `check_messages` once it looks ready for PTY input.
    pub(crate) pending_codex_peer_messages: HashMap<usize, VecDeque<PendingCodexPeerDelivery>>,
    /// Focused Codex panes show a local notification overlay instead
    /// of receiving an immediate PTY nudge.
    pub(crate) codex_peer_notification: Option<CodexPeerNotificationState>,
    /// Recently delivered peer messages, keyed by
    /// `(target_pane, from_pane, body)`, with the timestamp of last
    /// delivery. Used by `handle_peer_send` to drop duplicate
    /// re-sends arriving within `PEER_SEND_DEDUPE_TTL` so a noisy
    /// dispatcher / worker can't paper the receiver's transcript with
    /// phantom user-turns. See renga#221 acceptance criterion #2.
    pub(crate) recent_peer_sends: HashMap<(usize, usize, String), Instant>,
    // Reusable clipboard handle (lazy-initialized)
    pub(crate) clipboard: Option<arboard::Clipboard>,
    // Pane lifecycle event bus shared with IPC subscribers.
    pub event_bus: crate::ipc::EventBus,
    /// IME overlay mode resolved from config + CLI. `Off` disables
    /// the Ctrl+; hotkey so the keystroke reaches the PTY untouched.
    pub ime_mode: crate::config::ImeMode,
    /// When `true`, PTY-output-driven repaints are suppressed while
    /// the IME composition overlay is open (Issue #37 / #82 Phase 2).
    /// Populated from config + CLI via [`App::apply_config`]; consumed
    /// by [`App::drain_pty_events`]. State-changing events (pane exit,
    /// cwd update) still repaint because those affect non-pane UI
    /// (tab labels, sidebar).
    pub ime_freeze_panes_on_overlay: bool,
    /// Resolved UI language for status bar hints and preview error
    /// messages. `App::apply_config` collapses `[ui] lang`, `--lang`,
    /// and OS locale detection into this single value so renderers
    /// can dereference `app.messages()` without caring about the
    /// precedence chain.
    pub lang: crate::i18n::Lang,
    /// When freeze is enabled, optionally force a single repaint every
    /// `ime_overlay_catchup_ms` milliseconds so the user sees body-
    /// content progress periodically without the flicker of live
    /// repaints. `0` disables the periodic catch-up (pure freeze,
    /// matches the original Phase 2 behavior). Clamped to
    /// `MIN_OVERLAY_CATCHUP_MS` at apply time when non-zero to avoid
    /// a tight repaint loop.
    pub ime_overlay_catchup_ms: u64,
    /// Rows the focused pane's caret is pushed down before it is handed
    /// to the host as the native IME anchor (Issue #281). Resolved from
    /// `[ime] anchor_row_offset` + `--ime-anchor-row-offset` by
    /// [`App::apply_config`] and already clamped to
    /// [`crate::config::MAX_IME_ANCHOR_ROW_OFFSET`]. Consumed by
    /// `ui::render_panes`, which additionally drops it to `0` on hosts
    /// that don't anchor an IME to the caret.
    pub ime_anchor_row_offset: u16,
    /// Instant of the last overlay-era repaint (open or catch-up
    /// tick). Populated by [`App::maybe_tick_overlay_catchup`] and
    /// cleared when the overlay closes. `None` outside an overlay
    /// session.
    pub(crate) last_overlay_repaint: Option<Instant>,
    /// Minimum width (cols) each child must retain after a vertical
    /// split. Populated from `--min-pane-width`; `0` is clamped to `1`
    /// in `set_min_pane_size` to avoid degenerate halving math.
    /// Private — the setter is the only supported entry point so the
    /// clamp invariant cannot be bypassed.
    pub(crate) min_pane_width: u16,
    /// Minimum height (rows) each child must retain after a horizontal
    /// split. See [`App::set_min_pane_size`] for the clamp rule.
    pub(crate) min_pane_height: u16,
    /// Image preview protocol picker (upstream sync, PR #7). `None`
    /// when the host terminal exposes no supported graphics protocol
    /// (Sixel / Kitty / iTerm2 / halfblocks) — in that case image
    /// files fall back to the textual "binary file" placeholder in
    /// the preview panel.
    pub image_picker: Option<ratatui_image::picker::Picker>,
    /// First-launch macOS tip: when `true`, a 2-row banner above the
    /// status bar points users at the Option-as-Meta README section
    /// so `Alt+T` / `Alt+P` / `Alt+1..9` actually fire (see
    /// `crate::macos_tip`). Dismissed by any key press or the
    /// 10-second auto timeout; dismissal is persisted via the
    /// zero-byte marker file resolved by `macos_tip::marker_path`.
    pub macos_tip_visible: bool,
    /// Instant the banner was shown; `None` outside a banner session.
    /// Consumed by [`App::check_macos_tip_timeout`] every frame.
    pub(crate) macos_tip_shown_at: Option<Instant>,
    /// Marker path to touch on dismissal. `None` when the config dir
    /// couldn't be resolved — dismissal stays in-memory for this run.
    pub(crate) macos_tip_marker: Option<PathBuf>,
}
