use clap::{Parser, Subcommand};
use std::fmt;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "renga",
    version,
    about = "AI-native terminal for multi-agent coding"
)]
pub struct Cli {
    /// Subcommands talk to an already-running renga instance over IPC.
    /// When absent, renga launches as a TUI (using the flags below).
    #[command(subcommand)]
    pub command: Option<IpcCommand>,

    /// Working directory to change into before launching
    pub dir: Option<PathBuf>,

    /// Command to execute automatically in the initial pane after the shell is ready
    #[arg(long, value_name = "CMD", conflicts_with = "layout")]
    pub exec: Option<String>,

    /// Load a multi-pane layout by name from
    /// `./renga-layouts/<NAME>.toml` or `~/.config/renga/layouts/<NAME>.toml`
    #[arg(long, value_name = "NAME", conflicts_with = "exec")]
    pub layout: Option<String>,

    /// IME overlay mode. Overrides `[ime] mode` in config.toml.
    ///
    /// * `hotkey` (default) — Ctrl+; opens the IME composition overlay.
    /// * `off` — Ctrl+; is forwarded to the pane's PTY; the overlay is
    ///   never opened.
    #[arg(long, value_name = "MODE", value_enum)]
    pub ime: Option<crate::config::ImeMode>,

    /// Suppress pane repaints while the IME composition overlay is
    /// open (Issue #37 / #82 Phase 2). PTY output keeps flowing into
    /// the vt100 parser in the background; the screen just stops
    /// updating, so Claude's thinking spinner can't flicker the
    /// overlay. Panes catch up instantly when the overlay closes.
    /// Overrides `[ime] freeze_panes_on_overlay` in config.toml.
    /// **On by default** — the freeze is inert for users who never
    /// open the overlay. Pass `--ime-freeze-panes=false` to force-
    /// disable and keep live repaints during composition.
    #[arg(
        long,
        value_name = "BOOL",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
    )]
    pub ime_freeze_panes: Option<bool>,

    /// While `--ime-freeze-panes` is active, force a single repaint
    /// every `<MS>` milliseconds so body-content progress (Claude
    /// writing new lines, shell output scrolling) stays visible
    /// through an open overlay. **Defaults to 3000 ms** (the
    /// README-documented sweet spot: flicker stays barely noticeable
    /// while Claude's output still advances at a readable pace).
    /// Pass `0` for a pure freeze — the screen stops updating until
    /// the overlay closes. Non-zero values are clamped to at least
    /// 100 ms. Has no effect while freeze is disabled. Overrides
    /// `[ime] overlay_catchup_ms` in config.toml.
    #[arg(long, value_name = "MS")]
    pub ime_overlay_catchup_ms: Option<u64>,

    /// UI language for status bar hints and preview error messages.
    /// Overrides `[ui] lang` in config.toml.
    ///
    /// * `auto` (default) — detect from OS locale via `sys-locale`;
    ///   JA on `ja*` tags, EN otherwise.
    /// * `ja` — force Japanese regardless of locale.
    /// * `en` — force English regardless of locale.
    #[arg(long, value_name = "LANG", value_enum, ignore_case = true)]
    pub lang: Option<crate::i18n::UiLang>,

    /// Main event-loop target rate for ordinary rendering / input
    /// polling. Higher values reduce idle input latency and make
    /// animations smoother at the cost of more wakeups. Overrides
    /// `[ui] fps` in config.toml. `0` is accepted here and clamped to
    /// `1` at runtime to avoid a busy loop.
    #[arg(long, value_name = "FPS")]
    pub fps: Option<u16>,

    /// Minimum columns each child pane must retain after a vertical
    /// split. Splits that would produce a narrower child are refused.
    /// A value of `0` is clamped to `1` at runtime to avoid degenerate
    /// halving math. Default: 20.
    #[arg(long, value_name = "COLS", default_value_t = 20)]
    pub min_pane_width: u16,

    /// Minimum rows each child pane must retain after a horizontal
    /// split. Splits that would produce a shorter child are refused.
    /// A value of `0` is clamped to `1` at runtime. Default: 5.
    #[arg(long, value_name = "ROWS", default_value_t = 5)]
    pub min_pane_height: u16,

    /// Suppress the first-launch macOS Option-as-Meta banner for this
    /// run without touching the dismissal marker. Use when automated
    /// sessions shouldn't count as a user dismissal. Also settable via
    /// `RENGA_NO_MACOS_TIP=1` env var. No-op on non-macOS hosts.
    #[arg(long, conflicts_with = "show_macos_tip")]
    pub no_macos_tip: bool,

    /// Force the macOS Option-as-Meta banner on, ignoring the dismissal
    /// marker. Useful after editing your terminal config to verify the
    /// banner copy, or to re-read the hint without `rm`-ing the marker.
    /// Still macOS-gated — showing a macOS-specific setup tip on Linux
    /// would just be noise.
    #[arg(long, conflicts_with = "no_macos_tip")]
    pub show_macos_tip: bool,
}

/// Subcommands dispatched to a running renga instance via its IPC
/// endpoint. These always exit without starting the TUI.
#[derive(Subcommand, Debug, Clone)]
pub enum IpcCommand {
    /// List panes in the active workspace.
    List,
    /// Write text to a pane. Exactly one of --name / --id / --focused.
    Send {
        /// Pane name (defined in the layout or via `split --id NAME`).
        #[arg(long, conflicts_with_all = ["id", "focused"])]
        name: Option<String>,
        /// Numeric pane id as shown by `renga list`.
        #[arg(long, conflicts_with_all = ["name", "focused"])]
        id: Option<usize>,
        /// Target the currently focused pane.
        #[arg(long, conflicts_with_all = ["name", "id"])]
        focused: bool,
        /// Append Enter after the text so the shell executes it.
        #[arg(long)]
        enter: bool,
        /// Text to send.
        text: String,
    },
    /// Move keyboard focus to a pane.
    Focus {
        #[arg(long, conflicts_with = "id")]
        name: Option<String>,
        #[arg(long, conflicts_with = "name")]
        id: Option<usize>,
    },
    /// Close a pane (terminate its process and remove it from the
    /// layout). If the pane is the only one in its tab and other tabs
    /// exist, the whole tab is closed. Refuses when it's the last pane
    /// of the last tab.
    Close {
        #[arg(long, conflicts_with = "id")]
        name: Option<String>,
        #[arg(long, conflicts_with = "name")]
        id: Option<usize>,
    },
    /// Open a new tab with a fresh single pane. Focus switches to the
    /// new tab. Optionally pre-assigns a stable pane name and tab
    /// label.
    NewTab {
        /// Command to run in the new pane.
        #[arg(long)]
        command: Option<String>,
        /// Stable name for the new pane (lookup via `--name`).
        #[arg(long)]
        id: Option<String>,
        /// Tab label (overrides the cwd-derived default).
        #[arg(long)]
        label: Option<String>,
        /// Free-form role label attached to the new pane.
        #[arg(long)]
        role: Option<String>,
        /// Working directory for the new pane. Relative paths are
        /// resolved against the caller's shell cwd and sent as an
        /// absolute path to the renga server.
        #[arg(long)]
        cwd: Option<String>,
    },
    /// Split a pane and optionally run a command in the new side.
    Split {
        /// Target pane to split. Defaults to the focused pane.
        #[arg(long, conflicts_with_all = ["target_id", "target_focused"])]
        target_name: Option<String>,
        #[arg(long, conflicts_with_all = ["target_name", "target_focused"])]
        target_id: Option<usize>,
        #[arg(long, conflicts_with_all = ["target_name", "target_id"])]
        target_focused: bool,
        /// Split direction.
        #[arg(long, value_parser = ["vertical", "horizontal"])]
        direction: String,
        /// Command to run in the freshly-created pane.
        #[arg(long)]
        command: Option<String>,
        /// Stable name to assign to the new pane so it can be addressed
        /// later via `--name`.
        #[arg(long)]
        id: Option<String>,
        /// Free-form role label attached to the new pane.
        #[arg(long)]
        role: Option<String>,
        /// Working directory for the new pane. Relative paths are
        /// resolved against the caller's shell cwd and sent as an
        /// absolute path to the renga server. When omitted, the new
        /// pane inherits the target pane's cwd.
        #[arg(long)]
        cwd: Option<String>,
    },
    /// Snapshot the visible screen of a pane. Returns JSON with one
    /// entry per screen row so callers can match against fixed
    /// positions (e.g. a status-bar row) without re-flowing blank rows.
    Inspect {
        /// Pane name. Defaults to the focused pane.
        #[arg(long, conflicts_with_all = ["id", "focused"])]
        name: Option<String>,
        #[arg(long, conflicts_with_all = ["name", "focused"])]
        id: Option<usize>,
        #[arg(long, conflicts_with_all = ["name", "id"])]
        focused: bool,
        /// Return the last N rendered lines ending at the live bottom.
        /// N beyond the visible height continues into scrollback
        /// history (capped at 2000; scrollback rows have negative row
        /// indices). Omit to return the full visible screen.
        #[arg(long)]
        lines: Option<usize>,
        /// Include the cursor position and visibility in the payload.
        #[arg(long)]
        cursor: bool,
    },
    /// Subscribe to pane lifecycle events. Streams one JSON object per
    /// line to stdout until the renga server closes the connection or
    /// one of `--timeout` / `--count` stops the drain. Pipeable into
    /// `while read -r line` for reactive shell scripts.
    ///
    /// The stream is unscoped, exactly as it has always been: it carries
    /// `pane_started`, `pane_exited`, `events_dropped` and `heartbeat`,
    /// plus every `peer_inbox` no matter which pane the message was
    /// addressed to. Since #306 an IPC client may narrow that by sending
    /// `from_pane` on its `subscribe` request and receive only the
    /// `peer_inbox` for that one pane; this command deliberately does
    /// not, so its output is unchanged. Use the `renga mcp-peer` tools
    /// if you want just one pane's peer messages.
    Events {
        /// Stop after this duration (e.g. "2s", "500ms", "1m"). If
        /// unset the stream continues until the server closes the
        /// connection.
        #[arg(long)]
        timeout: Option<humantime::Duration>,
        /// Stop after receiving this many events. `EventsDropped`
        /// meta-events count toward this budget. If unset no cap.
        #[arg(long)]
        count: Option<usize>,
    },
    /// Rename or (re)assign the stable `name` / `role` of an existing
    /// pane. Useful when a session was launched without the intended
    /// layout, so a pane needs to be adopted into a role-based
    /// addressing scheme retroactively. Pass `--clear-name` /
    /// `--clear-role` to remove the current value; `--to-name` /
    /// `--to-role` to set. Exactly one of target selectors is required.
    Rename {
        /// Target pane by stable name.
        #[arg(long, conflicts_with_all = ["id", "focused"])]
        name: Option<String>,
        /// Target pane by numeric id.
        #[arg(long, conflicts_with_all = ["name", "focused"])]
        id: Option<usize>,
        /// Target the focused pane (default if no other selector is
        /// given).
        #[arg(long, conflicts_with_all = ["name", "id"])]
        focused: bool,
        /// New stable name to assign. Refused with `name_in_use` if
        /// another pane in the same tab already owns it.
        #[arg(long, conflicts_with = "clear_name")]
        to_name: Option<String>,
        /// Remove the pane's stable name.
        #[arg(long, conflicts_with = "to_name")]
        clear_name: bool,
        /// New role label to assign.
        #[arg(long, conflicts_with = "clear_role")]
        to_role: Option<String>,
        /// Remove the pane's role label.
        #[arg(long, conflicts_with = "to_role")]
        clear_role: bool,
    },
    /// Run as a stdio MCP server for Claude Code (see issue #97). Not
    /// a true IPC subcommand — it doesn't dispatch a request and
    /// expect a reply. `main` intercepts this variant and hands off to
    /// [`crate::mcp_peer::run`], which performs MCP handshakes over
    /// stdio while using the renga IPC as its peer-messaging backend.
    ///
    /// This subcommand is meant to be registered in
    /// `~/.claude/mcp_servers.json` (done explicitly via `renga mcp
    /// install`, not auto-installed — see #97's scope decision). Claude
    /// Code spawns it, inherits `RENGA_PANE_ID` / `RENGA_SOCKET` from
    /// the pane PTY, and never blocks on its own subcommand dispatch.
    McpPeer,
    /// Manage the `renga-peers` MCP server registration in Claude
    /// Code or Codex. Thin wrapper around their MCP management
    /// commands so users get a one-liner instead of having to know the
    /// exact registration payload. Requires the selected client CLI on
    /// PATH.
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },
}

/// Sub-subcommands of `renga mcp`. Kept as a separate enum so clap
/// renders `renga mcp install` / `renga mcp status` as nested in
/// `--help` output, which is the documentation affordance a first-time
/// user expects.
#[derive(Subcommand, Debug, Clone)]
pub enum McpAction {
    /// Register renga-peers in the selected client's MCP config.
    /// Idempotent — re-running overwrites the existing entry only when
    /// `--force` is passed, otherwise prints the current entry and
    /// bails so accidental re-runs don't silently repoint.
    Install {
        /// Which client to register renga-peers with.
        #[arg(long, value_enum, default_value_t = McpClient::Claude)]
        client: McpClient,
        /// Overwrite any existing renga-peers entry without prompting.
        /// Needed when upgrading renga and the command path changed.
        #[arg(long)]
        force: bool,
        /// For Codex only: patch the `renga-peers` config entry so
        /// `check_messages` / `send_message` default to auto-approve.
        /// Keeps riskier tools such as `send_keys` on prompt.
        #[arg(long)]
        codex_auto_approve_peer_tools: bool,
    },
    /// Remove renga-peers from the selected client's MCP config.
    /// No-op if the entry is not present.
    Uninstall {
        /// Which client to remove the registration from.
        #[arg(long, value_enum, default_value_t = McpClient::Claude)]
        client: McpClient,
    },
    /// Show whether renga-peers is currently registered, and if so
    /// what command it points at.
    Status {
        /// Which client to query the registration from.
        #[arg(long, value_enum, default_value_t = McpClient::Claude)]
        client: McpClient,
    },
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum McpClient {
    #[default]
    Claude,
    Codex,
}

impl fmt::Display for McpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Claude => write!(f, "claude"),
            Self::Codex => write!(f, "codex"),
        }
    }
}

impl Cli {
    /// Validate fields that clap cannot express declaratively.
    pub fn validate_exec(&self) -> anyhow::Result<()> {
        if let Some(cmd) = &self.exec {
            if cmd.is_empty() {
                anyhow::bail!("--exec value must not be empty");
            }
        }
        Ok(())
    }
}

impl IpcCommand {
    /// Translate a CLI subcommand into the over-the-wire IPC request.
    pub fn to_request(&self) -> anyhow::Result<crate::ipc::Request> {
        use crate::ipc::{Direction, PaneRef, Request};

        fn pick_ref(
            name: &Option<String>,
            id: &Option<usize>,
            focused: bool,
        ) -> anyhow::Result<PaneRef> {
            match (name, id, focused) {
                (Some(n), None, false) => Ok(PaneRef::Name(n.clone())),
                (None, Some(i), false) => Ok(PaneRef::Id(*i)),
                (None, None, true) => Ok(PaneRef::Focused),
                (None, None, false) => Ok(PaneRef::Focused),
                _ => {
                    anyhow::bail!("ambiguous target: use at most one of --name / --id / --focused")
                }
            }
        }

        match self {
            IpcCommand::List => Ok(Request::List { from_pane: None }),
            IpcCommand::NewTab {
                command,
                id,
                label,
                role,
                cwd,
            } => Ok(Request::NewTab {
                command: command.clone(),
                id: id.clone(),
                label: label.clone(),
                role: role.clone(),
                cwd: resolve_cli_cwd(cwd.as_deref())?,
            }),
            IpcCommand::Send {
                name,
                id,
                focused,
                enter,
                text,
            } => Ok(Request::Send {
                target: pick_ref(name, id, *focused)?,
                data: text.clone(),
                append_enter: *enter,
                // The CLI is a user typing at a shell, not a pane-bound
                // agent: "the current tab" is the one on screen, which
                // is what `from_pane: None` selects.
                from_pane: None,
            }),
            IpcCommand::Focus { name, id } => Ok(Request::Focus {
                target: pick_ref(name, id, false)?,
                from_pane: None,
            }),
            IpcCommand::Close { name, id } => Ok(Request::Close {
                target: pick_ref(name, id, false)?,
                // Same reasoning as `Send`, plus: `renga close` has
                // always searched every tab, and `from_pane: None` is
                // what preserves that (Issue #296).
                from_pane: None,
            }),
            IpcCommand::Split {
                target_name,
                target_id,
                target_focused,
                direction,
                command,
                id,
                role,
                cwd,
            } => {
                let dir = match direction.as_str() {
                    "vertical" => Direction::Vertical,
                    "horizontal" => Direction::Horizontal,
                    other => anyhow::bail!("invalid direction: {other}"),
                };
                Ok(Request::Split {
                    target: pick_ref(target_name, target_id, *target_focused)?,
                    direction: dir,
                    command: command.clone(),
                    id: id.clone(),
                    role: role.clone(),
                    cwd: resolve_cli_cwd(cwd.as_deref())?,
                    from_pane: None,
                    // The CLI has no tab-placement flag: `renga split`
                    // keeps splitting in the visible tab.
                    tab: None,
                })
            }
            IpcCommand::Rename {
                name,
                id,
                focused,
                to_name,
                clear_name,
                to_role,
                clear_role,
            } => {
                if to_name.is_none() && !clear_name && to_role.is_none() && !clear_role {
                    anyhow::bail!(
                        "rename requires at least one of --to-name / --clear-name / --to-role / --clear-role"
                    );
                }
                let name_change: Option<Option<String>> = if *clear_name {
                    Some(None)
                } else {
                    to_name.as_ref().map(|s| Some(s.clone()))
                };
                let role_change: Option<Option<String>> = if *clear_role {
                    Some(None)
                } else {
                    to_role.as_ref().map(|s| Some(s.clone()))
                };
                Ok(Request::SetPaneIdentity {
                    target: pick_ref(name, id, *focused)?,
                    name: name_change,
                    role: role_change,
                    // Same reasoning as `Close` (Issue #296).
                    from_pane: None,
                })
            }
            // `renga events` subscribes unscoped: it is a generic tap
            // for shell scripts, not a pane's inbox, and it has no pane
            // identity of its own to declare (it runs from whatever
            // shell the operator typed it in, which may not be a renga
            // pane at all). `from_pane: None` therefore declines #306's
            // opt-in narrowing, which is what keeps this stream exactly
            // what it was before #306 — `peer_inbox` lines for every pane
            // included. `from_pane` is for consumers that want one pane's
            // mail and nothing else; a CLI tap is the opposite of that,
            // so there is deliberately no flag for it here.
            IpcCommand::Events { .. } => Ok(Request::Subscribe { from_pane: None }),
            IpcCommand::Inspect {
                name,
                id,
                focused,
                lines,
                cursor,
            } => Ok(Request::Inspect {
                target: pick_ref(name, id, *focused)?,
                lines: *lines,
                include_cursor: *cursor,
                from_pane: None,
            }),
            IpcCommand::McpPeer => anyhow::bail!(
                "mcp-peer is a standalone subprocess, not an IPC request; \
                 this variant must be intercepted before to_request() in main.rs"
            ),
            IpcCommand::Mcp { .. } => anyhow::bail!(
                "mcp install/uninstall/status shells out to a client MCP CLI; \
                 this variant must be intercepted before to_request() in main.rs"
            ),
        }
    }
}

/// Resolve a `--cwd` CLI argument against the caller's shell cwd and
/// hand the renga server an absolute path. Server-side validation
/// (existence / directory check) still runs, so typos still produce a
/// `cwd_invalid` error — this helper is purely about matching user
/// intuition when they type a relative path at their shell.
fn resolve_cli_cwd(raw: Option<&str>) -> anyhow::Result<Option<String>> {
    let s = match raw {
        Some(s) => s.trim(),
        None => return Ok(None),
    };
    if s.is_empty() {
        return Ok(None);
    }
    let p = PathBuf::from(s);
    let absolute = if p.is_absolute() {
        p
    } else {
        std::env::current_dir()
            .map_err(|e| anyhow::anyhow!("failed to read current_dir for --cwd: {e}"))?
            .join(p)
    };
    Ok(Some(absolute.to_string_lossy().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_no_args() {
        let cli = Cli::try_parse_from(["renga"]).unwrap();
        assert_eq!(cli.dir, None);
        assert!(cli.command.is_none());
    }

    #[test]
    fn parses_directory_only() {
        let cli = Cli::try_parse_from(["renga", "/path"]).unwrap();
        assert_eq!(cli.dir, Some(PathBuf::from("/path")));
        assert!(cli.command.is_none());
    }

    #[test]
    fn parses_exec_flag() {
        let cli = Cli::try_parse_from(["renga", "--exec", "claude /company"]).unwrap();
        assert_eq!(cli.dir, None);
        assert_eq!(cli.exec.as_deref(), Some("claude /company"));
    }

    #[test]
    fn parses_dir_and_exec() {
        let cli = Cli::try_parse_from(["renga", ".", "--exec", "cce"]).unwrap();
        assert_eq!(cli.dir, Some(PathBuf::from(".")));
        assert_eq!(cli.exec.as_deref(), Some("cce"));
    }

    #[test]
    fn rejects_empty_exec() {
        let result = Cli::try_parse_from(["renga", "--exec", ""]);
        assert!(
            result.is_ok(),
            "clap is permissive; validation happens later"
        );
        let cli = result.unwrap();
        assert_eq!(cli.exec.as_deref(), Some(""));
        assert!(
            cli.validate_exec().is_err(),
            "empty exec should fail validation"
        );
    }

    #[test]
    fn parses_layout_flag() {
        let cli = Cli::try_parse_from(["renga", "--layout", "cc-campany"]).unwrap();
        assert_eq!(cli.layout.as_deref(), Some("cc-campany"));
        assert_eq!(cli.exec, None);
    }

    #[test]
    fn rejects_exec_and_layout_together() {
        let result = Cli::try_parse_from(["renga", "--exec", "x", "--layout", "y"]);
        assert!(
            result.is_err(),
            "exec and layout must be mutually exclusive"
        );
    }

    // -- Phase 3: IPC subcommands ------------------------------------

    #[test]
    fn parses_list_subcommand() {
        let cli = Cli::try_parse_from(["renga", "list"]).unwrap();
        assert!(matches!(cli.command, Some(IpcCommand::List)));
    }

    #[test]
    fn parses_send_subcommand_with_name() {
        let cli = Cli::try_parse_from([
            "renga",
            "send",
            "--name",
            "engineering",
            "--enter",
            "echo hi",
        ])
        .unwrap();
        match cli.command {
            Some(IpcCommand::Send {
                name,
                enter,
                text,
                focused,
                ..
            }) => {
                assert_eq!(name.as_deref(), Some("engineering"));
                assert!(enter);
                assert!(!focused);
                assert_eq!(text, "echo hi");
            }
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[test]
    fn parses_close_subcommand_with_name() {
        let cli = Cli::try_parse_from(["renga", "close", "--name", "worker-foo"]).unwrap();
        match cli.command {
            Some(IpcCommand::Close { name, id }) => {
                assert_eq!(name.as_deref(), Some("worker-foo"));
                assert!(id.is_none());
            }
            other => panic!("expected Close, got {other:?}"),
        }
    }

    #[test]
    fn close_to_request_translates_id() {
        let cli = Cli::try_parse_from(["renga", "close", "--id", "5"]).unwrap();
        let req = cli.command.unwrap().to_request().unwrap();
        match req {
            crate::ipc::Request::Close { target, from_pane } => {
                assert!(matches!(target, crate::ipc::PaneRef::Id(5)));
                assert_eq!(from_pane, None, "the CLI never claims a caller pane");
            }
            other => panic!("expected Close, got {other:?}"),
        }
    }

    #[test]
    fn close_rejects_name_and_id_together() {
        let result = Cli::try_parse_from(["renga", "close", "--name", "a", "--id", "1"]);
        assert!(result.is_err(), "name and id must be mutually exclusive");
    }

    #[test]
    fn parses_focus_subcommand_with_id() {
        let cli = Cli::try_parse_from(["renga", "focus", "--id", "2"]).unwrap();
        match cli.command {
            Some(IpcCommand::Focus { id, name }) => {
                assert_eq!(id, Some(2));
                assert!(name.is_none());
            }
            other => panic!("expected Focus, got {other:?}"),
        }
    }

    #[test]
    fn parses_split_subcommand() {
        let cli = Cli::try_parse_from([
            "renga",
            "split",
            "--direction",
            "horizontal",
            "--command",
            "ccr",
            "--id",
            "research",
        ])
        .unwrap();
        match cli.command {
            Some(IpcCommand::Split {
                direction,
                command,
                id,
                ..
            }) => {
                assert_eq!(direction, "horizontal");
                assert_eq!(command.as_deref(), Some("ccr"));
                assert_eq!(id.as_deref(), Some("research"));
            }
            other => panic!("expected Split, got {other:?}"),
        }
    }

    #[test]
    fn rejects_send_with_two_targets() {
        let result = Cli::try_parse_from(["renga", "send", "--name", "a", "--id", "1", "text"]);
        assert!(result.is_err(), "name and id are mutually exclusive");
    }

    #[test]
    fn split_rejects_bad_direction() {
        let result = Cli::try_parse_from(["renga", "split", "--direction", "diagonal"]);
        assert!(result.is_err(), "direction must be vertical or horizontal");
    }

    #[test]
    fn send_to_request_uses_focused_when_no_target() {
        let cli = Cli::try_parse_from(["renga", "send", "hi"]).unwrap();
        let cmd = cli.command.unwrap();
        let req = cmd.to_request().unwrap();
        match req {
            crate::ipc::Request::Send { target, data, .. } => {
                assert!(matches!(target, crate::ipc::PaneRef::Focused));
                assert_eq!(data, "hi");
            }
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[test]
    fn parses_new_tab_subcommand() {
        let cli = Cli::try_parse_from([
            "renga",
            "new-tab",
            "--command",
            "cce",
            "--id",
            "engineering",
            "--label",
            "eng",
        ])
        .unwrap();
        match cli.command {
            Some(IpcCommand::NewTab {
                command,
                id,
                label,
                role,
                cwd,
            }) => {
                assert_eq!(command.as_deref(), Some("cce"));
                assert_eq!(id.as_deref(), Some("engineering"));
                assert_eq!(label.as_deref(), Some("eng"));
                assert!(role.is_none());
                assert!(cwd.is_none());
            }
            other => panic!("expected NewTab, got {other:?}"),
        }
    }

    #[test]
    fn new_tab_to_request_preserves_fields() {
        let cli = Cli::try_parse_from(["renga", "new-tab", "--command", "ccr", "--id", "research"])
            .unwrap();
        let req = cli.command.unwrap().to_request().unwrap();
        match req {
            crate::ipc::Request::NewTab {
                command,
                id,
                label,
                role,
                cwd,
            } => {
                assert_eq!(command.as_deref(), Some("ccr"));
                assert_eq!(id.as_deref(), Some("research"));
                assert!(label.is_none());
                assert!(role.is_none());
                assert!(cwd.is_none());
            }
            other => panic!("expected NewTab, got {other:?}"),
        }
    }

    #[test]
    fn split_to_request_translates_direction() {
        let cli = Cli::try_parse_from(["renga", "split", "--direction", "vertical", "--id", "foo"])
            .unwrap();
        let req = cli.command.unwrap().to_request().unwrap();
        match req {
            crate::ipc::Request::Split { direction, id, .. } => {
                assert!(matches!(direction, crate::ipc::Direction::Vertical));
                assert_eq!(id.as_deref(), Some("foo"));
            }
            other => panic!("expected Split, got {other:?}"),
        }
    }

    #[test]
    fn parses_split_with_role() {
        let cli = Cli::try_parse_from([
            "renga",
            "split",
            "--direction",
            "horizontal",
            "--role",
            "worker",
        ])
        .unwrap();
        match cli.command {
            Some(IpcCommand::Split { role, .. }) => {
                assert_eq!(role.as_deref(), Some("worker"));
            }
            other => panic!("expected Split, got {other:?}"),
        }
    }

    #[test]
    fn parses_new_tab_with_role() {
        let cli = Cli::try_parse_from(["renga", "new-tab", "--role", "leader"]).unwrap();
        match cli.command {
            Some(IpcCommand::NewTab { role, .. }) => {
                assert_eq!(role.as_deref(), Some("leader"));
            }
            other => panic!("expected NewTab, got {other:?}"),
        }
    }

    #[test]
    fn split_to_request_carries_role() {
        let cli = Cli::try_parse_from([
            "renga",
            "split",
            "--direction",
            "vertical",
            "--role",
            "worker",
        ])
        .unwrap();
        let req = cli.command.unwrap().to_request().unwrap();
        match req {
            crate::ipc::Request::Split { role, .. } => {
                assert_eq!(role.as_deref(), Some("worker"));
            }
            other => panic!("expected Split, got {other:?}"),
        }
    }

    #[test]
    fn new_tab_to_request_carries_role() {
        let cli = Cli::try_parse_from(["renga", "new-tab", "--role", "leader"]).unwrap();
        let req = cli.command.unwrap().to_request().unwrap();
        match req {
            crate::ipc::Request::NewTab { role, .. } => {
                assert_eq!(role.as_deref(), Some("leader"));
            }
            other => panic!("expected NewTab, got {other:?}"),
        }
    }

    #[test]
    fn parses_events_without_limits() {
        let cli = Cli::try_parse_from(["renga", "events"]).unwrap();
        match cli.command {
            Some(IpcCommand::Events { timeout, count }) => {
                assert!(timeout.is_none());
                assert!(count.is_none());
            }
            other => panic!("expected Events, got {other:?}"),
        }
    }

    #[test]
    fn parses_events_with_timeout_and_count() {
        let cli =
            Cli::try_parse_from(["renga", "events", "--timeout", "2s", "--count", "5"]).unwrap();
        match cli.command {
            Some(IpcCommand::Events { timeout, count }) => {
                assert_eq!(count, Some(5));
                let d: std::time::Duration = timeout.expect("timeout").into();
                assert_eq!(d, std::time::Duration::from_secs(2));
            }
            other => panic!("expected Events, got {other:?}"),
        }
    }

    #[test]
    fn parses_events_with_millisecond_timeout() {
        let cli = Cli::try_parse_from(["renga", "events", "--timeout", "500ms"]).unwrap();
        match cli.command {
            Some(IpcCommand::Events { timeout, .. }) => {
                let d: std::time::Duration = timeout.expect("timeout").into();
                assert_eq!(d, std::time::Duration::from_millis(500));
            }
            other => panic!("expected Events, got {other:?}"),
        }
    }

    /// `renga events` must stay *unscoped* (`from_pane: None`). That is
    /// what keeps its output identical to every previous release under
    /// #306, and it keeps its wire form byte-identical too
    /// (`skip_serializing_if` drops the `None`, so the line is still
    /// exactly `{"cmd":"subscribe"}` and an older server sees nothing
    /// new). Pinning the exact variant rather than just "is a Subscribe"
    /// is the point: wiring some pane id in here would silently narrow
    /// the CLI's long-standing output to one pane's `peer_inbox`, which
    /// is the regression #306 must not cause.
    #[test]
    fn events_to_request_is_an_unscoped_subscribe() {
        let cli = Cli::try_parse_from(["renga", "events", "--count", "3"]).unwrap();
        let req = cli.command.unwrap().to_request().unwrap();
        assert_eq!(req, crate::ipc::Request::Subscribe { from_pane: None });
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            r#"{"cmd":"subscribe"}"#
        );
    }

    #[test]
    fn parses_inspect_focused_default() {
        let cli = Cli::try_parse_from(["renga", "inspect"]).unwrap();
        match cli.command {
            Some(IpcCommand::Inspect {
                name,
                id,
                focused,
                lines,
                cursor,
            }) => {
                assert!(name.is_none());
                assert!(id.is_none());
                assert!(!focused);
                assert!(lines.is_none());
                assert!(!cursor);
            }
            other => panic!("expected Inspect, got {other:?}"),
        }
    }

    #[test]
    fn parses_inspect_with_name_lines_cursor() {
        let cli = Cli::try_parse_from([
            "renga",
            "inspect",
            "--name",
            "worker-foo",
            "--lines",
            "4",
            "--cursor",
        ])
        .unwrap();
        match cli.command {
            Some(IpcCommand::Inspect {
                name,
                lines,
                cursor,
                ..
            }) => {
                assert_eq!(name.as_deref(), Some("worker-foo"));
                assert_eq!(lines, Some(4));
                assert!(cursor);
            }
            other => panic!("expected Inspect, got {other:?}"),
        }
    }

    #[test]
    fn inspect_to_request_preserves_fields() {
        let cli =
            Cli::try_parse_from(["renga", "inspect", "--id", "7", "--lines", "2", "--cursor"])
                .unwrap();
        let req = cli.command.unwrap().to_request().unwrap();
        match req {
            crate::ipc::Request::Inspect {
                target,
                lines,
                include_cursor,
                ..
            } => {
                assert!(matches!(target, crate::ipc::PaneRef::Id(7)));
                assert_eq!(lines, Some(2));
                assert!(include_cursor);
            }
            other => panic!("expected Inspect, got {other:?}"),
        }
    }

    #[test]
    fn parses_ime_mode_off() {
        let cli = Cli::try_parse_from(["renga", "--ime", "off"]).unwrap();
        assert_eq!(cli.ime, Some(crate::config::ImeMode::Off));
    }

    #[test]
    fn parses_ime_mode_hotkey() {
        let cli = Cli::try_parse_from(["renga", "--ime", "hotkey"]).unwrap();
        assert_eq!(cli.ime, Some(crate::config::ImeMode::Hotkey));
    }

    #[test]
    fn rejects_ime_mode_always() {
        // `always` was removed — clap must reject the value.
        assert!(Cli::try_parse_from(["renga", "--ime", "always"]).is_err());
    }

    #[test]
    fn rejects_unknown_ime_mode() {
        let err = Cli::try_parse_from(["renga", "--ime", "banana"]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("banana") || msg.contains("invalid value") || msg.contains("possible"),
            "got: {msg}"
        );
    }

    #[test]
    fn ime_flag_is_optional() {
        let cli = Cli::try_parse_from(["renga"]).unwrap();
        assert_eq!(cli.ime, None);
    }

    #[test]
    fn ime_freeze_panes_defaults_to_none() {
        let cli = Cli::try_parse_from(["renga"]).unwrap();
        assert_eq!(cli.ime_freeze_panes, None);
    }

    #[test]
    fn ime_freeze_panes_bare_flag_means_true() {
        let cli = Cli::try_parse_from(["renga", "--ime-freeze-panes"]).unwrap();
        assert_eq!(cli.ime_freeze_panes, Some(true));
    }

    #[test]
    fn ime_freeze_panes_explicit_false_overrides_config() {
        let cli = Cli::try_parse_from(["renga", "--ime-freeze-panes=false"]).unwrap();
        assert_eq!(cli.ime_freeze_panes, Some(false));
    }

    #[test]
    fn ime_overlay_catchup_ms_defaults_to_none() {
        let cli = Cli::try_parse_from(["renga"]).unwrap();
        assert_eq!(cli.ime_overlay_catchup_ms, None);
    }

    #[test]
    fn parses_ime_overlay_catchup_ms_override() {
        let cli = Cli::try_parse_from(["renga", "--ime-overlay-catchup-ms", "3000"]).unwrap();
        assert_eq!(cli.ime_overlay_catchup_ms, Some(3000));
    }

    #[test]
    fn lang_defaults_to_none() {
        let cli = Cli::try_parse_from(["renga"]).unwrap();
        assert_eq!(cli.lang, None);
    }

    #[test]
    fn fps_defaults_to_none() {
        let cli = Cli::try_parse_from(["renga"]).unwrap();
        assert_eq!(cli.fps, None);
    }

    #[test]
    fn parses_lang_auto_ja_en() {
        let cli = Cli::try_parse_from(["renga", "--lang", "auto"]).unwrap();
        assert_eq!(cli.lang, Some(crate::i18n::UiLang::Auto));
        let cli = Cli::try_parse_from(["renga", "--lang", "ja"]).unwrap();
        assert_eq!(cli.lang, Some(crate::i18n::UiLang::Ja));
        let cli = Cli::try_parse_from(["renga", "--lang", "en"]).unwrap();
        assert_eq!(cli.lang, Some(crate::i18n::UiLang::En));
    }

    #[test]
    fn parses_lang_case_insensitive() {
        // `ignore_case = true` on the clap attr — a user typing
        // `--lang JA` shouldn't hit a parse error that would kill
        // startup.
        let cli = Cli::try_parse_from(["renga", "--lang", "JA"]).unwrap();
        assert_eq!(cli.lang, Some(crate::i18n::UiLang::Ja));
        let cli = Cli::try_parse_from(["renga", "--lang", "En"]).unwrap();
        assert_eq!(cli.lang, Some(crate::i18n::UiLang::En));
    }

    #[test]
    fn rejects_unknown_lang() {
        assert!(Cli::try_parse_from(["renga", "--lang", "zh"]).is_err());
        assert!(Cli::try_parse_from(["renga", "--lang", "banana"]).is_err());
    }

    #[test]
    fn parses_fps_override_and_zero() {
        let cli = Cli::try_parse_from(["renga", "--fps", "60"]).unwrap();
        assert_eq!(cli.fps, Some(60));

        let cli = Cli::try_parse_from(["renga", "--fps", "0"]).unwrap();
        assert_eq!(cli.fps, Some(0));
    }

    #[test]
    fn min_pane_size_defaults_to_20_and_5() {
        let cli = Cli::try_parse_from(["renga"]).unwrap();
        assert_eq!(cli.min_pane_width, 20);
        assert_eq!(cli.min_pane_height, 5);
    }

    #[test]
    fn min_pane_size_accepts_override_and_zero() {
        // Non-zero override is stored verbatim; the runtime clamp of
        // `0 → 1` lives in `App::set_min_pane_size`, not in clap, so
        // this parses without error.
        let cli = Cli::try_parse_from(["renga", "--min-pane-width", "0", "--min-pane-height", "3"])
            .unwrap();
        assert_eq!(cli.min_pane_width, 0);
        assert_eq!(cli.min_pane_height, 3);

        let cli2 =
            Cli::try_parse_from(["renga", "--min-pane-width", "12", "--min-pane-height", "4"])
                .unwrap();
        assert_eq!(cli2.min_pane_width, 12);
        assert_eq!(cli2.min_pane_height, 4);
    }

    #[test]
    fn macos_tip_flags_default_to_false() {
        let cli = Cli::try_parse_from(["renga"]).unwrap();
        assert!(!cli.no_macos_tip);
        assert!(!cli.show_macos_tip);
    }

    #[test]
    fn parses_no_macos_tip_flag() {
        let cli = Cli::try_parse_from(["renga", "--no-macos-tip"]).unwrap();
        assert!(cli.no_macos_tip);
        assert!(!cli.show_macos_tip);
    }

    #[test]
    fn parses_show_macos_tip_flag() {
        let cli = Cli::try_parse_from(["renga", "--show-macos-tip"]).unwrap();
        assert!(cli.show_macos_tip);
        assert!(!cli.no_macos_tip);
    }

    #[test]
    fn parses_mcp_install_default_client_as_claude() {
        let cli = Cli::try_parse_from(["renga", "mcp", "install"]).unwrap();
        let Some(IpcCommand::Mcp {
            action:
                McpAction::Install {
                    client,
                    force,
                    codex_auto_approve_peer_tools,
                },
        }) = cli.command
        else {
            panic!("expected mcp install command");
        };
        assert_eq!(client, McpClient::Claude);
        assert!(!force);
        assert!(!codex_auto_approve_peer_tools);
    }

    #[test]
    fn parses_mcp_install_codex_auto_approve_flag() {
        let cli = Cli::try_parse_from([
            "renga",
            "mcp",
            "install",
            "--client",
            "codex",
            "--codex-auto-approve-peer-tools",
        ])
        .unwrap();
        let Some(IpcCommand::Mcp {
            action:
                McpAction::Install {
                    client,
                    force,
                    codex_auto_approve_peer_tools,
                },
        }) = cli.command
        else {
            panic!("expected mcp install command");
        };
        assert_eq!(client, McpClient::Codex);
        assert!(!force);
        assert!(codex_auto_approve_peer_tools);
    }

    #[test]
    fn parses_mcp_status_codex_client() {
        let cli = Cli::try_parse_from(["renga", "mcp", "status", "--client", "codex"]).unwrap();
        let Some(IpcCommand::Mcp {
            action: McpAction::Status { client },
        }) = cli.command
        else {
            panic!("expected mcp status command");
        };
        assert_eq!(client, McpClient::Codex);
    }

    #[test]
    fn macos_tip_flags_are_mutually_exclusive() {
        let err = Cli::try_parse_from(["renga", "--show-macos-tip", "--no-macos-tip"]);
        assert!(err.is_err(), "flags should conflict");
    }

    #[test]
    fn accepts_count_zero_at_parse_time() {
        // `--count 0` must parse (no clap-level `value_parser` restricts
        // it) so that `run_ipc_client` can short-circuit it as a true
        // no-op. Runtime short-circuit behavior is enforced by the
        // early-return in `main.rs::run_ipc_client` and is not covered
        // by this parse-level test.
        let cli = Cli::try_parse_from(["renga", "events", "--count", "0"]).unwrap();
        match cli.command {
            Some(IpcCommand::Events { count, .. }) => {
                assert_eq!(count, Some(0));
            }
            other => panic!("expected Events, got {other:?}"),
        }
    }
}
