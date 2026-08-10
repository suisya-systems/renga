use super::*;

/// Commands that flow from the IPC server thread into the App's event
/// loop. Each variant carries a `oneshot::Sender` so the server thread
/// can block-wait for the App to finish processing.
#[allow(dead_code)] // constructed by the IPC server (wired in Step 3.3)
#[derive(Debug)]
pub enum AppCommand {
    /// Snapshot the pane list of the caller's workspace — the active
    /// workspace when `from_pane` is `None` (legacy CLI semantics), the
    /// workspace owning `from_pane` otherwise — or of the tab(s) named
    /// by `tab` (Issue #329). See [`ipc::Request`]'s caller-tab scoping
    /// notes.
    List {
        from_pane: Option<usize>,
        tab: Option<ipc::ListTabSelector>,
        reply: oneshot::Sender<std::result::Result<Vec<PaneInfo>, ipc::CodedError>>,
    },
    /// Write `data` to the target pane's PTY. `from_pane` scopes target
    /// resolution; see [`ipc::Request`].
    Send {
        target: PaneRef,
        data: Vec<u8>,
        append_enter: bool,
        from_pane: Option<usize>,
        reply: oneshot::Sender<std::result::Result<(), ipc::CodedError>>,
    },
    /// Move keyboard focus to the target pane. Resolving to a pane in a
    /// non-visible tab also switches the visible tab — focus that the
    /// keyboard cannot reach is not focus.
    Focus {
        target: PaneRef,
        from_pane: Option<usize>,
        reply: oneshot::Sender<std::result::Result<(), ipc::CodedError>>,
    },
    /// Split the target pane. If `command` is given, it's queued on the
    /// new pane and flushed when its shell prompt appears. If `name` is
    /// given, it's registered so later IPC calls can address the pane by
    /// name. Returns the new pane's id on success. The split lands in
    /// the *target's* workspace, which `from_pane` scopes; a split in a
    /// non-visible tab leaves the visible tab's layout untouched.
    Split {
        target: PaneRef,
        direction: ipc::Direction,
        command: Option<String>,
        name: Option<String>,
        role: Option<String>,
        cwd: Option<String>,
        from_pane: Option<usize>,
        /// Tab hosting the split (Issue #290). `None` = prior behavior
        /// (the target resolves in the caller's tab). See
        /// [`ipc::Request::Split`].
        tab: Option<ipc::TabSelector>,
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
    /// Spawn a fresh single-pane tab in the **background** — the active
    /// tab does not change (Issue #290, the `tab: {new: …}` selector of
    /// the MCP `spawn_*` tools). The new tab's geometry is finalized
    /// (rects + PTY resize) before the reply is sent. Returns the new
    /// pane's id and the new tab's 0-based index.
    SpawnTab {
        command: Option<String>,
        name: Option<String>,
        label: Option<String>,
        role: Option<String>,
        cwd: Option<String>,
        from_pane: Option<usize>,
        reply: oneshot::Sender<std::result::Result<(usize, usize), ipc::CodedError>>,
    },
    /// Snapshot the visible screen of the target pane. See
    /// [`ipc::Request::Inspect`] for the response shape.
    Inspect {
        target: PaneRef,
        lines: Option<usize>,
        include_cursor: bool,
        from_pane: Option<usize>,
        reply: oneshot::Sender<std::result::Result<serde_json::Value, ipc::CodedError>>,
    },
    /// Close the target pane. Returns the id of the pane that was
    /// closed, so the caller can confirm which pane was resolved.
    /// `from_pane` scopes `Focused` / `Name` to the caller's tab
    /// (Issue #296); `None` keeps the pre-#296 all-workspace search.
    Close {
        target: PaneRef,
        from_pane: Option<usize>,
        reply: oneshot::Sender<std::result::Result<usize, ipc::CodedError>>,
    },
    /// List peers visible to `from_pane` — every other pane in every
    /// workspace, caller's tab first (Issue #289). Drives the MCP peer
    /// subprocess's `list_peers` tool.
    PeerList {
        from_pane: usize,
        reply: oneshot::Sender<std::result::Result<Vec<PeerInfo>, ipc::CodedError>>,
    },
    /// Route a peer message from `from_pane` to `target` — cross-tab
    /// targets deliver like same-tab ones since Issue #289. Numeric
    /// ids resolve across all tabs; names stay inside the sender's
    /// workspace. Emits `Event::PeerInbox` on the event bus so a
    /// subscribed MCP subprocess can push it out as a
    /// `notifications/claude/channel` frame. Unresolvable targets fail
    /// with `pane_not_found`.
    PeerSend {
        from_pane: usize,
        target: PaneRef,
        body: String,
        reply: oneshot::Sender<std::result::Result<(), ipc::CodedError>>,
    },
    /// Deliver `body` to `target` as a real **user turn** (Issue #323):
    /// type it into the recipient agent's composer and submit it, so
    /// slash commands actually arm. Target resolution is identical to
    /// [`AppCommand::PeerSend`]; everything after it differs.
    ///
    /// The reply is deferred: the App parks this `reply` in
    /// [`App::pending_user_turns`] and answers from
    /// `flush_pending_user_turns` once the settle → Enter → observe
    /// sequence reaches a terminal state. Never emits
    /// `Event::PeerInbox` — a user turn must not also arrive as a
    /// channel tag.
    PeerSendUserTurn {
        from_pane: usize,
        target: PaneRef,
        body: String,
        reply: oneshot::Sender<std::result::Result<serde_json::Value, ipc::CodedError>>,
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
    /// `from_pane` scopes `Focused` / `Name` to the caller's tab
    /// (Issue #296); `None` keeps the pre-#296 all-workspace search.
    SetPaneIdentity {
        target: PaneRef,
        name: Option<Option<String>>,
        role: Option<Option<String>>,
        from_pane: Option<usize>,
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

/// Pending Ctrl+W close confirmation (Issue #285).
///
/// The variants pin down *what* the user asked to close at request
/// time. Deliberately **not** expressed as "the focused pane" / "the
/// active tab": between the request and the `y` keystroke an MCP
/// client can move focus, close panes, split, or shift tab indices, so
/// re-reading `focused_pane_id` / `active_tab` on confirm could destroy
/// something the user never looked at. Everything needed to re-find
/// (and re-validate) the original target is captured here instead.
///
/// This is *only* reachable from the TUI key path. The MCP
/// `close_pane` tool keeps going straight through
/// [`App::handle_close`] — automation must never block on a human
/// keystroke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CloseConfirm {
    /// Close one pane out of a multi-pane tab.
    Pane { pane_id: usize },
    /// Close a whole tab (Ctrl+W on a tab that holds a single pane).
    ///
    /// `anchor_pane_id` re-locates the workspace without trusting the
    /// tab index, which shifts whenever an earlier tab closes.
    /// `expected_pane_ids` is the sorted pane-id snapshot taken at
    /// request time: if an MCP `split_pane` grew the tab while the
    /// prompt was up, confirming would silently destroy panes the user
    /// never saw, so the mismatch cancels instead.
    Tab {
        anchor_pane_id: usize,
        expected_pane_ids: Vec<usize>,
    },
}

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
    /// Pending Ctrl+W close confirmation. When `Some`, a centered
    /// modal is drawn and **every** key / paste / mouse event is
    /// consumed by the confirmation handler — nothing reaches the PTY
    /// (Ctrl+Q remains the one escape hatch, checked before this).
    /// See [`CloseConfirm`] for why the target is pinned rather than
    /// re-derived from focus on confirm.
    pub(crate) close_confirm: Option<CloseConfirm>,
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
    /// In-flight `deliver="user_turn"` deliveries (Issue #323), each
    /// holding the IPC reply channel it will answer once its
    /// settle → Enter → observe sequence finishes. Driven once per
    /// frame by [`App::flush_pending_user_turns`]; the App never
    /// sleeps on one.
    pub(crate) pending_user_turns: Vec<PendingUserTurn>,
    /// Dedupe ledger for user-turn deliveries, keyed like
    /// [`Self::recent_peer_sends`] but deliberately **separate** from
    /// it: a channel message and a user turn are different intentional
    /// operations, so an earlier `<channel>` report must not swallow a
    /// later `/loop`. An entry is recorded only once readiness has
    /// passed and bytes are about to be written, so a refusal leaves no
    /// trace and an identical retry gets through.
    pub(crate) recent_user_turn_sends: HashMap<(usize, usize, String), Instant>,
    /// Every byte string the user-turn path has written to a PTY, in
    /// order, as `(pane_id, bytes)`.
    ///
    /// Test-only, and it earns its place: "a refusal writes nothing" is
    /// the guarantee that makes every refusal safe to retry, and it is
    /// unobservable from outside — a real pane's PTY swallows the bytes
    /// with no way to read them back. Without this, a refactor that
    /// wrote the body before the readiness check would leave the whole
    /// suite green.
    #[cfg(test)]
    pub(crate) user_turn_writes: Vec<(usize, Vec<u8>)>,
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

    // ─── Org sidebar (Issue #291) ─────────────────────────
    //
    // All of this lives on `App`, not `Workspace`, because the panel is
    // a *cross-tab* view: it lists every tab at once, so per-tab copies
    // of its scroll position and selection would fight each other on
    // every tab switch. `FocusTarget::OrgSidebar` is the one piece that
    // stays per-workspace, since focus is inherently per-tab — see
    // [`App::switch_tab`] for how sidebar focus is carried across.
    /// Resolved `[ui] org_sidebar` mode. `Off` disables the panel and
    /// its toggle key outright.
    pub org_sidebar_mode: crate::config::OrgSidebarMode,
    /// Runtime visibility toggle (Ctrl+B). Meaningless when the mode is
    /// `Off`; gate on [`App::org_sidebar_active`] rather than reading
    /// this directly.
    pub org_sidebar_visible: bool,
    /// User-resized width, clamped to `ORG_SIDEBAR_MIN_WIDTH..=MAX` by
    /// the layout helper. This is the *requested* width — the effective
    /// one can be smaller when the degrade ladder forces compact mode.
    pub org_sidebar_width: u16,
    /// Cached rect from the last paint, used for mouse hit-testing.
    /// `None` when the panel was not painted (toggled off, or squeezed
    /// out by a narrow terminal).
    pub(crate) last_org_sidebar_rect: Option<Rect>,
    /// First visible row index.
    pub(crate) org_sidebar_scroll: usize,
    /// Keyboard selection, stored as `(tab, pane)` rather than a row
    /// index so it survives tabs and panes appearing or disappearing.
    pub(crate) org_sidebar_selection: Option<org_sidebar::OrgSidebarTarget>,
    /// Click-target list published by the renderer, indexed by row.
    /// Cleared whenever the tab set changes.
    pub(crate) org_sidebar_row_targets: Vec<org_sidebar::OrgSidebarTarget>,
    /// Set when something moved the selection, so the next paint scrolls
    /// it back into view. Without the flag the renderer would re-anchor
    /// the view on the selection every frame and the mouse wheel could
    /// never move the panel.
    pub(crate) org_sidebar_follow_selection: bool,
    /// Display-only Claude state, one entry per live pane, refreshed on
    /// a timer by [`App::tick_claude_snapshots`] instead of per frame.
    /// Painting the sidebar straight from `ClaudeMonitor::state()` would
    /// clone a `Vec<TodoItem>` and several `String`s for every pane of
    /// every tab on every frame; the snapshot is a small `PartialEq`
    /// value so the tick can also tell when nothing actually changed.
    pub(crate) claude_snapshots: HashMap<usize, crate::claude_monitor::ClaudeSnapshot>,
    /// Throttle for the snapshot sweep itself, so the cross-tab walk
    /// runs a few times a second rather than once per event-loop turn.
    pub(crate) last_claude_sweep: Option<Instant>,
}
