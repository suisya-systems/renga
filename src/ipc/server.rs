//! IPC server: accepts connections on a named endpoint and forwards
//! each request to the App's command channel.
//!
//! Wire protocol: newline-delimited JSON. A connection must start with
//! a `Hello` request; the server replies with a [`Response::Hello`]
//! carrying its PID and a per-instance session token. The client then
//! sends exactly one command and reads exactly one response before the
//! server closes its side.
//!
//! Threading model:
//! - One accept thread lives for the process lifetime and blocks on
//!   `listener.incoming()`.
//! - Each connection is handed to a short-lived worker thread so a slow
//!   client can't starve the accept loop.
//! - Workers communicate with the App by pushing an [`AppCommand`] into
//!   the shared `Sender<AppCommand>` and blocking on a [`oneshot`] reply
//!   with a timeout, so an unresponsive App can never hang a worker
//!   indefinitely.

use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use interprocess::local_socket::{prelude::*, ListenerOptions, Stream};

use super::endpoint::{EndpointKind, EndpointName};
use super::events::{EventBus, EventScope};
use super::{err_code, Event, PeerDelivery, Request, Response, APP_REPLY_TIMEOUT};
use crate::app::AppCommand;

/// Upper bound for waiting on the accept thread during shutdown.
/// `Drop` must not hang on an uncooperative accept thread — if the
/// self-connect wakeup somehow fails and the thread stays blocked in
/// `listener.incoming()`, we'd rather leak the thread (the OS reaps
/// it on process exit) than stall the whole process from teardown.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

pub struct IpcServer {
    pub endpoint: EndpointName,
    stop: Arc<AtomicBool>,
    /// Signaled once the accept thread returns. Using a channel rather
    /// than `JoinHandle::join` so Drop can wait with a timeout.
    done_rx: Option<mpsc::Receiver<()>>,
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        // Orderly shutdown so the accept thread exits before we remove
        // the socket file, avoiding the "stale listener, new path"
        // rebinding race: (1) flip the stop flag, (2) self-connect to
        // unblock the blocked `accept()` call, (3) wait for the thread
        // to signal completion (bounded), then (4) unlink on Unix.
        self.stop.store(true, Ordering::Release);
        unblock_accept(&self.endpoint);
        if let Some(rx) = self.done_rx.take() {
            let _ = rx.recv_timeout(SHUTDOWN_TIMEOUT);
        }
        if self.endpoint.kind() == EndpointKind::Socket {
            let _ = std::fs::remove_file(self.endpoint.as_str());
        }
    }
}

/// Open and immediately drop a client connection to the server's own
/// endpoint. This wakes the blocked `Listener::incoming()` call so the
/// accept thread can observe the stop flag and exit. Any error is
/// ignored — the endpoint may already be torn down from an earlier
/// Drop pass.
fn unblock_accept(endpoint: &EndpointName) {
    let name = match endpoint_to_name(endpoint) {
        Ok(n) => n,
        Err(_) => return,
    };
    let _ = Stream::connect(name);
}

fn endpoint_to_name(endpoint: &EndpointName) -> Result<interprocess::local_socket::Name<'_>> {
    #[cfg(windows)]
    {
        use interprocess::os::windows::local_socket::NamedPipe;
        Ok(endpoint.as_str().to_fs_name::<NamedPipe>()?)
    }
    #[cfg(unix)]
    {
        use interprocess::local_socket::GenericFilePath;
        Ok(endpoint.as_str().to_fs_name::<GenericFilePath>()?)
    }
}

impl IpcServer {
    /// Bind the listener and start accepting in a background thread.
    pub fn spawn(
        endpoint: EndpointName,
        command_tx: Sender<AppCommand>,
        session_token: String,
        event_bus: EventBus,
    ) -> Result<Self> {
        let listener = bind_listener(&endpoint)
            .with_context(|| format!("bind IPC endpoint {}", endpoint.as_str()))?;

        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = stop.clone();
        let token_for_thread = session_token.clone();
        let endpoint_for_log = endpoint.as_str().to_string();
        let (done_tx, done_rx) = mpsc::channel();
        thread::Builder::new()
            .name("renga-ipc-accept".into())
            .spawn(move || {
                accept_loop(
                    listener,
                    command_tx,
                    token_for_thread,
                    endpoint_for_log,
                    stop_for_thread,
                    event_bus,
                );
                // Signal Drop that the accept loop has returned. If the
                // receiver is already gone (Drop finished first because
                // of the timeout) the send errors out silently; we
                // don't care.
                let _ = done_tx.send(());
            })
            .context("spawn IPC accept thread")?;

        // Token is consumed by the accept thread via `token_for_thread`;
        // we don't need to keep a copy on the struct.
        drop(session_token);
        Ok(Self {
            endpoint,
            stop,
            done_rx: Some(done_rx),
        })
    }
}

fn bind_listener(endpoint: &EndpointName) -> Result<interprocess::local_socket::Listener> {
    // `to_fs_name` lets us pass an OS-native path (both the Windows
    // pipe name `\\.\pipe\…` and a Unix socket path are absolute file
    // names). `try_overwrite(true)` replaces a stale Unix socket file
    // left behind by a crashed previous instance — on Windows the
    // equivalent is a no-op because Named Pipes don't leak files.
    #[cfg(windows)]
    let name = {
        use interprocess::os::windows::local_socket::NamedPipe;
        endpoint.as_str().to_fs_name::<NamedPipe>()?
    };
    #[cfg(unix)]
    let name = {
        use interprocess::local_socket::GenericFilePath;
        endpoint.as_str().to_fs_name::<GenericFilePath>()?
    };

    let listener = ListenerOptions::new()
        .name(name)
        .try_overwrite(true)
        .create_sync()?;
    Ok(listener)
}

fn accept_loop(
    listener: interprocess::local_socket::Listener,
    command_tx: Sender<AppCommand>,
    session_token: String,
    endpoint_for_log: String,
    stop: Arc<AtomicBool>,
    event_bus: EventBus,
) {
    for conn in listener.incoming() {
        // The self-connect triggered by IpcServer::drop returns here;
        // observing the stop flag before handle_connection lets us
        // exit cleanly instead of serving one last spurious request.
        if stop.load(Ordering::Acquire) {
            return;
        }
        let conn = match conn {
            Ok(c) => c,
            Err(e) => {
                // Accept failures on a shutdown path are expected (the
                // listener got unlinked under us); on a normal path
                // they're transient and shouldn't kill the server.
                if stop.load(Ordering::Acquire) {
                    return;
                }
                eprintln!("renga IPC: accept failed on {endpoint_for_log}: {e}");
                continue;
            }
        };

        let tx = command_tx.clone();
        let token = session_token.clone();
        let bus = event_bus.clone();
        if let Err(e) = thread::Builder::new()
            .name("renga-ipc-worker".into())
            .spawn(move || {
                if let Err(e) = handle_connection(conn, tx, &token, bus) {
                    eprintln!("renga IPC: connection error: {e}");
                }
            })
        {
            // Thread spawn failures are extremely rare (EAGAIN under
            // system pressure). Dropping the connection is safe — the
            // client sees EOF and can retry. We deliberately don't fall
            // back to inline handling because that would block the
            // accept loop behind a slow request.
            eprintln!("renga IPC: worker spawn failed, dropping connection: {e}");
        }
    }
}

fn handle_connection(
    conn: Stream,
    command_tx: Sender<AppCommand>,
    session_token: &str,
    event_bus: EventBus,
) -> Result<()> {
    // The stream is split by wrapping in BufReader for line-buffered
    // reads; writes go through BufReader::get_mut. We can't construct
    // two BufReader clones without a split, so we borrow mutably.
    let mut reader = BufReader::new(conn);
    let mut line = String::new();

    // ── 1. Handshake ───────────────────────────────────────
    if read_line_or_eof(&mut reader, &mut line)?.is_none() {
        return Ok(());
    }
    let req: Request = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(e) => {
            return write_response_line(
                reader.get_mut(),
                &Response::err_coded(err_code::PARSE, format!("parse error on hello: {e}")),
            );
        }
    };
    match req {
        Request::Hello { client_pid: _ } => {
            let hello = Response::Hello {
                server_pid: std::process::id(),
                session_token: session_token.to_string(),
                capabilities: super::SERVER_CAPABILITIES
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            };
            write_response_line(reader.get_mut(), &hello)?;
        }
        _ => {
            write_response_line(
                reader.get_mut(),
                &Response::err_coded(err_code::PROTOCOL, "first message must be hello"),
            )?;
            return Ok(());
        }
    }

    // ── 2. One command ─────────────────────────────────────
    line.clear();
    if read_line_or_eof(&mut reader, &mut line)?.is_none() {
        return Ok(());
    }
    let req: Request = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(e) => {
            return write_response_line(
                reader.get_mut(),
                &Response::err_coded(err_code::PARSE, format!("parse error: {e}")),
            );
        }
    };
    // Matched by reference: `Subscribe` takes over the connection and
    // never reaches `dispatch_request`, but `req` is still needed on
    // the fall-through path below.
    if let Request::Subscribe { from_pane } = &req {
        // The pane binding comes from this explicit field and from
        // nothing else (Issue #306). The handshake's `client_pid` and
        // any `PeerRegisterClient` the same process made earlier both
        // describe a *process*; neither is tied to this particular
        // subscription, so inferring a scope from them would bind the
        // wrong stream as soon as a process holds two of them.
        //
        // No field ⇒ the caller did not opt in, so it stays
        // `EventScope::Unscoped` and keeps the full pre-#306 broadcast:
        // every event, including every `PeerInbox` whatever its
        // `target_pane`. Every pre-#306 client and `renga events` land
        // here and see exactly the stream they always saw — that is
        // deliberate, and it is what keeps #306 a minor rather than a
        // break (`docs/semver-policy-2.0.md` §3: a new optional input
        // whose default preserves prior behavior). Scoping is a
        // narrowing a client asks for, never one the server imposes.
        //
        // Register the subscriber *before* acking so no event emitted
        // after `Response::Subscribed` hits the wire can be lost
        // between the ack and the subscription. The contract is "any
        // event that occurs after the client sees Subscribed is
        // observable", which requires the registration to happen
        // first. Binding the scope at registration time is part of the
        // same contract: there is no window in which this subscriber is
        // registered under a different scope than the one it asked for.
        let (sub_id, rx) = match *from_pane {
            Some(pane_id) => event_bus.subscribe_scoped(EventScope::PaneInbox(pane_id)),
            // Identical to `subscribe_scoped(EventScope::Unscoped)`;
            // spelled with the plain entry point so the unchanged,
            // opted-out path reads as "the ordinary subscription" at
            // the call site — because that is exactly what it is.
            None => event_bus.subscribe(),
        };
        if let Err(e) = write_response_line(reader.get_mut(), &Response::Subscribed) {
            event_bus.unsubscribe(sub_id);
            return Err(e);
        }
        return stream_events(reader.into_inner(), event_bus, sub_id, rx);
    }
    let resp = dispatch_request(req, &command_tx);
    write_response_line(reader.get_mut(), &resp)?;
    Ok(())
}

/// Drain events from the bus into the wire until the connection dies
/// or the subscriber is unregistered. The subscription was already
/// registered by `handle_connection` before the Subscribed ack was
/// written, so any event observed from here on is part of the
/// post-ack stream the client can rely on.
///
/// Note there is deliberately **no filtering here**. What routing there
/// is happens in [`EventBus::emit`], before the event is offered to this
/// subscriber's bounded channel (Issue #306). For a connection that
/// opted into a pane scope, another pane's `PeerInbox` therefore never
/// occupies a slot in that channel and never counts toward its
/// dropped-event tally; for a connection that did not opt in, nothing is
/// filtered anywhere and it drains the full stream as it always has.
/// Filtering at this end of the pipe would get the observable behaviour
/// right and the queue pressure wrong, which is the whole payoff of
/// opting in.
///
/// If no real event shows up within [`HEARTBEAT_INTERVAL`], the loop
/// wakes up and writes a [`Event::Heartbeat`] to the wire. Its only
/// purpose is to force an I/O write: if the peer's read side is dead
/// (half-close) and the OS send buffer has filled, the write fails
/// and we unsubscribe promptly instead of holding a stale subscriber
/// slot until the next pane lifecycle event — which may be hours
/// away on a quiet session.
fn stream_events(
    conn: Stream,
    event_bus: EventBus,
    sub_id: super::events::SubId,
    rx: std::sync::mpsc::Receiver<super::Event>,
) -> Result<()> {
    stream_events_inner(conn, rx, HEARTBEAT_INTERVAL);
    event_bus.unsubscribe(sub_id);
    Ok(())
}

/// Inner loop split out so tests can drive it with a `Vec<u8>` sink
/// and a sub-second interval.
fn stream_events_inner<W: Write>(
    mut sink: W,
    rx: std::sync::mpsc::Receiver<super::Event>,
    heartbeat_interval: Duration,
) {
    loop {
        let event = match rx.recv_timeout(heartbeat_interval) {
            Ok(ev) => ev,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Event::Heartbeat {
                ts_ms: now_ms_ipc(),
            },
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let mut json = match serde_json::to_string(&event) {
            Ok(s) => s,
            Err(_) => continue,
        };
        json.push('\n');
        if sink.write_all(json.as_bytes()).is_err() || sink.flush().is_err() {
            break;
        }
    }
}

/// How often the subscribe stream emits a keep-alive when idle.
/// 30 s is short enough that a dropped client is released from the
/// subscriber table before it matters, and long enough that chatty
/// heartbeat noise in logs stays negligible.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

fn now_ms_ipc() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn read_line_or_eof<R: BufRead>(reader: &mut R, buf: &mut String) -> Result<Option<()>> {
    let n = reader.read_line(buf)?;
    Ok(if n == 0 { None } else { Some(()) })
}

fn write_response_line<W: Write>(w: &mut W, resp: &Response) -> Result<()> {
    let mut json = serde_json::to_string(resp)?;
    json.push('\n');
    w.write_all(json.as_bytes())?;
    w.flush()?;
    Ok(())
}

fn dispatch_request(req: Request, command_tx: &Sender<AppCommand>) -> Response {
    match req {
        Request::Hello { .. } => {
            Response::err_coded(err_code::PROTOCOL, "unexpected duplicate hello")
        }
        Request::List { from_pane } => {
            let (reply_tx, reply_rx) = oneshot::channel();
            if command_tx
                .send(AppCommand::List {
                    from_pane,
                    reply: reply_tx,
                })
                .is_err()
            {
                return Response::err_coded(err_code::SHUTTING_DOWN, "app shutting down");
            }
            match reply_rx.recv_timeout(APP_REPLY_TIMEOUT) {
                Ok(Ok(list)) => match serde_json::to_value(&list) {
                    Ok(v) => Response::ok_value(v),
                    Err(e) => {
                        Response::err_coded(err_code::INTERNAL, format!("serialize pane list: {e}"))
                    }
                },
                Ok(Err(err)) => err.into_response(),
                Err(e) => {
                    Response::err_coded(err_code::APP_TIMEOUT, format!("app did not respond: {e}"))
                }
            }
        }
        Request::Send {
            target,
            data,
            append_enter,
            from_pane,
        } => forward_unit(command_tx, |reply| AppCommand::Send {
            target,
            data: data.into_bytes(),
            append_enter,
            from_pane,
            reply,
        }),
        Request::Focus { target, from_pane } => {
            forward_unit(command_tx, |reply| AppCommand::Focus {
                target,
                from_pane,
                reply,
            })
        }
        Request::Close { target, from_pane } => {
            let (reply_tx, reply_rx) = oneshot::channel();
            if command_tx
                .send(AppCommand::Close {
                    target,
                    from_pane,
                    reply: reply_tx,
                })
                .is_err()
            {
                return Response::err_coded(err_code::SHUTTING_DOWN, "app shutting down");
            }
            match reply_rx.recv_timeout(APP_REPLY_TIMEOUT) {
                Ok(Ok(closed_id)) => {
                    Response::ok_value(serde_json::json!({ "id": closed_id, "closed": true }))
                }
                Ok(Err(err)) => err.into_response(),
                Err(e) => {
                    Response::err_coded(err_code::APP_TIMEOUT, format!("app did not respond: {e}"))
                }
            }
        }
        Request::Split {
            target,
            direction,
            command,
            id,
            role,
            cwd,
            from_pane,
            tab,
        } => {
            let (reply_tx, reply_rx) = oneshot::channel();
            if command_tx
                .send(AppCommand::Split {
                    target,
                    direction,
                    command,
                    name: id,
                    role,
                    cwd,
                    from_pane,
                    tab,
                    reply: reply_tx,
                })
                .is_err()
            {
                return Response::err_coded(err_code::SHUTTING_DOWN, "app shutting down");
            }
            match reply_rx.recv_timeout(APP_REPLY_TIMEOUT) {
                Ok(Ok(new_id)) => Response::ok_value(serde_json::json!({ "id": new_id })),
                Ok(Err(err)) => err.into_response(),
                Err(e) => {
                    Response::err_coded(err_code::APP_TIMEOUT, format!("app did not respond: {e}"))
                }
            }
        }
        Request::SpawnTab {
            command,
            id,
            label,
            role,
            cwd,
            from_pane,
        } => {
            let (reply_tx, reply_rx) = oneshot::channel();
            if command_tx
                .send(AppCommand::SpawnTab {
                    command,
                    name: id,
                    label,
                    role,
                    cwd,
                    from_pane,
                    reply: reply_tx,
                })
                .is_err()
            {
                return Response::err_coded(err_code::SHUTTING_DOWN, "app shutting down");
            }
            match reply_rx.recv_timeout(APP_REPLY_TIMEOUT) {
                Ok(Ok((new_id, tab_idx))) => {
                    Response::ok_value(serde_json::json!({ "id": new_id, "tab": tab_idx }))
                }
                Ok(Err(err)) => err.into_response(),
                Err(e) => {
                    Response::err_coded(err_code::APP_TIMEOUT, format!("app did not respond: {e}"))
                }
            }
        }
        Request::NewTab {
            command,
            id,
            label,
            role,
            cwd,
        } => {
            let (reply_tx, reply_rx) = oneshot::channel();
            if command_tx
                .send(AppCommand::NewTab {
                    command,
                    name: id,
                    label,
                    role,
                    cwd,
                    reply: reply_tx,
                })
                .is_err()
            {
                return Response::err_coded(err_code::SHUTTING_DOWN, "app shutting down");
            }
            match reply_rx.recv_timeout(APP_REPLY_TIMEOUT) {
                Ok(Ok(new_id)) => Response::ok_value(serde_json::json!({ "id": new_id })),
                Ok(Err(err)) => err.into_response(),
                Err(e) => {
                    Response::err_coded(err_code::APP_TIMEOUT, format!("app did not respond: {e}"))
                }
            }
        }
        // Subscribe is handled by the connection handler directly — it
        // switches the wire into event-stream mode rather than
        // round-tripping through App commands. If we see it here, the
        // handler called us by mistake; refuse rather than hang.
        Request::Subscribe { .. } => {
            Response::err_coded(err_code::PROTOCOL, "subscribe should be handled inline")
        }
        Request::Inspect {
            target,
            lines,
            include_cursor,
            from_pane,
        } => {
            let (reply_tx, reply_rx) = oneshot::channel();
            if command_tx
                .send(AppCommand::Inspect {
                    target,
                    lines,
                    include_cursor,
                    from_pane,
                    reply: reply_tx,
                })
                .is_err()
            {
                return Response::err_coded(err_code::SHUTTING_DOWN, "app shutting down");
            }
            match reply_rx.recv_timeout(APP_REPLY_TIMEOUT) {
                Ok(Ok(payload)) => Response::ok_value(payload),
                Ok(Err(err)) => err.into_response(),
                Err(e) => {
                    Response::err_coded(err_code::APP_TIMEOUT, format!("app did not respond: {e}"))
                }
            }
        }
        Request::PeerList { from_pane } => {
            let (reply_tx, reply_rx) = oneshot::channel();
            if command_tx
                .send(AppCommand::PeerList {
                    from_pane,
                    reply: reply_tx,
                })
                .is_err()
            {
                return Response::err_coded(err_code::SHUTTING_DOWN, "app shutting down");
            }
            match reply_rx.recv_timeout(APP_REPLY_TIMEOUT) {
                Ok(Ok(peers)) => match serde_json::to_value(&peers) {
                    Ok(v) => Response::ok_value(v),
                    Err(e) => {
                        Response::err_coded(err_code::INTERNAL, format!("serialize peers: {e}"))
                    }
                },
                Ok(Err(err)) => err.into_response(),
                Err(e) => {
                    Response::err_coded(err_code::APP_TIMEOUT, format!("app did not respond: {e}"))
                }
            }
        }
        // Channel delivery keeps the pre-#323 path verbatim: the same
        // `forward_unit` shape, the same `AppCommand::PeerSend`, the
        // same `Response::ok_unit()`. User-turn delivery needs a data
        // payload (it reports whether submission was observed) and
        // answers from the App's per-frame flush rather than inline,
        // so it gets its own command with a value-carrying reply.
        Request::PeerSend {
            from_pane,
            target,
            body,
            deliver: PeerDelivery::Channel,
        } => forward_unit(command_tx, |reply| AppCommand::PeerSend {
            from_pane,
            target,
            body,
            reply,
        }),
        Request::PeerSend {
            from_pane,
            target,
            body,
            deliver: PeerDelivery::UserTurn,
        } => {
            let (reply_tx, reply_rx) = oneshot::channel();
            if command_tx
                .send(AppCommand::PeerSendUserTurn {
                    from_pane,
                    target,
                    body,
                    reply: reply_tx,
                })
                .is_err()
            {
                return Response::err_coded(err_code::SHUTTING_DOWN, "app shutting down");
            }
            // The App answers this one asynchronously — it must not
            // block its render loop on a settle delay — but bounds
            // itself well inside `APP_REPLY_TIMEOUT`, so the ordinary
            // timeout still means "the App is wedged".
            match reply_rx.recv_timeout(APP_REPLY_TIMEOUT) {
                Ok(Ok(payload)) => Response::ok_value(payload),
                Ok(Err(err)) => err.into_response(),
                Err(e) => {
                    Response::err_coded(err_code::APP_TIMEOUT, format!("app did not respond: {e}"))
                }
            }
        }
        Request::PeerRegisterClient { pane_id, kind } => {
            forward_unit(command_tx, |reply| AppCommand::PeerRegisterClient {
                pane_id,
                kind,
                reply,
            })
        }
        Request::SetSummary { from_pane, summary } => {
            let (reply_tx, reply_rx) = oneshot::channel();
            if command_tx
                .send(AppCommand::SetSummary {
                    pane_id: from_pane,
                    summary,
                    reply: reply_tx,
                })
                .is_err()
            {
                return Response::err_coded(err_code::SHUTTING_DOWN, "app shutting down");
            }
            match reply_rx.recv_timeout(APP_REPLY_TIMEOUT) {
                Ok(Ok(pane)) => match serde_json::to_value(&pane) {
                    Ok(v) => Response::ok_value(serde_json::json!({ "pane": v })),
                    Err(e) => {
                        Response::err_coded(err_code::INTERNAL, format!("serialize pane: {e}"))
                    }
                },
                Ok(Err(err)) => err.into_response(),
                Err(e) => {
                    Response::err_coded(err_code::APP_TIMEOUT, format!("app did not respond: {e}"))
                }
            }
        }
        Request::SetPaneIdentity {
            target,
            name,
            role,
            from_pane,
        } => {
            let (reply_tx, reply_rx) = oneshot::channel();
            if command_tx
                .send(AppCommand::SetPaneIdentity {
                    target,
                    name,
                    role,
                    from_pane,
                    reply: reply_tx,
                })
                .is_err()
            {
                return Response::err_coded(err_code::SHUTTING_DOWN, "app shutting down");
            }
            match reply_rx.recv_timeout(APP_REPLY_TIMEOUT) {
                Ok(Ok(pane)) => match serde_json::to_value(&pane) {
                    Ok(v) => Response::ok_value(serde_json::json!({ "pane": v })),
                    Err(e) => {
                        Response::err_coded(err_code::INTERNAL, format!("serialize pane: {e}"))
                    }
                },
                Ok(Err(err)) => err.into_response(),
                Err(e) => {
                    Response::err_coded(err_code::APP_TIMEOUT, format!("app did not respond: {e}"))
                }
            }
        }
    }
}

/// Forward a command whose success result is `()` and translate the
/// reply into a [`Response`]. Factored out because three of the four
/// variants share this exact shape.
fn forward_unit(
    command_tx: &Sender<AppCommand>,
    build: impl FnOnce(oneshot::Sender<std::result::Result<(), super::CodedError>>) -> AppCommand,
) -> Response {
    let (reply_tx, reply_rx) = oneshot::channel();
    if command_tx.send(build(reply_tx)).is_err() {
        return Response::err_coded(err_code::SHUTTING_DOWN, "app shutting down");
    }
    match reply_rx.recv_timeout(APP_REPLY_TIMEOUT) {
        Ok(Ok(_)) => Response::ok_unit(),
        // App-originated error strings pass through uncoded for now —
        // plumbing a stable code through AppCommand replies is a
        // follow-up (see Issue #28 non-goals).
        Ok(Err(err)) => err.into_response(),
        Err(e) => Response::err_coded(err_code::APP_TIMEOUT, format!("app did not respond: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{Direction, PaneRef, PeerDelivery, Request};
    use std::sync::mpsc;

    #[test]
    fn dispatch_list_ok_when_app_replies() {
        // Pretend to be the App: spawn a thread that pulls a List
        // command off the channel and replies with an empty list.
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::List { reply, .. }) = rx.recv() {
                reply.send(Ok(Vec::new())).unwrap();
            }
        });

        let resp = dispatch_request(Request::List { from_pane: None }, &tx);
        handle.join().unwrap();

        match resp {
            Response::Ok { data } => {
                // An empty Vec<PaneInfo> serializes to a JSON array.
                assert!(data.is_array(), "expected array, got {data:?}");
                assert_eq!(data.as_array().map(|a| a.len()), Some(0));
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_focus_ok_when_app_replies_ok() {
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::Focus { reply, .. }) = rx.recv() {
                reply.send(Ok(())).unwrap();
            }
        });
        let resp = dispatch_request(
            Request::Focus {
                target: PaneRef::Focused,
                from_pane: None,
            },
            &tx,
        );
        handle.join().unwrap();
        assert!(matches!(resp, Response::Ok { .. }));
    }

    #[test]
    fn dispatch_focus_routes_app_coded_error_to_wire() {
        // When the App reply carries a code (new behavior on the
        // AppCommand reply type), the wire Response::Err must
        // surface that same code so clients can match on it.
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::Focus { reply, .. }) = rx.recv() {
                reply
                    .send(Err(super::super::CodedError::new(
                        super::super::err_code::PANE_NOT_FOUND,
                        "pane not found: Id(999)",
                    )))
                    .unwrap();
            }
        });
        let resp = dispatch_request(
            Request::Focus {
                target: PaneRef::Id(999),
                from_pane: None,
            },
            &tx,
        );
        handle.join().unwrap();
        match resp {
            Response::Err { message, code } => {
                assert!(message.contains("pane not found"));
                assert_eq!(
                    code.as_deref(),
                    Some(super::super::err_code::PANE_NOT_FOUND)
                );
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_focus_err_when_app_replies_err() {
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::Focus { reply, .. }) = rx.recv() {
                reply.send(Err("pane not found".into())).unwrap();
            }
        });
        let resp = dispatch_request(
            Request::Focus {
                target: PaneRef::Id(999),
                from_pane: None,
            },
            &tx,
        );
        handle.join().unwrap();
        match resp {
            Response::Err { message, .. } => assert!(message.contains("pane not found")),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_split_returns_new_id() {
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::Split { reply, .. }) = rx.recv() {
                reply.send(Ok(42)).unwrap();
            }
        });
        let resp = dispatch_request(
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
            &tx,
        );
        handle.join().unwrap();
        match resp {
            Response::Ok { data } => {
                assert_eq!(data.get("id").and_then(|v| v.as_u64()), Some(42));
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_send_forwards_data_and_enter() {
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::Send {
                data,
                append_enter,
                reply,
                ..
            }) = rx.recv()
            {
                assert_eq!(data, b"hello");
                assert!(append_enter);
                reply.send(Ok(())).unwrap();
            }
        });
        let resp = dispatch_request(
            Request::Send {
                target: PaneRef::Name("engineering".into()),
                data: "hello".into(),
                append_enter: true,
                from_pane: None,
            },
            &tx,
        );
        handle.join().unwrap();
        assert!(matches!(resp, Response::Ok { .. }));
    }

    #[test]
    fn dispatch_new_tab_returns_new_id() {
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::NewTab { reply, .. }) = rx.recv() {
                reply.send(Ok(11)).unwrap();
            }
        });
        let resp = dispatch_request(
            Request::NewTab {
                command: Some("cce".into()),
                id: Some("engineering".into()),
                label: None,
                role: None,
                cwd: None,
            },
            &tx,
        );
        handle.join().unwrap();
        match resp {
            Response::Ok { data } => {
                assert_eq!(data.get("id").and_then(|v| v.as_u64()), Some(11));
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    /// The two `deliver` arms route to different App commands, and
    /// which one a request lands on decides whether a body is *shown*
    /// to the recipient or *typed into its composer and submitted*.
    /// Swapping them would type every routine status report into a
    /// recipient's prompt — so pin both directions.
    #[test]
    fn peer_send_routes_channel_and_user_turn_to_different_commands() {
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            // Reply to each before waiting for the next: the caller
            // blocks on the first reply, so draining both up front
            // deadlocks.
            let first = rx.recv().expect("channel send arrives");
            match first {
                AppCommand::PeerSend { body, reply, .. } => {
                    assert_eq!(body, "status report");
                    reply.send(Ok(())).unwrap();
                }
                other => panic!("channel delivery must not take the user-turn path: {other:?}"),
            }
            let second = rx.recv().expect("user turn arrives");
            match second {
                AppCommand::PeerSendUserTurn { body, reply, .. } => {
                    assert_eq!(body, "/loop");
                    reply
                        .send(Ok(serde_json::json!({ "status": "submitted" })))
                        .unwrap();
                }
                other => panic!("user-turn delivery must not take the channel path: {other:?}"),
            }
        });

        // A pre-#323 caller sends no `deliver` at all; that deserializes
        // to Channel, which is the case this must never mis-route.
        let legacy: Request = serde_json::from_str(
            r#"{"cmd":"peer_send","from_pane":1,"target":{"id":4},"body":"status report"}"#,
        )
        .expect("legacy peer_send");
        let channel_resp = dispatch_request(legacy, &tx);
        let user_turn_resp = dispatch_request(
            Request::PeerSend {
                from_pane: 1,
                target: PaneRef::Id(4),
                body: "/loop".into(),
                deliver: PeerDelivery::UserTurn,
            },
            &tx,
        );
        handle.join().unwrap();

        // Channel keeps the pre-#323 unit reply verbatim.
        assert_eq!(channel_resp, Response::ok_unit());
        match user_turn_resp {
            Response::Ok { data } => assert_eq!(
                data.get("status").and_then(|v| v.as_str()),
                Some("submitted")
            ),
            other => panic!("expected Ok with a payload, got {other:?}"),
        }
    }

    #[test]
    fn forward_unit_shutting_down_is_coded() {
        // Drop the receiver to simulate the App having shut down; the
        // send should fail and the error must carry the SHUTTING_DOWN
        // code, not just a free-form message.
        let (tx, rx) = mpsc::channel::<AppCommand>();
        drop(rx);
        let resp = dispatch_request(
            Request::Focus {
                target: PaneRef::Focused,
                from_pane: None,
            },
            &tx,
        );
        match resp {
            Response::Err { code, .. } => {
                assert_eq!(code.as_deref(), Some(err_code::SHUTTING_DOWN))
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn stream_events_emits_heartbeat_when_idle() {
        use std::io::Cursor;
        use std::sync::mpsc as m;
        let (tx, rx) = m::channel::<Event>();
        // Run the inner loop on a worker with a short heartbeat
        // interval. Sleep long enough to cover multiple intervals
        // with generous slack for slow CI runners (macOS in GHA has
        // been observed not firing inside a tight 150 ms budget).
        let handle = thread::spawn(move || {
            let mut sink = Cursor::new(Vec::<u8>::new());
            stream_events_inner(&mut sink, rx, Duration::from_millis(50));
            sink.into_inner()
        });
        thread::sleep(Duration::from_millis(600));
        drop(tx);
        let bytes = handle.join().unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("\"heartbeat\""), "no heartbeat in {text:?}");
    }

    #[test]
    fn dispatch_refuses_second_hello() {
        // Duplicate hello after handshake should be an error path.
        let (tx, _rx) = mpsc::channel::<AppCommand>();
        let resp = dispatch_request(Request::Hello { client_pid: 1 }, &tx);
        match resp {
            Response::Err { message, .. } => assert!(message.contains("hello")),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_split_forwards_role() {
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::Split { role, reply, .. }) = rx.recv() {
                assert_eq!(role.as_deref(), Some("worker"));
                reply.send(Ok(7)).unwrap();
            }
        });
        let resp = dispatch_request(
            Request::Split {
                target: PaneRef::Focused,
                direction: Direction::Vertical,
                command: None,
                id: None,
                role: Some("worker".into()),
                cwd: None,
                from_pane: None,
                tab: None,
            },
            &tx,
        );
        handle.join().unwrap();
        assert!(matches!(resp, Response::Ok { .. }));
    }

    #[test]
    fn dispatch_split_forwards_tab_selector() {
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::Split { tab, reply, .. }) = rx.recv() {
                assert_eq!(tab, Some(super::super::TabSelector::Name("workers".into())));
                reply.send(Ok(7)).unwrap();
            }
        });
        let resp = dispatch_request(
            Request::Split {
                target: PaneRef::Focused,
                direction: Direction::Vertical,
                command: None,
                id: None,
                role: None,
                cwd: None,
                from_pane: Some(1),
                tab: Some(super::super::TabSelector::Name("workers".into())),
            },
            &tx,
        );
        handle.join().unwrap();
        assert!(matches!(resp, Response::Ok { .. }));
    }

    #[test]
    fn dispatch_spawn_tab_returns_pane_id_and_tab_index() {
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::SpawnTab {
                command,
                name,
                label,
                role,
                cwd,
                from_pane,
                reply,
            }) = rx.recv()
            {
                assert_eq!(command.as_deref(), Some("claude"));
                assert_eq!(name.as_deref(), Some("worker-a"));
                assert_eq!(label.as_deref(), Some("workers"));
                assert_eq!(role.as_deref(), Some("worker"));
                assert_eq!(cwd.as_deref(), Some("/tmp/work"));
                assert_eq!(from_pane, Some(2));
                reply.send(Ok((42, 3))).unwrap();
            }
        });
        let resp = dispatch_request(
            Request::SpawnTab {
                command: Some("claude".into()),
                id: Some("worker-a".into()),
                label: Some("workers".into()),
                role: Some("worker".into()),
                cwd: Some("/tmp/work".into()),
                from_pane: Some(2),
            },
            &tx,
        );
        handle.join().unwrap();
        match resp {
            Response::Ok { data } => {
                assert_eq!(data.get("id").and_then(|v| v.as_u64()), Some(42));
                assert_eq!(data.get("tab").and_then(|v| v.as_u64()), Some(3));
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn dispatch_new_tab_forwards_role() {
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::NewTab { role, reply, .. }) = rx.recv() {
                assert_eq!(role.as_deref(), Some("leader"));
                reply.send(Ok(9)).unwrap();
            }
        });
        let resp = dispatch_request(
            Request::NewTab {
                command: None,
                id: None,
                label: None,
                role: Some("leader".into()),
                cwd: None,
            },
            &tx,
        );
        handle.join().unwrap();
        assert!(matches!(resp, Response::Ok { .. }));
    }

    #[test]
    fn dispatch_inspect_forwards_payload() {
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::Inspect {
                lines,
                include_cursor,
                reply,
                ..
            }) = rx.recv()
            {
                assert_eq!(lines, Some(3));
                assert!(include_cursor);
                let payload = serde_json::json!({
                    "pane": { "id": 7, "name": "worker-foo" },
                    "screen": { "rows": 24, "cols": 80, "line_start": 21, "line_count": 3 },
                    "lines": [
                        { "row": 21, "text": "" },
                        { "row": 22, "text": "" },
                        { "row": 23, "text": "Allow this tool use? (y/n)" },
                    ],
                    "text": "\n\nAllow this tool use? (y/n)",
                    "cursor": { "visible": true, "row": 23, "col": 0 },
                });
                reply.send(Ok(payload)).unwrap();
            }
        });
        let resp = dispatch_request(
            Request::Inspect {
                target: PaneRef::Name("worker-foo".into()),
                lines: Some(3),
                include_cursor: true,
                from_pane: None,
            },
            &tx,
        );
        handle.join().unwrap();
        match resp {
            Response::Ok { data } => {
                assert_eq!(
                    data.get("pane")
                        .and_then(|p| p.get("id"))
                        .and_then(|v| v.as_u64()),
                    Some(7)
                );
                assert_eq!(
                    data.get("cursor")
                        .and_then(|c| c.get("visible"))
                        .and_then(|v| v.as_bool()),
                    Some(true)
                );
                let lines = data.get("lines").and_then(|v| v.as_array()).unwrap();
                assert_eq!(lines.len(), 3);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_close_returns_id_and_closed_flag() {
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::Close { reply, .. }) = rx.recv() {
                reply.send(Ok(13)).unwrap();
            }
        });
        let resp = dispatch_request(
            Request::Close {
                target: PaneRef::Name("worker-foo".into()),
                from_pane: None,
            },
            &tx,
        );
        handle.join().unwrap();
        match resp {
            Response::Ok { data } => {
                assert_eq!(data.get("id").and_then(|v| v.as_u64()), Some(13));
                assert_eq!(data.get("closed").and_then(|v| v.as_bool()), Some(true));
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_close_surfaces_last_pane_code() {
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::Close { reply, .. }) = rx.recv() {
                reply
                    .send(Err(super::super::CodedError::new(
                        super::super::err_code::LAST_PANE,
                        "cannot close the last pane of the only tab",
                    )))
                    .unwrap();
            }
        });
        let resp = dispatch_request(
            Request::Close {
                target: PaneRef::Focused,
                from_pane: None,
            },
            &tx,
        );
        handle.join().unwrap();
        match resp {
            Response::Err { code, .. } => {
                assert_eq!(code.as_deref(), Some(super::super::err_code::LAST_PANE));
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn drop_removes_unix_socket_file() {
        use std::path::PathBuf;

        // Bind on a unique temp path so the test doesn't race with a
        // real renga instance or other tests.
        let dir = std::env::temp_dir().join(format!(
            "renga-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path: PathBuf = dir.join("renga-test.sock");
        let endpoint = EndpointName::socket(sock_path.clone());

        let (tx, _rx) = mpsc::channel::<AppCommand>();
        let server = IpcServer::spawn(endpoint, tx, "test-token".into(), EventBus::new()).unwrap();

        // Socket file should exist after binding.
        assert!(sock_path.exists(), "socket file not created");

        // Dropping IpcServer should remove it.
        drop(server);
        assert!(!sock_path.exists(), "socket file not removed on drop");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── Issue #306: wire-level PeerInbox routing ──────────────
    //
    // The unit tests in `super::super::events` pin the routing rule
    // itself. These pin the half that only exists once a real socket is
    // involved: `Subscribe.from_pane` travelling over the wire, the
    // `EventScope` it binds on the bus, and which JSON lines actually
    // come back out of `client::subscribe_events` /
    // `client::subscribe_inbox_events`.
    //
    // Unix-only because the harness needs a filesystem path it can make
    // unique per test. Nothing platform-specific is left unverified —
    // the routing decision is transport-independent.

    #[cfg(unix)]
    use crate::ipc::client;
    #[cfg(unix)]
    use crate::ipc::endpoint::ENV_TOKEN;

    /// The token the harness publishes as `RENGA_TOKEN` *and* hands to
    /// `IpcServer::spawn`, so the real client handshake
    /// (`verify_session_token`) accepts the connection instead of
    /// refusing it as a foreign instance.
    #[cfg(unix)]
    const WIRE_TOKEN: &str = "renga-306-wire-test-token";

    /// Ceiling on every blocking wait in these tests. Long enough that
    /// a loaded CI runner never trips it, short enough that a routing
    /// regression fails the suite in seconds instead of hanging it
    /// until the harness is killed — which is why the assertions below
    /// use `recv_timeout` rather than `recv`. Also well under
    /// [`HEARTBEAT_INTERVAL`], so no keep-alive can arrive mid-test.
    #[cfg(unix)]
    const WIRE_TIMEOUT: Duration = Duration::from_secs(10);

    /// Pane id carried by the barrier event. Deliberately far outside
    /// the ids the tests use as real panes, so a mis-routed event can
    /// never be mistaken for the barrier.
    #[cfg(unix)]
    const BARRIER_PANE: usize = 999_000;

    /// `cargo test` runs test functions on several threads in one
    /// process, and the client handshake reads `RENGA_TOKEN` from the
    /// environment on each subscriber thread. Serialize the wire tests
    /// among themselves so one test's restore cannot land in the middle
    /// of another's handshake.
    #[cfg(unix)]
    static WIRE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Distinguishes concurrent harnesses inside one test process
    /// without spending path bytes on a nanosecond timestamp — see
    /// [`WireHarness::start`] for why every byte matters here.
    #[cfg(unix)]
    static WIRE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// Longest socket path these tests will build before refusing to
    /// run. `sockaddr_un::sun_path` is 108 bytes on Linux but only
    /// **104** on macOS, and the bind failure it produces surfaces as
    /// an opaque "length exceeds capacity" error from deep inside the
    /// socket library. Checking here turns that into a message naming
    /// the actual cause. 100 leaves headroom under the smaller limit.
    #[cfg(unix)]
    const WIRE_SOCKET_PATH_MAX: usize = 100;

    /// A live `IpcServer` on a private socket, plus the `EventBus` the
    /// test emits into and the environment it needs to be reachable.
    ///
    /// Teardown runs in `Drop` rather than at the end of each test so a
    /// failed assertion still unlinks the socket and puts `RENGA_TOKEN`
    /// back.
    #[cfg(unix)]
    struct WireHarness {
        /// `Option` only so `Drop` can shut the server down before the
        /// directory is removed.
        server: Option<IpcServer>,
        endpoint: EndpointName,
        bus: EventBus,
        dir: std::path::PathBuf,
        /// No wire test drives an `AppCommand`; the receiver is kept
        /// alive only so the server's sender never looks disconnected.
        _command_rx: mpsc::Receiver<AppCommand>,
        _env_guard: std::sync::MutexGuard<'static, ()>,
        prev_token: Option<String>,
    }

    #[cfg(unix)]
    impl WireHarness {
        fn start(tag: &str) -> Self {
            let env_guard = WIRE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            // The path has to stay *short*, which is why `tag` names the
            // harness in panic messages instead of appearing in the
            // directory name. A unix socket path must fit
            // `sockaddr_un::sun_path`: 104 bytes on macOS, where
            // `std::env::temp_dir()` is already ~49 of them
            // (`/var/folders/<2>/<30>/T/`). A descriptive
            // `renga-306-<tag>-<pid>-<nanos>` directory plus a
            // `renga-test.sock` leaf overflows that on every macos-latest
            // CI run while passing comfortably on Linux's 108-byte limit
            // and short `/tmp`. Pid plus a process-local counter is just
            // as unique — against a real renga instance, a sibling test,
            // and a rerun — in a fraction of the bytes.
            let dir = std::env::temp_dir().join(format!(
                "r306-{}-{}",
                std::process::id(),
                WIRE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir)
                .unwrap_or_else(|e| panic!("create wire-test dir for {tag}: {e}"));
            let sock_path = dir.join("s");
            assert!(
                sock_path.as_os_str().len() <= WIRE_SOCKET_PATH_MAX,
                "wire-test socket path for {tag} is {} bytes ({}); \
                 sockaddr_un::sun_path caps this at 104 on macOS. \
                 Shorten it or point TMPDIR somewhere shallower.",
                sock_path.as_os_str().len(),
                sock_path.display()
            );
            let endpoint = EndpointName::socket(sock_path);

            let prev_token = std::env::var(ENV_TOKEN).ok();
            std::env::set_var(ENV_TOKEN, WIRE_TOKEN);

            let bus = EventBus::new();
            let (command_tx, command_rx) = mpsc::channel::<AppCommand>();
            let server = IpcServer::spawn(
                endpoint.clone(),
                command_tx,
                WIRE_TOKEN.to_string(),
                bus.clone(),
            )
            .expect("bind wire-test IPC server");

            Self {
                server: Some(server),
                endpoint,
                bus,
                dir,
                _command_rx: command_rx,
                _env_guard: env_guard,
                prev_token,
            }
        }

        /// Start a subscriber and wait until the bus has actually
        /// registered it. Without the wait the test would race the
        /// handshake and could emit into an empty subscriber table.
        fn subscribe(&self, scope: EventScope) -> Collector {
            let want = self.bus.subscriber_count() + 1;
            let collector = spawn_collector(self.endpoint.clone(), scope);
            let deadline = std::time::Instant::now() + WIRE_TIMEOUT;
            loop {
                let have = self.bus.subscriber_count();
                if have >= want {
                    return collector;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "only {have} of {want} subscribers registered on the bus"
                );
                thread::sleep(Duration::from_millis(5));
            }
        }
    }

    #[cfg(unix)]
    impl Drop for WireHarness {
        fn drop(&mut self) {
            // Server first: its own Drop unlinks the socket file, and
            // doing that before the directory disappears keeps the
            // unlink from racing the removal.
            drop(self.server.take());
            match self.prev_token.take() {
                Some(v) => std::env::set_var(ENV_TOKEN, v),
                None => std::env::remove_var(ENV_TOKEN),
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// One real subscriber process-side: a thread blocked in
    /// `client::subscribe_*`, plus the channel it forwards events into.
    #[cfg(unix)]
    struct Collector {
        rx: mpsc::Receiver<Event>,
        handle: thread::JoinHandle<()>,
    }

    #[cfg(unix)]
    impl Collector {
        /// Everything this subscriber received, up to (and excluding)
        /// the barrier event.
        ///
        /// The barrier is what makes these tests deterministic instead
        /// of timing-based: proving a subscriber did *not* get an event
        /// otherwise means waiting some arbitrary interval and hoping.
        /// Emitting a lifecycle event that every scope accepts, after
        /// the events under test, turns "did not receive" into "reached
        /// the barrier without it".
        fn drain_to_barrier(&self, who: &str) -> Vec<Event> {
            let mut seen = Vec::new();
            loop {
                match self.rx.recv_timeout(WIRE_TIMEOUT) {
                    Ok(event) if is_barrier(&event) => return seen,
                    Ok(event) => seen.push(event),
                    Err(e) => {
                        panic!("{who} never reached the barrier event ({e:?}); saw {seen:?}")
                    }
                }
            }
        }

        fn join(self) {
            self.handle.join().expect("subscriber thread panicked");
        }
    }

    #[cfg(unix)]
    fn is_barrier(event: &Event) -> bool {
        matches!(event, Event::PaneExited { id, .. } if *id == BARRIER_PANE)
    }

    #[cfg(unix)]
    fn spawn_collector(endpoint: EndpointName, scope: EventScope) -> Collector {
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let forward = move |event: Event| -> bool {
                // Heartbeats are transport keep-alives, not part of what
                // these tests assert. (WIRE_TIMEOUT is below
                // HEARTBEAT_INTERVAL so one should never appear at all;
                // dropping them keeps a slow runner from turning that
                // into a spurious failure.)
                if matches!(event, Event::Heartbeat { .. }) {
                    return true;
                }
                let stop = is_barrier(&event);
                if tx.send(event).is_err() {
                    return false;
                }
                !stop
            };
            // The two entry points differ only in the `from_pane` they
            // put on the wire, which is exactly what is under test —
            // so drive the real ones rather than a shared helper.
            let result = match scope {
                EventScope::Unscoped => client::subscribe_events(&endpoint, forward),
                EventScope::PaneInbox(pane_id) => {
                    client::subscribe_inbox_events(&endpoint, pane_id, forward)
                }
            };
            // Surfacing this here beats letting a failed subscribe show
            // up only as a barrier timeout on the other side.
            result.expect("subscriber stream ended with an error");
        });
        Collector { rx, handle }
    }

    #[cfg(unix)]
    fn wire_inbox(target_pane: usize, body: &str) -> Event {
        Event::PeerInbox {
            target_pane,
            from_pane: 42,
            from_name: Some("sender".into()),
            from_kind: None,
            body: body.to_string(),
            ts_ms: 0,
        }
    }

    #[cfg(unix)]
    fn wire_started(id: usize) -> Event {
        Event::PaneStarted {
            id,
            name: None,
            role: None,
            ts_ms: 0,
        }
    }

    #[cfg(unix)]
    fn wire_barrier() -> Event {
        Event::PaneExited {
            id: BARRIER_PANE,
            name: None,
            role: None,
            ts_ms: 0,
        }
    }

    #[cfg(unix)]
    #[test]
    fn wire_peer_inbox_skips_a_pane_scope_it_is_not_addressed_to_but_still_reaches_the_unscoped() {
        const PANE_A: usize = 11;
        const PANE_B: usize = 22;

        let harness = WireHarness::start("inbox-routing");
        let a = harness.subscribe(EventScope::PaneInbox(PANE_A));
        let b = harness.subscribe(EventScope::PaneInbox(PANE_B));
        let unscoped = harness.subscribe(EventScope::Unscoped);

        harness.bus.emit(wire_inbox(PANE_A, "for pane A"));
        harness.bus.emit(wire_started(PANE_B));
        harness.bus.emit(wire_barrier());

        let got_a = a.drain_to_barrier("pane A subscriber");
        let got_b = b.drain_to_barrier("pane B subscriber");
        let got_unscoped = unscoped.drain_to_barrier("unscoped subscriber");
        a.join();
        b.join();
        unscoped.join();

        // (a) The peer message reaches the pane it is addressed to.
        assert_eq!(
            got_a,
            vec![wire_inbox(PANE_A, "for pane A"), wire_started(PANE_B)],
            "pane A must receive its own inbox event and the lifecycle event"
        );
        // (b) It does not reach a subscriber that opted into a *different*
        // pane — the narrowing #306 sells. The lifecycle event still
        // arrives, so this is routing rather than a dead stream.
        assert_eq!(
            got_b,
            vec![wire_started(PANE_B)],
            "pane B must see the lifecycle event but not pane A's inbox"
        );
        // (c) The wire-level proof that #306 is **non-breaking**: a
        // subscription that sent no `from_pane` — `renga events` and
        // every pre-#306 client — still receives pane A's `PeerInbox`,
        // in the same order, exactly as it did before the change. If
        // this ever flips to lifecycle-only the change has silently
        // become a break and needs a major, not a minor.
        assert_eq!(
            got_unscoped,
            vec![wire_inbox(PANE_A, "for pane A"), wire_started(PANE_B)],
            "an unscoped subscriber must still receive every PeerInbox, as it did before #306"
        );
    }

    #[cfg(unix)]
    #[test]
    fn wire_every_subscriber_bound_to_the_same_pane_receives_the_inbox_event() {
        const PANE: usize = 8;

        // Two clients can legitimately watch one pane at the same
        // moment — e.g. a restarted `renga mcp-peer` subprocess whose
        // predecessor has not been reaped yet. Routing must fan out to
        // both; picking one would silently drop peer messages for the
        // duration of the overlap.
        let harness = WireHarness::start("same-pane-fanout");
        let first = harness.subscribe(EventScope::PaneInbox(PANE));
        let second = harness.subscribe(EventScope::PaneInbox(PANE));
        let other = harness.subscribe(EventScope::PaneInbox(PANE + 1));

        harness
            .bus
            .emit(wire_inbox(PANE, "to everyone on this pane"));
        harness.bus.emit(wire_barrier());

        let got_first = first.drain_to_barrier("first subscriber on the pane");
        let got_second = second.drain_to_barrier("second subscriber on the pane");
        let got_other = other.drain_to_barrier("subscriber on a different pane");
        first.join();
        second.join();
        other.join();

        let expected = vec![wire_inbox(PANE, "to everyone on this pane")];
        assert_eq!(got_first, expected);
        assert_eq!(got_second, expected);
        assert!(
            got_other.is_empty(),
            "a neighbouring pane id must not receive the event: {got_other:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn wire_peer_inbox_for_a_pane_in_another_tab_is_routed_the_same_way() {
        // Pane ids are unique across the whole renga session, not per
        // tab, so this layer has no concept of tabs at all: a
        // `PeerSend` that resolved to a pane in another tab produces
        // exactly the same `PeerInbox { target_pane }` as a same-tab
        // one, and routing neither can nor needs to tell them apart.
        // Resolving a cross-tab target to a pane id in the first place
        // is the App's job and is covered in
        // `src/app/tests/codex_peer.rs`.
        const LOCAL_PANE: usize = 3;
        const OTHER_TAB_PANE: usize = 4_097;

        let harness = WireHarness::start("cross-tab-routing");
        let local = harness.subscribe(EventScope::PaneInbox(LOCAL_PANE));
        let other_tab = harness.subscribe(EventScope::PaneInbox(OTHER_TAB_PANE));
        let unscoped = harness.subscribe(EventScope::Unscoped);

        harness.bus.emit(wire_inbox(OTHER_TAB_PANE, "across tabs"));
        harness.bus.emit(wire_barrier());

        let got_local = local.drain_to_barrier("same-tab subscriber");
        let got_other_tab = other_tab.drain_to_barrier("other-tab subscriber");
        let got_unscoped = unscoped.drain_to_barrier("unscoped subscriber");
        local.join();
        other_tab.join();
        unscoped.join();

        assert_eq!(
            got_other_tab,
            vec![wire_inbox(OTHER_TAB_PANE, "across tabs")],
            "the addressed pane must receive it regardless of which tab it lives in"
        );
        assert!(
            got_local.is_empty(),
            "wrong pane received it: {got_local:?}"
        );
        // A cross-tab pane id is not special to an unscoped subscriber
        // either: it opted out of routing, so it sees this message just
        // like it saw every peer message before #306.
        assert_eq!(
            got_unscoped,
            vec![wire_inbox(OTHER_TAB_PANE, "across tabs")],
            "an unscoped subscriber must still receive a cross-tab PeerInbox"
        );
    }
}
