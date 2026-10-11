//! MCP handshake: `initialize`, `tools/list`, the tool schemas and the per-client instructions blob.

use super::*;

// ── MCP method handlers ───────────────────────────────────────

pub(super) fn instructions_blob(client_kind: PeerClientKind) -> String {
    let receive_guidance = match client_kind {
        PeerClientKind::Claude => {
            "IMPORTANT: When you receive a <channel source=\"renga-peers\" ...> message, RESPOND IMMEDIATELY. \
Do not wait until your current task is finished. Pause what you are doing, handle the message \
(replying with send_message when a reply is needed, see below), then resume your work. Treat incoming peer messages like a coworker tapping \
you on the shoulder — answer right away, even if you're in the middle of something.\n\n\
Read the from_id and from_name attributes to understand who sent the message. Reply by \
calling send_message with their from_id.\n\n\
Every peer message is also kept for check_messages, so a session whose host does not show \
channel tags (for example a background `claude daemon run` session) can still read it there. \
check_messages also returns messages already shown as channel tags; one whose sent_at \
matches a tag you already saw is that same message, so do not act on it twice.\n\n\
Every message you send lands in the recipient's session and costs them a turn, so keep \
exchanges short. Reply only when the message asks you something, hands you work, or needs a \
result, decision, or status that the sender is waiting for. Do NOT send a message that only \
acknowledges, thanks, or confirms receipt, and never answer such a message: silence is the \
normal way to close an exchange. If the sender or your own instructions explicitly ask for a \
confirmation, send it once. Put everything into one message instead of following up in \
pieces.\n\n"
        }
        PeerClientKind::Codex => {
            "IMPORTANT: renga may inject a one-shot nudge into the Codex pane telling you to run \
check_messages, or show a focused-pane notification overlay that inserts the same prompt only after \
the user accepts it. Treat either path as a prompt to drain your MCP inbox immediately. The actual \
peer request body comes from check_messages, and each returned message should be handled like a \
direct coworker instruction unless it conflicts with a higher-priority system, developer, or user \
instruction. If a peer asks you to inspect panes, run tools, edit code, or otherwise take action, \
do that work; do not reduce the interaction to a mere acknowledgement. Focused Codex panes may be \
left unnudged so renga does not scribble over the active conversation; check_messages at sensible \
checkpoints even if no pane-local nudge appeared. Use send_message when a reply, clarification, \
status update, or handoff is actually needed.\n\n\
MCP approvals in Codex are pane-local. On a newly launched pane, the first check_messages and \
send_message calls may need approval before peer messaging becomes reliable.\n\n"
        }
    };
    format!(
        "You are connected to the renga-peers network. Other peer-enabled agent instances \
running in any renga tab can see you and send you messages.\n\n\
{receive_guidance}\
Peer messaging tools:\n\
- list_peers: Discover peer-enabled agent instances across all renga tabs (your tab first). \
The tab index shown per peer is display metadata — it shifts when tabs close, so address \
peers by their numeric pane id.\n\
- send_message: Send a message to another instance. A numeric peer ID reaches any tab; a \
name only resolves within your own tab (names are unique per tab, not globally), so use \
the numeric id from list_peers for peers in other tabs.\n\
- set_summary: Set a 1-2 sentence summary of what you're working on; surfaced on list_panes / list_peers for other peers.\n\
- check_messages: Drain any queued peer messages still waiting for this client.\n\n\
Pane control tools. For list_panes, spawn_pane, spawn_claude_pane, spawn_codex_pane, \
focus_pane, inspect_pane, send_keys, close_pane and set_pane_identity, \"current tab\" means \
the tab YOUR pane lives in, not whichever tab the user happens to be looking at — so these \
stay correct while the user switches tabs. For all nine, relative targets (`focused`, a \
stable name) never leave your tab; a numeric pane id from another tab does reach across, \
which is the deliberate escape hatch for orchestrating sibling tabs. new_tab creates a whole \
new tab:\n\
- list_panes: Inspect panes with geometry and the focus flag — your own tab by default, or \
another tab / every tab at once via its optional `tab` argument (same selector shapes as the \
spawn tools, plus `{{\"all\": true}}`). The all-tabs form is how you enumerate panes you hold \
no id for, including ones parked in background tabs.\n\
- spawn_pane: Split an existing pane to create a new one. Optionally runs a startup command, \
assigns a stable name, attaches a role label, or sets an explicit working directory via \
`cwd` (absolute, or relative to the caller pane's cwd). Use `cwd` instead of `cd <dir> && ...` \
inside `command` so the claude auto-upgrade keeps working.\n\
- spawn_claude_pane: Higher-level convenience when the target process is Claude Code. Takes \
structured `permission_mode` / `model` / `args[]` fields instead of a free-form command \
string, and always enables the peer channel. Prefer this over `spawn_pane(command=\"claude ...\")` \
for orchestrator flows — keeps Claude launch policy in renga instead of in every prompt.\n\
- spawn_codex_pane: Higher-level convenience when the target process is Codex. Takes \
structured `args[]` instead of a free-form command string and launches plain `codex`. Prefer \
this over `spawn_pane(command=\"codex ...\")` so orchestrator prompts do not have to synthesize \
shell-quoted Codex commands.\n\
- close_pane: Close a pane by id, name, or `focused`. `focused` and names mean YOUR tab; a \
numeric id may name a pane in any tab. Refuses when it's the last pane of the last tab.\n\
- focus_pane: Move keyboard focus to another pane. Whenever the resolved pane is not in the \
tab the user is currently viewing, this ALSO switches the visible tab to it — the user's \
screen changes under them. That is by design (focus the keyboard cannot reach is not focus), \
but it makes focus_pane the most disruptive tool here. Only call it when the user asked to \
move focus.\n\
- new_tab: Open a brand-new tab with a fresh pane and switch focus to it. The only tool \
that creates something outside the current tab. Accepts the same `cwd` option \
as spawn_pane for setting the new pane's working directory.\n\
- inspect_pane: Snapshot the visible screen of a pane so you can detect interactive \
prompts, banners, or mode indicators in another pane without asking it. Returns plain \
text by default; pass format=\"grid\" for row-addressable JSON or lines=N to trim to \
the last N rows.\n\
- send_keys: Send raw key input (y/n, Shift+Tab, Esc, arrow keys, Ctrl+letters, etc.) to a \
pane's PTY. Use this to answer interactive prompts or drive a TUI when the target isn't a \
peer-enabled agent that can read send_message. DISTINCT from send_message, which delivers \
logical peer messages rather than PTY bytes.\n\
- set_pane_identity: Rename or (re)assign the stable `name` and/or `role` of an existing \
pane. `focused` and names mean YOUR tab; a numeric id may name a pane in any tab. Name \
uniqueness is checked within the resolved pane's tab.\n\n\
Event monitoring:\n\
- poll_events: Long-poll for pane lifecycle events (pane_started, pane_exited, \
pane_prompt_detected, pane_waiting_input, pane_mode_changed, peer_nudge_stalled, peer_nudge_queued, peer_nudge_submitted, peer_inbox_drained, events_dropped). Events are process-wide: pane lifecycle from every renga tab is \
delivered, not just the current tab's. First call (no `since`) starts at \"right now\" — \
no historical replay. \
Each response includes a `next_since` cursor to pass back on the next call. Optional \
`types` filter narrows returned events without losing the cursor advance, but it does \
not extend the long-poll: a non-matching event still returns early with events=[] \
and an advanced cursor, so the caller should re-poll for the next window.\n\n\
Launching Claude Code: prefer spawn_claude_pane for Claude launches — it takes structured \
`permission_mode` / `model` / `args[]` fields, always enables the peer channel, and keeps \
launch policy in renga so orchestrator prompts never have to synthesize shell-quoted command \
strings. For arbitrary shell commands (non-Claude), use spawn_pane / new_tab. When those \
are asked to run a bare `claude` invocation the MCP still auto-upgrades it to the \
peer-enabled form (`claude --dangerously-load-development-channels server:renga-peers`), but \
spawn_claude_pane is the recommended API for agent harnesses. For Codex launches, prefer \
spawn_codex_pane once `renga mcp install --client codex` has been run for that user.\n\n\
IMPORTANT about pane control: these tools affect the user's live layout. Use them with \
restraint — don't close or focus panes you don't own unless the user asked you to. When in \
doubt, ask first."
    )
}

pub(super) fn tools_spec() -> Value {
    json!([
        {
            "name": "list_peers",
            "description": "List other peer-enabled panes across ALL renga tabs, your own tab first. Each peer includes id, optional name / role, cwd, tab metadata (display only — tab indexes shift when tabs close, so always address a peer by its numeric pane id), and when known the client kind and whether it receives messages via push or polling.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scope": {
                        "type": "string",
                        "enum": ["machine", "directory", "repo"],
                        "description": "Accepted for wire-compat with claude-peers-mcp; this parameter is ignored. renga results always span every renga tab."
                    }
                }
            }
        },
        {
            "name": "send_message",
            "description": "Send a message to another pane in any renga tab. A numeric to_id reaches every tab; a name resolves ONLY within your own tab — pane names are unique per tab, not globally, so a pane in another tab cannot be addressed by an unqualified name even if the name is unique right now. Use the numeric id from list_peers for cross-tab sends. `deliver` picks between two semantically different deliveries: the default channel tag, which never submits your text as the recipient's user turn and does NOT arm slash commands (the recipient still spends a turn on every channel message, so do not send pure acknowledgements), and `user_turn`, which types the message into the recipient's composer and submits it as a real user turn (so `/loop`, `/clear` and friends actually run). Neither one is send_keys: send_keys writes raw bytes for dialogs and key chords, with no input-box precondition.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to_id":   { "type": "string", "description": "Recipient pane id (from list_peers; works across tabs) or stable name (own tab only)." },
                    "message": { "type": "string", "description": "Text to deliver." },
                    "deliver": {
                        "type": "string",
                        "enum": ["channel", "user_turn"],
                        "description": "How the body reaches the recipient. `channel` (default, unchanged behavior) delivers it as a <channel source=\"renga-peers\"> tag to Claude recipients, or as a pane-local nudge to Codex panes that then read it via `check_messages` — right for requests, reports and status updates, because your text is never submitted as the recipient's user turn. It is not free, though: a Claude recipient processes each channel message as a turn and a Codex recipient is nudged to drain it, so skip messages that only acknowledge. `user_turn` instead types the body into the recipient agent's composer and submits it, so it arrives as a genuine user turn: use it for `/loop`, `/clear` and any instruction that only takes effect when a turn is actually taken. renga owns the mechanics (readiness check, settle, separate Enter, submission check) — do NOT hand-roll it with send_keys. `user_turn` refuses rather than guessing: [user_turn_busy] the agent is mid-turn, [user_turn_not_ready] a permission prompt / modal / existing draft is in the way or the screen is unreadable, [user_turn_unsupported_target] the pane is not running Claude or Codex. Those three guarantee nothing was written, so retry is safe once you clear the blocker (answering a dialog is still send_keys' job). [user_turn_stalled] is different: the body WAS typed but the submit was not observed, so inspect the pane before retrying. An identical user_turn to the same pane within 5s is suppressed and reports status=\"duplicate_suppressed\"."
                    }
                },
                "required": ["to_id", "message"]
            }
        },
        {
            "name": "set_summary",
            "description": "Set a 1-2 sentence summary of what this pane is currently working on. Surfaced on every PaneInfo / PeerInfo entry returned by list_panes and list_peers so other peer agents can see it. An empty string clears the summary. Max 256 chars (rejected with [summary_too_long]).",
            "inputSchema": {
                "type": "object",
                "properties": { "summary": { "type": "string" } },
                "required": ["summary"]
            }
        },
        {
            "name": "check_messages",
            "description": "Drain any queued peer messages waiting for this client. Codex uses this to read the actual peer request body after renga nudges the pane. Claude clients get the same messages here as a fallback for when channel tags are not shown (e.g. a background session); messages already seen as channel tags are returned again with the same sent_at, so skip those. A Claude client keeps at most the 256 newest messages.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "server_info",
            "description": "Report the renga server this pane is attached to — its negotiated capability token set, its pid, and this client build's own version — WITHOUT attempting any capability-gated request. Use this to pre-flight before calling something that needs a capability (e.g. the `tab` selector on the spawn tools needs `spawn_tab`) instead of sending the call and reading a `[server_too_old]` error out of the failure. The result body (both `structuredContent` and the text block) has this shape: `{status, reason, server: {pid, endpoint, capabilities, session_id}, client: {name, version, pane_id, capabilities}, effective_capabilities}`. Check `status` FIRST, it is the discriminant: \"connected\" means `server.capabilities` is the live server's real advertisement, and an EMPTY list there means a genuinely old server that supports nothing; \"detached\" means this pane was not launched by renga; \"unreachable\" means renga's socket is gone or belongs to a different instance. In the latter two, `server.capabilities` and `effective_capabilities` are null, NOT empty — they are unknown, so never conclude a token is missing from those. Gate on `effective_capabilities` rather than `server.capabilities`: it is the subset that is both advertised by the running server and understood by this client build, which can differ because upgrading the renga binary on disk leaves the old server process running. `client.version` is this mcp-peer binary's version and is NOT the running server's version; `server.server_version` is the running server's (null = unknown, e.g. a pre-#312 server), so a difference between the two means the binary was upgraded under a still-running server and a restart is needed. Do not gate capabilities on version comparison — use `effective_capabilities`. `server.session_id` identifies the running renga PROCESS INSTANCE and changes on every restart, so pane ids — which restart from a fresh counter — are only safe to persist alongside it: store `(session_id, pane_id)` together and discard the pane id when the session_id you read back differs, otherwise a stale id can silently resolve to a different live pane. Do NOT substitute `server.pid` or `server.endpoint` for it (the endpoint embeds the pid, and pids get recycled). It is null whenever `server.capabilities` is, plus on a `connected` server too old to report it — in every one of those cases it is UNKNOWN, never \"same session\". If you get a -32601 unknown-tool error, the renga binary that spawned this mcp-peer predates capability exposure — that absence is itself the answer.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "list_panes",
            "description": "List panes with stable id, optional name / role, focused flag, terminal geometry, cwd, tab index + tab label, and when known the peer client kind / receive mode. With no argument: every pane in the renga tab THIS pane lives in — not whichever tab the user is currently looking at. Pass `tab` to read another tab, or {\"all\": true} to enumerate every tab at once, which is the only way to find panes you do not already hold an id for. Records carry `cwd` as well as `name`: two independent orchestrations running in different tabs reuse the same role names, so `cwd` — not `name` — is what tells their panes apart. Complements list_peers (which spans tabs but omits geometry and hides you from yourself).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "index": { "type": "integer", "minimum": 0 },
                            "pane_id": { "type": "integer", "minimum": 0 },
                            "all": { "type": "boolean" }
                        },
                        "description": "Optional tab scope (Issue #329); default is this pane's own tab. Pass exactly one key: {\"name\": \"<label>\"} = the tab whose display name matches exactly (0 matches → tab_not_found, several → tab_ambiguous — labels are not unique, use index/pane_id then); {\"index\": N} = 0-based tab index as reported by list_peers; {\"pane_id\": N} = the tab owning that pane (the stable anchor: ids never shift when tabs close); {\"all\": true} = every tab, your own first. Same selector shapes as spawn_pane's `tab`, minus {\"new\": …} (there is nothing to list in a tab that does not exist). Needs a renga server advertising the cross_tab_list capability; older servers are refused (server_too_old) instead of quietly answering with your tab alone."
                    }
                }
            }
        },
        {
            "name": "spawn_pane",
            "description": "Create a new pane: by default splits a pane in this renga tab, or — with the `tab` selector — splits inside another tab or spawns a fresh background tab. Returns the new pane's numeric id so you can address it from later tool calls. A refusal names its cause, and the two causes need different reactions: `target_too_small` is TARGET-LOCAL — only this target is out of room, and the message reports its geometry, the minimum required and the tab's pane count, so retry against a bigger pane (list_panes shows the geometry) or along the other direction; `pane_limit_reached` is tab-global — the tab is at its 16-pane cap and no target will help, so close a pane or pass `tab: {\"new\": {}}`. Unless the refusal reports `pane_limit_reached`, it does NOT mean the tab is out of capacity. The legacy `split_refused` code survives only for causes that are neither (currently: the whole terminal is below the layout threshold) — treat it as cause unknown. Needs a renga server advertising the split_refusal_causes capability; an older one answers both causes with the ambiguous `split_refused`, so it is refused (server_too_old) rather than allowed to break the target-local promise above.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "direction": {
                        "type": "string",
                        "enum": ["vertical", "horizontal"],
                        "description": "`vertical` splits side-by-side (new pane to the right); `horizontal` splits top/bottom (new pane on the bottom). Required unless `tab` is `{\"new\": …}` (a fresh tab has nothing to split — omit it there)."
                    },
                    "target": {
                        "type": "string",
                        "description": "Pane to split. Numeric id (from list_panes), stable name, or the literal 'focused'. Defaults to 'focused' when omitted. All-digit strings are always interpreted as ids — a pane literally named '7' cannot be addressed by name, use its id instead. With a `tab` selector the target must live in the selected tab (names and 'focused' resolve there; a mismatching numeric id is refused with target_tab_mismatch). Omit with `tab: {\"new\": …}`."
                    },
                    "tab": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "index": { "type": "integer", "minimum": 0 },
                            "pane_id": { "type": "integer", "minimum": 0 },
                            "new": {
                                "type": "object",
                                "properties": { "name": { "type": "string" } }
                            }
                        },
                        "description": "Optional tab placement (Issue #290); default is this pane's own tab. Pass exactly one key: {\"name\": \"<label>\"} = the tab whose display name matches exactly (0 matches → tab_not_found, several → tab_ambiguous — labels are not unique, use index/pane_id then); {\"index\": N} = 0-based tab index as reported by list_peers; {\"pane_id\": N} = the tab owning that pane (the stable anchor: ids never shift when tabs close); {\"new\": {}} or {\"new\": {\"name\": \"<label>\"}} = create a fresh single-pane BACKGROUND tab — the user's visible tab does not change, and `direction`/`target` must be omitted. Needs a renga server advertising the spawn_tab capability; older servers are refused (server_too_old) instead of spawning into the wrong tab."
                    },
                    "command": {
                        "type": "string",
                        "description": "Optional shell command to run in the new pane once the shell is ready (e.g. 'claude', 'cargo test'). A bare `claude` (or `claude <args>`) is auto-upgraded to the Alt+P form so the new instance joins the renga-peers network — you don't need to pass the --dangerously-load-development-channels flag yourself. If you pass that flag explicitly, it is left alone."
                    },
                    "auto_upgrade_claude": {
                        "type": "boolean",
                        "description": "Default true. Set false to launch `command` verbatim: a bare `claude` is NOT rewritten to the peer-enabled form."
                    },
                    "name": {
                        "type": "string",
                        "description": "Optional stable id for the new pane so it can be addressed by name later."
                    },
                    "role": {
                        "type": "string",
                        "description": "Optional free-form role label (e.g. 'worker', 'leader'). Shown in the UI and in list_panes output."
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Optional working directory for the new pane. Absolute paths are used as-is; relative paths are resolved against the caller pane's cwd. When omitted, the new pane inherits the target pane's cwd (prior behavior), or the caller pane's cwd with `tab: {\"new\": …}`. Use this instead of embedding `cd <path> && ...` in `command` — keeps the shell-quoting and the claude auto-upgrade intact."
                    },
                    "min_pane_width": {
                        "type": "integer",
                        "minimum": 5,
                        "maximum": 65535,
                        "description": "Optional per-call minimum width (columns) each half of a vertical split must keep. Overrides renga's `--min-pane-width` for this call only; omitted = the process-global value. Values below 5 are refused with `min_pane_size_invalid`. Not valid with `tab: {\"new\": …}`."
                    },
                    "min_pane_height": {
                        "type": "integer",
                        "minimum": 2,
                        "maximum": 65535,
                        "description": "Optional per-call minimum height (rows) each half of a horizontal split must keep. Overrides renga's `--min-pane-height` for this call only; omitted = the process-global value. Values below 2 are refused with `min_pane_size_invalid`. Not valid with `tab: {\"new\": …}`."
                    }
                }
            }
        },
        {
            "name": "spawn_claude_pane",
            "description": "Higher-level convenience over `spawn_pane`: splits a pane and launches Claude Code with the renga-peers channel enabled by construction, so the orchestrating caller never has to synthesize the `--dangerously-load-development-channels server:renga-peers` flag. Structured fields (`permission_mode`, `model`) are rendered into the final command exactly once; extra `args[]` are appended after them. renga applies POSIX-style shell quoting for values that contain whitespace or shell metacharacters, targeting bash / zsh / Git Bash — values containing single quotes may not round-trip cleanly on PowerShell-fallback Windows hosts, so prefer alphanumerics + `_-./:@+%=` in structured values. Conflicting overrides inside `args[]` (--dangerously-load-development-channels / --permission-mode / --model) are rejected with `invalid-params` — use the structured fields instead. Pane creation semantics (split refusal, cwd validation, name / role attachment) match `spawn_pane`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "direction": {
                        "type": "string",
                        "enum": ["vertical", "horizontal"],
                        "description": "`vertical` splits side-by-side (new pane to the right); `horizontal` splits top/bottom (new pane on the bottom). Required unless `tab` is `{\"new\": …}` (omit it there)."
                    },
                    "target": {
                        "type": "string",
                        "description": "Pane to split. Numeric id, stable name, or the literal 'focused'. Defaults to 'focused' when omitted. With a `tab` selector the target must live in the selected tab. Omit with `tab: {\"new\": …}`."
                    },
                    "tab": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "index": { "type": "integer", "minimum": 0 },
                            "pane_id": { "type": "integer", "minimum": 0 },
                            "new": {
                                "type": "object",
                                "properties": { "name": { "type": "string" } }
                            }
                        },
                        "description": "Optional tab placement — same selector and semantics as `spawn_pane`'s `tab`: exactly one of {\"name\"}, {\"index\"}, {\"pane_id\"}, or {\"new\": {…}} for a fresh single-pane background tab (visible tab unchanged; `direction`/`target` must be omitted). Requires the server's spawn_tab capability; older servers are refused instead of spawning into the wrong tab."
                    },
                    "name": {
                        "type": "string",
                        "description": "Optional stable id for the new pane so it can be addressed by name later."
                    },
                    "role": {
                        "type": "string",
                        "description": "Optional free-form role label (e.g. 'worker', 'foreman', 'curator'). Shown in the UI and in list_panes output."
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Optional working directory for the new pane. Absolute paths are used as-is; relative paths are resolved against the caller pane's cwd. Same semantics as `spawn_pane`'s cwd."
                    },
                    "min_pane_width": {
                        "type": "integer",
                        "minimum": 5,
                        "maximum": 65535,
                        "description": "Optional per-call minimum width (columns) each half of a vertical split must keep. Overrides renga's `--min-pane-width` for this call only; omitted = the process-global value. Values below 5 are refused with `min_pane_size_invalid`. Not valid with `tab: {\"new\": …}`."
                    },
                    "min_pane_height": {
                        "type": "integer",
                        "minimum": 2,
                        "maximum": 65535,
                        "description": "Optional per-call minimum height (rows) each half of a horizontal split must keep. Overrides renga's `--min-pane-height` for this call only; omitted = the process-global value. Values below 2 are refused with `min_pane_size_invalid`. Not valid with `tab: {\"new\": …}`."
                    },
                    "permission_mode": {
                        "type": "string",
                        "description": "Rendered into the launch command as `--permission-mode <value>`. Typical values: 'default', 'acceptEdits', 'bypassPermissions', 'plan'. Not pre-validated against a fixed enum so new Claude permission modes work without a renga release."
                    },
                    "model": {
                        "type": "string",
                        "description": "Rendered into the launch command as `--model <value>` (e.g. 'sonnet', 'opus', or a fully-qualified model id)."
                    },
                    "args": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Additional Claude CLI args appended after the structured fields. Must NOT contain --dangerously-load-development-channels, --permission-mode, or --model — pass those via the structured fields instead, or the call is rejected with invalid-params."
                    }
                }
            }
        },
        {
            "name": "spawn_codex_pane",
            "description": "Higher-level convenience over `spawn_pane`: splits a pane and launches Codex without the orchestrating caller having to synthesize a shell-quoted `codex ...` command string. This helper assumes the user has already run `renga mcp install --client codex`; that registration injects the `RENGA_PEER_CLIENT_KIND=codex` env into Codex's MCP server subprocess, so a plain `codex` launch is enough for the new pane to register as a pull-based peer. Extra `args[]` are appended after the `codex` token using the same POSIX-style shell quoting as spawn_claude_pane. Pane creation semantics (split refusal, cwd validation, name / role attachment) match `spawn_pane`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "direction": {
                        "type": "string",
                        "enum": ["vertical", "horizontal"],
                        "description": "`vertical` splits side-by-side (new pane to the right); `horizontal` splits top/bottom (new pane on the bottom). Required unless `tab` is `{\"new\": …}` (omit it there)."
                    },
                    "target": {
                        "type": "string",
                        "description": "Pane to split. Numeric id, stable name, or the literal 'focused'. Defaults to 'focused' when omitted. With a `tab` selector the target must live in the selected tab. Omit with `tab: {\"new\": …}`."
                    },
                    "tab": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "index": { "type": "integer", "minimum": 0 },
                            "pane_id": { "type": "integer", "minimum": 0 },
                            "new": {
                                "type": "object",
                                "properties": { "name": { "type": "string" } }
                            }
                        },
                        "description": "Optional tab placement — same selector and semantics as `spawn_pane`'s `tab`: exactly one of {\"name\"}, {\"index\"}, {\"pane_id\"}, or {\"new\": {…}} for a fresh single-pane background tab (visible tab unchanged; `direction`/`target` must be omitted). Requires the server's spawn_tab capability; older servers are refused instead of spawning into the wrong tab."
                    },
                    "name": {
                        "type": "string",
                        "description": "Optional stable id for the new pane so it can be addressed by name later."
                    },
                    "role": {
                        "type": "string",
                        "description": "Optional free-form role label (e.g. 'worker', 'reviewer', 'curator'). Shown in the UI and in list_panes output."
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Optional working directory for the new pane. Absolute paths are used as-is; relative paths are resolved against the caller pane's cwd. Same semantics as `spawn_pane`'s cwd."
                    },
                    "min_pane_width": {
                        "type": "integer",
                        "minimum": 5,
                        "maximum": 65535,
                        "description": "Optional per-call minimum width (columns) each half of a vertical split must keep. Overrides renga's `--min-pane-width` for this call only; omitted = the process-global value. Values below 5 are refused with `min_pane_size_invalid`. Not valid with `tab: {\"new\": …}`."
                    },
                    "min_pane_height": {
                        "type": "integer",
                        "minimum": 2,
                        "maximum": 65535,
                        "description": "Optional per-call minimum height (rows) each half of a horizontal split must keep. Overrides renga's `--min-pane-height` for this call only; omitted = the process-global value. Values below 2 are refused with `min_pane_size_invalid`. Not valid with `tab: {\"new\": …}`."
                    },
                    "args": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Additional Codex CLI args appended after the `codex` token. renga owns shell quoting for each item, so callers should pass one logical token per array entry."
                    }
                }
            }
        },
        {
            "name": "close_pane",
            "description": "Close a pane, terminating its process. Relative targets ('focused', a name) resolve inside your own tab; a numeric id may name a pane in any tab, which is the deliberate cross-tab escape hatch. Fails with code 'last_pane' when the target is the last pane of the only remaining tab. Needs a renga server advertising the caller_scope_close_identity capability; older servers are refused (server_too_old) rather than closing a pane in whatever tab the user is viewing.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "Pane to close. Numeric id (from list_panes), stable name, or the literal 'focused' (your own tab's focused pane — NOT whichever pane the user is looking at). Names and 'focused' resolve inside your own tab only; a numeric id may name a pane in any tab. All-digit strings are always interpreted as ids — a pane literally named '7' cannot be addressed by name, use its id instead."
                    }
                },
                "required": ["target"]
            }
        },
        {
            "name": "focus_pane",
            "description": "Move keyboard focus to a pane. The focused pane is what the user's keystrokes go to, so use sparingly — yanking focus away from the user is disruptive. Relative targets ('focused', a name) resolve inside your own tab; a numeric id may name a pane in any tab. IMPORTANT: whenever the resolved pane lives outside the tab the user is currently viewing, this also switches the visible tab to it, changing what is on the user's screen. This applies even when the target is in your own tab and the user is looking elsewhere. Only call it when the user asked to move focus.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "Pane to focus. Numeric id (from list_panes), stable name, or the literal 'focused' (your own tab's focused pane — note this is no longer a pure no-op: if your tab is not the visible one, it brings your tab forward). Names and 'focused' resolve inside your own tab only; a numeric id may name a pane in any tab. All-digit strings are always interpreted as ids — a pane literally named '7' cannot be addressed by name, use its id instead."
                    }
                },
                "required": ["target"]
            }
        },
        {
            "name": "new_tab",
            "description": "Create a new renga tab with a fresh single pane. Focus switches to the new tab. Returns the new pane's numeric id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Optional shell command to run in the new pane once the shell is ready. A bare `claude` (or `claude <args>`) is auto-upgraded to the Alt+P peer-enabled form so the new instance joins the renga-peers network. If you pass --dangerously-load-development-channels explicitly, it is left alone."
                    },
                    "auto_upgrade_claude": {
                        "type": "boolean",
                        "description": "Default true. Set false to launch `command` verbatim: a bare `claude` is NOT rewritten to the peer-enabled form."
                    },
                    "name": {
                        "type": "string",
                        "description": "Optional stable id for the new pane."
                    },
                    "label": {
                        "type": "string",
                        "description": "Optional tab label. Defaults to a label derived from the cwd."
                    },
                    "role": {
                        "type": "string",
                        "description": "Optional free-form role label attached to the new pane."
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Optional working directory for the new tab's pane. Absolute paths are used as-is; relative paths are resolved against the caller pane's cwd. When omitted, the renga server's current cwd is used."
                    }
                }
            }
        },
        {
            "name": "inspect_pane",
            "description": "Snapshot the rendered contents of a pane in the current renga tab. Returns the rendered text so you can detect interactive prompts (e.g. y/n confirmations), error banners, or mode indicators in another pane without asking its Claude. The `lines` option returns the last N lines ending at the live bottom; when N exceeds the pane's visible height the remainder is pulled from scrollback history (up to 2000 lines total), so recent output stays reachable even in small panes. (Scrollback only exists for main-screen output; a full-screen TUI on the alternate screen has no history to walk.) Reads are pinned to the live tail regardless of the pane's scroll position. `format=\"grid\"` switches the text block to JSON with one row object per line; the full structured payload is always available in `structuredContent`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "Pane to inspect. Numeric id (from list_panes), stable name, or the literal 'focused'. All-digit strings are always interpreted as ids — a pane literally named '7' cannot be addressed by name, use its id instead."
                    },
                    "lines": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Optional — return the last N rendered lines ending at the live bottom. N up to the visible height reads the screen grid (blank rows preserved, useful for anchoring on a status bar); larger N continues into scrollback history, capped at 2000 lines. Scrollback rows have negative `row` indices (-1 = just above the visible top). Omit for the full visible screen."
                    },
                    "include_cursor": {
                        "type": "boolean",
                        "description": "When true, the payload includes a `cursor` object ({visible, row, col}). Defaults to false."
                    },
                    "format": {
                        "type": "string",
                        "enum": ["text", "grid"],
                        "description": "'text' (default) returns the plain rendered screen as the content text. 'grid' returns a JSON blob with one object per row. `structuredContent` is always populated with the full payload regardless of this choice."
                    }
                },
                "required": ["target"]
            }
        },
        {
            "name": "send_keys",
            "description": "Send raw keystrokes to a pane's PTY — useful for answering interactive prompts (y/n), toggling Claude Code's permission mode (Shift+Tab), or driving any TUI that expects real key events instead of logical messages. Named special keys are translated to terminal escape sequences server-side; `text` passes through verbatim; the two can be combined. NOTE: this is NOT send_message. send_message delivers a logical peer message to another Claude via a channel notification; send_keys writes bytes into a PTY and is visible to whatever application is running in that pane.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "Pane to send to. Numeric id, stable name, or 'focused'. All-digit strings are always ids."
                    },
                    "text": {
                        "type": "string",
                        "description": "Literal text sent before any named keys. Use this for anything that doesn't need special-key translation (e.g. 'y', 'npm install')."
                    },
                    "keys": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Ordered list of named special keys appended after `text`. Supported vocabulary: Enter / Return, Tab, Shift+Tab (a.k.a. BackTab), Esc / Escape, Backspace, Delete / Del, Up / Down / Left / Right, Home, End, PageUp, PageDown, Space, Ctrl+<letter> where <letter> is A-Z. Unknown names return an -32602 invalid-params error."
                    },
                    "enter": {
                        "type": "boolean",
                        "description": "Convenience — append an Enter after `text` and `keys`. Equivalent to adding 'Enter' to the end of `keys`."
                    }
                },
                "required": ["target"]
            }
        },
        {
            "name": "set_pane_identity",
            "description": "Rename or (re)assign the stable `name` and/or `role` of an existing pane. Relative targets ('focused', a name) resolve inside your own tab; a numeric id may name a pane in any tab. Needs a renga server advertising the caller_scope_close_identity capability; older servers are refused (server_too_old) rather than renaming a pane in whatever tab the user is viewing. Use this to recover from sessions launched without the intended layout (e.g. when the secretary pane was spawned without an `id`, so peers can't address it as `to_id=\"secretary\"`). Both fields use three-state semantics: omit the key to keep the current value, pass `null` to clear it, or pass a string to set it. Validation: name cannot be empty, all-digits, or collide with another pane in the target pane's tab (uniqueness is per tab, not global); allowed characters are [A-Za-z0-9_-]. Role has no uniqueness constraint. Returns the updated pane record so callers can confirm without a separate list round-trip.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "Pane to update. Numeric id (from list_panes), stable name, or the literal 'focused' (default — your own tab's focused pane, NOT whichever pane the user is looking at). Names and 'focused' resolve inside your own tab only; a numeric id may name a pane in any tab. All-digit strings are always ids."
                    },
                    "name": {
                        "type": ["string", "null"],
                        "description": "New name, or null to clear. Omit to leave unchanged."
                    },
                    "role": {
                        "type": ["string", "null"],
                        "description": "New role label, or null to clear. Omit to leave unchanged."
                    }
                }
            }
        },
        {
            "name": "poll_events",
            "description": "Long-poll for pane lifecycle events (pane_started, pane_exited, pane_prompt_detected, pane_waiting_input, pane_mode_changed, peer_nudge_stalled, peer_nudge_queued, peer_nudge_submitted, peer_inbox_drained, events_dropped, and any forward-compatible variants). Events are process-wide: pane lifecycle from every renga tab is delivered, not just the caller's tab. Returns events accumulated since the given cursor; if none are buffered, blocks up to `timeout_ms` for the next one. The first call (omit `since`) starts at \"right now\" — no historical replay, matching `renga events --timeout` semantics. Each response body is a JSON object with `next_since` (an opaque cursor string to pass back) and `events` (an array of event objects in renga's wire format).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "since": {
                        "type": "string",
                        "description": "Cursor from a prior response's `next_since`. Omit on the first call to start at the present."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Maximum milliseconds to block when no event is immediately available. Default 2000; clamped to a 30000 ms maximum. Pass 0 for a non-blocking drain.",
                        "minimum": 0
                    },
                    "types": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional filter — only return events whose `type` field is in this list. The cursor still advances past filtered-out events so they won't reappear. Note: the filter narrows returned results but does not extend the long-poll; if a non-matching event arrives during the wait, `poll_events` returns early with `events: []` and an advanced cursor, and the caller should re-poll for the next window."
                    }
                }
            }
        }
    ])
}

pub(super) fn handle_initialize(id: &Value, params: &Value, ctx: &PeerCtx) -> Value {
    let client_protocol = params
        .get("protocolVersion")
        .and_then(|v| v.as_str())
        .unwrap_or("2025-06-18");
    let experimental = match ctx.client_kind {
        PeerClientKind::Claude => json!({ "claude/channel": {} }),
        PeerClientKind::Codex => json!({}),
    };
    ok_response(
        id,
        json!({
            "protocolVersion": client_protocol,
            "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
            "capabilities": {
                "experimental": experimental,
                "tools": {}
            },
            "instructions": instructions_blob(ctx.client_kind)
        }),
    )
}

pub(super) fn handle_tools_list(id: &Value) -> Value {
    ok_response(id, json!({ "tools": tools_spec() }))
}
