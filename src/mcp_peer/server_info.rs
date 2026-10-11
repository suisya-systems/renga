//! `server_info` tool and the `renga capabilities` CLI that shares its payload.

use super::*;

// ── server_info: ungated capability exposure (#304) ───────────

/// What a `server_info` call found out, before it is rendered.
///
/// Three states, deliberately distinct. Collapsing `Unreachable` into
/// `Connected { capabilities: [] }` would destroy the distinction
/// Issue #304's second acceptance criterion turns on: "this server
/// supports nothing" is a fact about the server, while "I could not
/// ask" is the absence of a fact, and a client that treats the second
/// as the first will fail closed forever against a renga that is
/// merely momentarily unreachable.
pub(super) enum ServerProbe {
    /// Handshake completed. `handshake.capabilities` is the running
    /// server's real advertisement — an empty vec means a genuinely
    /// old server, not a failure.
    Connected {
        pane_id: usize,
        endpoint: String,
        handshake: client::ServerHandshake,
    },
    /// Inside renga, but the handshake failed (socket gone, server
    /// died, or it belongs to a different renga instance). The
    /// endpoint we *tried* is kept: it is the one useful fact we still
    /// have, and it disambiguates concurrent renga instances.
    Unreachable {
        pane_id: usize,
        endpoint: String,
        reason: String,
    },
    /// This pane was never launched by renga at all.
    Detached { reason: String },
}

/// Capability tokens *this mcp-peer build* knows how to drive.
///
/// Sourced from [`ipc::SERVER_CAPABILITIES`], which is the same const
/// this binary's server half advertises. That is sound because both
/// come from one crate compiled together: a build whose const carries
/// token `T` necessarily also carries the request-side wiring for `T`.
pub(super) fn known_capability_tokens() -> Vec<String> {
    ipc::SERVER_CAPABILITIES
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

/// Tokens that are actually usable: advertised by the running server
/// **and** understood by this mcp-peer build.
///
/// Both halves are required and the pair can genuinely differ, because
/// renga registers `renga mcp-peer` by absolute path — upgrading the
/// binary on disk leaves the old server process running, and a *newer*
/// server can likewise advertise tokens an older mcp-peer has no code
/// to send. Gating on the server's list alone would over-promise in
/// the second case. Ordered by [`ipc::SERVER_CAPABILITIES`] so the
/// output is stable rather than dependent on wire order.
pub(super) fn effective_capability_tokens(advertised: &[String]) -> Vec<String> {
    ipc::SERVER_CAPABILITIES
        .iter()
        .filter(|cap| advertised.iter().any(|a| a == *cap))
        .map(|s| (*s).to_string())
        .collect()
}

/// Render a [`ServerProbe`] into the tool's structured payload.
///
/// Pure on purpose: everything that decides what a caller concludes
/// lives here, so it is unit-testable without a live IPC server (this
/// repo has no harness that connects a client to a real one).
pub(super) fn server_info_payload(probe: &ServerProbe) -> Value {
    // Every key is always present, explicitly null when unknown, so a
    // typed consumer gets `None` (or a type error) rather than a
    // silently-plausible default, and is pushed to branch on `status`
    // first.
    let (status, server, pane_id, reason) = match probe {
        ServerProbe::Connected {
            pane_id,
            endpoint,
            handshake,
        } => (
            "connected",
            json!({
                "pid": handshake.server_pid,
                "endpoint": endpoint,
                "capabilities": handshake.capabilities,
                // Null here does NOT mean "no session" — it means the
                // running server predates #326 and cannot say. Same
                // rule as `capabilities` above: absence of a fact is
                // not a fact.
                "session_id": handshake.session_id,
                // Null = pre-#312 server that cannot say; never "same as client".
                "server_version": handshake.server_version,
            }),
            Some(*pane_id),
            Value::Null,
        ),
        ServerProbe::Unreachable {
            pane_id,
            endpoint,
            reason,
        } => (
            "unreachable",
            json!({
                "pid": null,
                "endpoint": endpoint,
                "capabilities": null,
                "session_id": null,
                "server_version": null,
            }),
            Some(*pane_id),
            Value::String(reason.clone()),
        ),
        ServerProbe::Detached { reason } => (
            "detached",
            json!({
                "pid": null,
                "endpoint": null,
                "capabilities": null,
                "session_id": null,
                "server_version": null,
            }),
            None,
            Value::String(reason.clone()),
        ),
    };
    let effective = match probe {
        ServerProbe::Connected { handshake, .. } => {
            Value::from(effective_capability_tokens(&handshake.capabilities))
        }
        // Not `[]`: nothing was learned, and a consumer must not read
        // "unknown" as "supports nothing".
        _ => Value::Null,
    };
    json!({
        "status": status,
        "reason": reason,
        "server": server,
        "client": {
            "name": SERVER_NAME,
            "version": SERVER_VERSION,
            "pane_id": pane_id,
            "capabilities": known_capability_tokens(),
        },
        "effective_capabilities": effective,
    })
}

/// Human/LLM-readable summary mirroring [`server_info_payload`].
pub(super) fn format_server_info(probe: &ServerProbe) -> String {
    match probe {
        ServerProbe::Connected {
            pane_id,
            endpoint,
            handshake,
        } => {
            let effective = effective_capability_tokens(&handshake.capabilities);
            let mut out = format!(
                "renga server: connected (pid {}, endpoint {endpoint})\n",
                handshake.server_pid
            );
            if handshake.capabilities.is_empty() {
                out.push_str(
                    "advertised: (none — this server supports no gated features; restart \
                     renga to pick up the newer binary)\n",
                );
            } else {
                out.push_str(&format!(
                    "advertised: {}\n",
                    handshake.capabilities.join(", ")
                ));
            }
            out.push_str(&format!(
                "usable here (advertised AND supported by this client build): {}\n",
                if effective.is_empty() {
                    "(none)".to_string()
                } else {
                    effective.join(", ")
                }
            ));
            let unusable: Vec<&String> = handshake
                .capabilities
                .iter()
                .filter(|c| !effective.contains(c))
                .collect();
            if !unusable.is_empty() {
                out.push_str(&format!(
                    "advertised but NOT usable (this client build is older than the \
                     server): {}\n",
                    unusable
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            out.push_str(&match &handshake.server_version {
                Some(v) => format!("server version: {v}\n"),
                None => "server version: (UNKNOWN — this server predates #312)\n".to_string(),
            });
            out.push_str(&match &handshake.session_id {
                Some(sid) => format!(
                    "session: {sid} (changes on every renga restart — pane ids stored \
                     under a different session must not be reused)\n"
                ),
                None => "session: (UNKNOWN — this server predates session ids; a stored \
                         pane id cannot be checked against it, so re-resolve panes by \
                         name instead of reusing the id)\n"
                    .to_string(),
            });
            out.push_str(&format!(
                "this client: {SERVER_NAME} v{SERVER_VERSION} (pane {pane_id})\n"
            ));
            out
        }
        ServerProbe::Unreachable {
            pane_id, reason, ..
        } => format!(
            "renga server: unreachable — {reason}\n\
             capabilities: (UNKNOWN, which is not the same as \"none\" — the server was \
             never asked; do not conclude a token is missing from this result)\n\
             this client: {SERVER_NAME} v{SERVER_VERSION} (pane {pane_id})\n"
        ),
        ServerProbe::Detached { reason } => format!(
            "renga server: detached — {reason}\n\
             capabilities: (UNKNOWN, which is not the same as \"none\" — there is no renga \
             server to ask; do not conclude a token is missing from this result)\n\
             this client: {SERVER_NAME} v{SERVER_VERSION}\n"
        ),
    }
}

pub(super) fn probe_server_state(mode: &Mode) -> ServerProbe {
    match mode {
        Mode::Connected { pane_id, endpoint } => match client::probe_server(endpoint) {
            Ok(handshake) => ServerProbe::Connected {
                pane_id: *pane_id,
                endpoint: endpoint.as_str().to_string(),
                handshake,
            },
            Err(e) => ServerProbe::Unreachable {
                pane_id: *pane_id,
                endpoint: endpoint.as_str().to_string(),
                reason: format!("{e}"),
            },
        },
        Mode::Detached { reason } => ServerProbe::Detached {
            reason: reason.clone(),
        },
    }
}

/// Entry point for `renga capabilities` (#313): the `server_info`
/// payload for callers that are not MCP clients. Same probe, same
/// JSON, so the two surfaces cannot drift. Exits 0 in every state —
/// like `server_info`, "could not ask" is an answer carried in
/// `status`, not a failure.
pub fn run_capabilities(text: bool) -> Result<()> {
    let probe = probe_server_state(&PeerCtx::load().mode);
    if text {
        print!("{}", format_server_info(&probe));
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&server_info_payload(&probe))?
        );
    }
    Ok(())
}

pub(super) fn handle_server_info(id: &Value, ctx: &PeerCtx) -> Value {
    let probe = probe_server_state(&ctx.mode);
    // Never a JSON-RPC error, in any state. A caller pre-flighting
    // capabilities must be able to read the answer out of a normal
    // result; turning "renga is unreachable" into a protocol error
    // would push it straight back to parsing failure strings, which is
    // what #304 exists to stop.
    ok_response(
        id,
        json!({
            "content": [{ "type": "text", "text": format_server_info(&probe) }],
            "structuredContent": server_info_payload(&probe),
            "isError": false,
        }),
    )
}
