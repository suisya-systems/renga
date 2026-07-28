mod app;
mod claude_monitor;
mod cli;
mod config;
mod filetree;
mod i18n;
mod input;
mod ipc;
mod layout_config;
mod macos_tip;
mod mcp_peer;
mod pane;
mod preview;
mod ui;
mod version_check;
#[cfg(windows)]
mod win_job;

use std::io;
use std::panic;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

fn main() -> Result<()> {
    // Parse CLI args. clap handles --help / --version and exits cleanly
    // before we enter raw mode below.
    let cli = cli::Cli::parse();
    cli.validate_exec()?;

    // Phase 3: subcommands (`renga list`, `renga send`, …) are IPC
    // clients and MUST be runnable from inside a renga pane — that's
    // the whole point. Dispatch them before the nested-TUI guard kicks
    // in, so the `RENGA=1` env var set by the parent doesn't block
    // legitimate client invocations.
    //
    // `mcp-peer` and `mcp {install,uninstall,status}` are exceptions:
    // the first is a stdio MCP server (not an IPC request) and the
    // second shells out to a client MCP CLI. Route both directly to
    // their handlers before the shared IPC dispatcher.
    if let Some(cmd) = cli.command.as_ref() {
        match cmd {
            cli::IpcCommand::McpPeer => return mcp_peer::run(),
            cli::IpcCommand::Mcp { action } => return mcp_peer::install::run(action),
            _ => return run_ipc_client(cmd),
        }
    }

    // No subcommand: we're about to launch another TUI. Refuse if we're
    // already inside a renga pane, since nesting vt100 parsers in
    // vt100 parsers produces unreadable output and confuses the mouse.
    if std::env::var("RENGA").is_ok() {
        eprintln!("renga: already running inside a renga pane (nested instance not allowed).");
        eprintln!(
            "       Open a new tab with Alt+T (or Ctrl+T) or split with Ctrl+D / Ctrl+E instead."
        );
        std::process::exit(1);
    }

    // If a directory is passed as argument, cd into it first
    if let Some(dir) = &cli.dir {
        if dir.is_dir() {
            std::env::set_current_dir(dir)?;
        } else {
            eprintln!("renga: not a directory: {}", dir.display());
            std::process::exit(1);
        }
    }

    // Install panic hook to restore terminal state on crash
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), crossterm::event::DisableMouseCapture);
        let _ = execute!(io::stdout(), crossterm::event::DisableBracketedPaste);
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        default_hook(info);
    }));

    // Query terminal for graphics protocol support BEFORE raw mode.
    // Falls back to halfblocks if detection fails.
    let image_picker = Some(
        ratatui_image::picker::Picker::from_query_stdio()
            .unwrap_or_else(|_| ratatui_image::picker::Picker::halfblocks()),
    );

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    execute!(stdout, crossterm::event::EnableMouseCapture)?;
    execute!(stdout, crossterm::event::EnableBracketedPaste)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Get initial terminal size
    let size = terminal.size()?;

    // Phase 3: start the IPC server BEFORE spawning child PTYs so the
    // first `RENGA_SOCKET` value children see is the real one. Children
    // inherit env from this process (via portable-pty's CommandBuilder),
    // and we publish `RENGA` as a "you're inside renga" flag here too.
    let our_pid = std::process::id();
    // Endpoint resolution can fail on Unix if we can't create the
    // owner-only socket directory (read-only FS, permission-constrained
    // mount, …). IPC is non-essential — fall through without it so the
    // TUI still works as a plain multiplexer, mirroring the IpcServer
    // soft-fail path below.
    let ipc_endpoint = match ipc::endpoint::endpoint_for_pid(our_pid) {
        Ok(ep) => Some(ep),
        Err(e) => {
            eprintln!("renga: IPC endpoint unavailable ({e}); external commands disabled.");
            None
        }
    };
    if let Some(ep) = &ipc_endpoint {
        std::env::set_var(ipc::endpoint::ENV_SOCKET, ep.as_str());
    }
    std::env::set_var("RENGA", "1");

    // Session token derived from the process's start nanoseconds so a
    // client connecting through a stale socket file whose PID got
    // re-used cannot be silently fooled — the server echoes this token
    // on hello, and the client verifies it against `RENGA_TOKEN`.
    let session_token = format!(
        "{}-{}",
        our_pid,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    // Publish the token before spawning panes so children inherit it.
    std::env::set_var(ipc::endpoint::ENV_TOKEN, &session_token);

    // Load user config + apply CLI override (CLI > file > default).
    let mut user_config = config::Config::load();
    user_config.apply_cli_overrides(
        cli.ime,
        cli.ime_freeze_panes,
        cli.ime_overlay_catchup_ms,
        cli.ime_anchor_row_offset,
        cli.lang,
        cli.fps,
    );
    let event_poll_timeout = Duration::from_secs_f64(1.0 / f64::from(user_config.ui.fps));

    // If a layout was requested and its root node is a single pane
    // with an explicit cwd, pre-load the layout so we can spawn the
    // initial pane in that directory instead of the process cwd.
    // Keep the full `apply_layout` call below — this is just a
    // bootstrap detail for the root leaf.
    let preloaded_layout: Option<layout_config::LayoutConfig> = match cli.layout.as_deref() {
        Some(name) => Some(layout_config::LayoutConfig::load(name)?),
        None => None,
    };
    let initial_cwd = preloaded_layout
        .as_ref()
        .and_then(|cfg| cfg.root_pane_cwd())
        .map(|s| {
            let p = std::path::PathBuf::from(s);
            if p.is_absolute() {
                p
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| std::path::PathBuf::from("."))
                    .join(p)
            }
        });

    // Create app (spawns the initial pane, which captures the env above).
    let mut app = app::App::new_with_cwd(size.height, size.width, initial_cwd)?;
    app.apply_config(&user_config);
    app.set_min_pane_size(cli.min_pane_width, cli.min_pane_height);
    app.image_picker = image_picker;

    // First-launch macOS Option-as-Meta tip. Gated on host OS + a
    // zero-byte marker file so returning users never see it twice.
    // Non-macOS hosts and already-dismissed users short-circuit to
    // false here.
    let tip_marker = macos_tip::marker_path();
    if macos_tip::should_show(cli.no_macos_tip, cli.show_macos_tip, tip_marker.as_deref()) {
        app.show_macos_tip(tip_marker);
    }

    // Keep the server handle alive for the process lifetime; its Drop
    // impl cleans up the Unix socket file on exit.
    let _ipc_server = match ipc_endpoint.clone() {
        Some(endpoint) => match ipc::server::IpcServer::spawn(
            endpoint,
            app.command_tx.clone(),
            session_token.clone(),
            app.event_bus.clone(),
        ) {
            Ok(server) => Some(server),
            Err(e) => {
                // IPC is non-essential for the TUI itself — fail soft so users
                // without the required socket permissions can still use renga
                // as a plain multiplexer.
                eprintln!("renga: IPC server failed to start ({e}); external commands disabled.");
                None
            }
        },
        None => None,
    };

    // Phase 1 (--exec): queue the requested command on the initial focused
    // pane. The command will be flushed into the PTY by the main event
    // loop once the shell prompt is ready (see `try_flush_startup`).
    if let Some(cmd) = cli.exec.as_deref() {
        let focused_id = app.ws().focused_pane_id;
        if let Some(pane) = app.ws_mut().panes.get_mut(&focused_id) {
            pane.queue_startup_command(cmd);
        }
    }

    // Phase 2 (--layout): expand a multi-pane layout from a TOML file.
    // Each leaf pane's command (if any) is queued via the same Phase 1
    // mechanism so all panes flush once their shells are ready.
    if let Some(cfg) = preloaded_layout.as_ref() {
        app.apply_layout(cfg)?;
    }

    // Main event loop
    let result = run_event_loop(&mut terminal, &mut app, event_poll_timeout);

    // Cleanup
    app.shutdown();
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        crossterm::event::DisableMouseCapture
    )?;
    execute!(
        terminal.backend_mut(),
        crossterm::event::DisableBracketedPaste
    )?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

/// Handle an IPC subcommand (`renga send …`, `renga list`, etc.).
/// Resolves the endpoint from the `RENGA_SOCKET` env var the parent
/// renga published to its child PTYs; prints the server's response to
/// stdout and exits with a non-zero code on error so shell scripts can
/// branch on it.
fn run_ipc_client(cmd: &cli::IpcCommand) -> Result<()> {
    // `--count 0` on `events` is a degenerate "drain zero events"
    // request. Short-circuit before any environment lookup so the
    // command is a true no-op: it must succeed even when run outside
    // a renga pane (where `RENGA_SOCKET` would be unset).
    if let cli::IpcCommand::Events { count: Some(0), .. } = cmd {
        return Ok(());
    }

    let endpoint = ipc::endpoint::endpoint_from_env()
        .map_err(|e| anyhow::anyhow!("{e}; run this from inside a renga pane"))?;

    // `events` uses the subscription path (long-lived stream), not the
    // single-shot request/response path.
    if let cli::IpcCommand::Events { timeout, count } = cmd {
        return run_events(&endpoint, timeout.map(|d| d.into()), *count);
    }

    let request = cmd.to_request()?;
    let response = ipc::client::send_request(&endpoint, &request)?;
    match response {
        ipc::Response::Ok { data } => {
            // `null` → nothing to print; anything else goes out as
            // pretty JSON so shell scripts can `jq` it. We don't print
            // spurious newlines for empty responses so pipelines stay
            // tight.
            if !data.is_null() {
                let pretty =
                    serde_json::to_string_pretty(&data).unwrap_or_else(|_| data.to_string());
                println!("{pretty}");
            }
            Ok(())
        }
        ipc::Response::Hello { .. } | ipc::Response::Subscribed => {
            // These are handshake replies, never command responses.
            Err(anyhow::anyhow!("unexpected control response to command"))
        }
        ipc::Response::Err { message, code } => {
            if let Some(c) = code {
                Err(anyhow::anyhow!("[{c}] {message}"))
            } else {
                Err(anyhow::anyhow!("{message}"))
            }
        }
    }
}

/// Run `renga events` with optional stop budgets. Bounds the drain so
/// shell callers can poll inside a `/loop` cycle without hanging.
///
/// Architecture: a worker thread holds the subscription and forwards
/// events into a channel; the main thread selects on that channel with
/// a deadline, printing each event and decrementing the count budget
/// as we go. When main returns, the `Receiver` is dropped and the
/// worker's next `tx.send` fails, making its `on_event` callback
/// return `false` so the subscription exits cleanly. The worker may
/// still be blocked in `read_line` at that point; we detach it and
/// let the OS reap on process exit (CLI is short-lived).
fn run_events(
    endpoint: &ipc::endpoint::EndpointName,
    timeout: Option<std::time::Duration>,
    count: Option<usize>,
) -> Result<()> {
    use std::io::Write;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    // `--count 0` is a degenerate "drain zero events" request; honor it
    // by returning immediately so we never open a connection or spawn
    // a reader for it.
    if let Some(0) = count {
        return Ok(());
    }

    let (tx, rx) = mpsc::channel::<ipc::Event>();
    let endpoint_clone = endpoint.clone();
    std::thread::Builder::new()
        .name("renga-events-reader".into())
        .spawn(move || {
            let _ = ipc::client::subscribe_events(&endpoint_clone, |event| tx.send(event).is_ok());
        })
        .map_err(|e| anyhow::anyhow!("spawn events reader: {e}"))?;

    let deadline = timeout.map(|d| Instant::now() + d);
    let mut remaining = count;
    loop {
        let wait = match deadline {
            Some(d) => match d.checked_duration_since(Instant::now()) {
                Some(remaining_time) => remaining_time,
                None => return Ok(()),
            },
            None => Duration::from_secs(60 * 60 * 24 * 365),
        };
        match rx.recv_timeout(wait) {
            Ok(event) => {
                if let Ok(s) = serde_json::to_string(&event) {
                    println!("{s}");
                    let _ = std::io::stdout().flush();
                }
                if let Some(ref mut n) = remaining {
                    *n = n.saturating_sub(1);
                    if *n == 0 {
                        return Ok(());
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => return Ok(()),
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut app::App,
    event_poll_timeout: Duration,
) -> Result<()> {
    let mut paste_buffer: Vec<u8> = Vec::new();

    loop {
        // Drain any PTY output events
        app.drain_pty_events();

        // Phase 3: dispatch any commands delivered from the IPC server
        // thread. No-op when the channel is empty, so it's cheap to call
        // every frame.
        app.drain_app_commands();

        // Phase 1 (--exec): flush queued startup commands once the shell
        // prompt is observed. This is a no-op for panes without a queued
        // command, so it's safe to run every frame.
        for ws in &mut app.workspaces {
            for pane in ws.panes.values_mut() {
                let _ = pane.try_flush_startup();
            }
        }

        app.flush_pending_codex_peer_messages();

        // After paste, wait a few frames for PTY echo to settle
        if app.paste_cooldown > 0 {
            app.paste_cooldown -= 1;
            if app.paste_cooldown == 0 {
                app.dirty = true;
            }
        }

        // After a layout change (split/close/sidebar/terminal resize),
        // wait a few frames so child PTYs can respond to SIGWINCH with
        // a fresh redraw. Prevents the "old buffer at new size" flash.
        if app.resize_cooldown > 0 {
            app.resize_cooldown -= 1;
            if app.resize_cooldown == 0 {
                app.dirty = true;
            }
        }

        // Phase 2 (#37) catch-up: when freeze+catch-up is enabled,
        // periodically force a single repaint so body content stays
        // visible through an open overlay. No-op otherwise.
        app.maybe_tick_overlay_catchup();

        // First-launch macOS tip: hide the banner if it's been up for
        // more than the auto-dismiss budget (~20 s). Persists the
        // marker file via the same path as a key-press dismissal.
        // Cheap no-op when the banner isn't showing.
        app.check_macos_tip_timeout();

        // Only render when something changed (and no cooldown is active)
        if app.dirty && app.paste_cooldown == 0 && app.resize_cooldown == 0 {
            app.dirty = false;
            // Defense-in-depth for the Windows conpty caret-leak
            // originally reported in #25 / fixed in #36: while any pane
            // diff paints, ratatui-crossterm emits MoveTo+Print without
            // hiding the hardware cursor, and conpty leaks each MoveTo
            // to Windows Terminal's caret. Windows Terminal anchors IME
            // pre-edit to that host caret, so an intermediate MoveTo on
            // Claude's spinner row can pull native IME composition away
            // from Claude's input row.
            //
            // Force-hide the cursor for the whole draw transaction;
            // ratatui re-shows it only after the frame's final
            // `set_cursor_position` has been applied.
            //
            // Scoped to Windows because conpty is the observed
            // culprit; macOS / Linux terminals don't exhibit the
            // leak, and the gate avoids any unintended side effect.
            //
            // The same conpty path is in play under WSL: there the renga
            // binary is Linux (so `cfg(windows)` is false at compile time)
            // but the outer terminal is still Windows Terminal via conpty,
            // which leaks intermediate MoveTo the same way. Native Linux /
            // macOS terminals don't, so gate the Linux side on a runtime WSL
            // check to avoid changing their behavior.
            if hide_cursor_during_draw() {
                let _ = execute!(terminal.backend_mut(), crossterm::cursor::Hide);
            }
            terminal.draw(|frame| {
                ui::render(app, frame);
            })?;
            // Apply the caret AFTER the draw, while the cursor is still hidden
            // from the pre-draw `Hide`. On conpty, `ui::render` deferred the
            // caret here instead of calling `frame.set_cursor_position`, so
            // ratatui left the frame cursor hidden at frame end rather than
            // re-showing it at its stale post-paint position (the one-frame
            // flicker onto Claude's spinner row, #260). MoveTo first, then
            // Show, so the cursor only becomes visible at its final position.
            // Gated identically to the pre-draw Hide; non-conpty targets keep
            // the original in-frame `set_cursor_position` path (#253).
            if hide_cursor_during_draw() {
                if let Some((x, y)) = app.deferred_caret.take() {
                    let _ = execute!(
                        terminal.backend_mut(),
                        crossterm::cursor::MoveTo(x, y),
                        crossterm::cursor::Show
                    );
                }
            }
        }

        if app.should_quit {
            break;
        }

        // Poll for crossterm events at the configured idle rate.
        if event::poll(event_poll_timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    let consumed = app.handle_key_event(key)?;
                    if !consumed {
                        // Collect rapid key events as potential paste
                        if let Some(bytes) = crate::app::key_event_to_bytes_pub(&key) {
                            paste_buffer.extend_from_slice(&bytes);
                            // Drain all immediately available key events (paste burst)
                            while event::poll(Duration::from_millis(1))? {
                                if let Event::Key(k) = event::read()? {
                                    if k.kind == KeyEventKind::Press {
                                        if app.handle_key_event(k)? {
                                            // Shortcut consumed — flush buffer first
                                            if !paste_buffer.is_empty() {
                                                flush_paste_buffer(app, &mut paste_buffer)?;
                                            }
                                            break;
                                        }
                                        if let Some(b) = crate::app::key_event_to_bytes_pub(&k) {
                                            paste_buffer.extend_from_slice(&b);
                                        }
                                    }
                                } else {
                                    break;
                                }
                            }
                            flush_paste_buffer(app, &mut paste_buffer)?;
                        }
                    }
                    app.dirty = true;
                }
                Event::Key(_) => {}
                Event::Paste(text) => {
                    let routed_to_overlay = app.handle_paste(&text)?;
                    if !routed_to_overlay {
                        app.paste_cooldown = 5;
                    }
                    app.dirty = true;
                }
                Event::Mouse(mouse) => {
                    app.handle_mouse_event(mouse);
                    app.dirty = true;
                }
                Event::Resize(cols, rows) => {
                    // Propagate the new terminal size to App so every
                    // pane's PTY gets a prompt SIGWINCH, and hold the
                    // paint for a few frames while the children redraw.
                    app.on_terminal_resize(cols, rows);
                }
                _ => {}
            }
        }
    }

    Ok(())
}

/// Flush accumulated key buffer to PTY. If multiple characters were collected
/// (indicating a paste), wrap in bracketed paste sequences only when the PTY
/// application has enabled the mode. Unconditional wrapping causes shells that
/// haven't opted in to display the escape sequences as literal text (issue #2).
fn flush_paste_buffer(app: &mut app::App, buffer: &mut Vec<u8>) -> Result<()> {
    if buffer.is_empty() {
        return Ok(());
    }

    let focused_id = app.ws().focused_pane_id;
    if let Some(pane) = app.ws_mut().panes.get_mut(&focused_id) {
        pane.scroll_reset();
        pane.clear_codex_transcript_overlay_hint();
        if buffer.len() > 6 {
            if pane.is_bracketed_paste_enabled() {
                let mut data = Vec::with_capacity(buffer.len() + 12);
                data.extend_from_slice(b"\x1b[200~");
                data.extend_from_slice(buffer);
                data.extend_from_slice(b"\x1b[201~");
                pane.write_input(&data)?;
            } else {
                pane.write_input(buffer)?;
            }
            app.paste_cooldown = 5;
        } else {
            // Normal typing — send directly
            pane.write_input(buffer)?;
        }
    }
    buffer.clear();
    Ok(())
}

/// Whether the hardware cursor must be force-hidden for the whole draw
/// transaction to avoid conpty leaking intermediate `MoveTo` to the host
/// caret (see the call site for the full rationale). Exactly the conpty
/// hosts — see [`is_conpty_host`].
pub(crate) fn hide_cursor_during_draw() -> bool {
    is_conpty_host()
}

/// Whether the host terminal anchors a native IME (pre-edit + candidate
/// window) to the terminal caret. That is the mechanism Issue #34 leans
/// on to keep composition at the user's input position, and the one
/// Issue #281 needed nudged one row down.
///
/// Deliberately stricter than [`is_conpty_host`]. That predicate answers
/// "does conpty sit between us and the host", which is true for *any*
/// WSL session; this one gates a shift of the visible caret, so it must
/// also be true that a Windows IME is actually attached. SSH into a WSL
/// distro, or a Linux terminal under WSLg, is conpty-free from the
/// user's point of view — shifting their caret would cost them the
/// accurate caret position and buy nothing. Windows Terminal forwards
/// `WT_SESSION` / `WT_PROFILE_ID` across the WSL boundary via `WSLENV`,
/// so their presence is the signal that the frontend really is Windows
/// Terminal.
///
/// Other conpty frontends (wezterm, Alacritty driving `wsl.exe`) fall
/// through to `false` and keep the pre-#281 anchor. That is the safe
/// side: no fix, but no caret shift either.
pub(crate) fn host_anchors_ime_to_caret() -> bool {
    #[cfg(windows)]
    {
        // Running as a native Windows process, the console host (conhost
        // or Windows Terminal) is by definition the IME's window owner.
        true
    }
    #[cfg(not(windows))]
    {
        is_wsl() && is_windows_terminal()
    }
}

/// Whether a WSL process is running *in* Windows Terminal. Cached like
/// [`is_wsl`]: the render path consults it every frame, and the
/// environment cannot change under a running process.
#[cfg(not(windows))]
fn is_windows_terminal() -> bool {
    use std::sync::OnceLock;
    static WT: OnceLock<bool> = OnceLock::new();
    *WT.get_or_init(|| {
        let wt_marker =
            std::env::var_os("WT_SESSION").is_some() || std::env::var_os("WT_PROFILE_ID").is_some();
        windows_terminal_from_env(
            wt_marker,
            std::env::var("TERM_PROGRAM").ok().as_deref(),
            std::env::var("TERM").ok().as_deref(),
        )
    })
}

/// Decide whether the frontend is Windows Terminal from environment
/// facts alone. Split out from [`is_windows_terminal`] so the policy is
/// unit-testable without mutating process-global env.
///
/// `WT_SESSION` / `WT_PROFILE_ID` are inherited by *children* of a
/// Windows Terminal shell, so their presence alone does not prove the
/// current frontend is Windows Terminal — launch a GUI terminal from a
/// WT-hosted WSL shell and it inherits them. Reject the marker whenever
/// some other terminal identifies itself: Windows Terminal sets neither
/// `TERM_PROGRAM` nor a self-naming `TERM`, so any such value means a
/// different emulator owns this tty.
///
/// Known residual gap: emulators that announce themselves through
/// *neither* variable (bare `xterm`, `gnome-terminal`) still read as
/// Windows Terminal when launched from a WT shell. There is no way to
/// identify the owning frontend from inside a Linux process, so that
/// case is left to the `[ime] anchor_row_offset = 0` escape hatch and
/// documented in `docs/ime.md`.
#[cfg(not(windows))]
fn windows_terminal_from_env(
    wt_marker: bool,
    term_program: Option<&str>,
    term: Option<&str>,
) -> bool {
    if !wt_marker {
        return false;
    }
    // wezterm, VS Code, ghostty, Apple Terminal, … all set this.
    if term_program.is_some_and(|v| !v.is_empty()) {
        return false;
    }
    // Emulators that self-name through TERM instead of TERM_PROGRAM.
    !matches!(
        term,
        Some("alacritty" | "xterm-kitty" | "foot" | "foot-extra" | "contour" | "rio")
    )
}

/// Native Windows, or Linux under WSL where the outer terminal is still
/// Windows Terminal driving a conpty.
fn is_conpty_host() -> bool {
    #[cfg(windows)]
    {
        true
    }
    #[cfg(not(windows))]
    {
        is_wsl()
    }
}

/// Detect WSL once and cache the result. WSL exports `WSL_DISTRO_NAME` /
/// `WSL_INTEROP`, and its kernel release string contains "microsoft".
#[cfg(not(windows))]
fn is_wsl() -> bool {
    use std::sync::OnceLock;
    static WSL: OnceLock<bool> = OnceLock::new();
    *WSL.get_or_init(|| {
        if std::env::var_os("WSL_DISTRO_NAME").is_some()
            || std::env::var_os("WSL_INTEROP").is_some()
        {
            return true;
        }
        std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| {
                let s = s.to_ascii_lowercase();
                s.contains("microsoft") || s.contains("wsl")
            })
            .unwrap_or(false)
    })
}

#[cfg(all(test, not(windows)))]
mod windows_terminal_detection_tests {
    use super::windows_terminal_from_env;

    #[test]
    fn wt_hosted_wsl_shell_is_windows_terminal() {
        // The reporter's environment (#281): WT forwards WT_SESSION
        // through WSLENV, and sets neither TERM_PROGRAM nor a
        // self-naming TERM.
        assert!(windows_terminal_from_env(
            true,
            None,
            Some("xterm-256color")
        ));
    }

    #[test]
    fn no_wt_marker_is_never_windows_terminal() {
        // SSH into a WSL distro, or a login shell that never saw WT.
        assert!(!windows_terminal_from_env(
            false,
            None,
            Some("xterm-256color")
        ));
        assert!(!windows_terminal_from_env(false, Some("WezTerm"), None));
    }

    #[test]
    fn a_self_identifying_frontend_beats_an_inherited_wt_marker() {
        // Launch one of these from a WT-hosted WSL shell and it
        // inherits WT_SESSION — but it, not WT, owns the tty and the
        // caret, so the offset must stay off.
        for term_program in ["WezTerm", "vscode", "ghostty", "Apple_Terminal"] {
            assert!(
                !windows_terminal_from_env(true, Some(term_program), Some("xterm-256color")),
                "TERM_PROGRAM={term_program} must not read as Windows Terminal"
            );
        }
        for term in ["alacritty", "xterm-kitty", "foot", "contour", "rio"] {
            assert!(
                !windows_terminal_from_env(true, None, Some(term)),
                "TERM={term} must not read as Windows Terminal"
            );
        }
    }

    #[test]
    fn an_empty_term_program_does_not_disqualify() {
        // Some shells export TERM_PROGRAM="" rather than unsetting it;
        // that carries no frontend identity and must not suppress the
        // fix for a genuine WT session.
        assert!(windows_terminal_from_env(true, Some(""), None));
    }
}

#[cfg(test)]
mod tests {
    use super::flush_paste_buffer;
    use crate::app::App;

    #[test]
    fn flush_paste_buffer_clears_codex_transcript_overlay_hint() {
        let mut app = App::new(40, 80).expect("App::new");
        let focused_id = app.ws().focused_pane_id;
        let pane = app
            .ws_mut()
            .panes
            .get_mut(&focused_id)
            .expect("focused pane exists");
        pane.set_codex_transcript_overlay_hint_for_test(true);

        let mut buffer = b"x".to_vec();
        flush_paste_buffer(&mut app, &mut buffer).expect("flush succeeds");

        let pane = app
            .ws()
            .panes
            .get(&focused_id)
            .expect("focused pane exists");
        assert!(
            !pane.codex_transcript_overlay_hint_for_test(),
            "typing/paste flush must clear transcript fallback state"
        );
        assert!(buffer.is_empty(), "flush should consume the buffered input");
        app.shutdown();
    }
}
