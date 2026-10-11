//! `poll_events`: long-poll over the buffered lifecycle events.

use super::*;

// ── poll_events (long-poll over buffered lifecycle events) ────

/// Outcome of a single buffer scan. Separated from the tool response
/// so the scan can be written as a pure function against a locked
/// `EventBuffer`, independent of the long-poll / timeout / JSON shape.
#[derive(Debug, PartialEq)]
pub(super) struct PollScan {
    /// Events in the window (seq >= start_cursor) that matched the
    /// optional `types` filter.
    pub(super) matched: Vec<Value>,
    /// Highest seq in the window regardless of filter. `None` when no
    /// events fall in the window at all. When `Some`, this becomes the
    /// response's `next_since` so filtered-out events don't make the
    /// caller re-scan the same range.
    pub(super) window_max_seq: Option<u64>,
}

pub(super) fn scan_buffer(
    buf: &EventBuffer,
    start_cursor: u64,
    types_filter: Option<&[String]>,
) -> PollScan {
    let mut matched = Vec::new();
    let mut window_max_seq: Option<u64> = None;
    for e in &buf.events {
        if e.seq < start_cursor {
            continue;
        }
        window_max_seq = Some(window_max_seq.map_or(e.seq, |prev| prev.max(e.seq)));
        if event_matches_filter(&e.value, types_filter) {
            matched.push(e.value.clone());
        }
    }
    PollScan {
        matched,
        window_max_seq,
    }
}

pub(super) fn event_matches_filter(event: &Value, filter: Option<&[String]>) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    if filter.is_empty() {
        return true;
    }
    let Some(ty) = event.get("type").and_then(|v| v.as_str()) else {
        return false;
    };
    filter.iter().any(|f| f == ty)
}

pub(super) fn poll_events_payload(events: Vec<Value>, next_since: u64) -> Value {
    let body = json!({
        "next_since": next_since.to_string(),
        "events": events,
    });
    let text = serde_json::to_string(&body).unwrap_or_else(|_| body.to_string());
    json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": false,
        "structuredContent": body,
    })
}

/// Compute the effective long-poll duration from a caller-supplied
/// `timeout_ms`. Missing → default; oversize → clamped to the hard
/// cap. Factored out so the clamping can be unit-tested without
/// actually blocking a test thread for the full cap.
pub(super) fn effective_poll_timeout(requested: Option<u64>) -> Duration {
    let ms = requested
        .unwrap_or(POLL_DEFAULT_TIMEOUT_MS)
        .min(POLL_MAX_TIMEOUT_MS);
    Duration::from_millis(ms)
}

pub(super) fn handle_poll_events(id: &Value, args: &Value, ctx: &PeerCtx) -> Value {
    let since = args
        .get("since")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| s.trim().parse::<u64>().ok());
    let timeout = effective_poll_timeout(args.get("timeout_ms").and_then(|v| v.as_u64()));
    let types_filter: Option<Vec<String>> =
        args.get("types").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        });

    // Detached mode: no subscriber thread is running, so the buffer
    // will stay empty forever. Return immediately with a cursor of 0
    // rather than blocking the stdio dispatcher for `timeout_ms`.
    if matches!(ctx.mode, Mode::Detached { .. }) {
        return ok_response(id, poll_events_payload(Vec::new(), since.unwrap_or(0)));
    }

    let (lock, cvar) = &*ctx.events;
    let mut buf = lock.lock().unwrap_or_else(|p| p.into_inner());

    // Start inclusive lower bound. `since` is "the highest seq the
    // caller already knows about", so the next delivery window is
    // `since + 1`. `since = None` means "no history — give me events
    // that arrive after this call".
    let mut start_cursor = match since {
        Some(s) => s.saturating_add(1),
        None => buf.last_seq.saturating_add(1),
    };

    let deadline = Instant::now() + timeout;
    loop {
        let scan = scan_buffer(&buf, start_cursor, types_filter.as_deref());
        if let Some(max_seq) = scan.window_max_seq {
            // A filtered poll keeps waiting past events it filtered
            // out: since #72 `pane_waiting_input` arrives every few
            // seconds per pane and would otherwise wake every
            // `types=["pane_exited"]` long-poll with `events: []`.
            if !scan.matched.is_empty() || types_filter.is_none() {
                return ok_response(id, poll_events_payload(scan.matched, max_seq));
            }
            start_cursor = max_seq.saturating_add(1);
        }

        let now = Instant::now();
        if now >= deadline {
            // Timeout with no events in window. Hold the cursor where
            // it was so the next call resumes from the same point.
            let next = start_cursor.saturating_sub(1);
            return ok_response(id, poll_events_payload(Vec::new(), next));
        }
        let remaining = deadline - now;
        buf = match cvar.wait_timeout(buf, remaining) {
            Ok((g, _)) => g,
            Err(p) => p.into_inner().0,
        };
    }
}
