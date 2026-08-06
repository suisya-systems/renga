//! IPC protocol for controlling a running renga instance from outside.
//!
//! Wire format: newline-delimited JSON. Each line is one [`Request`] from
//! client to server, followed by exactly one [`Response`] from server to
//! client. Connections are short-lived in the v1 protocol — clients open,
//! send one request, read one response, close.
//!
//! # Threat model
//!
//! IPC is a local-only control channel between processes running as
//! the same user. OS-level isolation handles the cross-user boundary;
//! IPC itself is **not** a secrecy or authentication boundary against
//! other processes running as that same user.
//!
//! - On Unix, the socket lives under an owner-only directory
//!   (`$XDG_RUNTIME_DIR/renga/` or `/tmp/renga-UID/` with mode `0700`).
//!   A different UID on the same host cannot reach it.
//! - On Windows, the Named Pipe is named `\\.\pipe\renga-<pid>` and
//!   inherits default session-scoped permissions from the OS.
//!
//! The `RENGA_TOKEN` env var is **not** a secret. It exists only to
//! detect PID re-use: if a child shell inherited a stale `RENGA_SOCKET`
//! whose PID now belongs to a different renga instance, the token on
//! the wire won't match the child's `RENGA_TOKEN` and the client
//! refuses the command. Any same-user process that can read
//! `/proc/<pid>/environ` can also read the token — on the same-user
//! trust model that's already inside the boundary.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Server-side budget for waking the App event loop with a single
/// command. The App drains commands every frame (~30Hz), so 5s is
/// orders of magnitude more than the expected latency — a timeout
/// here means the App is genuinely wedged.
pub(crate) const APP_REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// Client margin for connect + JSON write/read + scheduling on top of
/// the server's [`APP_REPLY_TIMEOUT`]. Kept small so `renga send` in
/// a shell script aborts within a few seconds if something is wrong.
pub(crate) const CLIENT_MARGIN: Duration = Duration::from_secs(5);

/// Total time the client waits for a full response before erroring out.
/// Derived so the two timeouts stay in sync if one is tuned later.
pub(crate) const RESPONSE_TIMEOUT: Duration =
    Duration::from_secs(APP_REPLY_TIMEOUT.as_secs() + CLIENT_MARGIN.as_secs());

pub mod client;
pub mod endpoint;
pub mod events;
pub mod server;

pub use events::EventBus;

/// Hard cap on the total number of lines an [`Request::Inspect`] call
/// returns (visible screen + scrollback continuation). The vt100
/// scrollback holds up to 10,000 lines per pane, but an uncapped read
/// would produce megabyte-scale payloads that no MCP / LLM consumer
/// can usefully ingest in one response. Oversized requests are
/// clamped silently, consistent with how `lines` has always treated
/// values beyond what is available.
pub const INSPECT_MAX_LINES: usize = 2000;

/// Capability token advertised in [`Response::Hello::capabilities`] by
/// servers that understand the `from_pane` field on [`Request::List`] /
/// `Send` / `Split` / `Focus` / `Inspect` (Issue #288).
///
/// A server that omits this token resolves those requests against the
/// **active** tab — whichever tab the human is looking at — regardless
/// of any `from_pane` the client sent. Clients that depend on
/// caller-tab scoping (the bundled `renga mcp-peer`) must therefore
/// **fail closed** when the token is absent rather than silently
/// operating on the wrong tab: a renga binary can be upgraded on disk
/// while the old server process keeps running, so a new mcp-peer
/// subprocess talking to an old server is a live scenario, not a
/// theoretical one.
pub const CAP_CALLER_SCOPE: &str = "caller_scope";

/// Capability token advertised by servers whose peer messaging spans
/// tabs (Issue #289): [`Request::PeerList`] enumerates every workspace
/// and [`Request::PeerSend`] delivers to panes in other tabs instead of
/// silently dropping them.
///
/// Deliberately distinct from [`CAP_CALLER_SCOPE`]: a #288-era server
/// advertises `caller_scope` while still silently dropping cross-tab
/// sends, so the bundled mcp-peer must gate its `list_peers` /
/// `send_message` tools on *this* token to keep "Delivered" honest.
/// Absent token ⇒ fail closed (see [`client::send_request_requiring`]).
pub const CAP_CROSS_TAB_PEERS: &str = "cross_tab_peers";

/// Capability token advertised by servers that understand tab-directed
/// spawning (Issue #290): the `tab` selector on [`Request::Split`] and
/// the [`Request::SpawnTab`] background-tab variant.
///
/// Deliberately distinct from [`CAP_CALLER_SCOPE`] /
/// [`CAP_CROSS_TAB_PEERS`]: `Request` does not use
/// `deny_unknown_fields`, so a #289-era server would silently drop an
/// unknown `tab` field and spawn into the caller's tab — the same
/// wrong-tab accident #288 fixed for targeting. Clients sending a `tab`
/// selector must gate on *this* token via
/// [`client::send_request_requiring`] and fail closed when it is
/// absent.
pub const CAP_SPAWN_TAB: &str = "spawn_tab";

/// Capability token advertised by servers that understand `from_pane`
/// on the two *mutating* requests that #288 left behind:
/// [`Request::Close`] and [`Request::SetPaneIdentity`] (Issue #296).
///
/// Deliberately distinct from [`CAP_CALLER_SCOPE`]: a #288-era server
/// advertises `caller_scope` while still resolving `Focused` / `Name`
/// on these two against the **active** tab. Since `Request` does not
/// use `deny_unknown_fields`, such a server drops the new `from_pane`
/// silently — and `close_pane(target: "focused")` from a background
/// tab then terminates a pane in whatever tab the human is watching.
/// That is the exact accident this token exists to make impossible, so
/// clients must gate on it via [`client::send_request_requiring`] and
/// fail closed when it is absent.
pub const CAP_CALLER_SCOPE_CLOSE_IDENTITY: &str = "caller_scope_close_identity";

/// Every capability token this build's server advertises. Additive by
/// construction — clients match on tokens they know and ignore the
/// rest.
pub const SERVER_CAPABILITIES: &[&str] = &[
    CAP_CALLER_SCOPE,
    CAP_CROSS_TAB_PEERS,
    CAP_SPAWN_TAB,
    CAP_CALLER_SCOPE_CLOSE_IDENTITY,
];

/// One IPC call from a client to the running renga instance.
///
/// # Caller-tab scoping (`from_pane`)
///
/// The pane-targeting requests carry an optional `from_pane`: the id of
/// the pane the *caller itself* runs in (published to every PTY as
/// `RENGA_PANE_ID`). It is a **new optional input with a
/// prior-behavior-preserving default**, not a required field — see
/// `docs/semver-policy.md` §3.
///
/// - `from_pane: None` — legacy semantics: everything resolves inside
///   the **active** workspace. This is what `renga send` / `renga
///   split` and any pre-#288 client send, and what they keep getting.
/// - `from_pane: Some(id)` — resolution is scoped to the workspace that
///   *owns* `id`. `PaneRef::Focused` and `PaneRef::Name` never leave
///   that tab; `PaneRef::Id` may address a pane in another tab (the
///   cross-tab escape hatch [`Request::Close`] already established).
///   An unknown / vanished `from_pane` fails with `pane_not_found`
///   before the target is even looked at.
///
/// [`Request::Close`] and [`Request::SetPaneIdentity`] joined the set
/// in Issue #296 and gate on their own [`CAP_CALLER_SCOPE_CLOSE_IDENTITY`]
/// token. They differ from the five above in their **legacy** branch
/// only: with `from_pane: None` they keep searching every workspace
/// (`renga close --id`/`--name` has always been cross-tab), whereas the
/// five resolve strictly inside the active tab.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// First message after connecting — exchanges PIDs and a session
    /// token so a stale socket file with a re-used PID cannot be
    /// silently mistaken for a live instance.
    Hello { client_pid: u32 },
    /// List all panes in the caller's workspace (the active workspace
    /// when `from_pane` is omitted).
    ///
    /// Wire note: this was a unit variant before #288. With
    /// `skip_serializing_if` on the added field, `List { from_pane:
    /// None }` still serializes to exactly `{"cmd":"list"}` and the
    /// bare `{"cmd":"list"}` still deserializes — see
    /// `list_request_raw_json_shape_is_unchanged_without_from_pane`.
    List {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_pane: Option<usize>,
    },
    /// Write `data` to the target pane's PTY. If `append_enter` is true,
    /// a newline is appended so the shell executes the command.
    Send {
        target: PaneRef,
        data: String,
        #[serde(default)]
        append_enter: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_pane: Option<usize>,
    },
    /// Split the target pane and (optionally) run a command in the new
    /// pane. The new pane is named `id` if provided.
    Split {
        target: PaneRef,
        direction: Direction,
        #[serde(default)]
        command: Option<String>,
        #[serde(default)]
        id: Option<String>,
        /// Free-form role label (see [`PaneInfo::role`]).
        #[serde(default)]
        role: Option<String>,
        /// Working directory for the new pane. Absolute paths are used
        /// as-is; relative paths are resolved against the target pane's
        /// cwd at server-side. When omitted, inherits the target pane's
        /// cwd (prior behavior). Fails with `cwd_invalid` before any
        /// layout mutation when the resolved path is missing or not a
        /// directory.
        ///
        /// Note that the base for a *relative* path is the **target**
        /// pane, not the caller: `from_pane` scopes which panes the
        /// `target` may name, it does not re-base cwd. Clients that
        /// want caller-relative paths (the MCP `spawn_*` tools) resolve
        /// them to absolute before sending.
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_pane: Option<usize>,
        /// Which tab hosts the split (Issue #290). `None` keeps the
        /// prior behavior: the target resolves inside the caller's tab
        /// (or the active tab without `from_pane`). `Some(selector)`
        /// resolves the tab first, then resolves `target` strictly
        /// inside it — a numeric target in another tab fails with
        /// `target_tab_mismatch` instead of silently escaping the
        /// selected tab. [`TabSelector::New`] is not valid here (a
        /// split needs an existing layout); use [`Request::SpawnTab`].
        ///
        /// Only send this through
        /// [`client::send_request_requiring`] with [`CAP_SPAWN_TAB`]:
        /// `Request` tolerates unknown fields, so an older server
        /// would silently ignore the selector and split in the wrong
        /// tab.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab: Option<TabSelector>,
    },
    /// Move keyboard focus to the target pane. When the resolved pane
    /// lives in a different tab than the one on screen, the server also
    /// switches the visible tab — "focus" means the keyboard actually
    /// lands there, which is impossible for a hidden tab.
    Focus {
        target: PaneRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_pane: Option<usize>,
    },
    /// Close the target pane. Terminates its underlying process, drops
    /// it from the layout, and emits a `PaneExited` event. If the pane
    /// is the only leaf in its workspace and other workspaces exist,
    /// the whole tab is closed. Fails with `last_pane` if it's the last
    /// pane of the only remaining tab.
    ///
    /// `from_pane` (Issue #296) scopes the *relative* targets exactly
    /// as it does for [`Request::Send`] & co: `Focused` and `Name`
    /// resolve inside the caller's own tab, `Id` still reaches any tab.
    /// Closing is destructive and irreversible, which is why the
    /// pre-#296 behavior — `Focused` meaning "whatever pane the human
    /// is looking at" — was the worst place for the #288 bug to
    /// survive. `None` keeps the pre-#296 cross-tab search for the
    /// `renga close` CLI. Send `Some(_)` only through
    /// [`client::send_request_requiring`] with
    /// [`CAP_CALLER_SCOPE_CLOSE_IDENTITY`].
    Close {
        target: PaneRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_pane: Option<usize>,
    },
    /// Create a new tab with a fresh single pane. The server switches
    /// focus to the new tab (matching the existing Alt+T keybinding).
    NewTab {
        /// Startup command for the new pane.
        #[serde(default)]
        command: Option<String>,
        /// Stable name to register for the new pane so it can be
        /// addressed via `PaneRef::Name` later.
        #[serde(default)]
        id: Option<String>,
        /// Override the tab label (otherwise derived from the cwd).
        #[serde(default)]
        label: Option<String>,
        /// Free-form role label (see [`PaneInfo::role`]).
        #[serde(default)]
        role: Option<String>,
        /// Working directory for the new tab's initial pane. Absolute
        /// paths are used as-is; relative paths are resolved against
        /// the renga server's process cwd. When omitted, the server's
        /// current cwd is used (prior behavior). Fails with
        /// `cwd_invalid` before any layout mutation when the resolved
        /// path is missing or not a directory.
        #[serde(default)]
        cwd: Option<String>,
    },
    /// Spawn a fresh single-pane tab **in the background** (Issue
    /// #290): unlike [`Request::NewTab`], the active tab does not
    /// change — the human keeps looking at whatever they were looking
    /// at while an orchestrator places a worker in a new tab. The
    /// server finishes the new tab's rect computation and PTY resize
    /// before answering, so the reported geometry is real (never the
    /// 10x40 placeholder), and emits exactly one `pane_started` for
    /// the new pane after its name/role are set.
    ///
    /// This is the wire form of the MCP `spawn_*` tools' `tab: {new:
    /// …}` selector. It intentionally has no `target` / `direction` —
    /// a brand-new tab has nothing to split. Fails with
    /// `tab_limit_reached` when `MAX_TABS` tabs already exist.
    ///
    /// Only send this through [`client::send_request_requiring`] with
    /// [`CAP_SPAWN_TAB`] — an older server rejects the unknown `cmd`,
    /// but gating on the capability gives the caller the actionable
    /// `server_too_old` message instead of a generic parse error.
    SpawnTab {
        /// Startup command for the new pane.
        #[serde(default)]
        command: Option<String>,
        /// Stable name to register for the new pane so it can be
        /// addressed via [`PaneRef::Name`] later.
        #[serde(default)]
        id: Option<String>,
        /// Custom label for the new tab (the `{new: {name: …}}` field
        /// of the MCP selector). Otherwise derived from the cwd.
        #[serde(default)]
        label: Option<String>,
        /// Free-form role label (see [`PaneInfo::role`]).
        #[serde(default)]
        role: Option<String>,
        /// Working directory for the new tab's initial pane. Absolute
        /// paths are used as-is; relative paths resolve against the
        /// **caller pane's** cwd (unlike [`Request::NewTab`], which
        /// resolves against the server process cwd). When omitted, the
        /// caller pane's cwd is inherited — a spawn API places workers
        /// relative to the orchestrator that asked, not relative to
        /// wherever the renga process happened to start. Falls back to
        /// the server process cwd when `from_pane` is absent. Fails
        /// with `cwd_invalid` before any layout mutation.
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_pane: Option<usize>,
    },
    /// Switch the connection to live event stream mode. After the
    /// server acknowledges with [`Response::Subscribed`], it emits
    /// [`Event`] JSON Lines until the client disconnects. No further
    /// [`Request`]s are accepted on this connection.
    Subscribe,
    /// Snapshot the rendered contents of the target pane. Returns
    /// plain text in row-addressable form so orchestrators can detect
    /// prompts like "Allow this tool use?", error banners, or mode
    /// indicators without relying on worker self-reports.
    ///
    /// `lines = Some(N)` returns the last `N` rendered lines ending
    /// at the live bottom of the pane (including blank rows — the row
    /// layout is preserved on purpose so callers can match against
    /// fixed positions like the status bar). When `N` exceeds the
    /// pane's visible height, the shortfall continues into scrollback
    /// history, capped at [`INSPECT_MAX_LINES`]; scrollback rows carry
    /// negative `row` indices (`-1` = the line just above the visible
    /// top) and `line_start` may be negative. `None` returns the full
    /// visible screen. Reads are pinned to the live tail — the result
    /// does not depend on the pane's user scroll position, which is
    /// preserved across the call. `include_cursor = true` adds a
    /// `cursor` object to the payload.
    Inspect {
        target: PaneRef,
        #[serde(default)]
        lines: Option<usize>,
        #[serde(default)]
        include_cursor: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_pane: Option<usize>,
    },
    /// List peers visible from the caller's pane. Scope is "every pane
    /// in every workspace, excluding `from_pane` itself" (Issue #289) —
    /// the caller's own tab is listed first so same-tab siblings stay
    /// at the top. Servers advertising [`CAP_CROSS_TAB_PEERS`] answer
    /// with this scope; older servers only ever listed the caller's
    /// workspace. Used by the bundled MCP peer server (`renga
    /// mcp-peer`) to serve its `list_peers` tool. Wire-compat with
    /// claude-peers-mcp's tool signature is handled in the MCP layer;
    /// this request is renga-internal.
    PeerList {
        /// The caller's own pane id (from `RENGA_PANE_ID` env).
        from_pane: usize,
    },
    /// Deliver `body` to `target`'s peer inbox. Cross-tab targets are
    /// deliverable since Issue #289 (servers advertising
    /// [`CAP_CROSS_TAB_PEERS`]; older servers silently dropped them):
    /// a numeric id reaches any tab, while a name resolves only inside
    /// `from_pane`'s own workspace — pane names are unique per tab,
    /// not globally, so an unqualified name can never address another
    /// tab. An unresolvable target fails with `pane_not_found` rather
    /// than pretending to deliver. On success the server emits an
    /// `Event::PeerInbox` on the event bus so any MCP peer subprocess
    /// subscribed on behalf of the target can push it out as a
    /// `notifications/claude/channel` frame.
    PeerSend {
        from_pane: usize,
        target: PaneRef,
        body: String,
    },
    /// Publish the MCP client kind currently attached to a pane.
    /// Sent by `renga mcp-peer` after startup so pane/peer listings can
    /// expose whether the recipient supports push or pull delivery.
    PeerRegisterClient {
        pane_id: usize,
        kind: PeerClientKind,
    },
    /// Rename or (re)assign the stable `name` / `role` of an existing
    /// pane. Both fields use three-state semantics over the wire:
    ///
    /// - missing key        → leave the current value unchanged
    /// - `null`             → clear the current value
    /// - `"some-string"`    → set to the provided value
    ///
    /// Serde handles this via [`double_option`] on the two fields.
    ///
    /// Validation (server-side):
    /// - a non-empty `name` must not collide with another pane in the
    ///   same tab (`name_in_use`).
    /// - a non-empty `name` must not be all-digits, since the pane
    ///   addressing rule interprets digit strings as numeric ids
    ///   (`name_invalid`).
    /// - setting `name` to the target's current name or `role` to the
    ///   target's current role is a silent no-op (idempotent).
    ///
    /// `from_pane` (Issue #296) scopes `Focused` / `Name` to the
    /// caller's own tab, leaving `Id` cross-tab; `None` keeps the
    /// pre-#296 all-workspace search. Name uniqueness is still checked
    /// inside the *resolved* pane's tab, which is what makes per-tab
    /// names coherent: a caller can only mint a name in a tab it can
    /// actually address. Send `Some(_)` only through
    /// [`client::send_request_requiring`] with
    /// [`CAP_CALLER_SCOPE_CLOSE_IDENTITY`].
    SetPaneIdentity {
        target: PaneRef,
        #[serde(
            default,
            with = "double_option",
            skip_serializing_if = "Option::is_none"
        )]
        name: Option<Option<String>>,
        #[serde(
            default,
            with = "double_option",
            skip_serializing_if = "Option::is_none"
        )]
        role: Option<Option<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_pane: Option<usize>,
    },
    /// Set or clear the per-pane summary string. The summary is
    /// surfaced on `PaneInfo.summary` / `PeerInfo.summary` so other
    /// peer agents can read what this pane is working on.
    ///
    /// Wire contract:
    /// - `summary == ""` clears the existing value (round-trips to
    ///   `Option::None` on the server side).
    /// - Strings longer than 256 Unicode scalar values (`chars()`)
    ///   are rejected with `summary_too_long` before any state
    ///   mutation. The cap is in `chars`, not bytes, so multi-byte
    ///   scripts get the same effective ceiling as ASCII.
    /// - `from_pane` is the caller's own pane id, taken from
    ///   `RENGA_PANE_ID` by the MCP peer subprocess.
    SetSummary { from_pane: usize, summary: String },
}

/// Strip every control character from a caller-supplied display label
/// (pane name, role, tab label) before it is interpolated into text
/// that some *other* pane's agent will read.
///
/// Three of those interpolation sites are not merely cosmetic:
///
/// * the Codex peer nudge types its text straight into the target
///   pane's PTY and follows it with Enter a second later, so a bare
///   `\r` inside a pane name submits whatever precedes it as a prompt
///   in someone else's composer;
/// * the `notifications/claude/channel` banner and the `check_messages`
///   listing are prepended to the message body a receiving agent reads,
///   so a `\n` lets a name forge banner lines around content it does
///   not own;
/// * `\x1b` reaches the PTY as a live ANSI escape — cursor moves,
///   screen clears, and terminal queries that write a *reply* back onto
///   the pane's stdin.
///
/// [`char::is_control`] is the Unicode `Cc` category, which is exactly
/// the set that matters here: C0 (including `\t`, `\r`, `\n`), DEL, and
/// C1. Printable confusables (RTL overrides, zero-width joiners) are
/// deliberately left alone — they can mislead a human reading the tab
/// bar, but they cannot forge a line or drive a terminal, and stripping
/// them would mangle legitimate non-ASCII labels.
///
/// Removing rather than replacing can run two tokens together
/// (`"a\nb"` → `"ab"`); that is preferred over leaving a placeholder
/// that still has to be escaped everywhere downstream.
pub(crate) fn strip_control_chars(raw: &str) -> String {
    raw.chars().filter(|c| !c.is_control()).collect()
}

/// [`strip_control_chars`] for an optional field, borrowing when the
/// input is already clean so the common path allocates nothing.
pub(crate) fn sanitized_label(raw: &str) -> std::borrow::Cow<'_, str> {
    if raw.contains(char::is_control) {
        std::borrow::Cow::Owned(strip_control_chars(raw))
    } else {
        std::borrow::Cow::Borrowed(raw)
    }
}

/// Serde helper for the "missing / null / value" three-state pattern
/// used by [`Request::SetPaneIdentity`]. Missing key deserializes to
/// `None`, explicit `null` to `Some(None)`, and any value to
/// `Some(Some(value))`. Requires `#[serde(default)]` on the field so
/// missing keys resolve to `None` rather than an error.
pub mod double_option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S, T>(value: &Option<Option<T>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: Serialize,
    {
        match value {
            Some(inner) => inner.serialize(serializer),
            // `skip_serializing_if = "Option::is_none"` on the field
            // means we never reach this arm for outer-None; serializing
            // the outer-None as a bare `null` would be wrong anyway —
            // a caller round-tripping that record back through the
            // wire would turn "leave unchanged" into "clear".
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Ok(Some(Option::<T>::deserialize(deserializer)?))
    }
}

/// Identifies a pane in a request. Names are user-friendly and stable
/// across splits; numeric ids are stable across the session but assigned
/// internally; `Focused` is "whichever pane is focused right now".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneRef {
    Id(usize),
    Name(String),
    Focused,
}

/// Identifies which tab a spawn lands in (Issue #290). Externally
/// tagged on the wire, mirroring [`PaneRef`]: `{"name":"workers"}` /
/// `{"index":2}` / `{"pane_id":17}` / `{"new":{}}` /
/// `{"new":{"name":"workers"}}`.
///
/// A tagged enum instead of an overloaded string on purpose: tab
/// labels are free-form, so a reserved string like `"new"` would make
/// a tab actually named "new" unaddressable.
///
/// Resolution rules (server-side):
/// - `Name` — exact match against each tab's display name (custom
///   label, else the cwd-derived name). Zero matches fail with
///   `tab_not_found`; multiple matches fail with `tab_ambiguous` —
///   never first-match, since labels are not unique.
/// - `Index` — 0-based position in the tab strip, the same index
///   `list_peers` reports in `PeerInfo::tab`. Out of range fails with
///   `tab_not_found`.
/// - `PaneId` — the tab that owns the given pane. The stable anchor
///   for orchestrators: pane ids never shift, while names collide and
///   indices move when tabs close. Unknown pane fails with
///   `pane_not_found`.
/// - `New` — create a fresh background tab, optionally labeled. Only
///   meaningful for [`Request::SpawnTab`]; [`Request::Split`] rejects
///   it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabSelector {
    Name(String),
    Index(usize),
    PaneId(usize),
    New {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Vertical,
    Horizontal,
}

/// Peer client type published by a pane's attached MCP subprocess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PeerClientKind {
    Claude,
    Codex,
}

impl PeerClientKind {
    pub fn receive_mode(self) -> PeerReceiveMode {
        match self {
            PeerClientKind::Claude => PeerReceiveMode::Push,
            PeerClientKind::Codex => PeerReceiveMode::Pull,
        }
    }
}

/// How a peer receives logical renga messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PeerReceiveMode {
    Push,
    Pull,
}

/// One entry in the `PeerList` response payload. Describes a single
/// Claude-or-shell pane as a peer of the requesting pane. Spans every
/// workspace since Issue #289 (previously scoped to the caller's tab).
/// The MCP peer subprocess maps this into its `list_peers` tool output
/// for Claude; see `src/mcp_peer/`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerInfo {
    pub id: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Index of the tab (workspace) the pane lives in. **Display
    /// metadata only** — tab indexes shift when tabs close, so the
    /// stable address for a peer is its pane `id`, never this. All
    /// three tab fields are optional for wire compat both ways: a
    /// pre-#289 server omits them (new client decodes `None`) and a
    /// pre-#289 client ignores them as unknown fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab: Option<usize>,
    /// Display label of that tab (custom rename or cwd-derived).
    /// Display metadata only, same caveat as [`PeerInfo::tab`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_name: Option<String>,
    /// True when the pane shares the caller's tab. Same-tab peers can
    /// be addressed by bare name; peers in other tabs require the
    /// numeric pane id (names are only unique per tab).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_tab: Option<bool>,
    /// Working directory the pane was spawned with. Surfaced so the
    /// asking Claude can tell which repo a sibling pane is in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<PeerClientKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receive_mode: Option<PeerReceiveMode>,
    /// Optional pane-authored summary set via the MCP `set_summary`
    /// tool. Absent until the pane calls the tool; cleared when the
    /// pane sets it to an empty string. In-memory only — does not
    /// survive renga restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// One entry in the `List` response payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneInfo {
    pub id: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Free-form label. Set via layout TOML `role = ...`, `renga split
    /// --role ...`, or `renga new-tab --role ...`. Unlike `name`, not
    /// unique.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub focused: bool,
    /// Terminal column of the pane's top-left corner (origin = 0).
    /// Reflects the current layout including file-tree / preview
    /// sidebar offsets. `0` before the first layout pass.
    #[serde(default)]
    pub x: u16,
    /// Terminal row of the pane's top-left corner (origin = 0).
    /// `0` before the first layout pass.
    #[serde(default)]
    pub y: u16,
    /// Pane width in columns. `0` before the first layout pass.
    #[serde(default)]
    pub width: u16,
    /// Pane height in rows. `0` before the first layout pass.
    #[serde(default)]
    pub height: u16,
    /// Resolved working directory the pane was spawned with. Mirrors
    /// [`PeerInfo::cwd`] so `list_panes` / `list_peers` agree on the
    /// pane's launch cwd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<PeerClientKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receive_mode: Option<PeerReceiveMode>,
    /// Optional pane-authored summary; see [`PeerInfo::summary`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// Server reply to one [`Request`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    /// Successful call. `data` is request-specific (e.g. the pane list).
    Ok {
        #[serde(default)]
        data: serde_json::Value,
    },
    /// Hello reply: server identifies itself with PID and a session
    /// token derived from its start time, so the client can detect
    /// PID re-use from a previous crashed instance.
    Hello {
        server_pid: u32,
        session_token: String,
        /// Feature tokens this server understands (see
        /// [`SERVER_CAPABILITIES`]). Absent / empty on pre-#288
        /// servers, which is exactly the signal capability-dependent
        /// clients use to refuse rather than degrade. Additive: unknown
        /// tokens must be ignored, never rejected.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        capabilities: Vec<String>,
    },
    /// Ack that the connection has entered event-stream mode. The
    /// server follows this with newline-delimited [`Event`] records
    /// until the client disconnects.
    Subscribed,
    /// Server-side failure. `message` is human-readable; `code` is a
    /// stable short identifier for programmatic matching (see the
    /// `err_code` module). `code` is optional for backwards
    /// compatibility with clients built against the pre-coded protocol.
    Err {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
    },
}

/// Stable short identifiers for [`Response::Err::code`].
///
/// # Stability
///
/// These string values are **wire ABI**, not internal symbols.
/// Changing an existing constant's value is a breaking protocol
/// change and requires a deprecation window:
///
/// 1. Introduce the new code alongside the old one (both servers emit
///    the old value; old clients keep matching).
/// 2. Flip servers to emit the new value in the next minor release,
///    keeping the old constant exported so clients still build.
/// 3. Remove the old constant only after external clients (including
///    `aainc-ops`) have migrated.
///
/// Adding a new code is additive and safe — clients must treat
/// unknown codes as generic errors (fall back to `message`).
///
/// Heartbeat events (`Event::Heartbeat`) follow the same rule: new
/// event variants are additive, and clients skip unknown `type`
/// tags instead of aborting the stream (see
/// [`crate::ipc::client::subscribe_events`]).
pub mod err_code {
    /// The server is shutting down and cannot accept new commands.
    pub const SHUTTING_DOWN: &str = "shutting_down";
    /// The App event loop did not respond within the server-side
    /// budget. Usually means the UI thread is wedged.
    pub const APP_TIMEOUT: &str = "app_timeout";
    /// Request JSON failed to parse.
    pub const PARSE: &str = "parse";
    /// Protocol violation (wrong message at wrong time, duplicate
    /// hello, Subscribe reaching the one-shot dispatcher, etc.).
    pub const PROTOCOL: &str = "protocol";
    /// A sibling server-side invariant was violated while serializing
    /// the response payload.
    pub const INTERNAL: &str = "internal";
    /// The referenced pane (by id, name, or Focused) does not exist
    /// in the active workspace.
    pub const PANE_NOT_FOUND: &str = "pane_not_found";
    /// A pane id resolved on lookup but disappeared before the App
    /// could act on it (close / exit race). Rare.
    pub const PANE_VANISHED: &str = "pane_vanished";
    /// The workspace cannot accept another split — either the
    /// MAX_PANES cap is reached or the target pane is already at
    /// the minimum geometry.
    pub const SPLIT_REFUSED: &str = "split_refused";
    /// PTY write / spawn / OS-level I/O failure surfaced to the
    /// client so it can distinguish "setup broken" from "request
    /// invalid".
    pub const IO_ERROR: &str = "io_error";
    /// `renga close` was asked to remove the only pane of the only
    /// remaining tab. Refused so the TUI doesn't end up with an empty
    /// layout; the caller should shut down renga instead.
    pub const LAST_PANE: &str = "last_pane";
    /// Caller supplied a `cwd` that does not exist or is not a
    /// directory. Emitted by `Split` / `NewTab` before any pane is
    /// created so failed calls never leave a half-mutated layout.
    pub const CWD_INVALID: &str = "cwd_invalid";
    /// `SetPaneIdentity` was asked to assign a name that another pane
    /// in the same workspace already holds. The caller should either
    /// pick a different name or retire the existing holder first.
    pub const NAME_IN_USE: &str = "name_in_use";
    /// `SetPaneIdentity` was asked to assign a name that violates
    /// the naming rules (currently: non-empty and not all-digits,
    /// since digit strings are interpreted as numeric ids by
    /// `PaneRef::Name` resolution).
    pub const NAME_INVALID: &str = "name_invalid";
    /// `SetSummary` was passed a `summary` string longer than the
    /// per-pane cap (256 Unicode scalar values). The caller should
    /// either truncate the summary or send an empty string to clear.
    pub const SUMMARY_TOO_LONG: &str = "summary_too_long";
    /// A `tab` selector named a tab that does not exist: no tab's
    /// display name matches exactly, or the 0-based index is out of
    /// range. Emitted by `Split` before any layout mutation.
    pub const TAB_NOT_FOUND: &str = "tab_not_found";
    /// A `tab: {name: …}` selector matched more than one tab. Tab
    /// labels are not unique, so the server refuses to guess — the
    /// caller should switch to a `{pane_id: …}` or `{index: …}`
    /// anchor, or relabel the tabs.
    pub const TAB_AMBIGUOUS: &str = "tab_ambiguous";
    /// A `Split` combined a `tab` selector with a numeric `target`
    /// that lives in a *different* tab. Refused instead of silently
    /// following either side — the two halves of the request
    /// contradict each other.
    pub const TARGET_TAB_MISMATCH: &str = "target_tab_mismatch";
    /// Creating another tab would exceed `MAX_TABS`. Emitted by
    /// `NewTab` / `SpawnTab` (and the `tab: {new: …}` selector).
    /// Deliberately distinct from `SPLIT_REFUSED`, which is about pane
    /// capacity *inside* one tab.
    pub const TAB_LIMIT_REACHED: &str = "tab_limit_reached";
}

/// App-side error carrying a free-form message plus an optional
/// stable code from [`err_code`]. Replaces the previous
/// `Result<T, String>` reply shape on [`crate::app::AppCommand`] so
/// renga clients (including `aainc-ops`) can match on the code
/// instead of grepping human-readable text.
///
/// Uncoded variants still work — older App paths or new cases that
/// don't warrant a stable code yet can call
/// [`CodedError::uncoded`], and the wire response still carries the
/// message so humans can read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodedError {
    pub message: String,
    pub code: Option<&'static str>,
}

impl CodedError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: Some(code),
        }
    }

    pub fn uncoded(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: None,
        }
    }

    /// Convert into a wire [`Response::Err`], preserving the code
    /// when present.
    pub fn into_response(self) -> Response {
        match self.code {
            Some(c) => Response::err_coded(c, self.message),
            None => Response::err(self.message),
        }
    }
}

impl std::fmt::Display for CodedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            Some(c) => write!(f, "[{c}] {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl From<String> for CodedError {
    fn from(s: String) -> Self {
        CodedError::uncoded(s)
    }
}

impl From<&str> for CodedError {
    fn from(s: &str) -> Self {
        CodedError::uncoded(s.to_string())
    }
}

/// Server-pushed lifecycle event on a subscribed connection. Emitted
/// as one JSON object per line after the server has acknowledged
/// [`Request::Subscribe`] with [`Response::Subscribed`].
///
/// Delivery is **best-effort**: slow subscribers may miss events,
/// in which case the server synthesizes an [`Event::EventsDropped`]
/// meta-event. Consumers that need exact state should reconcile
/// with [`Request::List`] after reacting to a gap.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Emitted when a pane has been added to the active workspace.
    /// `name` is populated if the pane was given a stable IPC name
    /// (layout `id` or `renga split --id`). `role` is the free-form
    /// label set via Phase 1 mechanisms.
    PaneStarted {
        id: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<String>,
        ts_ms: u64,
    },
    /// Emitted exactly once per pane id when it is removed from the
    /// workspace (user-initiated close, tab close, or the underlying
    /// shell exiting).
    PaneExited {
        id: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<String>,
        ts_ms: u64,
    },
    /// Meta-event synthesized by the server when a slow subscriber
    /// has caused real events to be dropped. `count` is the number of
    /// events discarded since the last delivered event.
    EventsDropped { count: u64, ts_ms: u64 },
    /// Periodic keep-alive emitted while no real events are in flight.
    /// Its only purpose is to trigger a wire write so the server can
    /// detect half-closed connections (client dead but OS buffer still
    /// accepting). Clients can safely ignore it, or surface it as a
    /// liveness indicator.
    Heartbeat { ts_ms: u64 },
    /// A peer message destined for `target_pane`. Emitted by the server
    /// in response to a `Request::PeerSend` whose target resolved to a
    /// live pane — in any tab, since Issue #289 removed the same-tab
    /// restriction. Subscribers filter on `target_pane` to pick out
    /// their own inbox (pane ids are unique across the whole session,
    /// so the filter needs no tab awareness); all other subscribers
    /// ignore the event.
    PeerInbox {
        /// Pane the message is addressed to.
        target_pane: usize,
        /// Pane that originated the message.
        from_pane: usize,
        /// Sender's stable IPC name (if any). Clients display this in
        /// the channel notification meta so recipients know who is
        /// talking.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_name: Option<String>,
        /// Sender's registered peer client kind, if known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_kind: Option<PeerClientKind>,
        body: String,
        ts_ms: u64,
    },
}

impl Response {
    pub fn ok_unit() -> Self {
        Response::Ok {
            data: serde_json::Value::Null,
        }
    }
    pub fn ok_value(value: serde_json::Value) -> Self {
        Response::Ok { data: value }
    }
    pub fn err(message: impl Into<String>) -> Self {
        Response::Err {
            message: message.into(),
            code: None,
        }
    }
    pub fn err_coded(code: &'static str, message: impl Into<String>) -> Self {
        Response::Err {
            message: message.into(),
            code: Some(code.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(req: &Request) -> Request {
        let s = serde_json::to_string(req).unwrap();
        serde_json::from_str(&s).unwrap()
    }

    // ─── Issue #288 wire compatibility ────────────────────
    //
    // `from_pane` is an *optional* addition to five stable requests.
    // These tests are the proof that `docs/semver-policy.md` §3's
    // "required-input addition" line was not crossed: byte-identical
    // output for the old shape in, old input still decoding.

    /// `List` changed from a unit variant to a struct variant. That is
    /// only safe because a fully-skipped struct variant serializes to
    /// the same object an internally-tagged unit variant does.
    #[test]
    fn list_request_raw_json_shape_is_unchanged_without_from_pane() {
        let s = serde_json::to_string(&Request::List { from_pane: None }).unwrap();
        assert_eq!(s, r#"{"cmd":"list"}"#);
    }

    #[test]
    fn scoped_list_request_carries_from_pane() {
        let s = serde_json::to_string(&Request::List { from_pane: Some(7) }).unwrap();
        assert_eq!(s, r#"{"cmd":"list","from_pane":7}"#);
    }

    /// Verbatim payloads a pre-#288 client puts on the wire. All must
    /// decode, and all must land on the legacy `from_pane: None`
    /// semantics.
    #[test]
    fn pre_288_raw_requests_decode_as_legacy_active_tab_scope() {
        let cases: &[(&str, Request)] = &[
            (r#"{"cmd":"list"}"#, Request::List { from_pane: None }),
            (
                r#"{"cmd":"send","target":"focused","data":"hi","append_enter":true}"#,
                Request::Send {
                    target: PaneRef::Focused,
                    data: "hi".into(),
                    append_enter: true,
                    from_pane: None,
                },
            ),
            (
                r#"{"cmd":"focus","target":{"id":3}}"#,
                Request::Focus {
                    target: PaneRef::Id(3),
                    from_pane: None,
                },
            ),
            (
                r#"{"cmd":"inspect","target":"focused","lines":10,"include_cursor":false}"#,
                Request::Inspect {
                    target: PaneRef::Focused,
                    lines: Some(10),
                    include_cursor: false,
                    from_pane: None,
                },
            ),
            (
                r#"{"cmd":"split","target":"focused","direction":"vertical"}"#,
                Request::Split {
                    target: PaneRef::Focused,
                    direction: Direction::Vertical,
                    command: None,
                    id: None,
                    role: None,
                    cwd: None,
                    from_pane: None,
                    tab: None,
                },
            ),
        ];
        for (raw, expected) in cases {
            let parsed: Request =
                serde_json::from_str(raw).unwrap_or_else(|e| panic!("decode {raw}: {e}"));
            assert_eq!(&parsed, expected, "decoding {raw}");
        }
    }

    /// Shapes a client can legitimately put on the wire that are
    /// neither "old" nor "new": an explicit `null`, and a field this
    /// build does not know. Both must land on the legacy semantics
    /// rather than erroring — the struct-variant conversion of `List`
    /// must not have narrowed what the old unit variant accepted.
    #[test]
    fn list_tolerates_explicit_null_and_unknown_fields() {
        for raw in [
            r#"{"cmd":"list","from_pane":null}"#,
            r#"{"cmd":"list","some_future_field":1}"#,
        ] {
            let parsed: Request =
                serde_json::from_str(raw).unwrap_or_else(|e| panic!("decode {raw}: {e}"));
            assert_eq!(parsed, Request::List { from_pane: None }, "decoding {raw}");
        }
    }

    #[test]
    fn scoped_raw_requests_decode_with_from_pane() {
        let parsed: Request =
            serde_json::from_str(r#"{"cmd":"send","target":"focused","data":"x","from_pane":4}"#)
                .unwrap();
        assert_eq!(
            parsed,
            Request::Send {
                target: PaneRef::Focused,
                data: "x".into(),
                append_enter: false,
                from_pane: Some(4),
            }
        );
    }

    /// A pre-#288 server's hello has no `capabilities` key. It must
    /// still decode — that empty list is precisely the signal
    /// capability-gated clients fail closed on.
    #[test]
    fn pre_288_hello_response_decodes_with_no_capabilities() {
        let parsed: Response =
            serde_json::from_str(r#"{"status":"hello","server_pid":9,"session_token":"t"}"#)
                .unwrap();
        match parsed {
            Response::Hello { capabilities, .. } => assert!(capabilities.is_empty()),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn hello_response_advertises_caller_scope_and_omits_an_empty_list() {
        let with = serde_json::to_string(&Response::Hello {
            server_pid: 1,
            session_token: "t".into(),
            capabilities: SERVER_CAPABILITIES.iter().map(|s| s.to_string()).collect(),
        })
        .unwrap();
        assert!(
            with.contains(CAP_CALLER_SCOPE),
            "server must advertise caller scope: {with}"
        );
        assert!(
            with.contains(CAP_CROSS_TAB_PEERS),
            "server must advertise cross-tab peers: {with}"
        );

        let without = serde_json::to_string(&Response::Hello {
            server_pid: 1,
            session_token: "t".into(),
            capabilities: Vec::new(),
        })
        .unwrap();
        assert!(
            !without.contains("capabilities"),
            "an empty capability list stays off the wire: {without}"
        );
    }

    #[test]
    fn hello_response_advertises_spawn_tab() {
        let with = serde_json::to_string(&Response::Hello {
            server_pid: 1,
            session_token: "t".into(),
            capabilities: SERVER_CAPABILITIES.iter().map(|s| s.to_string()).collect(),
        })
        .unwrap();
        assert!(
            with.contains(CAP_SPAWN_TAB),
            "server must advertise tab-directed spawning: {with}"
        );
    }

    #[test]
    fn hello_response_advertises_caller_scope_close_identity() {
        let with = serde_json::to_string(&Response::Hello {
            server_pid: 1,
            session_token: "t".into(),
            capabilities: SERVER_CAPABILITIES.iter().map(|s| s.to_string()).collect(),
        })
        .unwrap();
        assert!(
            with.contains(CAP_CALLER_SCOPE_CLOSE_IDENTITY),
            "server must advertise caller-scoped close / rename: {with}"
        );
    }

    // ─── Issue #296 wire compatibility ────────────────────

    /// `from_pane` is optional on `Close` / `SetPaneIdentity` too, and
    /// must not leak onto the wire for the `renga` CLI, which never
    /// sets it.
    #[test]
    fn close_and_identity_raw_json_shape_is_unchanged_without_from_pane() {
        let close = serde_json::to_string(&Request::Close {
            target: PaneRef::Focused,
            from_pane: None,
        })
        .unwrap();
        assert_eq!(close, r#"{"cmd":"close","target":"focused"}"#);

        let identity = serde_json::to_string(&Request::SetPaneIdentity {
            target: PaneRef::Focused,
            name: None,
            role: None,
            from_pane: None,
        })
        .unwrap();
        assert!(!identity.contains("from_pane"), "{identity}");
    }

    #[test]
    fn scoped_close_and_identity_requests_carry_from_pane() {
        let close = serde_json::to_string(&Request::Close {
            target: PaneRef::Focused,
            from_pane: Some(7),
        })
        .unwrap();
        assert_eq!(close, r#"{"cmd":"close","target":"focused","from_pane":7}"#);

        let identity = serde_json::to_string(&Request::SetPaneIdentity {
            target: PaneRef::Focused,
            name: Some(Some("worker".into())),
            role: None,
            from_pane: Some(7),
        })
        .unwrap();
        assert!(identity.contains(r#""from_pane":7"#), "{identity}");
    }

    /// Verbatim payloads a pre-#296 client puts on the wire. Both must
    /// decode onto the legacy `from_pane: None` (cross-tab) semantics.
    #[test]
    fn pre_296_raw_requests_decode_as_legacy_cross_tab_scope() {
        let close: Request = serde_json::from_str(r#"{"cmd":"close","target":{"id":3}}"#).unwrap();
        assert_eq!(
            close,
            Request::Close {
                target: PaneRef::Id(3),
                from_pane: None,
            }
        );

        let identity: Request = serde_json::from_str(
            r#"{"cmd":"set_pane_identity","target":{"name":"worker"},"role":"lead"}"#,
        )
        .unwrap();
        assert_eq!(
            identity,
            Request::SetPaneIdentity {
                target: PaneRef::Name("worker".into()),
                name: None,
                role: Some(Some("lead".into())),
                from_pane: None,
            }
        );
    }

    #[test]
    fn close_request_roundtrips_with_from_pane() {
        for from_pane in [None, Some(4)] {
            let r = Request::Close {
                target: PaneRef::Name("worker".into()),
                from_pane,
            };
            assert_eq!(roundtrip(&r), r);
        }
    }

    /// The wire shapes the docs promise for the tab selector — one per
    /// variant, byte-exact, since MCP callers construct these by hand.
    #[test]
    fn tab_selector_wire_shapes() {
        let cases: &[(TabSelector, &str)] = &[
            (TabSelector::Name("workers".into()), r#"{"name":"workers"}"#),
            (TabSelector::Index(2), r#"{"index":2}"#),
            (TabSelector::PaneId(17), r#"{"pane_id":17}"#),
            (TabSelector::New { name: None }, r#"{"new":{}}"#),
            (
                TabSelector::New {
                    name: Some("workers".into()),
                },
                r#"{"new":{"name":"workers"}}"#,
            ),
        ];
        for (selector, wire) in cases {
            let ser = serde_json::to_string(selector).unwrap();
            assert_eq!(&ser, wire);
            let de: TabSelector = serde_json::from_str(wire).unwrap();
            assert_eq!(&de, selector);
        }
    }

    #[test]
    fn split_request_with_tab_roundtrips() {
        for tab in [
            TabSelector::Name("workers".into()),
            TabSelector::Index(0),
            TabSelector::PaneId(3),
        ] {
            let r = Request::Split {
                target: PaneRef::Focused,
                direction: Direction::Vertical,
                command: None,
                id: None,
                role: None,
                cwd: None,
                from_pane: Some(1),
                tab: Some(tab),
            };
            assert_eq!(roundtrip(&r), r);
        }
    }

    /// With `tab: None` the split request's raw JSON is byte-identical
    /// to what a pre-#290 client sends — the added field must never
    /// leak onto the wire for callers that don't use it.
    #[test]
    fn split_request_raw_json_shape_is_unchanged_without_tab() {
        let r = Request::Split {
            target: PaneRef::Focused,
            direction: Direction::Vertical,
            command: None,
            id: None,
            role: None,
            cwd: None,
            from_pane: None,
            tab: None,
        };
        let ser = serde_json::to_string(&r).unwrap();
        assert!(!ser.contains("tab"), "tab must stay off the wire: {ser}");
        assert!(!ser.contains("from_pane"), "{ser}");
    }

    #[test]
    fn spawn_tab_request_roundtrips() {
        let r = Request::SpawnTab {
            command: Some("claude".into()),
            id: Some("worker-a".into()),
            label: Some("workers".into()),
            role: Some("worker".into()),
            cwd: Some("/tmp/work".into()),
            from_pane: Some(2),
        };
        assert_eq!(roundtrip(&r), r);
    }

    #[test]
    fn spawn_tab_request_defaults_all_fields() {
        let parsed: Request = serde_json::from_str(r#"{"cmd":"spawn_tab"}"#).unwrap();
        assert_eq!(
            parsed,
            Request::SpawnTab {
                command: None,
                id: None,
                label: None,
                role: None,
                cwd: None,
                from_pane: None,
            }
        );
    }

    #[test]
    fn list_request_roundtrips() {
        assert_eq!(
            roundtrip(&Request::List { from_pane: None }),
            Request::List { from_pane: None }
        );
    }

    #[test]
    fn send_request_roundtrips_with_enter() {
        let r = Request::Send {
            target: PaneRef::Name("engineering".into()),
            data: "hello".into(),
            append_enter: true,
            from_pane: None,
        };
        assert_eq!(roundtrip(&r), r);
    }

    #[test]
    fn split_request_roundtrips() {
        let r = Request::Split {
            target: PaneRef::Focused,
            direction: Direction::Vertical,
            command: Some("cce".into()),
            id: Some("engineering".into()),
            role: None,
            cwd: None,
            from_pane: None,
            tab: None,
        };
        assert_eq!(roundtrip(&r), r);
    }

    #[test]
    fn pane_ref_id_serializes_with_numeric() {
        let s = serde_json::to_string(&PaneRef::Id(7)).unwrap();
        assert!(s.contains("\"id\""), "{s}");
        assert!(s.contains("7"), "{s}");
    }

    #[test]
    fn pane_ref_focused_serializes_with_unit() {
        let s = serde_json::to_string(&PaneRef::Focused).unwrap();
        assert!(s.contains("focused"), "{s}");
    }

    #[test]
    fn unknown_command_fails_to_parse() {
        let bad = r#"{"cmd":"explode","target":{"focused":null}}"#;
        let parsed: Result<Request, _> = serde_json::from_str(bad);
        assert!(parsed.is_err());
    }

    #[test]
    fn response_err_serializes_message() {
        let r = Response::err("nope");
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"err\""), "{s}");
        assert!(s.contains("nope"), "{s}");
    }

    #[test]
    fn response_ok_unit_has_null_data() {
        let r = Response::ok_unit();
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"ok\""), "{s}");
        assert!(s.contains("null"), "{s}");
    }

    #[test]
    fn hello_request_carries_pid() {
        let r = Request::Hello { client_pid: 42 };
        assert_eq!(roundtrip(&r), r);
    }

    #[test]
    fn new_tab_request_roundtrips() {
        let r = Request::NewTab {
            command: Some("cce".into()),
            id: Some("engineering".into()),
            label: Some("eng".into()),
            role: None,
            cwd: None,
        };
        assert_eq!(roundtrip(&r), r);
    }

    #[test]
    fn new_tab_request_defaults_all_fields() {
        let minimal = r#"{"cmd":"new_tab"}"#;
        let parsed: Request = serde_json::from_str(minimal).unwrap();
        match parsed {
            Request::NewTab {
                command: None,
                id: None,
                label: None,
                role: None,
                cwd: None,
            } => {}
            other => panic!("expected empty NewTab, got {other:?}"),
        }
    }

    #[test]
    fn set_pane_identity_missing_fields_default_to_keep() {
        // Bare payload with just `target` must deserialize to both
        // name / role in "keep" state (outer None). Prevents a regression
        // where `#[serde(default)]` would be dropped from the field and
        // every call implicitly cleared the values.
        let raw = r#"{"cmd":"set_pane_identity","target":{"focused":null}}"#;
        let parsed: Request = serde_json::from_str(raw).unwrap();
        match parsed {
            Request::SetPaneIdentity {
                target,
                name: None,
                role: None,
                from_pane: None,
            } => {
                assert!(matches!(target, PaneRef::Focused));
            }
            other => panic!("expected SetPaneIdentity keep/keep, got {other:?}"),
        }
    }

    #[test]
    fn set_pane_identity_null_means_clear() {
        let raw =
            r#"{"cmd":"set_pane_identity","target":{"focused":null},"name":null,"role":null}"#;
        let parsed: Request = serde_json::from_str(raw).unwrap();
        match parsed {
            Request::SetPaneIdentity {
                name: Some(None),
                role: Some(None),
                ..
            } => {}
            other => panic!("expected clear/clear, got {other:?}"),
        }
    }

    #[test]
    fn set_pane_identity_string_means_set() {
        let raw = r#"{"cmd":"set_pane_identity","target":{"focused":null},"name":"secretary","role":"leader"}"#;
        let parsed: Request = serde_json::from_str(raw).unwrap();
        match parsed {
            Request::SetPaneIdentity {
                name: Some(Some(n)),
                role: Some(Some(r)),
                ..
            } => {
                assert_eq!(n, "secretary");
                assert_eq!(r, "leader");
            }
            other => panic!("expected set/set, got {other:?}"),
        }
    }

    #[test]
    fn set_pane_identity_omits_keys_when_keep_keep() {
        // Regression guard: `skip_serializing_if = "Option::is_none"`
        // on name / role must keep omit semantics on the wire. If a
        // future refactor drops the attribute, a server round-tripping
        // a "leave unchanged" record would silently turn it into
        // "clear both" — `null` deserializes to Some(None) via the
        // double_option helper.
        let r = Request::SetPaneIdentity {
            target: PaneRef::Focused,
            name: None,
            role: None,
            from_pane: None,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(!s.contains("\"name\""), "name key leaked: {s}");
        assert!(!s.contains("\"role\""), "role key leaked: {s}");
    }

    #[test]
    fn set_pane_identity_roundtrips() {
        let r = Request::SetPaneIdentity {
            target: PaneRef::Name("worker".into()),
            name: Some(Some("renamed".into())),
            role: Some(None),
            from_pane: Some(4),
        };
        assert_eq!(roundtrip(&r), r);
    }

    #[test]
    fn split_request_with_cwd_roundtrips() {
        let r = Request::Split {
            target: PaneRef::Focused,
            direction: Direction::Horizontal,
            command: Some("claude".into()),
            id: None,
            role: None,
            cwd: Some("/tmp/work".into()),
            from_pane: None,
            tab: None,
        };
        assert_eq!(roundtrip(&r), r);
    }

    #[test]
    fn new_tab_request_with_cwd_roundtrips() {
        let r = Request::NewTab {
            command: None,
            id: None,
            label: None,
            role: None,
            cwd: Some("/tmp/work".into()),
        };
        assert_eq!(roundtrip(&r), r);
    }

    #[test]
    fn hello_response_carries_token() {
        let r = Response::Hello {
            server_pid: 100,
            session_token: "abc".into(),
            capabilities: Vec::new(),
        };
        let parsed: Response = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(parsed, r);
    }

    #[test]
    fn pane_info_role_is_omitted_when_none() {
        let info = PaneInfo {
            id: 1,
            name: None,
            role: None,
            focused: false,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            cwd: None,
            kind: None,
            receive_mode: None,
            summary: None,
        };
        let s = serde_json::to_string(&info).unwrap();
        assert!(!s.contains("role"), "unexpected role field: {s}");
    }

    #[test]
    fn pane_info_role_roundtrips_when_present() {
        let info = PaneInfo {
            id: 1,
            name: Some("president".into()),
            role: Some("leader".into()),
            focused: true,
            x: 0,
            y: 0,
            width: 80,
            height: 24,
            cwd: Some("/home/user/project".into()),
            kind: Some(PeerClientKind::Claude),
            receive_mode: Some(PeerReceiveMode::Push),
            summary: None,
        };
        let parsed: PaneInfo =
            serde_json::from_str(&serde_json::to_string(&info).unwrap()).unwrap();
        assert_eq!(parsed, info);
    }

    #[test]
    fn pane_info_rect_fields_roundtrip() {
        let info = PaneInfo {
            id: 7,
            name: Some("editor".into()),
            role: None,
            focused: false,
            x: 3,
            y: 1,
            width: 120,
            height: 40,
            cwd: None,
            kind: None,
            receive_mode: None,
            summary: None,
        };
        let s = serde_json::to_string(&info).unwrap();
        assert!(s.contains("\"x\":3"), "missing x: {s}");
        assert!(s.contains("\"y\":1"), "missing y: {s}");
        assert!(s.contains("\"width\":120"), "missing width: {s}");
        assert!(s.contains("\"height\":40"), "missing height: {s}");
        let parsed: PaneInfo = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed, info);
    }

    #[test]
    fn pane_info_without_rect_fields_deserializes_to_zero() {
        // Older clients may emit JSON without x/y/width/height. Serde
        // defaults should fill those with 0 so the type stays
        // backward-compatible with pre-#80 payloads.
        let legacy = r#"{"id":2,"focused":true}"#;
        let parsed: PaneInfo = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.id, 2);
        assert!(parsed.focused);
        assert_eq!(parsed.x, 0);
        assert_eq!(parsed.y, 0);
        assert_eq!(parsed.width, 0);
        assert_eq!(parsed.height, 0);
    }

    #[test]
    fn split_request_with_role_roundtrips() {
        let r = Request::Split {
            target: PaneRef::Focused,
            direction: Direction::Vertical,
            command: None,
            id: None,
            role: Some("worker".into()),
            cwd: None,
            from_pane: None,
            tab: None,
        };
        assert_eq!(roundtrip(&r), r);
    }

    #[test]
    fn new_tab_request_with_role_roundtrips() {
        let r = Request::NewTab {
            command: None,
            id: None,
            label: None,
            role: Some("leader".into()),
            cwd: None,
        };
        assert_eq!(roundtrip(&r), r);
    }

    #[test]
    fn subscribe_request_roundtrips() {
        assert_eq!(roundtrip(&Request::Subscribe), Request::Subscribe);
    }

    #[test]
    fn subscribed_response_roundtrips() {
        let parsed: Response =
            serde_json::from_str(&serde_json::to_string(&Response::Subscribed).unwrap()).unwrap();
        assert_eq!(parsed, Response::Subscribed);
    }

    #[test]
    fn pane_started_event_roundtrips() {
        let ev = Event::PaneStarted {
            id: 3,
            name: Some("foreman".into()),
            role: Some("foreman".into()),
            ts_ms: 1_700_000_000_000,
        };
        let parsed: Event = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        assert_eq!(parsed, ev);
    }

    #[test]
    fn pane_exited_event_omits_optional_fields_when_none() {
        let ev = Event::PaneExited {
            id: 5,
            name: None,
            role: None,
            ts_ms: 42,
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(!s.contains("\"name\""), "should omit name: {s}");
        assert!(!s.contains("\"role\""), "should omit role: {s}");
    }

    #[test]
    fn heartbeat_event_roundtrips() {
        let ev = Event::Heartbeat { ts_ms: 123 };
        let parsed: Event = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        assert_eq!(parsed, ev);
    }

    #[test]
    fn response_err_code_is_omitted_when_none() {
        let r = Response::err("plain");
        let s = serde_json::to_string(&r).unwrap();
        assert!(!s.contains("\"code\""), "should omit code: {s}");
    }

    #[test]
    fn response_err_coded_roundtrips() {
        let r = Response::err_coded(err_code::PROTOCOL, "boom");
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"code\""), "{s}");
        assert!(s.contains("protocol"), "{s}");
        let parsed: Response = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed, r);
    }

    #[test]
    fn response_err_without_code_parses_into_none() {
        // Pre-coded-protocol clients / servers don't emit `code` at
        // all. Confirm we round-trip their payload into Err.code = None
        // without rejecting the message.
        let legacy = r#"{"status":"err","message":"older peer"}"#;
        let parsed: Response = serde_json::from_str(legacy).unwrap();
        match parsed {
            Response::Err { message, code } => {
                assert_eq!(message, "older peer");
                assert_eq!(code, None);
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn coded_error_display_includes_code_prefix() {
        let e = CodedError::new(err_code::PANE_NOT_FOUND, "pane not found: Id(3)");
        let s = e.to_string();
        assert!(s.contains("[pane_not_found]"), "{s}");
        assert!(s.contains("pane not found"), "{s}");
    }

    #[test]
    fn coded_error_uncoded_display_has_no_prefix() {
        let e = CodedError::uncoded("plain message");
        assert_eq!(e.to_string(), "plain message");
    }

    #[test]
    fn coded_error_from_string_is_uncoded() {
        let e: CodedError = String::from("boom").into();
        assert_eq!(e.code, None);
        assert_eq!(e.message, "boom");
    }

    #[test]
    fn coded_error_from_str_is_uncoded() {
        let e: CodedError = "boom".into();
        assert_eq!(e.code, None);
        assert_eq!(e.message, "boom");
    }

    #[test]
    fn coded_error_into_response_preserves_code() {
        let e = CodedError::new(err_code::SPLIT_REFUSED, "too small");
        match e.into_response() {
            Response::Err { message, code } => {
                assert_eq!(message, "too small");
                assert_eq!(code.as_deref(), Some(err_code::SPLIT_REFUSED));
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn coded_error_into_response_omits_missing_code() {
        let e = CodedError::uncoded("no code");
        match e.into_response() {
            Response::Err { message, code } => {
                assert_eq!(message, "no code");
                assert_eq!(code, None);
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn events_dropped_meta_event_roundtrips() {
        let ev = Event::EventsDropped {
            count: 17,
            ts_ms: 1,
        };
        let parsed: Event = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        assert_eq!(parsed, ev);
    }

    #[test]
    fn inspect_request_roundtrips_with_defaults() {
        let r = Request::Inspect {
            target: PaneRef::Focused,
            lines: None,
            include_cursor: false,
            from_pane: None,
        };
        assert_eq!(roundtrip(&r), r);
    }

    #[test]
    fn inspect_request_roundtrips_with_lines_and_cursor() {
        let r = Request::Inspect {
            target: PaneRef::Name("worker-foo".into()),
            lines: Some(4),
            include_cursor: true,
            from_pane: None,
        };
        assert_eq!(roundtrip(&r), r);
    }

    #[test]
    fn inspect_request_accepts_minimal_json() {
        // `lines` and `include_cursor` default so a minimal
        // `{cmd: inspect, target: ...}` must still parse.
        let minimal = r#"{"cmd":"inspect","target":{"focused":null}}"#;
        let parsed: Request = serde_json::from_str(minimal).unwrap();
        match parsed {
            Request::Inspect {
                target: PaneRef::Focused,
                lines: None,
                include_cursor: false,
                from_pane: None,
            } => {}
            other => panic!("expected Inspect defaults, got {other:?}"),
        }
    }

    #[test]
    fn peer_list_request_roundtrips() {
        let r = Request::PeerList { from_pane: 3 };
        assert_eq!(roundtrip(&r), r);
    }

    #[test]
    fn peer_send_request_roundtrips() {
        let r = Request::PeerSend {
            from_pane: 1,
            target: PaneRef::Name("worker".into()),
            body: "hi".into(),
        };
        assert_eq!(roundtrip(&r), r);
    }

    fn peer_info_with_ids_only(id: usize) -> PeerInfo {
        PeerInfo {
            id,
            name: None,
            role: None,
            tab: None,
            tab_name: None,
            same_tab: None,
            cwd: None,
            kind: None,
            receive_mode: None,
            summary: None,
        }
    }

    #[test]
    fn peer_info_tab_fields_roundtrip() {
        let info = PeerInfo {
            name: Some("worker".into()),
            tab: Some(2),
            tab_name: Some("renga".into()),
            same_tab: Some(false),
            ..peer_info_with_ids_only(7)
        };
        let s = serde_json::to_string(&info).unwrap();
        let parsed: PeerInfo = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed, info);
    }

    #[test]
    fn peer_info_omits_tab_fields_when_none() {
        // Additive serde: a server talking to a pre-#289 client must
        // not emit `tab: null` keys the old decoder never asked for.
        let s = serde_json::to_string(&peer_info_with_ids_only(1)).unwrap();
        for key in ["tab", "tab_name", "same_tab"] {
            assert!(!s.contains(key), "must omit {key}: {s}");
        }
    }

    #[test]
    fn peer_info_deserializes_legacy_payload_without_tab_fields() {
        // New client × pre-#289 server: the tab fields are simply
        // absent and must decode to None, not fail the whole
        // `Vec<PeerInfo>` decode.
        let raw = r#"{"id":4,"name":"worker"}"#;
        let info: PeerInfo = serde_json::from_str(raw).unwrap();
        assert_eq!(info.tab, None);
        assert_eq!(info.tab_name, None);
        assert_eq!(info.same_tab, None);
    }

    #[test]
    fn peer_info_ignores_unknown_future_fields() {
        // Old client × new server relies on serde's default
        // ignore-unknown-fields behavior; guard it so nobody adds
        // `deny_unknown_fields` and breaks the forward path.
        let raw = r#"{"id":4,"tab":1,"tab_name":"kura","same_tab":false,"future_field":true}"#;
        let info: PeerInfo = serde_json::from_str(raw).unwrap();
        assert_eq!(info.id, 4);
        assert_eq!(info.tab, Some(1));
    }

    #[test]
    fn peer_inbox_event_roundtrips() {
        let ev = Event::PeerInbox {
            target_pane: 2,
            from_pane: 1,
            from_name: Some("leader".into()),
            from_kind: Some(PeerClientKind::Claude),
            body: "ping".into(),
            ts_ms: 42,
        };
        let parsed: Event = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        assert_eq!(parsed, ev);
    }

    #[test]
    fn set_summary_request_roundtrips() {
        let r = Request::SetSummary {
            from_pane: 7,
            summary: "drafting design doc".into(),
        };
        assert_eq!(roundtrip(&r), r);
    }

    #[test]
    fn set_summary_empty_string_roundtrips() {
        // Empty string is the documented "clear" form; must survive
        // the wire as-is so the App handler can detect it.
        let r = Request::SetSummary {
            from_pane: 1,
            summary: String::new(),
        };
        assert_eq!(roundtrip(&r), r);
    }

    #[test]
    fn pane_info_omits_summary_when_none() {
        // additive serde: clients on old protocol must not see a new
        // `summary: null` key — `skip_serializing_if = Option::is_none`
        // guarantees omission.
        let info = PaneInfo {
            id: 1,
            name: None,
            role: None,
            focused: false,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            cwd: None,
            kind: None,
            receive_mode: None,
            summary: None,
        };
        let s = serde_json::to_string(&info).unwrap();
        assert!(!s.contains("summary"), "must omit summary key: {s}");
    }

    #[test]
    fn pane_info_includes_summary_when_set() {
        let info = PaneInfo {
            id: 1,
            name: None,
            role: None,
            focused: false,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            cwd: None,
            kind: None,
            receive_mode: None,
            summary: Some("hello".into()),
        };
        let s = serde_json::to_string(&info).unwrap();
        assert!(s.contains("\"summary\":\"hello\""), "{s}");
    }

    #[test]
    fn pane_info_deserializes_legacy_payload_without_summary() {
        // Old servers that don't emit `summary` must still deserialize.
        // Guards the additive-change promise from semver-policy.md.
        let raw = r#"{"id":1,"focused":false,"x":0,"y":0,"width":0,"height":0}"#;
        let info: PaneInfo = serde_json::from_str(raw).unwrap();
        assert!(info.summary.is_none());
    }

    #[test]
    fn peer_inbox_event_omits_name_when_none() {
        let ev = Event::PeerInbox {
            target_pane: 5,
            from_pane: 6,
            from_name: None,
            from_kind: None,
            body: "no name".into(),
            ts_ms: 1,
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(!s.contains("\"from_name\""), "should omit from_name: {s}");
    }
}
