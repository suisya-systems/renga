//! Pane control tools: `list_panes`, `spawn_pane`, `close_pane`, `focus_pane`, `new_tab`, `set_pane_identity`, `set_summary`.

use super::*;

// ── pane control handlers ────────────────────────────────────

/// Resolve a tool `target` argument string into a [`PaneRef`].
///
/// Resolution order (first match wins):
/// 1. `None`, empty, whitespace-only, or `"focused"` (case-insensitive)
///    → `PaneRef::Focused`.
/// 2. Parses cleanly as `usize` → `PaneRef::Id(n)`.
/// 3. Otherwise → `PaneRef::Name(s)` (trimmed).
///
/// Edge cases folded into step 3 on purpose: negative-sign strings
/// like `"-1"` and digit strings that overflow `usize` both resolve
/// to `Name`. (Rust's `usize::from_str` accepts a leading `+`, so
/// `"+3"` still parses as `Id(3)` — a quirk inherited from the
/// stdlib, not a renga decision.) renga pane ids live in a small
/// fixed range (capped by `MAX_PANES`), so an overflow-sized "id"
/// can't refer to a real pane either way — letting the server reply
/// with `pane_not_found` on a bogus `Name` is indistinguishable from
/// erroring on `Id`, and keeps `parse_target` infallible.
pub(super) fn parse_target(raw: Option<&str>) -> PaneRef {
    let Some(s) = raw else {
        return PaneRef::Focused;
    };
    let trimmed = s.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("focused") {
        return PaneRef::Focused;
    }
    match trimmed.parse::<usize>() {
        Ok(n) => PaneRef::Id(n),
        Err(_) => PaneRef::Name(trimmed.to_string()),
    }
}

pub(super) fn parse_direction(raw: Option<&str>) -> std::result::Result<Direction, String> {
    match raw.map(str::trim) {
        Some("vertical") => Ok(Direction::Vertical),
        Some("horizontal") => Ok(Direction::Horizontal),
        Some(other) => Err(format!(
            "invalid direction {other:?}; expected 'vertical' or 'horizontal'"
        )),
        None => Err("direction is required ('vertical' or 'horizontal')".to_string()),
    }
}

/// Where a `spawn_*` call places its new pane (Issue #290).
#[derive(Debug, Clone, PartialEq)]
pub(super) enum SpawnPlacement {
    /// No `tab` argument — split inside the caller's own tab, the
    /// pre-#290 behavior.
    Here,
    /// `tab: {name|index|pane_id}` — split inside the selected
    /// existing tab.
    Tab(crate::ipc::TabSelector),
    /// `tab: {new: {…}}` — spawn a fresh single-pane background tab.
    NewTab { label: Option<String> },
}

impl SpawnPlacement {
    /// The capability the outgoing request must be gated on.
    ///
    /// Both split placements require
    /// [`crate::ipc::CAP_SPLIT_REFUSAL_CAUSES`] (Issue #335, flipped in
    /// 3.0): this tool's contract now says a refusal that is not
    /// `pane_limit_reached` is target-local, and a pre-3.0 server
    /// cannot honor that — it answers both causes with `split_refused`,
    /// so the promise would be a lie exactly when a caller acts on it
    /// (giving up on a tab that has room). Fail closed with
    /// `server_too_old` instead; the running renga process can predate
    /// the binary on disk, so this is a live skew, not a theoretical
    /// one. `SERVER_CAPABILITIES` is append-only, so this token
    /// subsumes the `caller_scope` / `spawn_tab` gates the two arms
    /// used to carry — a #335 server understands both.
    ///
    /// `NewTab` keeps [`crate::ipc::CAP_SPAWN_TAB`]: it sends
    /// `SpawnTab`, not `Split`, and #335 did not touch that request's
    /// refusals.
    pub(super) fn required_cap(&self) -> &'static str {
        match self {
            SpawnPlacement::Here | SpawnPlacement::Tab(_) => crate::ipc::CAP_SPLIT_REFUSAL_CAUSES,
            SpawnPlacement::NewTab { .. } => crate::ipc::CAP_SPAWN_TAB,
        }
    }
}

/// Parse the optional `min_pane_width` / `min_pane_height` overrides
/// shared by the three `spawn_*` tools (Issue #206). Only the type and
/// `u16` range are checked here; the hard floor is the server's call
/// (`min_pane_size_invalid`), so it lives in exactly one place.
///
/// Refused with `tab: {new: …}` for the same reason `direction` /
/// `target` are: a brand-new tab has nothing to split.
pub(super) fn parse_min_pane_size(
    args: &Value,
    placement: &SpawnPlacement,
) -> std::result::Result<(Option<u16>, Option<u16>), String> {
    let mut out = [None, None];
    for (slot, key) in out.iter_mut().zip(["min_pane_width", "min_pane_height"]) {
        *slot =
            match args.get(key) {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.as_u64().and_then(|n| u16::try_from(n).ok()).ok_or_else(
                    || format!("{key} must be an integer that fits in u16, got {v}"),
                )?),
            };
        if slot.is_some() && matches!(placement, SpawnPlacement::NewTab { .. }) {
            return Err(format!(
                "{key} is not valid with tab: {{\"new\": …}} — a new tab has nothing to split"
            ));
        }
    }
    Ok((out[0], out[1]))
}

/// The capability a split must be gated on. An override needs
/// [`crate::ipc::CAP_SPLIT_MIN_PANE_SIZE`] — an older server would drop
/// it silently — which implies the placement's own token (capabilities
/// are append-only). Without one, the gate is unchanged.
pub(super) fn split_required_cap(
    placement: &SpawnPlacement,
    min_pane_size: (Option<u16>, Option<u16>),
) -> &'static str {
    if min_pane_size == (None, None) {
        placement.required_cap()
    } else {
        crate::ipc::CAP_SPLIT_MIN_PANE_SIZE
    }
}

/// Parse the `tab` argument shared by the three `spawn_*` tools.
///
/// Strict on purpose — every rejected shape here would otherwise be a
/// pane spawned into the wrong tab: exactly one selector key, known
/// keys only, correct JSON types, and `{new: …}` refuses `direction` /
/// `target` outright instead of silently ignoring them (a brand-new
/// tab has nothing to split, so a caller passing them is confused
/// about what will happen).
pub(super) fn parse_spawn_placement(args: &Value) -> std::result::Result<SpawnPlacement, String> {
    const FORM: &str = "expected one of {\"name\": \"<tab label>\"}, {\"index\": <0-based>}, \
                        {\"pane_id\": <pane id>}, {\"new\": {}} or {\"new\": {\"name\": \"<label>\"}}";
    let raw = match args.get("tab") {
        None | Some(Value::Null) => return Ok(SpawnPlacement::Here),
        Some(v) => v,
    };
    let obj = raw
        .as_object()
        .ok_or_else(|| format!("invalid tab selector {raw}: {FORM}"))?;
    if obj.len() != 1 {
        return Err(format!(
            "invalid tab selector: exactly one selector key is required; {FORM}"
        ));
    }
    let (key, val) = obj.iter().next().expect("len checked above");
    match key.as_str() {
        // Deliberately NOT trimmed: tab selection is an exact
        // display-name match, and raw-IPC `new_tab` labels are stored
        // verbatim — trimming here would turn a valid selector for a
        // whitespace-padded label into `tab_not_found`, or worse,
        // match a *different* tab whose label is the trimmed form.
        "name" => match val.as_str() {
            Some(s) if !s.trim().is_empty() => Ok(SpawnPlacement::Tab(
                crate::ipc::TabSelector::Name(s.to_string()),
            )),
            _ => Err(format!("tab.name must be a non-empty string; {FORM}")),
        },
        // Checked conversions, not `as`: on a 32-bit target an
        // oversized u64 would silently truncate — `4294967296` becomes
        // index 0 — and route the spawn into the wrong tab.
        "index" => match val.as_u64().and_then(|n| usize::try_from(n).ok()) {
            Some(n) => Ok(SpawnPlacement::Tab(crate::ipc::TabSelector::Index(n))),
            None => Err(format!(
                "tab.index must be a non-negative integer (0-based, as reported by list_peers); {FORM}"
            )),
        },
        "pane_id" => match val.as_u64().and_then(|n| usize::try_from(n).ok()) {
            Some(n) => Ok(SpawnPlacement::Tab(crate::ipc::TabSelector::PaneId(n))),
            None => Err(format!(
                "tab.pane_id must be a non-negative integer pane id; {FORM}"
            )),
        },
        "new" => {
            let nested = val
                .as_object()
                .ok_or_else(|| format!("tab.new must be an object; {FORM}"))?;
            if let Some(unknown) = nested.keys().find(|k| k.as_str() != "name") {
                return Err(format!(
                    "unknown tab.new field {unknown:?}; only \"name\" (the new tab's label) is accepted"
                ));
            }
            let label = match nested.get("name") {
                // Explicit null means the same as omission, matching
                // how `tab: null` is treated above.
                None | Some(Value::Null) => None,
                Some(v) => match v.as_str().map(str::trim) {
                    Some(s) if !s.is_empty() => Some(s.to_string()),
                    _ => {
                        return Err(
                            "tab.new.name must be a non-empty string when present".to_string()
                        );
                    }
                },
            };
            // Refuse, never ignore: with `direction` or `target` in
            // the call, the caller believes this is a split — spawning
            // an unrelated single-pane tab instead would honor the
            // letter of the request and betray its intent. Explicit
            // null counts as absent, consistent with the split path
            // (where `direction: null` / `target: null` read as
            // omitted) and with `tab: null` above.
            let given =
                |key: &str| args.get(key).is_some_and(|v| !v.is_null());
            if given("direction") || given("target") {
                return Err(
                    "tab: {new: …} creates a fresh single-pane tab, so `direction` and `target` \
                     must be omitted"
                        .to_string(),
                );
            }
            Ok(SpawnPlacement::NewTab { label })
        }
        other => Err(format!("unknown tab selector key {other:?}; {FORM}")),
    }
}

/// What a `list_panes` call enumerates, decided by its `tab` argument
/// (Issue #329). Distinct from [`SpawnPlacement`] because the two
/// vocabularies differ at exactly the two points where they must: a
/// list can say `all`, a spawn cannot; a spawn can say `new`, a list
/// has nothing to read in a tab that does not exist yet.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum ListScope {
    /// No `tab` argument — the caller's own tab, the pre-#329
    /// behavior, byte for byte.
    CallerTab,
    /// `tab: {name|index|pane_id}` — one selected tab.
    SelectedTab(crate::ipc::ListTabSelector),
    /// `tab: {all: true}` — every tab, caller's first.
    AllTabs,
}

impl ListScope {
    /// The wire selector, or `None` for the unchanged default path.
    fn selector(&self) -> Option<crate::ipc::ListTabSelector> {
        match self {
            ListScope::CallerTab => None,
            ListScope::SelectedTab(sel) => Some(sel.clone()),
            ListScope::AllTabs => Some(crate::ipc::ListTabSelector::All),
        }
    }

    /// The capability the outgoing request must be gated on. Any
    /// explicit selector — **including `{pane_id: <the caller's own
    /// pane>}`, which resolves to the caller's own tab** — requires
    /// [`crate::ipc::CAP_CROSS_TAB_LIST`]: a pre-#329 server drops the
    /// unknown field and answers with the caller's tab, which for that
    /// one selector is even the *right* answer — and for every other
    /// selector is a wrong answer wearing the same clothes. Gating on
    /// the shape rather than on the resolved tab is what makes the
    /// failure mode uniform. Mirrors [`SpawnPlacement::required_cap`];
    /// `SERVER_CAPABILITIES` is additive, so a `cross_tab_list` server
    /// always understands `caller_scope` too.
    pub(super) fn required_cap(&self) -> &'static str {
        match self {
            ListScope::CallerTab => crate::ipc::CAP_CALLER_SCOPE,
            _ => crate::ipc::CAP_CROSS_TAB_LIST,
        }
    }
}

/// Parse the `tab` argument of `list_panes`.
///
/// Strict for the same reason [`parse_spawn_placement`] is, with one
/// difference in what a mistake costs: a mis-parsed spawn puts a pane
/// in the wrong tab, where it is at least visible, whereas a mis-parsed
/// list quietly returns the wrong *population* and reads as a correct
/// answer. So every malformed shape is rejected rather than
/// interpreted, and `{"all": false}` in particular is an error instead
/// of a synonym for "my tab" — a caller who wrote it meant something,
/// and it was not that.
pub(super) fn parse_list_scope(args: &Value) -> std::result::Result<ListScope, String> {
    const FORM: &str = "expected one of {\"name\": \"<tab label>\"}, {\"index\": <0-based>}, \
                        {\"pane_id\": <pane id>} or {\"all\": true}";
    let raw = match args.get("tab") {
        None | Some(Value::Null) => return Ok(ListScope::CallerTab),
        Some(v) => v,
    };
    let obj = raw
        .as_object()
        .ok_or_else(|| format!("invalid tab selector {raw}: {FORM}"))?;
    if obj.len() != 1 {
        return Err(format!(
            "invalid tab selector: exactly one selector key is required; {FORM}"
        ));
    }
    let (key, val) = obj.iter().next().expect("len checked above");
    match key.as_str() {
        // Deliberately NOT trimmed, for the same reason
        // `parse_spawn_placement` does not trim: labels are stored
        // verbatim, so a trimmed selector either misses with
        // `tab_not_found` or matches a *different* tab.
        "name" => match val.as_str() {
            Some(s) if !s.trim().is_empty() => Ok(ListScope::SelectedTab(
                crate::ipc::ListTabSelector::Name(s.to_string()),
            )),
            _ => Err(format!("tab.name must be a non-empty string; {FORM}")),
        },
        // Checked conversions, not `as`: a 32-bit truncation would
        // enumerate the wrong tab and answer `Ok` about it.
        "index" => match val.as_u64().and_then(|n| usize::try_from(n).ok()) {
            Some(n) => Ok(ListScope::SelectedTab(crate::ipc::ListTabSelector::Index(
                n,
            ))),
            None => Err(format!(
                "tab.index must be a non-negative integer (0-based, as reported by list_peers); {FORM}"
            )),
        },
        "pane_id" => match val.as_u64().and_then(|n| usize::try_from(n).ok()) {
            Some(n) => Ok(ListScope::SelectedTab(crate::ipc::ListTabSelector::PaneId(
                n,
            ))),
            None => Err(format!(
                "tab.pane_id must be a non-negative integer pane id; {FORM}"
            )),
        },
        "all" => match val.as_bool() {
            Some(true) => Ok(ListScope::AllTabs),
            _ => Err(format!(
                "tab.all must be the boolean true (omit `tab` entirely for just your own tab); {FORM}"
            )),
        },
        other => Err(format!("unknown tab selector key {other:?}; {FORM}")),
    }
}

/// Send a [`Request::SpawnTab`] (the `tab: {new: …}` path of the
/// `spawn_*` tools) and format the tool response. Shared by the three
/// spawn handlers — only the command construction and the success
/// wording differ between them.
pub(super) fn dispatch_spawn_tab(
    id: &Value,
    tool: &str,
    what: &str,
    endpoint: &EndpointName,
    req: &Request,
    launch_command: Option<&str>,
) -> Value {
    match client::send_request_requiring(endpoint, req, crate::ipc::CAP_SPAWN_TAB) {
        Ok(Response::Ok { data }) => {
            let new_id = data.get("id").and_then(|v| v.as_u64());
            let tab_idx = data.get("tab").and_then(|v| v.as_u64());
            let mut msg = match (new_id, tab_idx) {
                (Some(n), Some(t)) => format!(
                    "Spawned {what} id={n} in a new background tab (tab index {t}; focus unchanged)."
                ),
                (Some(n), None) => format!("Spawned {what} id={n} in a new background tab."),
                _ => format!("Spawned {what} in a new background tab (id not reported)."),
            };
            if let Some(cmd) = launch_command {
                msg.push_str(&format!(" Launch command: {cmd}"));
            }
            ok_response(id, tool_text_result(&msg))
        }
        Ok(Response::Err { message, code }) => err_response(
            id,
            -32603,
            &format!("renga refused {tool}: {}", fmt_code(&message, &code)),
        ),
        Ok(other) => err_response(id, -32603, &format!("unexpected renga response: {other:?}")),
        Err(e) => err_response(id, -32603, &format!("renga call failed: {e}")),
    }
}

/// Optional string-valued argument extractor. Empty strings map to None
/// so Claude can send `{"command": ""}` without accidentally shoving an
/// empty command line into the new pane.
/// `command` arg, auto-upgraded unless the caller passed `auto_upgrade_claude: false`.
pub(super) fn command_arg(args: &Value) -> Option<String> {
    let optout = args.get("auto_upgrade_claude").and_then(|v| v.as_bool()) == Some(false);
    opt_string(args, "command").map(|c| {
        if optout {
            c
        } else {
            upgrade_claude_command(&c)
        }
    })
}

pub(super) fn opt_string(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Upgrade a bare `claude …` command to the peer-enabled invocation
/// that Alt+P types into a pane. When the caller asks to spawn Claude
/// Code without the `--dangerously-load-development-channels
/// server:renga-peers` flag, the new instance can't see the peer
/// network, which silently defeats half the reason renga wraps it.
/// Injecting the flag at this seam keeps the MCP as a "launch Claude
/// and have it join the network" affordance without making the LLM
/// remember the exact incantation.
///
/// Rules:
/// - If the command already contains
///   `--dangerously-load-development-channels`, leave it alone — the
///   caller knew what they wanted.
/// - Match only when the first whitespace-delimited token is exactly
///   `claude`. `claude-mobile`, `claudex`, `./claude`, or `cargo run
///   -- claude` all fall through untouched so we never rewrite an
///   unrelated command by accident.
/// - Preserve the caller's trailing arguments: `"claude --resume"`
///   becomes `"claude --dangerously-load-development-channels
///   server:renga-peers --resume"`.
pub(crate) fn upgrade_claude_command(cmd: &str) -> String {
    if cmd.contains("--dangerously-load-development-channels") {
        return cmd.to_string();
    }
    let trimmed = cmd.trim_start();
    let leading_ws_len = cmd.len() - trimmed.len();
    let Some(rest) = trimmed.strip_prefix("claude") else {
        return cmd.to_string();
    };
    // Reject `claudex`, `claude-mobile`, etc. — the next char after
    // the literal token `claude` must be whitespace or end-of-string.
    if !rest.is_empty() && !rest.starts_with(|c: char| c.is_whitespace()) {
        return cmd.to_string();
    }
    let leading = &cmd[..leading_ws_len];
    format!("{leading}{CLAUDE_PEER_LAUNCH_CMD}{rest}")
}

/// Require `Mode::Connected`, otherwise respond with a user-visible
/// "renga unreachable" text result (not a JSON-RPC error, so Claude
/// surfaces the explanation to the user instead of treating the tool
/// as broken).
pub(super) fn require_connected<'a>(
    ctx: &'a PeerCtx,
    id: &Value,
    action: &str,
) -> std::result::Result<(usize, &'a EndpointName), Value> {
    match &ctx.mode {
        Mode::Connected { pane_id, endpoint } => Ok((*pane_id, endpoint)),
        Mode::Detached { reason } => Err(ok_response(
            id,
            tool_text_result(&format!(
                "(cannot {action} — renga not reachable: {reason})"
            )),
        )),
    }
}

pub(super) fn handle_list_panes(id: &Value, args: &Value, ctx: &PeerCtx) -> Value {
    // `require_connected` first, before the `tab` argument is even
    // looked at: a detached pane gets the same friendly "renga not
    // reachable" text whatever it asked for, rather than an argument
    // complaint about a call that could never have been served.
    let (caller_pane, endpoint) = match require_connected(ctx, id, "list panes") {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    let scope = match parse_list_scope(args) {
        Ok(s) => s,
        Err(msg) => return err_response(id, -32602, &msg),
    };
    match client::send_request_requiring(
        endpoint,
        &Request::List {
            from_pane: Some(caller_pane),
            tab: scope.selector(),
        },
        scope.required_cap(),
    ) {
        Ok(Response::Ok { data }) => match serde_json::from_value::<Vec<PaneInfo>>(data) {
            Ok(panes) => ok_response(id, tool_text_result(&format_pane_list(&panes, &scope))),
            Err(e) => err_response(id, -32603, &format!("decode pane list: {e}")),
        },
        Ok(Response::Err { message, code }) => err_response(
            id,
            -32603,
            &format!("renga refused list_panes: {}", fmt_code(&message, &code)),
        ),
        Ok(other) => err_response(id, -32603, &format!("unexpected renga response: {other:?}")),
        Err(e) => err_response(id, -32603, &format!("renga call failed: {e}")),
    }
}

/// Render a pane list for the agent. `scope` selects the wording and
/// decides whether each record is annotated with the tab it lives in.
///
/// The `CallerTab` strings are frozen: they are documented output
/// prefixes, and `docs/semver-policy-2.0.md` §3 classifies a changed
/// output prefix as breaking. The cross-tab wordings are *additional*
/// strings chosen by request shape, so the default path stays byte
/// identical end to end — which is also what the per-record tab
/// annotation is gated on.
pub(super) fn format_pane_list(panes: &[PaneInfo], scope: &ListScope) -> String {
    let cross_tab = !matches!(scope, ListScope::CallerTab);
    if panes.is_empty() {
        return match scope {
            ListScope::CallerTab => "No panes in this tab.".to_string(),
            ListScope::SelectedTab(_) => "No panes in the selected tab.".to_string(),
            ListScope::AllTabs => "No panes in any tab.".to_string(),
        };
    }
    let mut out = String::from(match scope {
        ListScope::CallerTab => "Panes in this tab:\n\n",
        ListScope::SelectedTab(_) => "Panes in the selected tab:\n\n",
        ListScope::AllTabs => "Panes in every tab (yours first):\n\n",
    });
    for p in panes {
        // Same reasoning as `format_peer_list`.
        out.push_str(&format!("- id={}", p.id));
        if let Some(name) = &p.name {
            out.push_str(&format!(" name={}", ipc::sanitized_label(name)));
        }
        if let Some(role) = &p.role {
            out.push_str(&format!(" role={}", ipc::sanitized_label(role)));
        }
        if p.focused {
            out.push_str(" (focused)");
        }
        // Only when the set can span tabs — on the default path every
        // record would carry the same marker, and a pre-#329 server's
        // reply carries no tab metadata to render at all.
        if cross_tab {
            match (p.same_tab, p.tab) {
                (Some(true), _) => out.push_str(" [your tab]"),
                (_, Some(tab)) => match &p.tab_name {
                    // `sanitized_label`, never the raw label: a tab
                    // name is free-form text landing in another
                    // agent's context, and the control-char strip is
                    // what stops a forged list entry.
                    Some(tab_name) => out.push_str(&format!(
                        " [tab {tab} \"{}\"]",
                        ipc::sanitized_label(tab_name)
                    )),
                    None => out.push_str(&format!(" [tab {tab}]")),
                },
                _ => {}
            }
        }
        out.push_str(&format!(
            "\n  geometry: x={} y={} width={} height={}",
            p.x, p.y, p.width, p.height
        ));
        if let Some(cwd) = &p.cwd {
            out.push_str(&format!("\n  cwd: {cwd}"));
        }
        if let Some(d) = p.peer_delivery {
            let state = match d.state {
                ipc::PeerDeliveryState::Queued => "queued",
                ipc::PeerDeliveryState::Nudged => "nudged",
            };
            out.push_str(&format!(
                "\n  peer_delivery: {state} pending={} since_ms={}",
                d.pending, d.since_ms
            ));
        }
        push_summary(&mut out, p.summary.as_deref());
        out.push('\n');
    }
    out
}

/// Resolve a user-supplied `cwd` into what the IPC layer wants: either
/// `None` (use server default) or an absolute-path string. Relative
/// paths are joined onto the caller pane's cwd — the pane the Claude
/// agent is running inside — so Claude's tool calls map to the same
/// cwd its shell would interpret `cd <path>` against. Returns
/// `Err(message)` on unresolvable input (caller pane vanished, etc.);
/// server-side `CWD_INVALID` handles filesystem-level validation.
pub(super) fn resolve_mcp_cwd(
    endpoint: &EndpointName,
    caller_pane: usize,
    cwd: Option<&str>,
) -> std::result::Result<Option<String>, String> {
    let s = match cwd {
        Some(s) => s.trim(),
        None => return Ok(None),
    };
    if s.is_empty() {
        return Ok(None);
    }
    let path = std::path::Path::new(s);
    if path.is_absolute() {
        return Ok(Some(s.to_string()));
    }
    // Relative path — need caller pane's cwd. A single `Request::List`
    // round-trip is cheap and keeps IPC stateless.
    //
    // Snapshot semantics: we resolve against whatever cwd the server
    // knows at this instant, which is driven by OSC 7 updates from the
    // pane's shell. If the shell has `cd`-ed but the update hasn't
    // reached renga yet, the resolution uses the stale value. Callers
    // that need strict ordering should send an absolute path instead
    // of trusting "current" cwd.
    let panes: Vec<PaneInfo> = match client::send_request_requiring(
        endpoint,
        // Deliberately `tab: None` on `CAP_CALLER_SCOPE`: this is an
        // internal caller-cwd lookup behind every spawn tool, and it
        // only ever wants the caller's own tab. Escalating it to
        // `cross_tab_list` would break relative-`cwd` spawns against
        // every #290..#328 server for no gain.
        &Request::List {
            from_pane: Some(caller_pane),
            tab: None,
        },
        crate::ipc::CAP_CALLER_SCOPE,
    ) {
        Ok(Response::Ok { data }) => serde_json::from_value(data)
            .map_err(|e| format!("decode pane list while resolving cwd: {e}"))?,
        Ok(Response::Err { message, code }) => {
            return Err(format!(
                "list panes to resolve cwd: {}",
                fmt_code(&message, &code)
            ));
        }
        Ok(other) => return Err(format!("unexpected renga response: {other:?}")),
        Err(e) => return Err(format!("list panes to resolve cwd: {e}")),
    };
    let base = panes
        .iter()
        .find(|p| p.id == caller_pane)
        .and_then(|p| p.cwd.clone())
        .ok_or_else(|| {
            format!("cannot resolve relative cwd: caller pane {caller_pane} has no known cwd")
        })?;
    let joined = std::path::Path::new(&base).join(path);
    Ok(Some(joined.to_string_lossy().to_string()))
}

pub(super) fn handle_spawn_pane(id: &Value, args: &Value, ctx: &PeerCtx) -> Value {
    // Placement first (Issue #290): `tab: {new: …}` changes which
    // other arguments are even meaningful, so it must be interpreted
    // before `direction` is demanded.
    let placement = match parse_spawn_placement(args) {
        Ok(p) => p,
        Err(msg) => return err_response(id, -32602, &msg),
    };
    let min_pane_size = match parse_min_pane_size(args, &placement) {
        Ok(v) => v,
        Err(msg) => return err_response(id, -32602, &msg),
    };
    let split_params = match &placement {
        SpawnPlacement::NewTab { .. } => None,
        _ => {
            let direction = match parse_direction(args.get("direction").and_then(|v| v.as_str())) {
                Ok(d) => d,
                Err(msg) => return err_response(id, -32602, &msg),
            };
            Some((
                direction,
                parse_target(args.get("target").and_then(|v| v.as_str())),
            ))
        }
    };
    let command = command_arg(args);
    let name = opt_string(args, "name");
    let role = opt_string(args, "role");
    let cwd = opt_string(args, "cwd");

    let (caller_pane, endpoint) = match require_connected(ctx, id, "spawn pane") {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    // Resolve a relative cwd against the caller pane's cwd so relative
    // paths in Claude's tool calls behave the way a user would expect
    // when typing them into the pane's shell. Absolute paths are left
    // untouched; `None` is forwarded as-is so the server falls back to
    // its default (target pane's cwd for Split, the caller pane's cwd
    // for SpawnTab).
    let cwd = match resolve_mcp_cwd(endpoint, caller_pane, cwd.as_deref()) {
        Ok(v) => v,
        Err(msg) => return err_response(id, -32602, &msg),
    };
    if let SpawnPlacement::NewTab { label } = placement {
        return dispatch_spawn_tab(
            id,
            "spawn_pane",
            "pane",
            endpoint,
            &Request::SpawnTab {
                command,
                id: name,
                label,
                role,
                cwd,
                from_pane: Some(caller_pane),
            },
            None,
        );
    }
    let required_cap = split_required_cap(&placement, min_pane_size);
    let tab = match placement {
        SpawnPlacement::Tab(selector) => Some(selector),
        _ => None,
    };
    let (direction, target) = split_params.expect("split params parsed for non-new placement");
    match client::send_request_requiring(
        endpoint,
        &Request::Split {
            target,
            direction,
            command,
            id: name,
            role,
            cwd,
            from_pane: Some(caller_pane),
            tab,
            min_pane_width: min_pane_size.0,
            min_pane_height: min_pane_size.1,
        },
        required_cap,
    ) {
        Ok(Response::Ok { data }) => {
            let new_id = data.get("id").and_then(|v| v.as_u64());
            let msg = match new_id {
                Some(n) => format!("Spawned pane id={n}."),
                None => "Spawned pane (id not reported).".to_string(),
            };
            ok_response(id, tool_text_result(&msg))
        }
        Ok(Response::Err { message, code }) => err_response(
            id,
            -32603,
            &format!("renga refused spawn_pane: {}", fmt_code(&message, &code)),
        ),
        Ok(other) => err_response(id, -32603, &format!("unexpected renga response: {other:?}")),
        Err(e) => err_response(id, -32603, &format!("renga call failed: {e}")),
    }
}

pub(super) fn handle_close_pane(id: &Value, args: &Value, ctx: &PeerCtx) -> Value {
    let raw = args.get("target").and_then(|v| v.as_str()).unwrap_or("");
    if raw.trim().is_empty() {
        return err_response(
            id,
            -32602,
            "close_pane requires a non-empty target (pane id or name)",
        );
    }
    let target = parse_target(Some(raw));
    let (caller_pane, endpoint) = match require_connected(ctx, id, "close pane") {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    // Issue #296: `focused` / a name must mean *this* pane's tab.
    // Gated on its own capability because a pre-#296 server would drop
    // `from_pane` and close a pane in the user's visible tab instead —
    // silently, and irreversibly.
    match client::send_request_requiring(
        endpoint,
        &Request::Close {
            target,
            from_pane: Some(caller_pane),
        },
        crate::ipc::CAP_CALLER_SCOPE_CLOSE_IDENTITY,
    ) {
        Ok(Response::Ok { data }) => {
            let closed_id = data.get("id").and_then(|v| v.as_u64());
            let msg = match closed_id {
                Some(n) => format!("Closed pane id={n}."),
                None => "Closed pane.".to_string(),
            };
            ok_response(id, tool_text_result(&msg))
        }
        Ok(Response::Err { message, code }) => err_response(
            id,
            -32603,
            &format!("renga refused close_pane: {}", fmt_code(&message, &code)),
        ),
        Ok(other) => err_response(id, -32603, &format!("unexpected renga response: {other:?}")),
        Err(e) => err_response(id, -32603, &format!("renga call failed: {e}")),
    }
}

pub(super) fn handle_focus_pane(id: &Value, args: &Value, ctx: &PeerCtx) -> Value {
    let raw = args.get("target").and_then(|v| v.as_str()).unwrap_or("");
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return err_response(
            id,
            -32602,
            "focus_pane requires a non-empty target (pane id or name)",
        );
    }
    let target = parse_target(Some(trimmed));
    let (caller_pane, endpoint) = match require_connected(ctx, id, "focus pane") {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    match client::send_request_requiring(
        endpoint,
        &Request::Focus {
            target,
            from_pane: Some(caller_pane),
        },
        crate::ipc::CAP_CALLER_SCOPE,
    ) {
        // Focus replies with `ok_unit` per the IPC contract (see
        // `src/ipc/server.rs`), so there's no resolved id to echo.
        // Echoing the trimmed user input is the most informative thing
        // we can do without a second round-trip.
        Ok(Response::Ok { .. }) => {
            ok_response(id, tool_text_result(&format!("Focused {trimmed}.")))
        }
        Ok(Response::Err { message, code }) => err_response(
            id,
            -32603,
            &format!("renga refused focus_pane: {}", fmt_code(&message, &code)),
        ),
        Ok(other) => err_response(id, -32603, &format!("unexpected renga response: {other:?}")),
        Err(e) => err_response(id, -32603, &format!("renga call failed: {e}")),
    }
}

pub(super) fn handle_new_tab(id: &Value, args: &Value, ctx: &PeerCtx) -> Value {
    let command = command_arg(args);
    let name = opt_string(args, "name");
    let label = opt_string(args, "label");
    let role = opt_string(args, "role");
    let cwd = opt_string(args, "cwd");

    let (caller_pane, endpoint) = match require_connected(ctx, id, "open new tab") {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    let cwd = match resolve_mcp_cwd(endpoint, caller_pane, cwd.as_deref()) {
        Ok(v) => v,
        Err(msg) => return err_response(id, -32602, &msg),
    };
    match client::send_request(
        endpoint,
        &Request::NewTab {
            command,
            id: name,
            label,
            role,
            cwd,
        },
    ) {
        Ok(Response::Ok { data }) => {
            // The IPC contract for `Request::NewTab` replies with the
            // id of the single pane that was created inside the new
            // tab — that pane is also the focused one after the
            // switch, so surfacing it as "new pane id" is both
            // accurate and what a caller needs to address it later.
            let new_id = data.get("id").and_then(|v| v.as_u64());
            let msg = match new_id {
                Some(n) => format!("Opened new tab; new pane id={n} (now focused)."),
                None => "Opened new tab.".to_string(),
            };
            ok_response(id, tool_text_result(&msg))
        }
        Ok(Response::Err { message, code }) => err_response(
            id,
            -32603,
            &format!("renga refused new_tab: {}", fmt_code(&message, &code)),
        ),
        Ok(other) => err_response(id, -32603, &format!("unexpected renga response: {other:?}")),
        Err(e) => err_response(id, -32603, &format!("renga call failed: {e}")),
    }
}

// ── set_pane_identity (rename / re-assign role) ──────────────

/// Three-state arg extractor for `set_pane_identity`. Maps:
///
/// - key absent → `None`                       (leave unchanged)
/// - key present & JSON null → `Some(None)`    (clear)
/// - key present & JSON string → `Some(Some))` (set)
///
/// Any other JSON type (number, bool, object, array) is rejected —
/// the schema forbids it but Claude might still try, and silently
/// accepting a coerced value would confuse the three-state contract.
pub(super) fn parse_identity_field(
    args: &Value,
    key: &str,
) -> std::result::Result<Option<Option<String>>, String> {
    match args.get(key) {
        None => Ok(None),
        Some(v) if v.is_null() => Ok(Some(None)),
        Some(Value::String(s)) => Ok(Some(Some(s.clone()))),
        Some(other) => Err(format!("`{key}` must be a string or null; got {}", other)),
    }
}

pub(super) fn handle_set_pane_identity(id: &Value, args: &Value, ctx: &PeerCtx) -> Value {
    let target = parse_target(args.get("target").and_then(|v| v.as_str()));
    let name = match parse_identity_field(args, "name") {
        Ok(v) => v,
        Err(msg) => return err_response(id, -32602, &msg),
    };
    let role = match parse_identity_field(args, "role") {
        Ok(v) => v,
        Err(msg) => return err_response(id, -32602, &msg),
    };
    if name.is_none() && role.is_none() {
        // Nothing to do — return an explicit error so Claude doesn't
        // silently succeed on a typo'd payload (`nmae` instead of
        // `name`, etc.). The server would otherwise treat it as a
        // valid "no-op" call.
        return err_response(
            id,
            -32602,
            "set_pane_identity requires at least one of `name` / `role`",
        );
    }

    let (caller_pane, endpoint) = match require_connected(ctx, id, "set pane identity") {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    // Issue #296 — see `handle_close_pane` for why this is gated.
    match client::send_request_requiring(
        endpoint,
        &Request::SetPaneIdentity {
            target,
            name,
            role,
            from_pane: Some(caller_pane),
        },
        crate::ipc::CAP_CALLER_SCOPE_CLOSE_IDENTITY,
    ) {
        Ok(Response::Ok { data }) => {
            // Surface the updated pane record as a human-readable
            // line so Claude can confirm the new identity without
            // parsing structuredContent.
            let pane = data.get("pane").cloned().unwrap_or(Value::Null);
            let pane_id = pane.get("id").and_then(|v| v.as_u64());
            let pane_name = pane
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let pane_role = pane
                .get("role")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let mut parts = Vec::new();
            if let Some(n) = pane_id {
                parts.push(format!("id={n}"));
            }
            parts.push(format!("name={}", pane_name.as_deref().unwrap_or("(none)")));
            parts.push(format!("role={}", pane_role.as_deref().unwrap_or("(none)")));
            let msg = format!("Updated pane: {}.", parts.join(" "));
            ok_response(id, tool_text_result(&msg))
        }
        Ok(Response::Err { message, code }) => err_response(
            id,
            -32603,
            &format!(
                "renga refused set_pane_identity: {}",
                fmt_code(&message, &code)
            ),
        ),
        Ok(other) => err_response(id, -32603, &format!("unexpected renga response: {other:?}")),
        Err(e) => err_response(id, -32603, &format!("renga call failed: {e}")),
    }
}

// ── set_summary (per-pane summary string) ────────────────────

pub(super) fn handle_set_summary(id: &Value, args: &Value, ctx: &PeerCtx) -> Value {
    // The schema requires `summary` as a string. Reject anything else
    // (number, null, etc.) explicitly so callers get a clear error
    // rather than silently coercing.
    let summary = match args.get("summary") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => {
            return err_response(
                id,
                -32602,
                &format!("`summary` must be a string; got {}", other),
            );
        }
        None => {
            return err_response(id, -32602, "set_summary requires a `summary` argument");
        }
    };

    let (caller_pane, endpoint) = match require_connected(ctx, id, "set summary") {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    match client::send_request(
        endpoint,
        &Request::SetSummary {
            from_pane: caller_pane,
            summary: summary.clone(),
        },
    ) {
        Ok(Response::Ok { .. }) => {
            let msg = if summary.is_empty() {
                "Summary cleared.".to_string()
            } else {
                format!("Summary set: {summary}")
            };
            ok_response(id, tool_text_result(&msg))
        }
        Ok(Response::Err { message, code }) => err_response(
            id,
            -32603,
            &format!("renga refused set_summary: {}", fmt_code(&message, &code)),
        ),
        Ok(other) => err_response(id, -32603, &format!("unexpected renga response: {other:?}")),
        Err(e) => err_response(id, -32603, &format!("renga call failed: {e}")),
    }
}
