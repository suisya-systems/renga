//! IPC client: opens a short-lived connection to the running renga
//! instance, performs the [`Request::Hello`] handshake, then sends
//! exactly one [`Request`] and reads exactly one [`Response`].
//!
//! Connection lifecycle matches the server in [`super::server`]: one
//! request per connection, closed by the client dropping the stream.

use std::io::{BufRead, BufReader, Write};
use std::sync::mpsc;
use std::thread;

use anyhow::{anyhow, Context, Result};
use interprocess::local_socket::{prelude::*, Stream};
use subtle::ConstantTimeEq;

use super::endpoint::{EndpointName, ENV_TOKEN};
use super::{Event, EventScope, Request, Response, RESPONSE_TIMEOUT};

/// Send a single request to the endpoint and return the response.
///
/// The blocking read on `interprocess::Stream` has no portable timeout
/// API in 2.x, so we run `converse` on a helper thread and wait on a
/// channel with [`RESPONSE_TIMEOUT`]. If the server deadlocks, the main
/// thread unblocks and returns an error instead of hanging forever —
/// the helper thread is detached and cleaned up by the OS when the
/// client process exits.
pub fn send_request(endpoint: &EndpointName, request: &Request) -> Result<Response> {
    send_request_inner(endpoint, request, None)
}

/// Like [`send_request`], but refuses to send unless the server
/// advertised `required_cap` in its hello (see
/// [`super::CAP_CALLER_SCOPE`]).
///
/// This is the **fail-closed** path for version skew. renga registers
/// `renga mcp-peer` by absolute path, so upgrading the binary on disk
/// leaves the *old* server process running while every newly spawned
/// mcp-peer is the *new* one. An old server parses `from_pane` as an
/// unknown field, drops it, and happily operates on whatever tab the
/// human is looking at — a wrong-tab `send_keys` with no error
/// anywhere. Erroring out with "restart renga" is the only safe
/// answer; silently falling back to the old semantics is exactly the
/// bug #288 exists to remove.
pub fn send_request_requiring(
    endpoint: &EndpointName,
    request: &Request,
    required_cap: &'static str,
) -> Result<Response> {
    send_request_inner(endpoint, request, Some(required_cap))
}

fn send_request_inner(
    endpoint: &EndpointName,
    request: &Request,
    required_cap: Option<&'static str>,
) -> Result<Response> {
    let name_string = endpoint.as_str().to_string();
    let endpoint_clone = endpoint.clone();
    let request_clone = request.clone();
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("renga-ipc-client".into())
        .spawn(move || {
            let result = (|| -> Result<Response> {
                let name = make_connection_name(&endpoint_clone)?;
                let conn = Stream::connect(name)
                    .with_context(|| format!("connect to {}", endpoint_clone.as_str()))?;
                converse(conn, &request_clone, required_cap)
            })();
            let _ = tx.send(result);
        })
        .context("spawn IPC client thread")?;

    match rx.recv_timeout(RESPONSE_TIMEOUT) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(anyhow!(
            "no response from renga within {:?} (endpoint: {})",
            RESPONSE_TIMEOUT,
            name_string
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(anyhow!("IPC client thread panicked")),
    }
}

fn make_connection_name(endpoint: &EndpointName) -> Result<interprocess::local_socket::Name<'_>> {
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

/// What the running renga server said about itself in its
/// [`Response::Hello`].
///
/// Every field here has always been on the wire — the handshake that
/// precedes *every* request already carries it. Before #304 it was
/// parsed and dropped on the floor inside [`converse`], reachable only
/// indirectly through the `[server_too_old]` string
/// [`require_capability`] builds. This type is what stops it being
/// thrown away, so a caller can ask "what does this server support?"
/// instead of inferring it from a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerHandshake {
    /// PID of the renga process actually serving this endpoint. Note
    /// this is the *server* process, which can be an older build than
    /// the binary running this client — see [`send_request_requiring`].
    pub server_pid: u32,
    /// Feature tokens the server advertised (see
    /// [`super::SERVER_CAPABILITIES`]). Empty from any pre-#288
    /// server: they omit the field entirely, and `serde(default)`
    /// turns that into an empty vec. Empty therefore means "asked, and
    /// it supports nothing", which is a *different* fact from "could
    /// not ask" — callers must not conflate the two.
    pub capabilities: Vec<String>,
}

/// Complete the [`Request::Hello`] handshake and return what the
/// server advertised, **without sending any command**.
///
/// This is the ungated introspection path behind the `server_info` MCP
/// tool (#304). Three properties make it safe against arbitrarily old
/// servers, which is the whole point:
///
/// 1. It writes only `hello`. The server answers the handshake before
///    reading a command and treats the following EOF as a clean close
///    (`read_line_or_eof` → `Ok(())` in [`super::server`]), so a
///    handshake-only connection is valid against every renga server
///    that has ever shipped.
/// 2. It sends no [`Request`] variant beyond `hello`. A dedicated
///    variant would be rejected as `protocol` by existing servers —
///    that is learning-by-failed-attempt, exactly the pattern #304
///    exists to replace.
/// 3. It gates on nothing, so it still answers for a server that
///    advertises no capabilities at all.
///
/// The session-token check is deliberately kept: reporting the
/// capabilities of a renga instance that is not ours would be worse
/// than reporting nothing, because the caller would pre-flight against
/// one instance and then send commands to another.
pub fn probe_server(endpoint: &EndpointName) -> Result<ServerHandshake> {
    let name_string = endpoint.as_str().to_string();
    let endpoint_clone = endpoint.clone();
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("renga-ipc-probe".into())
        .spawn(move || {
            let result = (|| -> Result<ServerHandshake> {
                let name = make_connection_name(&endpoint_clone)?;
                let conn = Stream::connect(name)
                    .with_context(|| format!("connect to {}", endpoint_clone.as_str()))?;
                perform_handshake(&mut BufReader::new(conn))
            })();
            let _ = tx.send(result);
        })
        .context("spawn IPC probe thread")?;

    match rx.recv_timeout(RESPONSE_TIMEOUT) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(anyhow!(
            "no response from renga within {:?} (endpoint: {})",
            RESPONSE_TIMEOUT,
            name_string
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(anyhow!("IPC probe thread panicked")),
    }
}

/// Send `hello`, validate the reply, and hand back what the server
/// advertised. Shared by [`converse`], [`subscribe_events_scoped`], and
/// [`probe_server`] so the three cannot drift on token verification or
/// on the error strings operators grep for.
fn perform_handshake(reader: &mut BufReader<Stream>) -> Result<ServerHandshake> {
    let hello = Request::Hello {
        client_pid: std::process::id(),
    };
    write_request_line(reader.get_mut(), &hello)?;
    match read_response_line(reader)? {
        Response::Hello {
            server_pid,
            session_token,
            capabilities,
        } => {
            // Verifying the token here is also what makes a cached
            // capability answer safe without any staleness key: a
            // `ServerHandshake` can only ever come from the instance
            // that published this process's `RENGA_TOKEN`. If renga
            // restarts, either the PID-derived endpoint no longer
            // exists or the token no longer matches — both fail, and
            // neither can masquerade as a successful probe of a server
            // whose capabilities have silently changed.
            verify_session_token(&session_token, std::env::var(ENV_TOKEN).ok().as_deref())?;
            Ok(ServerHandshake {
                server_pid,
                capabilities,
            })
        }
        Response::Err { message, code } => Err(anyhow!(
            "server refused hello: {}",
            fmt_err(&message, &code)
        )),
        Response::Ok { .. } | Response::Subscribed => Err(anyhow!("unexpected response to hello")),
    }
}

fn converse(
    conn: Stream,
    request: &Request,
    required_cap: Option<&'static str>,
) -> Result<Response> {
    let mut reader = BufReader::new(conn);

    // Handshake
    let handshake = perform_handshake(&mut reader)?;
    if let Some(cap) = required_cap {
        require_capability(cap, &handshake.capabilities)?;
    }

    // Actual command
    write_request_line(reader.get_mut(), request)?;
    let resp = read_response_line(&mut reader)?;
    Ok(resp)
}

/// Subscribe to the **whole** event stream: every lifecycle event
/// (`pane_started`, `pane_exited`, `events_dropped`, `heartbeat`) *and*
/// every [`Event::PeerInbox`], whatever pane it is addressed to. This is
/// the tap behind `renga events` and it behaves exactly as it did before
/// Issue #306 — the request it puts on the wire declares no pane, so the
/// server applies no routing to it.
///
/// Scoping is opt-in and this entry point declines it. A caller that
/// only cares about one pane's mail should use [`subscribe_inbox_events`]
/// instead: it gets the same lifecycle events without the rest of the
/// session's peer traffic passing through its queue. Everything else
/// about the two is identical.
///
/// Opens a long-lived connection, completes the handshake, sends
/// [`Request::Subscribe`], then streams [`Event`]s into `on_event`
/// until either the server closes the connection, the callback
/// returns `false`, or an I/O error occurs.
///
/// Unlike [`send_request`], this function blocks on the caller's
/// thread for the full lifetime of the stream. Callers that want a
/// finite stream should wrap it in a thread or return `false` from
/// `on_event` when done.
pub fn subscribe_events<F>(endpoint: &EndpointName, on_event: F) -> Result<()>
where
    F: FnMut(Event) -> bool,
{
    subscribe_events_scoped(endpoint, EventScope::Unscoped, on_event)
}

/// Subscribe to lifecycle events **plus only** the [`Event::PeerInbox`]
/// messages addressed to `pane_id` — the opt-in narrowing added by Issue
/// #306. Otherwise identical to [`subscribe_events`], including the
/// blocking / `false`-to-stop contract; the difference is purely that
/// other panes' peer messages are never sent down this connection.
///
/// `pane_id` is the pane the caller itself runs in, read from
/// `RENGA_PANE_ID`. It travels as `Request::Subscribe { from_pane }` and
/// is the *only* thing that binds this connection to an inbox: the
/// server deliberately does not infer it from the handshake pid or from
/// an earlier `PeerRegisterClient`, since neither describes *this*
/// subscription.
///
/// **The caller must still check `target_pane` on every `PeerInbox` it
/// receives.** Server-side routing is an optimisation layered on top of
/// that check, not a replacement for it, for a concrete reason: a renga
/// binary can be upgraded on disk while the old server process keeps
/// running, so this client may well be talking to a pre-#306 server that
/// broadcasts every peer message to every subscriber. The client-side
/// comparison is what keeps that combination correct. It also costs
/// nothing to keep — see `classify_inbox_event` in the `mcp_peer`
/// module.
///
/// Naming a pane here is not authentication; any process running as this
/// user can name any pane id (see the threat model in [`super`]). What
/// it buys is defense in depth: another pane's peer messages are no
/// longer copied into *this* subscriber's queue, which removes both the
/// unintended delivery to a pane the message was not meant for and the
/// queue pressure those copies caused. Callers that decline the opt-in
/// keep the full stream and give up nothing else.
pub fn subscribe_inbox_events<F>(endpoint: &EndpointName, pane_id: usize, on_event: F) -> Result<()>
where
    F: FnMut(Event) -> bool,
{
    subscribe_events_scoped(endpoint, EventScope::PaneInbox(pane_id), on_event)
}

/// Shared body of [`subscribe_events`] and [`subscribe_inbox_events`].
///
/// The two public entry points exist so a caller has to say which slice
/// of the stream it wants; the wire difference between them is exactly
/// the `from_pane` field this function derives from `scope`. Keeping the
/// I/O in one place means the handshake, the `Subscribed` ack handling
/// and the forward-compat skip logic cannot drift between them.
fn subscribe_events_scoped<F>(
    endpoint: &EndpointName,
    scope: EventScope,
    mut on_event: F,
) -> Result<()>
where
    F: FnMut(Event) -> bool,
{
    let name = make_connection_name(endpoint)?;
    let conn =
        Stream::connect(name).with_context(|| format!("connect to {}", endpoint.as_str()))?;
    let mut reader = BufReader::new(conn);

    // Handshake (same as converse). Event subscribers don't gate on
    // capabilities — not on `subscribe_pane_scope` either: unknown
    // `Event` variants are skipped by the read loop below, and a
    // pre-#306 server drops the unknown `from_pane` and simply
    // broadcasts, which the caller's own `target_pane` check absorbs. An
    // old server is therefore degraded-but-correct here rather than
    // silently wrong, and failing closed would only take away a stream
    // that still works.
    perform_handshake(&mut reader)?;

    // Switch into event-stream mode.
    write_request_line(reader.get_mut(), &subscribe_request_for(scope))?;
    match read_response_line(&mut reader)? {
        Response::Subscribed => {}
        Response::Err { message, code } => {
            return Err(anyhow!("subscribe refused: {}", fmt_err(&message, &code)));
        }
        other => return Err(anyhow!("unexpected response to subscribe: {other:?}")),
    }

    // Stream events as JSON Lines until EOF or callback stops.
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(());
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        // Forward-compat: skip events whose `type` tag this client
        // doesn't know about. A future renga server may emit new
        // Event variants, and older subscribers should tolerate them
        // rather than abort the whole stream.
        //
        // Narrowly scoped: we first parse to a `Value`, and only the
        // specific case of "well-formed JSON object with a string
        // `type` we don't recognize" is dropped silently. Malformed
        // JSON or shape mismatches on known variants still surface
        // as errors, because hiding those would make wire bugs
        // invisible.
        match serde_json::from_str::<Event>(trimmed) {
            Ok(event) => {
                if !on_event(event) {
                    return Ok(());
                }
            }
            Err(_) => {
                if is_unknown_event_variant(trimmed) {
                    continue;
                }
                return Err(anyhow!("parse event line: {trimmed:?}"));
            }
        }
    }
}

/// Translate a client-side [`EventScope`] into the `subscribe` request
/// that asks for it.
///
/// Split out of [`subscribe_events_scoped`] so the wire consequence of
/// each entry point can be asserted without standing up a server — in
/// particular that [`subscribe_events`] names no pane and therefore
/// keeps serializing to exactly `{"cmd":"subscribe"}`, the shape every
/// renga server ever shipped already understands and answers with the
/// full broadcast.
fn subscribe_request_for(scope: EventScope) -> Request {
    Request::Subscribe {
        from_pane: match scope {
            EventScope::Unscoped => None,
            EventScope::PaneInbox(pane_id) => Some(pane_id),
        },
    }
}

/// True when `line` is valid JSON for an object whose `type` field is
/// a string but not one of the [`Event`] variants this client knows
/// about. Only this narrow case is swallowed by the subscribe loop;
/// malformed JSON or wrong shapes on known variants still surface.
///
/// [`KNOWN_EVENT_TAGS`] must name **every** [`Event`] variant. A missing
/// tag is not a harmless omission: it reclassifies a shape error on a
/// real variant as "some future server sent something new", and the
/// subscribe loop then discards the line without a word. `peer_inbox`
/// was missing here until Issue #306, which meant a malformed peer
/// message vanished instead of surfacing as a parse error.
fn is_unknown_event_variant(line: &str) -> bool {
    let value: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let ty = match value.get("type").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return false,
    };
    !KNOWN_EVENT_TAGS.contains(&ty)
}

/// The serde `type` tag of every [`Event`] variant this client can
/// parse.
///
/// **Adding an `Event` variant means adding its tag here.** Rust cannot
/// enforce that on its own — a `&str` match has no exhaustiveness check
/// — so the guard is split across two places that a new variant does
/// break: `wire_tag` in this module's tests is an exhaustive `match` and
/// stops compiling, and `every_event_variant_tag_is_known_to_the_client`
/// asserts this list and the sample set agree in length. Between them a
/// maintainer has to touch this constant deliberately rather than by
/// remembering to.
const KNOWN_EVENT_TAGS: &[&str] = &[
    "pane_started",
    "pane_exited",
    "events_dropped",
    "heartbeat",
    "peer_inbox",
];

/// Render an error `message` plus optional machine-readable `code` as
/// a single human string. Shell-visible so operators can grep by code.
fn fmt_err(message: &str, code: &Option<String>) -> String {
    match code {
        Some(c) => format!("[{c}] {message}"),
        None => message.to_string(),
    }
}

fn write_request_line<W: Write>(w: &mut W, req: &Request) -> Result<()> {
    let mut json = serde_json::to_string(req)?;
    json.push('\n');
    w.write_all(json.as_bytes())?;
    w.flush()?;
    Ok(())
}

/// Compare the server-provided session token with the expected one
/// that the parent renga published to `RENGA_TOKEN`.
///
/// A mismatch means the `RENGA_SOCKET` path we connected through points
/// to a renga instance that doesn't own the current shell — most likely
/// the PID got re-used and a stale socket path was inherited. Refuse
/// rather than silently deliver the command to the wrong process.
///
/// Uses a constant-time comparison; same-user tokens are not a secrecy
/// boundary (see the crate-level threat model), but comparing byte-by-
/// byte in constant time is the cheap hardening default.
fn verify_session_token(server_token: &str, expected: Option<&str>) -> Result<()> {
    match expected {
        Some(e) => {
            let a = server_token.as_bytes();
            let b = e.as_bytes();
            if a.len() == b.len() && bool::from(a.ct_eq(b)) {
                Ok(())
            } else {
                Err(anyhow!(
                    "session token mismatch; {ENV_SOCKET} likely points to a different renga instance",
                    ENV_SOCKET = super::endpoint::ENV_SOCKET
                ))
            }
        }
        None => Err(anyhow!(
            "{ENV_TOKEN} not set; are you running inside renga?"
        )),
    }
}

/// Reject the call when the connected server does not advertise
/// `cap`. The message names the remedy (restart renga) because the
/// cause is always the same: a renga process started from an older
/// binary than the client that is talking to it.
fn require_capability(cap: &str, advertised: &[String]) -> Result<()> {
    if advertised.iter().any(|c| c == cap) {
        return Ok(());
    }
    Err(anyhow!(
        "[server_too_old] this renga server does not support the `{cap}` protocol capability \
         (it advertised: {advertised}). The running renga process predates this feature — \
         restart renga so the server and its panes speak the same protocol.",
        advertised = if advertised.is_empty() {
            "none".to_string()
        } else {
            advertised.join(", ")
        }
    ))
}

fn read_response_line<R: BufRead>(r: &mut R) -> Result<Response> {
    let mut buf = String::new();
    let n = r.read_line(&mut buf)?;
    if n == 0 {
        return Err(anyhow!("server closed connection before replying"));
    }
    let resp: Response = serde_json::from_str(buf.trim())
        .with_context(|| format!("parse response json: {buf:?}"))?;
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{Direction, PaneRef};

    #[test]
    fn unknown_event_variant_is_detected() {
        assert!(is_unknown_event_variant(
            r#"{"type":"pane_renamed","id":1}"#
        ));
    }

    #[test]
    fn known_event_variant_is_not_skipped() {
        assert!(!is_unknown_event_variant(
            r#"{"type":"heartbeat","ts_ms":1}"#
        ));
        assert!(!is_unknown_event_variant(
            r#"{"type":"pane_started","id":1,"ts_ms":1}"#
        ));
    }

    #[test]
    fn malformed_json_is_not_classified_as_unknown_variant() {
        // Broken JSON must surface as an error, not be silently
        // dropped as "unknown event".
        assert!(!is_unknown_event_variant(r#"{"type":"heartbeat""#));
        assert!(!is_unknown_event_variant("not json at all"));
    }

    #[test]
    fn value_without_type_field_is_not_classified_as_unknown_variant() {
        // A JSON object with no `type` is a shape violation on a
        // known variant, not a forward-compat skip.
        assert!(!is_unknown_event_variant(r#"{"id":1,"ts_ms":1}"#));
    }

    /// The wire tag serde derives for each [`Event`] variant.
    ///
    /// Exhaustive by construction: no wildcard arm, so adding an
    /// `Event` variant stops this file compiling until someone states
    /// its tag here. That is the one compile-time hook available —
    /// [`KNOWN_EVENT_TAGS`] is a `&str` list and `is_unknown_event_variant`
    /// matches against it at runtime, neither of which Rust can check
    /// for exhaustiveness. So this arm is where a maintainer is
    /// *stopped*, and the length assertion in
    /// `every_event_variant_tag_is_known_to_the_client` is what then
    /// sends them to [`KNOWN_EVENT_TAGS`] and to
    /// [`one_of_every_event_variant`] rather than letting a green suite
    /// imply the work is finished. `peer_inbox` was silently absent
    /// from the matcher for as long as that list was maintained purely
    /// by hand.
    fn wire_tag(event: &Event) -> &'static str {
        match event {
            Event::PaneStarted { .. } => "pane_started",
            Event::PaneExited { .. } => "pane_exited",
            Event::EventsDropped { .. } => "events_dropped",
            Event::Heartbeat { .. } => "heartbeat",
            Event::PeerInbox { .. } => "peer_inbox",
        }
    }

    /// One sample of every [`Event`] variant, so the pin below can
    /// serialize each and check the real serde output rather than a
    /// hand-written string that could drift from it.
    fn one_of_every_event_variant() -> Vec<Event> {
        vec![
            Event::PaneStarted {
                id: 1,
                name: None,
                role: None,
                ts_ms: 1,
            },
            Event::PaneExited {
                id: 1,
                name: None,
                role: None,
                ts_ms: 1,
            },
            Event::EventsDropped { count: 2, ts_ms: 1 },
            Event::Heartbeat { ts_ms: 1 },
            Event::PeerInbox {
                target_pane: 3,
                from_pane: 4,
                from_name: Some("sender".into()),
                from_kind: None,
                body: "hi".into(),
                ts_ms: 1,
            },
        ]
    }

    #[test]
    fn every_event_variant_tag_is_known_to_the_client() {
        let samples = one_of_every_event_variant();
        // Catches the half `wire_tag`'s exhaustive match cannot: a new
        // variant that was given a tag there but never added to
        // `KNOWN_EVENT_TAGS`, or added to the constant but left without
        // a sample, so that the loop below silently exercises nothing.
        let mut tags: Vec<&str> = samples.iter().map(wire_tag).collect();
        tags.sort_unstable();
        tags.dedup();
        assert_eq!(
            tags.len(),
            samples.len(),
            "one_of_every_event_variant must not sample the same variant twice"
        );
        let mut known = KNOWN_EVENT_TAGS.to_vec();
        known.sort_unstable();
        assert_eq!(
            tags, known,
            "KNOWN_EVENT_TAGS and the sample set have diverged — a new Event \
             variant needs a tag in the constant AND a sample here, or a \
             malformed line of that type will be silently discarded"
        );
        for event in samples {
            let json = serde_json::to_string(&event).expect("serialize event");
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(
                parsed.get("type").and_then(|v| v.as_str()),
                Some(wire_tag(&event)),
                "serde tag drifted from wire_tag for {event:?}"
            );
            assert!(
                !is_unknown_event_variant(&json),
                "{} is a real Event variant but the client treats it as unknown \
                 and would silently drop it",
                wire_tag(&event)
            );
        }
    }

    // ─── Issue #306 subscribe scoping ─────────────────────

    /// The split API only helps if the two entry points really do put
    /// different things on the wire. Pin both directions, plus the byte
    /// shape of the unscoped one: `renga events` and every pre-#306
    /// client send exactly `{"cmd":"subscribe"}`, and a new client must
    /// keep doing so — both so old servers never see an unfamiliar line,
    /// and so that declining the #306 opt-in really is a no-op on the
    /// wire rather than a subtly different request.
    #[test]
    fn unscoped_sends_the_legacy_subscribe() {
        let req = subscribe_request_for(EventScope::Unscoped);
        assert_eq!(req, Request::Subscribe { from_pane: None });
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            r#"{"cmd":"subscribe"}"#
        );
    }

    #[test]
    fn inbox_scope_names_the_pane_on_the_wire() {
        let req = subscribe_request_for(EventScope::PaneInbox(7));
        assert_eq!(req, Request::Subscribe { from_pane: Some(7) });
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            r#"{"cmd":"subscribe","from_pane":7}"#
        );
    }

    /// Pane 0 is a real pane id in renga, and `Option` makes it easy to
    /// write a mapping where a falsy pane silently degrades to "no pane
    /// named". A subscriber for pane 0 must bind pane 0; degrading it to
    /// the unscoped stream would not lose events — the unscoped stream is
    /// a superset — but it would silently hand pane 0 the whole session's
    /// peer traffic, i.e. the firehose it explicitly opted out of.
    #[test]
    fn pane_zero_binds_the_pane_instead_of_degrading_to_unscoped() {
        assert_eq!(
            subscribe_request_for(EventScope::PaneInbox(0)),
            Request::Subscribe { from_pane: Some(0) }
        );
    }

    /// The bug the missing `peer_inbox` tag caused, pinned directly: a
    /// `peer_inbox` line whose shape is wrong (here `target_pane` is a
    /// string, and the required fields are absent) must be reported as
    /// a parse error by the subscribe loop, not swallowed as "a future
    /// server sent a variant we don't know".
    #[test]
    fn malformed_peer_inbox_is_not_swallowed_as_a_future_variant() {
        assert!(!is_unknown_event_variant(
            r#"{"type":"peer_inbox","target_pane":"not-a-number"}"#
        ));
        assert!(!is_unknown_event_variant(r#"{"type":"peer_inbox"}"#));
    }

    #[test]
    fn write_request_line_is_newline_terminated() {
        let mut out: Vec<u8> = Vec::new();
        let req = Request::List { from_pane: None };
        write_request_line(&mut out, &req).unwrap();
        assert!(out.ends_with(b"\n"));
        // The line without the trailing newline must parse back to the
        // original request — protects against accidentally emitting
        // multi-line JSON.
        let line = std::str::from_utf8(&out).unwrap().trim_end();
        let parsed: Request = serde_json::from_str(line).unwrap();
        assert_eq!(parsed, Request::List { from_pane: None });
    }

    #[test]
    fn read_response_line_parses_ok() {
        let input: &[u8] = b"{\"status\":\"ok\",\"data\":null}\n";
        let mut reader = std::io::BufReader::new(input);
        let resp = read_response_line(&mut reader).unwrap();
        assert!(matches!(resp, Response::Ok { .. }));
    }

    // ─── Issue #288 version-skew gate ─────────────────────

    /// #304 exposes the capability set but must not mint a token for
    /// doing so — that would be circular (you would have to read the
    /// list to learn the list is readable) and would break its own
    /// primary use case, since an old server cannot advertise a new
    /// token and old servers are exactly what the probe must answer
    /// for. Pinned so a future change has to be deliberate.
    ///
    /// The list is pinned whole, so every addition lands here on
    /// purpose. `peer_user_turn` is #323's, and earns a token for the
    /// reason #304 does not: it changes what a request *does*, and an
    /// older server would ignore the new `deliver` field and perform a
    /// channel send while answering `Ok`.
    ///
    /// `subscribe_pane_scope` is #306's, and is a third kind again:
    /// advertise-only. Nothing in this file passes it to
    /// [`send_request_requiring`], because an old server's fallback
    /// (ignore `from_pane`, broadcast everything — which is also what a
    /// new server does for a subscription that names no pane) is still
    /// correct once the subscriber applies its own `target_pane` check.
    /// It is on the list purely so `server_info` can report whether a
    /// `from_pane` on subscribe will actually be honored.
    #[test]
    fn capability_exposure_mints_no_new_token() {
        assert_eq!(
            super::super::SERVER_CAPABILITIES,
            &[
                super::super::CAP_CALLER_SCOPE,
                super::super::CAP_CROSS_TAB_PEERS,
                super::super::CAP_SPAWN_TAB,
                super::super::CAP_CALLER_SCOPE_CLOSE_IDENTITY,
                super::super::CAP_PEER_USER_TURN,
                super::super::CAP_SUBSCRIBE_PANE_SCOPE,
            ],
            "#304 is introspection only and adds no capability token"
        );
    }

    #[test]
    fn require_capability_accepts_an_advertised_token() {
        let advertised = vec![super::super::CAP_CALLER_SCOPE.to_string()];
        assert!(require_capability(super::super::CAP_CALLER_SCOPE, &advertised).is_ok());
    }

    /// An old renga process advertises nothing. Failing closed here is
    /// what keeps a new mcp-peer from issuing a `from_pane` request the
    /// old server silently strips and executes against the wrong tab.
    #[test]
    fn require_capability_fails_closed_and_names_the_remedy() {
        let err = require_capability(super::super::CAP_CALLER_SCOPE, &[]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("server_too_old"), "got: {msg}");
        assert!(msg.contains("restart renga"), "got: {msg}");
    }

    #[test]
    fn require_capability_ignores_unrelated_tokens() {
        let advertised = vec!["something_else".to_string()];
        assert!(require_capability(super::super::CAP_CALLER_SCOPE, &advertised).is_err());
    }

    /// A #288-era server advertises `caller_scope` but still silently
    /// drops cross-tab peer sends. The peer tools gate on the distinct
    /// `cross_tab_peers` token, so that server must be rejected — a
    /// success here would let "Delivered" lie about a dropped message.
    #[test]
    fn require_cross_tab_peers_fails_closed_against_a_288_server() {
        let advertised = vec![super::super::CAP_CALLER_SCOPE.to_string()];
        let err = require_capability(super::super::CAP_CROSS_TAB_PEERS, &advertised).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("server_too_old"), "got: {msg}");
        assert!(msg.contains("cross_tab_peers"), "got: {msg}");
    }

    #[test]
    fn require_cross_tab_peers_accepts_a_289_server() {
        let advertised: Vec<String> = super::super::SERVER_CAPABILITIES
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(require_capability(super::super::CAP_CROSS_TAB_PEERS, &advertised).is_ok());
    }

    /// A #289-era server advertises `caller_scope` and
    /// `cross_tab_peers` but ignores the unknown `tab` field on a
    /// split — it would spawn into the caller's tab and report
    /// success. Tab-directed spawns gate on the distinct `spawn_tab`
    /// token, so that server must be rejected (Issue #290).
    #[test]
    fn require_spawn_tab_fails_closed_against_a_289_server() {
        let advertised = vec![
            super::super::CAP_CALLER_SCOPE.to_string(),
            super::super::CAP_CROSS_TAB_PEERS.to_string(),
        ];
        let err = require_capability(super::super::CAP_SPAWN_TAB, &advertised).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("server_too_old"), "got: {msg}");
        assert!(msg.contains("spawn_tab"), "got: {msg}");
    }

    #[test]
    fn require_spawn_tab_accepts_a_290_server() {
        let advertised: Vec<String> = super::super::SERVER_CAPABILITIES
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(require_capability(super::super::CAP_SPAWN_TAB, &advertised).is_ok());
    }

    /// A #290-era server advertises the three earlier tokens but still
    /// resolves `close`'s `focused` against the visible tab, dropping
    /// the unknown `from_pane`. Since `close_pane` is destructive and
    /// irreversible, that server must be refused rather than trusted
    /// (Issue #296).
    #[test]
    fn require_caller_scope_close_identity_fails_closed_against_a_290_server() {
        let advertised = vec![
            super::super::CAP_CALLER_SCOPE.to_string(),
            super::super::CAP_CROSS_TAB_PEERS.to_string(),
            super::super::CAP_SPAWN_TAB.to_string(),
        ];
        let err = require_capability(super::super::CAP_CALLER_SCOPE_CLOSE_IDENTITY, &advertised)
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("server_too_old"), "got: {msg}");
        assert!(msg.contains("caller_scope_close_identity"), "got: {msg}");
    }

    #[test]
    fn require_caller_scope_close_identity_accepts_a_296_server() {
        let advertised: Vec<String> = super::super::SERVER_CAPABILITIES
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(
            require_capability(super::super::CAP_CALLER_SCOPE_CLOSE_IDENTITY, &advertised).is_ok()
        );
    }

    #[test]
    fn verify_session_token_matches() {
        assert!(verify_session_token("abc-123", Some("abc-123")).is_ok());
    }

    #[test]
    fn verify_session_token_rejects_mismatch() {
        let err = verify_session_token("abc-123", Some("xyz-999")).unwrap_err();
        assert!(err.to_string().contains("mismatch"), "got: {err}");
    }

    #[test]
    fn verify_session_token_rejects_missing_env() {
        let err = verify_session_token("abc-123", None).unwrap_err();
        assert!(err.to_string().contains("RENGA_TOKEN"), "got: {err}");
    }

    #[test]
    fn verify_session_token_rejects_length_mismatch() {
        let err = verify_session_token("short", Some("much-longer-token")).unwrap_err();
        assert!(err.to_string().contains("mismatch"), "got: {err}");
    }

    #[test]
    fn verify_session_token_rejects_whitespace_wrap() {
        // Whitespace is not trimmed; comparison is exact.
        assert!(verify_session_token("abc", Some(" abc")).is_err());
        assert!(verify_session_token("abc", Some("abc\n")).is_err());
    }

    #[test]
    fn verify_session_token_unicode_roundtrip() {
        assert!(verify_session_token("トークン", Some("トークン")).is_ok());
        assert!(verify_session_token("トークン", Some("トーケン")).is_err());
    }

    #[test]
    fn verify_session_token_rejects_empty_server_token() {
        assert!(verify_session_token("", Some("nonempty")).is_err());
    }

    #[test]
    fn read_response_line_eof_is_error() {
        let input: &[u8] = b"";
        let mut reader = std::io::BufReader::new(input);
        assert!(read_response_line(&mut reader).is_err());
    }

    #[test]
    fn write_request_line_roundtrips_split() {
        let req = Request::Split {
            target: PaneRef::Focused,
            direction: Direction::Horizontal,
            command: Some("echo".into()),
            id: Some("foo".into()),
            role: None,
            cwd: None,
            from_pane: None,
            tab: None,
        };
        let mut out: Vec<u8> = Vec::new();
        write_request_line(&mut out, &req).unwrap();
        let line = std::str::from_utf8(&out).unwrap().trim_end();
        let parsed: Request = serde_json::from_str(line).unwrap();
        assert_eq!(parsed, req);
    }
}
