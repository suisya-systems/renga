//! Pane screen I/O tools: `inspect_pane` and `send_keys`.

use super::*;

// ── inspect_pane (pane screen snapshot over MCP) ──────────────

/// Cap on the `lines` argument, shared with the IPC handler. `lines`
/// beyond the pane's visible height continues into scrollback
/// history, so the cap bounds the total payload (not just sanitizes
/// input). Values above it are clamped silently, matching how
/// `renga inspect --lines` treats oversized requests.
pub(super) const INSPECT_MAX_LINES: u64 = crate::ipc::INSPECT_MAX_LINES as u64;

pub(super) fn parse_inspect_format(
    raw: Option<&str>,
) -> std::result::Result<InspectFormat, String> {
    match raw.map(str::trim) {
        None | Some("") | Some("text") => Ok(InspectFormat::Text),
        Some("grid") => Ok(InspectFormat::Grid),
        Some(other) => Err(format!(
            "invalid format {other:?}; expected 'text' or 'grid'"
        )),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InspectFormat {
    Text,
    Grid,
}

/// Render the Inspect IPC payload's `text` field as the content
/// block, defaulting to an empty string when absent so Claude
/// never sees a missing field crash.
pub(super) fn inspect_text_block(payload: &Value) -> String {
    payload
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Render the Inspect IPC payload's `lines` array as a
/// human-inspectable JSON grid. Falls back to the raw payload text
/// when the array is absent so a malformed payload doesn't silently
/// produce an empty response.
pub(super) fn inspect_grid_block(payload: &Value) -> String {
    match payload.get("lines") {
        Some(lines) => serde_json::to_string_pretty(lines).unwrap_or_else(|_| lines.to_string()),
        None => inspect_text_block(payload),
    }
}

pub(super) fn handle_inspect_pane(id: &Value, args: &Value, ctx: &PeerCtx) -> Value {
    let raw_target = args.get("target").and_then(|v| v.as_str()).unwrap_or("");
    if raw_target.trim().is_empty() {
        return err_response(
            id,
            -32602,
            "inspect_pane requires a non-empty target (pane id or name)",
        );
    }
    let target = parse_target(Some(raw_target));
    let lines = args.get("lines").and_then(|v| v.as_u64()).map(|n| {
        let clamped = n.min(INSPECT_MAX_LINES);
        clamped as usize
    });
    let include_cursor = args
        .get("include_cursor")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let format = match parse_inspect_format(args.get("format").and_then(|v| v.as_str())) {
        Ok(f) => f,
        Err(msg) => return err_response(id, -32602, &msg),
    };

    let (caller_pane, endpoint) = match require_connected(ctx, id, "inspect pane") {
        Ok(t) => t,
        Err(resp) => return resp,
    };

    match client::send_request_requiring(
        endpoint,
        &Request::Inspect {
            target,
            lines,
            include_cursor,
            from_pane: Some(caller_pane),
        },
        crate::ipc::CAP_CALLER_SCOPE,
    ) {
        Ok(Response::Ok { data }) => {
            let text = match format {
                InspectFormat::Text => inspect_text_block(&data),
                InspectFormat::Grid => inspect_grid_block(&data),
            };
            ok_response(
                id,
                json!({
                    "content": [ { "type": "text", "text": text } ],
                    "isError": false,
                    "structuredContent": data,
                }),
            )
        }
        Ok(Response::Err { message, code }) => err_response(
            id,
            -32603,
            &format!("renga refused inspect_pane: {}", fmt_code(&message, &code)),
        ),
        Ok(other) => err_response(id, -32603, &format!("unexpected renga response: {other:?}")),
        Err(e) => err_response(id, -32603, &format!("renga call failed: {e}")),
    }
}

// ── send_keys (raw PTY key input over MCP) ────────────────────

/// Translate a named special-key token into the byte sequence that a
/// VT-style terminal expects. Returns `None` for unknown names so the
/// caller surfaces a -32602 invalid-params error with the verbatim
/// input.
///
/// The vocabulary is intentionally conservative — the named set
/// covers the keys aainc-ops-style orchestrators actually need today
/// (y/n answers, Shift+Tab for Claude Code's Plan → AcceptEdits
/// toggle, Esc, arrow keys for menus, Ctrl+<letter> for signalling).
/// Escape sequences match xterm's default mode (no application-cursor
/// quirks) since that is what renga's vt100 parser speaks.
pub(super) fn translate_key(name: &str) -> Option<String> {
    let trimmed = name.trim();
    match trimmed {
        // Raw-mode TUIs read bytes directly from the PTY — including
        // Claude Code, which is the prime target here — so Enter must
        // be carriage return (CR, 0x0D), not line feed. This matches
        // what renga's own `Request::Send { append_enter: true }`
        // writes on the send path.
        "Enter" | "Return" => return Some("\r".into()),
        "Tab" => return Some("\t".into()),
        "Shift+Tab" | "BackTab" => return Some("\x1b[Z".into()),
        "Esc" | "Escape" => return Some("\x1b".into()),
        "Backspace" => return Some("\x7f".into()),
        "Delete" | "Del" => return Some("\x1b[3~".into()),
        "Up" => return Some("\x1b[A".into()),
        "Down" => return Some("\x1b[B".into()),
        "Right" => return Some("\x1b[C".into()),
        "Left" => return Some("\x1b[D".into()),
        "Home" => return Some("\x1b[H".into()),
        "End" => return Some("\x1b[F".into()),
        "PageUp" => return Some("\x1b[5~".into()),
        "PageDown" => return Some("\x1b[6~".into()),
        "Space" => return Some(" ".into()),
        _ => {}
    }
    if let Some(suffix) = trimmed.strip_prefix("Ctrl+") {
        let mut chars = suffix.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            let upper = c.to_ascii_uppercase();
            if upper.is_ascii_alphabetic() {
                let byte = (upper as u8) - b'A' + 1;
                return Some(String::from(byte as char));
            }
        }
    }
    None
}

/// Assemble the final byte stream to push at the target pane from the
/// tool arguments. Returns an error string on an unknown key or an
/// empty request (no text, no keys, no enter) so the caller produces a
/// -32602 JSON-RPC error without an IPC round-trip.
pub(crate) fn build_send_keys_payload(
    text: &str,
    keys: Option<&[Value]>,
    append_enter: bool,
) -> std::result::Result<String, String> {
    let mut buffer = String::from(text);
    if let Some(keys) = keys {
        for key in keys {
            let name = key
                .as_str()
                .ok_or_else(|| format!("send_keys.keys elements must be strings; got {key:?}"))?;
            let bytes = translate_key(name).ok_or_else(|| {
                format!(
                    "send_keys: unknown key {name:?}. See the tool description for the supported vocabulary."
                )
            })?;
            buffer.push_str(&bytes);
        }
    }
    if append_enter {
        // Mirror the Enter key mapping above: raw-mode TUIs want CR,
        // not LF. Using \r here also keeps this path byte-identical
        // to `Request::Send { append_enter: true }` in renga itself,
        // so callers don't have to reason about two Enter dialects.
        buffer.push('\r');
    }
    if buffer.is_empty() {
        return Err(
            "send_keys requires at least one of `text`, a non-empty `keys` array, or `enter=true`"
                .into(),
        );
    }
    Ok(buffer)
}

pub(super) fn handle_send_keys(id: &Value, args: &Value, ctx: &PeerCtx) -> Value {
    let raw_target = args.get("target").and_then(|v| v.as_str()).unwrap_or("");
    if raw_target.trim().is_empty() {
        return err_response(
            id,
            -32602,
            "send_keys requires a non-empty target (pane id or name)",
        );
    }
    let target = parse_target(Some(raw_target));

    let text = args
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let keys = args.get("keys").and_then(|v| v.as_array());
    let enter = args.get("enter").and_then(|v| v.as_bool()).unwrap_or(false);

    let payload = match build_send_keys_payload(text, keys.map(|v| v.as_slice()), enter) {
        Ok(p) => p,
        Err(msg) => return err_response(id, -32602, &msg),
    };

    let (caller_pane, endpoint) = match require_connected(ctx, id, "send keys") {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    match client::send_request_requiring(
        endpoint,
        &Request::Send {
            target,
            data: payload,
            from_pane: Some(caller_pane),
            // We assemble the Enter bit into `payload` above so every
            // call path (text-only / keys-only / combined) takes the
            // same branch server-side. `append_enter` stays false.
            append_enter: false,
        },
        crate::ipc::CAP_CALLER_SCOPE,
    ) {
        Ok(Response::Ok { .. }) => ok_response(
            id,
            tool_text_result(&format!("Sent keys to {}.", raw_target.trim())),
        ),
        Ok(Response::Err { message, code }) => err_response(
            id,
            -32603,
            &format!("renga refused send_keys: {}", fmt_code(&message, &code)),
        ),
        Ok(other) => err_response(id, -32603, &format!("unexpected renga response: {other:?}")),
        Err(e) => err_response(id, -32603, &format!("renga call failed: {e}")),
    }
}
