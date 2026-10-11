//! `renga mcp-peer` — the stdio MCP server Claude Code spawns per pane.
//!
//! Stage 3 of issue #97: the real implementation that replaces
//! `src/bin/renga-mcp-peer-spike.rs`. Where the spike looped messages
//! back to the same Claude, this module routes them through renga's
//! existing IPC server so a message sent from pane A shows up in pane
//! B's context as a `<channel source="renga-peers">` tag. Since Issue
//! #289 delivery spans every renga tab, not just the sender's own.
//!
//! # Lifecycle
//!
//! 1. Claude Code spawns `renga mcp-peer` as a stdio subprocess. The
//!    PTY env published by renga (`RENGA_PANE_ID`, `RENGA_SOCKET`,
//!    `RENGA_TOKEN`) is inherited all the way down.
//! 2. [`run`] negotiates the MCP `initialize` handshake, declares the
//!    `claude/channel` experimental capability, and spawns a background
//!    thread that subscribes to renga's event bus.
//! 3. Inbound `Request::PeerSend` deliveries land on the event bus as
//!    [`crate::ipc::Event::PeerInbox`]. The background thread pushes a
//!    `notifications/claude/channel` frame to stdout for the ones
//!    addressed to us — the only thing that makes peer messages show up
//!    as a channel tag instead of an ordinary tool result.
//!
//!    Since Issue #306 the subscription *opts in* to pane-scoped
//!    routing by declaring our `RENGA_PANE_ID`
//!    (`Request::Subscribe { from_pane }`), and a current server then
//!    routes `PeerInbox` to the subscribers that named its
//!    `target_pane`. Opting in is what this client wants: it only ever
//!    cares about one pane, so nobody else's mail has to travel through
//!    its bounded queue. Naming no pane is still legal and still means
//!    the full pre-#306 broadcast — that is the default, which is why
//!    #306 did not change behaviour for anyone who did not ask for it.
//!
//!    The `target_pane == our pane id` comparison in
//!    [`classify_inbox_event`] therefore decides nothing against a
//!    current server; it stays as a backstop, because a pre-#306 server
//!    ignores the new field and still broadcasts every peer message to
//!    every subscriber. Keeping both is defense-in-depth: the opt-in
//!    removes unintended delivery to other panes and the queue pressure
//!    of copies nobody wanted, and the check keeps us correct on the
//!    servers where that routing does not happen. It is not a boundary
//!    of any kind — any process running as this user can declare any
//!    pane id (see the threat model in [`crate::ipc`]).
//!
//! # Outside-renga fallback
//!
//! If `RENGA_PANE_ID` is absent (Claude was launched from a terminal
//! renga didn't spawn), the module still handshakes and advertises the
//! tools — they just return empty/no-op results. This keeps the stdio
//! MCP installed globally in `~/.claude/mcp_servers.json` from erroring
//! out every time Claude starts outside renga.

mod events;
pub mod install;
mod launch;
mod messaging;
mod pane_io;
mod panes;
mod parent_watch;
mod server_info;
mod spec;

use std::collections::{HashSet, VecDeque};
use std::io::{self, BufRead, Write};
use std::process::Command;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use crate::app::CLAUDE_PEER_LAUNCH_CMD;
use crate::ipc::endpoint::{endpoint_from_env, EndpointName, ENV_SOCKET};
use crate::ipc::{
    self, client, Direction, PaneInfo, PaneRef, PeerClientKind, PeerInfo, Request, Response,
};

use events::*;
use launch::*;
use messaging::*;
use pane_io::*;
use panes::*;
use server_info::*;
use spec::*;

pub(crate) use pane_io::build_send_keys_payload;
pub(crate) use panes::upgrade_claude_command;
pub use server_info::run_capabilities;

const SERVER_NAME: &str = "renga-peers";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const ENV_PANE_ID: &str = "RENGA_PANE_ID";
pub(crate) const ENV_CLIENT_KIND: &str = "RENGA_PEER_CLIENT_KIND";

fn log_stderr(msg: &str) {
    eprintln!("[renga-mcp-peer] {msg}");
}

/// Entry point called by `renga mcp-peer`. Blocks on stdin until EOF
/// or an unrecoverable error — with a parent-process watchdog as the
/// authoritative backstop, because stdin EOF is not guaranteed to
/// arrive on Windows when the pipe's write end leaked into sibling
/// processes via handle inheritance (renga-9fs).
pub fn run() -> Result<()> {
    log_stderr(&format!("starting {SERVER_NAME} v{SERVER_VERSION}"));

    parent_watch::spawn(|| {
        log_stderr("parent process exited; shutting down");
        std::process::exit(0);
    });

    let ctx = PeerCtx::load();
    match &ctx.mode {
        Mode::Connected { pane_id, .. } => {
            log_stderr(&format!(
                "connected mode: pane_id={pane_id}, client_kind={:?}",
                ctx.client_kind
            ));
            register_client_kind(&ctx);
            spawn_inbox_subscriber(ctx.clone());
        }
        Mode::Detached { reason } => {
            log_stderr(&format!("detached mode: {reason}"));
        }
    }

    stdio_loop(&ctx)
}

fn register_client_kind(ctx: &PeerCtx) {
    let Mode::Connected { pane_id, endpoint } = &ctx.mode else {
        return;
    };
    match client::send_request(
        endpoint,
        &Request::PeerRegisterClient {
            pane_id: *pane_id,
            kind: ctx.client_kind,
        },
    ) {
        Ok(Response::Ok { .. }) => {}
        Ok(other) => log_stderr(&format!("peer kind registration returned: {other:?}")),
        Err(e) => log_stderr(&format!("peer kind registration failed: {e}")),
    }
}

/// Runtime context shared between the main stdio loop and the inbox
/// subscriber thread. Cloneable because both halves read the same
/// `(pane_id, endpoint)` pair to contact the renga server and the
/// same [`EventSink`] for `poll_events` buffering.
#[derive(Clone)]
struct PeerCtx {
    mode: Mode,
    client_kind: PeerClientKind,
    events: EventSink,
    inbox: InboxSink,
}

/// Soft cap on the per-process lifecycle event buffer used by
/// `poll_events`. Older entries are evicted on overflow; a caller that
/// falls behind by more than this many events will miss the oldest
/// ones. The upstream `EventsDropped` meta-event (emitted when the
/// subscribe channel itself drops) still flows through as a regular
/// buffered event so the caller can notice.
const EVENT_BUFFER_CAP: usize = 4096;

/// Default `timeout_ms` for `poll_events` when the caller doesn't
/// specify one. Long enough to absorb a quiet period without spinning,
/// short enough to keep the stdio dispatcher responsive if Claude Code
/// wants to interleave tool calls.
const POLL_DEFAULT_TIMEOUT_MS: u64 = 2000;

/// Hard cap on `timeout_ms` regardless of what the caller requests.
/// A single `poll_events` call blocks the mcp-peer stdio dispatcher
/// for its duration — this bound keeps an unresponsive client from
/// wedging the whole MCP.
const POLL_MAX_TIMEOUT_MS: u64 = 30_000;

#[derive(Clone, Debug)]
struct SeqEvent {
    seq: u64,
    value: Value,
}

/// Ring buffer of lifecycle events assigned monotonic 1-based
/// sequence numbers. `seq = 0` is the "nothing yet" sentinel returned
/// as `next_since` when the caller polls an empty stream.
#[derive(Default)]
struct EventBuffer {
    events: VecDeque<SeqEvent>,
    /// Seq of the most recently pushed event. `0` before any event.
    last_seq: u64,
}

impl EventBuffer {
    fn push(&mut self, value: Value) -> u64 {
        self.last_seq = self.last_seq.saturating_add(1);
        let seq = self.last_seq;
        self.events.push_back(SeqEvent { seq, value });
        while self.events.len() > EVENT_BUFFER_CAP {
            self.events.pop_front();
        }
        seq
    }
}

type EventSink = Arc<(Mutex<EventBuffer>, Condvar)>;

fn new_event_sink() -> EventSink {
    Arc::new((Mutex::new(EventBuffer::default()), Condvar::new()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueuedPeerMessage {
    from_id: String,
    from_name: Option<String>,
    from_kind: Option<PeerClientKind>,
    body: String,
    sent_at: String,
    /// `PeerInbox::msg_id`, echoed back when the message is drained.
    msg_id: Option<u64>,
}

type InboxSink = Arc<Mutex<VecDeque<QueuedPeerMessage>>>;

fn new_inbox_sink() -> InboxSink {
    Arc::new(Mutex::new(VecDeque::new()))
}

#[derive(Clone)]
enum Mode {
    /// Running inside a renga pane with a reachable IPC endpoint.
    Connected {
        pane_id: usize,
        endpoint: EndpointName,
    },
    /// Missing `RENGA_PANE_ID` or `RENGA_SOCKET`. Tools still respond
    /// but with empty/no-op payloads so `claude` launched outside
    /// renga doesn't log MCP errors on startup.
    Detached { reason: String },
}

impl PeerCtx {
    fn load() -> Self {
        let events = new_event_sink();
        let inbox = new_inbox_sink();
        let client_kind = std::env::var(ENV_CLIENT_KIND)
            .ok()
            .and_then(|s| parse_client_kind(&s))
            .unwrap_or(PeerClientKind::Claude);
        let pane_id = match std::env::var(ENV_PANE_ID) {
            Ok(s) => match s.parse::<usize>() {
                Ok(v) => v,
                Err(_) => {
                    return PeerCtx {
                        mode: Mode::Detached {
                            reason: format!("{ENV_PANE_ID} is set but not a valid usize: {s:?}"),
                        },
                        events,
                        inbox,
                        client_kind,
                    };
                }
            },
            Err(_) => {
                return PeerCtx {
                    mode: Mode::Detached {
                        reason: format!(
                            "{ENV_PANE_ID} not set — this process was not launched inside a renga pane"
                        ),
                    },
                    events,
                    inbox,
                    client_kind,
                };
            }
        };
        match endpoint_from_env() {
            Ok(endpoint) => PeerCtx {
                mode: Mode::Connected { pane_id, endpoint },
                events,
                inbox,
                client_kind,
            },
            Err(e) => PeerCtx {
                mode: Mode::Detached {
                    reason: format!("{ENV_SOCKET} missing or invalid: {e}"),
                },
                events,
                inbox,
                client_kind,
            },
        }
    }
}

fn parse_client_kind(raw: &str) -> Option<PeerClientKind> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "claude" => Some(PeerClientKind::Claude),
        "codex" => Some(PeerClientKind::Codex),
        _ => None,
    }
}

// ── stdio JSON-RPC frame plumbing ─────────────────────────────

fn write_frame(value: &Value) -> Result<()> {
    let mut line = serde_json::to_string(value).context("serialize frame")?;
    line.push('\n');
    let out = io::stdout();
    let mut guard = out.lock();
    guard
        .write_all(line.as_bytes())
        .context("write frame to stdout")?;
    guard.flush().context("flush stdout")?;
    Ok(())
}

fn ok_response(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn err_response(id: &Value, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

fn tool_text_result(text: &str) -> Value {
    json!({ "content": [ { "type": "text", "text": text } ], "isError": false })
}

/// Most messages a push client keeps for `check_messages` before the
/// oldest is dropped. Its queue is only a fallback copy that a client
/// seeing channel tags may never drain (Issue #334). Pull queues stay
/// unbounded: renga counts every message in them as unread, so evicting
/// one would leave its id unread forever.
const PUSH_INBOX_CAP: usize = 256;

fn queue_pull_message(inbox: &InboxSink, message: QueuedPeerMessage) {
    let mut q = inbox.lock().unwrap_or_else(|p| p.into_inner());
    q.push_back(message);
}

/// Hand one delivered message to this client. Every client queues it so
/// `check_messages` can return it; a push client also gets the
/// `notifications/claude/channel` frame to write. Queuing on push is the
/// #334 fix: a pane can host several Claude clients (a front-end `claude`
/// plus a background `claude daemon run` session), each with its own
/// mcp-peer, and a host that does not render channel frames silently
/// drops the push — the queue is then its only way to read the message.
fn deliver_inbox_message(
    inbox: &InboxSink,
    receive_mode: ipc::PeerReceiveMode,
    message: QueuedPeerMessage,
) -> Option<Value> {
    if receive_mode == ipc::PeerReceiveMode::Pull {
        queue_pull_message(inbox, message);
        return None;
    }
    // Same `sent_at` in the tag and the queued copy, so an agent that saw
    // the tag can recognise the message when check_messages returns it.
    let frame = channel_notification(
        &message.body,
        &message.from_id,
        message.from_name.as_deref(),
        &message.sent_at,
    );
    let mut q = inbox.lock().unwrap_or_else(|p| p.into_inner());
    if q.len() >= PUSH_INBOX_CAP {
        q.pop_front();
    }
    q.push_back(message);
    Some(frame)
}

// ── channel notification (the whole point of #97) ─────────────

/// Build the `notifications/claude/channel` push that makes a peer
/// message show up as `<channel source="renga-peers">...</channel>`
/// in the receiver's context. The `source=` attribute is derived by
/// Claude Code from our `serverInfo.name`, not from this payload, so
/// `params.meta` here only carries sender metadata.
///
/// Claude Code currently injects channel notifications into a
/// user-slot turn, which the transcript renders with a `Human:`
/// prefix even though the content is from a peer. To keep operators
/// from mistaking peer chatter for things the human typed, the body
/// is wrapped with a loud banner that's obviously machine-generated
/// (uppercase, emoji, explicit "not from user"). See renga#221.
fn channel_notification(
    body: &str,
    from_id: &str,
    from_name: Option<&str>,
    sent_at: &str,
) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel",
        "params": {
            "content": peer_banner_wrap(body, from_id, from_name),
            "meta": {
                "from_id": from_id,
                "from_name": from_name.unwrap_or(""),
                "sent_at": sent_at,
            }
        }
    })
}

/// Prepend a visible "this is a peer message, not user input" banner
/// to a peer-channel body. Renga-side wrap of choice for renga#221:
/// the receiving Claude Code displays channel notifications under a
/// `Human:` heading, so without an obvious in-body marker, peer
/// chatter looks like the human typed it. The banner is uppercase
/// with an emoji and an explicit disclaimer so a human scanning the
/// transcript can tell at a glance.
fn peer_banner_wrap(body: &str, from_id: &str, from_name: Option<&str>) -> String {
    // The banner is prepended to the body the receiving agent reads, so
    // a newline in the sender's name would let it forge banner lines
    // around content it does not own. The body itself is left intact —
    // it is the message, and it is legitimately multi-line.
    let name = ipc::sanitized_label(from_name.unwrap_or(""));
    let name = name.trim();
    let header = if name.is_empty() {
        format!("📡 PEER MESSAGE — from id={from_id} — NOT FROM USER")
    } else {
        format!("📡 PEER MESSAGE — from {name} (id={from_id}) — NOT FROM USER")
    };
    format!("{header}\n\n{body}")
}

fn now_ts_string() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    // ISO-ish with second precision — Claude Code doesn't parse this,
    // it's only shown to the receiving Claude for context.
    format!("{}.{:09}", d.as_secs(), d.subsec_nanos())
}

fn fmt_code(message: &str, code: &Option<String>) -> String {
    match code {
        Some(c) => format!("[{c}] {message}"),
        None => message.to_string(),
    }
}

fn handle_tools_call(id: &Value, params: &Value, ctx: &PeerCtx) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("tools/call missing 'name'"))?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    Ok(match name {
        "list_peers" => handle_list_peers(id, ctx),
        "send_message" => handle_send_message(id, &args, ctx),
        "set_summary" => handle_set_summary(id, &args, ctx),
        "check_messages" => handle_check_messages(id, ctx),
        "server_info" => handle_server_info(id, ctx),
        "list_panes" => handle_list_panes(id, &args, ctx),
        "spawn_pane" => handle_spawn_pane(id, &args, ctx),
        "spawn_claude_pane" => handle_spawn_claude_pane(id, &args, ctx),
        "spawn_codex_pane" => handle_spawn_codex_pane(id, &args, ctx),
        "close_pane" => handle_close_pane(id, &args, ctx),
        "focus_pane" => handle_focus_pane(id, &args, ctx),
        "new_tab" => handle_new_tab(id, &args, ctx),
        "inspect_pane" => handle_inspect_pane(id, &args, ctx),
        "send_keys" => handle_send_keys(id, &args, ctx),
        "poll_events" => handle_poll_events(id, &args, ctx),
        "set_pane_identity" => handle_set_pane_identity(id, &args, ctx),
        other => err_response(id, -32601, &format!("unknown tool: {other}")),
    })
}

// ── stdin dispatch loop ───────────────────────────────────────

fn dispatch(req: &Value, ctx: &PeerCtx) -> Result<Vec<Value>> {
    let is_notification = req.get("id").is_none();
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = match req.get("method").and_then(|v| v.as_str()) {
        Some(m) => m,
        None => {
            if is_notification {
                log_stderr("dropping malformed notification with no method");
                return Ok(Vec::new());
            }
            return Ok(vec![err_response(
                &id,
                -32600,
                "invalid request: missing or non-string 'method'",
            )]);
        }
    };
    let params = req.get("params").cloned().unwrap_or(json!({}));
    if is_notification {
        // Lifecycle notifications are accepted silently; unknown ones logged.
        if !matches!(
            method,
            "notifications/initialized" | "initialized" | "notifications/cancelled" | "$/cancel"
        ) {
            log_stderr(&format!("ignored unknown notification: {method}"));
        }
        return Ok(Vec::new());
    }
    let frames = match method {
        "initialize" => vec![handle_initialize(&id, &params, ctx)],
        "tools/list" => vec![handle_tools_list(&id)],
        "tools/call" => vec![handle_tools_call(&id, &params, ctx)?],
        "ping" => vec![ok_response(&id, json!({}))],
        other => vec![err_response(
            &id,
            -32601,
            &format!("method not found: {other}"),
        )],
    };
    Ok(frames)
}

fn stdio_loop(ctx: &PeerCtx) -> Result<()> {
    let stdin = io::stdin();
    let reader = stdin.lock();
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                log_stderr(&format!("stdin read error: {e}"));
                break;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                log_stderr(&format!("malformed JSON frame: {e} — raw={trimmed}"));
                // JSON-RPC 2.0 §5.1: on parse error, the server MUST
                // respond with id=null + code -32700. Clients that
                // correlate replies by id will otherwise hang.
                let parse_err = err_response(&Value::Null, -32700, &format!("parse error: {e}"));
                let _ = write_frame(&parse_err);
                continue;
            }
        };
        match dispatch(&value, ctx) {
            Ok(frames) => {
                for f in &frames {
                    if let Err(e) = write_frame(f) {
                        log_stderr(&format!("failed to write frame: {e}"));
                    }
                }
            }
            Err(e) => {
                log_stderr(&format!("dispatch error: {e}"));
                if let Some(id) = value.get("id") {
                    let payload = err_response(id, -32603, &format!("internal error: {e}"));
                    let _ = write_frame(&payload);
                }
            }
        }
    }
    log_stderr("stdin closed; exiting");
    Ok(())
}

// ── event bus subscriber (background thread) ──────────────────

/// Subscribe to renga's event bus and turn the events addressed to this
/// pane into either a `notifications/claude/channel` frame on stdout
/// (push clients) or a queued message (pull clients). The thread is
/// detached — it dies naturally when the IPC stream closes (renga
/// exited) or when the subprocess is killed.
///
/// The subscription opts in to pane-scoped routing by naming our pane
/// id ([`client::subscribe_inbox_events`], Issue #306), so a current
/// server only ever enqueues an [`ipc::Event::PeerInbox`] whose
/// `target_pane` is ours. That is the whole payoff of opting in: this
/// thread's bounded queue never carries another pane's mail, and the
/// bus never has to copy it there. Subscribing without a pane id — what
/// `renga events` does — still yields the full pre-#306 stream, so the
/// narrowing is ours alone and costs no other consumer anything.
/// [`classify_inbox_event`] still checks `target_pane` itself; against a
/// pre-#306 server — which ignores the binding and broadcasts every peer
/// message to every subscriber — that check is the only thing keeping
/// this client from announcing another pane's mail, so it stays as a
/// backstop rather than being deleted as redundant.
fn spawn_inbox_subscriber(ctx: PeerCtx) {
    let Mode::Connected { pane_id, endpoint } = ctx.mode.clone() else {
        return;
    };
    let endpoint_clone = endpoint.clone();
    let sink = ctx.events.clone();
    let inbox = ctx.inbox.clone();
    let client_kind = ctx.client_kind;
    thread::Builder::new()
        .name("renga-mcp-peer-inbox".into())
        .spawn(move || {
            let result = client::subscribe_inbox_events(&endpoint_clone, pane_id, |event| {
                // Buffer lifecycle events for `poll_events` first, as
                // always. Heartbeat is a wire-keepalive (not a lifecycle
                // signal) and PeerInbox is delivered out-of-band via
                // channel notifications, so neither belongs in the poll
                // buffer. Everything else — PaneStarted / PaneExited /
                // EventsDropped plus any forward-compatible variants
                // added later — gets stashed.
                if should_buffer_for_poll(&event) {
                    match serde_json::to_value(&event) {
                        Ok(value) => {
                            let (lock, cvar) = &*sink;
                            let mut buf = lock.lock().unwrap_or_else(|p| p.into_inner());
                            buf.push(value);
                            cvar.notify_all();
                        }
                        Err(e) => {
                            log_stderr(&format!("failed to serialize event for poll buffer: {e}"))
                        }
                    }
                }
                // The EventBus bounds each subscriber at 256 events and
                // drops new events for slow consumers, reporting the gap
                // via EventsDropped. Log it here — the operator-facing
                // half of the notice, which the classifier deliberately
                // has no way to emit.
                if let ipc::Event::EventsDropped { count, .. } = &event {
                    log_stderr(&format!(
                        "event bus dropped {count} event(s) due to slow subscriber"
                    ));
                }
                if let InboxDelivery::Deliver {
                    from_id,
                    from_name,
                    from_kind,
                    body,
                    msg_id,
                } = classify_inbox_event(&event, pane_id)
                {
                    let message = QueuedPeerMessage {
                        from_id,
                        from_name,
                        from_kind,
                        body,
                        sent_at: now_ts_string(),
                        msg_id,
                    };
                    if let Some(note) =
                        deliver_inbox_message(&inbox, client_kind.receive_mode(), message)
                    {
                        // Both notices go out through the same sink, but
                        // their write failures have always been
                        // distinguishable in the log and operators grep
                        // for them. Pick the label off the variant
                        // rather than teaching `InboxDelivery` about its
                        // own provenance.
                        let failure_label = match &event {
                            ipc::Event::EventsDropped { .. } => "drop notice",
                            _ => "channel notification",
                        };
                        if let Err(e) = write_frame(&note) {
                            log_stderr(&format!("failed to push {failure_label}: {e}"));
                        }
                    }
                }
                true
            });
            match result {
                Ok(()) => log_stderr("event stream closed"),
                Err(e) => log_stderr(&format!("event subscription ended: {e}")),
            }
        })
        .expect("spawn inbox subscriber thread");
}

/// What the inbox subscriber should do with one event.
///
/// Deliberately free of timestamps, I/O and any handle to the
/// subprocess' sinks: the decision is a pure function of the event and
/// our pane id, so it can be asserted on directly. The caller stamps
/// [`now_ts_string`] and picks push
/// ([`channel_notification`] + [`write_frame`]) versus pull
/// ([`queue_pull_message`]) from the client's
/// [`ipc::PeerClientKind::receive_mode`].
#[derive(Debug, Clone, PartialEq)]
enum InboxDelivery {
    /// Nothing reaches the agent: no channel notification, no pull-queue
    /// entry. (Lifecycle variants may still have been buffered for
    /// `poll_events` by the caller — that is a separate stream.)
    Ignore,
    /// Surface this to the agent as a peer message.
    Deliver {
        from_id: String,
        from_name: Option<String>,
        from_kind: Option<PeerClientKind>,
        body: String,
        msg_id: Option<u64>,
    },
}

/// Decide what an event coming off the subscription means for the pane
/// this subprocess serves.
///
/// - [`ipc::Event::PeerInbox`] addressed to `pane_id` → deliver it,
///   attributed to the sending pane.
/// - [`ipc::Event::PeerInbox`] addressed to any other pane → ignore.
///   Since Issue #306 a current server never routes one of these to us
///   at all — but only because *this* client names its pane when it
///   subscribes, not because the server withholds peer mail from
///   everyone. A subscription that names no pane still receives every
///   `PeerInbox` exactly as it did before #306. So this arm remains the
///   backstop for the two cases the opt-in cannot cover: a pre-#306
///   server that ignores the binding and broadcasts to every
///   subscriber, and any future caller that reaches this classifier
///   from an unscoped stream. It is not a boundary — see the module
///   docs and the threat model in [`crate::ipc`] — it is what stops
///   another pane's mail from being announced in this pane's context.
/// - [`ipc::Event::EventsDropped`] → deliver a runtime notice
///   attributed to renga itself, so the agent knows a peer message may
///   have been lost and can ask the sender to retry rather than
///   assuming all is well.
/// - everything else (PaneStarted / PaneExited / Heartbeat and any
///   forward-compatible variant added later) → ignore. Lifecycle
///   variants reach the agent through `poll_events`, not through the
///   channel.
fn classify_inbox_event(event: &ipc::Event, pane_id: usize) -> InboxDelivery {
    match event {
        ipc::Event::PeerInbox {
            target_pane,
            from_pane,
            from_name,
            from_kind,
            body,
            msg_id,
            ..
        } if *target_pane == pane_id => InboxDelivery::Deliver {
            from_id: from_pane.to_string(),
            from_name: from_name.clone(),
            from_kind: *from_kind,
            body: body.clone(),
            msg_id: *msg_id,
        },
        ipc::Event::EventsDropped { count, .. } => InboxDelivery::Deliver {
            from_id: "renga".to_string(),
            from_name: Some("renga runtime".to_string()),
            from_kind: None,
            body: format!(
                "renga event bus dropped {count} event(s) before they reached this peer client. A peer message may have been lost — consider asking the sender to retry."
            ),
            msg_id: None,
        },
        _ => InboxDelivery::Ignore,
    }
}

/// True for events that belong in the `poll_events` ring buffer. A
/// free function so tests can pin the classification without spinning
/// up a subscriber thread.
fn should_buffer_for_poll(event: &ipc::Event) -> bool {
    !matches!(
        event,
        ipc::Event::Heartbeat { .. } | ipc::Event::PeerInbox { .. }
    )
}

#[cfg(test)]
mod tests;
