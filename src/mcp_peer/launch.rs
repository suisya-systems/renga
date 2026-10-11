//! `spawn_claude_pane` / `spawn_codex_pane`: launch-command building and `claude --help` flag validation.

use super::*;

/// Flags that `spawn_claude_pane` must own — the structured fields
/// render these exactly once, so letting callers also inject them via
/// `args[]` would produce ambiguous command lines (e.g. two
/// `--permission-mode` entries, or a dropped peer-channel flag if a
/// caller overrides it with a narrower value). Rejecting is cleaner
/// than silent de-dup.
pub(super) const CLAUDE_RESERVED_FLAGS: &[&str] = &[
    "--dangerously-load-development-channels",
    "--permission-mode",
    "--model",
];

/// POSIX-style shell quoting targeted at the shells `renga` actually
/// runs Claude under on the agent-harness path: bash / zsh / sh on
/// Unix, Git Bash on Windows (the default when present).
///
/// A value made of "safe" chars (alphanumerics plus a small punctuation
/// set that never triggers word-splitting / globbing / variable
/// expansion) passes through unquoted so the resulting command line
/// stays readable. Anything else gets wrapped in single quotes with
/// embedded single quotes escaped as `'\''`.
///
/// **Scope limitation:** PowerShell's single-quoted literal does not
/// interpret the `'\''` escape sequence, so a value that mixes single
/// quotes with other characters won't round-trip cleanly when the
/// caller's Windows host lacks Git Bash and falls back to PowerShell.
/// Realistic `spawn_claude_pane` values (permission modes, model ids,
/// flag tokens) never contain single quotes, so the practical exposure
/// is minimal; if callers need PowerShell-safe launches for exotic
/// values they should pass an absolute path or pre-quoted string
/// through `args[]` and understand the shell contract themselves.
///
/// Shared between `build_claude_launch_command` and its tests.
pub(super) fn shell_quote(value: &str) -> String {
    // Empty string can never be left bare — the shell would drop it
    // entirely, silently losing an argument slot.
    if value.is_empty() {
        return "''".to_string();
    }
    let is_safe = value.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '_' | '-' | '.' | '/' | ':' | '@' | '+' | '%' | '=')
    });
    if is_safe {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for c in value.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Build the final `claude` launch command for `spawn_claude_pane`.
/// Order (matches the issue #137 spec):
///   1. `claude --dangerously-load-development-channels server:renga-peers`
///   2. `--permission-mode <permission_mode>` if present
///   3. `--model <model>` if present
///   4. caller-supplied `args[]`
///
/// Each value (structured field or extra arg) flows through
/// `shell_quote` so whitespace and shell metacharacters can't
/// re-split the command when the PTY's shell parses it. The
/// `CLAUDE_PEER_LAUNCH_CMD` prefix is a trusted, space-delimited
/// constant and is emitted verbatim.
pub(super) fn build_claude_launch_command(
    permission_mode: Option<&str>,
    model: Option<&str>,
    extra_args: &[String],
) -> String {
    let mut parts: Vec<String> = vec![CLAUDE_PEER_LAUNCH_CMD.to_string()];
    if let Some(mode) = permission_mode {
        parts.push("--permission-mode".to_string());
        parts.push(shell_quote(mode));
    }
    if let Some(m) = model {
        parts.push("--model".to_string());
        parts.push(shell_quote(m));
    }
    for a in extra_args {
        parts.push(shell_quote(a));
    }
    parts.join(" ")
}

pub(super) fn build_codex_launch_command(extra_args: &[String]) -> String {
    let mut parts: Vec<String> = vec!["codex".to_string()];
    for a in extra_args {
        parts.push(shell_quote(a));
    }
    parts.join(" ")
}

pub(super) fn parse_string_args_array(args: &Value) -> std::result::Result<Vec<String>, String> {
    match args.get("args") {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for (idx, v) in items.iter().enumerate() {
                match v.as_str() {
                    Some(s) => out.push(s.to_string()),
                    None => return Err(format!("args[{idx}] must be a string, got {v}")),
                }
            }
            Ok(out)
        }
        Some(other) => Err(format!("`args` must be an array of strings; got {other}")),
    }
}

/// TTL for the `claude --help` allowlist cache. Issue #229 calls for
/// "process lifetime or ~5 minutes, whichever is shorter" — we keep
/// the upper bound at 5 minutes so an in-place Claude upgrade that
/// adds new flags is picked up without restarting renga.
pub(super) const CLAUDE_HELP_TTL: Duration = Duration::from_secs(300);

/// Cache for the parsed allowlist. The Mutex is only held briefly to
/// read or write the cache slot; the (potentially slow) `claude
/// --help` subprocess runs *outside* the lock so concurrent spawns
/// don't serialize on it. A racing double-fetch on cache miss is
/// harmless — the second writer just overwrites the first with the
/// same result.
pub(super) static CLAUDE_HELP_CACHE: Mutex<Option<(Instant, Arc<HashSet<String>>)>> =
    Mutex::new(None);

/// Spawn-time soft validation (issue #229): consult `claude --help`
/// and return the set of recognized CLI flags (long forms like
/// `--resume`, short forms like `-p`). Returns `None` when the help
/// text can't be obtained or parsed — the caller falls open in that
/// case rather than blocking the spawn (a missing or upgraded Claude
/// binary should never wedge renga's launch path).
pub(super) fn claude_help_flag_allowlist() -> Option<Arc<HashSet<String>>> {
    // Fast path: cache hit, lock held briefly.
    {
        let guard = CLAUDE_HELP_CACHE.lock().ok()?;
        if let Some((stamp, set)) = guard.as_ref() {
            if stamp.elapsed() < CLAUDE_HELP_TTL {
                return Some(Arc::clone(set));
            }
        }
    }
    // Slow path: fetch fresh outside the lock so other panes that hit
    // the cache simultaneously aren't blocked behind our subprocess.
    let parsed = match fetch_claude_help_text() {
        Ok(text) => Arc::new(parse_claude_help_flags(&text)),
        Err(e) => {
            log_stderr(&format!(
                "spawn_claude_pane: `claude --help` parse failed; \
                 falling open on flag allowlist ({e})"
            ));
            return None;
        }
    };
    if let Ok(mut guard) = CLAUDE_HELP_CACHE.lock() {
        *guard = Some((Instant::now(), Arc::clone(&parsed)));
    }
    Some(parsed)
}

/// Run `claude --help` and capture stdout. Errors out on missing
/// binary, non-zero exit, or non-UTF-8 output — all of which trigger
/// the fall-open path in `claude_help_flag_allowlist`.
pub(super) fn fetch_claude_help_text() -> std::result::Result<String, String> {
    let output = Command::new("claude")
        .arg("--help")
        .output()
        .map_err(|e| format!("spawn failed: {e}"))?;
    if !output.status.success() {
        return Err(format!("non-zero exit: {}", output.status));
    }
    String::from_utf8(output.stdout).map_err(|e| format!("non-UTF-8 stdout: {e}"))
}

/// Extract recognized flag tokens from `claude --help` output.
///
/// Every option line in Claude's help starts with whitespace + a flag
/// (e.g. `  --resume   …`, `  -p, --print   …`). We pick those lines
/// up by trimming leading whitespace and checking for a leading `-`,
/// then split on whitespace and commas to walk the flag tokens. The
/// first non-flag token (a value placeholder like `<dir>`, `[name]`,
/// or the start of the description column) marks the boundary.
///
/// The `--foo=value` form is collapsed to its head (`--foo`) so the
/// validator's `head` lookup matches regardless of which form a
/// caller used.
///
/// Subcommand lines (`agents [options]`, `doctor`, …) are skipped
/// because they don't start with `-`. Wrapped description text that
/// happens to mention a `--flag` token is *not* picked up — the help
/// emits each option on a single line, and continuation lines (if
/// any) start with description text rather than a leading dash.
pub(super) fn parse_claude_help_flags(help_text: &str) -> HashSet<String> {
    let mut flags = HashSet::new();
    for line in help_text.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('-') {
            continue;
        }
        // Normalize the comma between aliases (`-p, --print`) so a
        // single split_whitespace pass walks both names.
        let normalized = trimmed.replace(',', " ");
        for tok in normalized.split_whitespace() {
            if !tok.starts_with('-') {
                // Hit the value placeholder or description — done.
                break;
            }
            // Strip the `=value` half so `--foo=bar` registers as
            // `--foo`. Tokens without `=` keep their full form.
            let head = tok.split('=').next().unwrap_or(tok);
            // Bare `-` / `--` aren't real flags; skip them so the
            // allowlist doesn't accidentally accept them.
            if head == "-" || head == "--" {
                continue;
            }
            flags.insert(head.to_string());
        }
    }
    flags
}

/// Render an abbreviated, sorted view of an allowlist for use in the
/// `[invalid-params]` error message. Keeps the response small — full
/// dumps of ~50 Claude flags would crowd the agent's context.
pub(super) fn abbreviate_flag_list(allowed: &HashSet<String>) -> String {
    const MAX: usize = 12;
    let mut sorted: Vec<&str> = allowed.iter().map(String::as_str).collect();
    sorted.sort();
    if sorted.len() > MAX {
        let head_list = sorted[..MAX].join(", ");
        format!("{head_list}, … ({} more)", sorted.len() - MAX)
    } else {
        sorted.join(", ")
    }
}

/// Parse the `args` JSON array for `spawn_claude_pane`, rejecting:
///
/// 1. Entries that match a structured-field flag (`--permission-mode`,
///    `--model`, `--dangerously-load-development-channels`) — these
///    are owned by the structured fields, and letting `args[]` also
///    inject them produces ambiguous command lines.
/// 2. Flag-shaped entries (`-x` / `--foo` / `--foo=bar`) that don't
///    appear in the soft-validation allowlist (`claude --help` output)
///    — protects callers from typos and silently-forwarded unknown
///    flags that surface as a Claude exit-1 inside the spawned pane
///    (issue #229).
///
/// Both checks match on the head (the chunk before any `=`) so a
/// caller can't sneak a reserved or unknown flag through by combining
/// it with its value. Non-flag args (positional values, prompts,
/// paths) pass through unconditionally.
///
/// `allowlist == None` disables soft validation — used both by the
/// fall-open path when `claude --help` fails and by tests that want
/// to exercise the reserved-flag branch in isolation.
pub(super) fn validate_claude_extra_args(
    args: &[String],
    allowlist: Option<&HashSet<String>>,
) -> std::result::Result<(), String> {
    for a in args {
        // `split('=')` always yields at least one element, so
        // `next().unwrap_or("")` degrades to an empty head for inputs
        // that start with `=` or are empty — neither of which matches
        // any reserved flag or starts with `-`, so both checks below
        // fall through cleanly to "allowed".
        let head = a.split('=').next().unwrap_or("");
        if CLAUDE_RESERVED_FLAGS.contains(&head) {
            return Err(format!(
                "args[] must not contain {head:?} — pass it via the structured field \
                 ({}) instead",
                match head {
                    "--permission-mode" => "permission_mode",
                    "--model" => "model",
                    "--dangerously-load-development-channels" =>
                        "implicit (always added by spawn_claude_pane)",
                    _ => "<structured>",
                }
            ));
        }
        // Soft validation: only kicks in when the token looks like a
        // flag *and* an allowlist is available. Positional values
        // (prompts, paths) and the fall-open path both pass through.
        if let Some(allowed) = allowlist {
            if a.starts_with('-') && !allowed.contains(head) {
                return Err(format!(
                    "unknown Claude CLI flag {head:?}; valid options from `claude --help`: {}",
                    abbreviate_flag_list(allowed)
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn handle_spawn_claude_pane(id: &Value, args: &Value, ctx: &PeerCtx) -> Value {
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
    let name = opt_string(args, "name");
    let role = opt_string(args, "role");
    let cwd = opt_string(args, "cwd");
    let permission_mode = opt_string(args, "permission_mode");
    let model = opt_string(args, "model");

    // `args` must be a JSON array of strings when present — reject
    // anything else instead of silently coercing, so typos surface.
    let extra_args = match parse_string_args_array(args) {
        Ok(v) => v,
        Err(msg) => return err_response(id, -32602, &msg),
    };
    // Skip the `claude --help` round-trip when there are no caller-
    // supplied args — there's nothing to validate against, and we'd
    // rather not pay the spawn-time cost on the trivial path.
    let allowlist = if extra_args.is_empty() {
        None
    } else {
        claude_help_flag_allowlist()
    };
    if let Err(msg) = validate_claude_extra_args(&extra_args, allowlist.as_deref()) {
        return err_response(id, -32602, &msg);
    }

    let command =
        build_claude_launch_command(permission_mode.as_deref(), model.as_deref(), &extra_args);

    let (caller_pane, endpoint) = match require_connected(ctx, id, "spawn claude pane") {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    // Relative cwd resolution mirrors `spawn_pane` so the two tools
    // give identical path semantics; only the command construction
    // differs.
    let cwd = match resolve_mcp_cwd(endpoint, caller_pane, cwd.as_deref()) {
        Ok(v) => v,
        Err(msg) => return err_response(id, -32602, &msg),
    };
    if let SpawnPlacement::NewTab { label } = placement {
        return dispatch_spawn_tab(
            id,
            "spawn_claude_pane",
            "Claude pane",
            endpoint,
            &Request::SpawnTab {
                command: Some(command.clone()),
                id: name,
                label,
                role,
                cwd,
                from_pane: Some(caller_pane),
            },
            Some(&command),
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
            command: Some(command.clone()),
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
                Some(n) => format!("Spawned Claude pane id={n}. Launch command: {command}"),
                None => format!("Spawned Claude pane (id not reported). Launch command: {command}"),
            };
            ok_response(id, tool_text_result(&msg))
        }
        Ok(Response::Err { message, code }) => err_response(
            id,
            -32603,
            &format!(
                "renga refused spawn_claude_pane: {}",
                fmt_code(&message, &code)
            ),
        ),
        Ok(other) => err_response(id, -32603, &format!("unexpected renga response: {other:?}")),
        Err(e) => err_response(id, -32603, &format!("renga call failed: {e}")),
    }
}

pub(super) fn handle_spawn_codex_pane(id: &Value, args: &Value, ctx: &PeerCtx) -> Value {
    handle_spawn_codex_pane_with(id, args, ctx, install::verify_codex_renga_peers_install)
}

/// Inner form with an injectable verifier so unit tests can drive the
/// `RENGA_PEER_CLIENT_KIND` check independently of the host machine's
/// `~/.codex/config.toml`.
pub(super) fn handle_spawn_codex_pane_with(
    id: &Value,
    args: &Value,
    ctx: &PeerCtx,
    verify_codex_install: fn() -> std::result::Result<(), String>,
) -> Value {
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
    let name = opt_string(args, "name");
    let role = opt_string(args, "role");
    let cwd = opt_string(args, "cwd");
    let extra_args = match parse_string_args_array(args) {
        Ok(v) => v,
        Err(msg) => return err_response(id, -32602, &msg),
    };
    let command = build_codex_launch_command(&extra_args);

    let (caller_pane, endpoint) = match require_connected(ctx, id, "spawn codex pane") {
        Ok(t) => t,
        Err(resp) => return resp,
    };

    // Issue #203: refuse to spawn unless Codex's MCP config will
    // inject `RENGA_PEER_CLIENT_KIND=codex` into the new pane's
    // mcp-peer subprocess. Otherwise the new pane registers as a
    // push (claude) client and `send_message` delivery silently
    // bifurcates from what the orchestrator expects. Runs after the
    // detached/connected gate so a renga-not-reachable failure isn't
    // hidden by a spurious `[codex_not_installed]`.
    if let Err(reason) = verify_codex_install() {
        // Always surface the remediation command — the verifier's
        // detail string explains *which* check failed (file missing /
        // entry missing / wrong value), but the user-actionable
        // recovery is always the same.
        return err_response(
            id,
            -32603,
            &format!(
                "renga refused spawn_codex_pane: [codex_not_installed] {reason} \
                 (run `renga mcp install --client codex` to register Codex \
                 with `RENGA_PEER_CLIENT_KIND=codex`)"
            ),
        );
    }
    let cwd = match resolve_mcp_cwd(endpoint, caller_pane, cwd.as_deref()) {
        Ok(v) => v,
        Err(msg) => return err_response(id, -32602, &msg),
    };
    if let SpawnPlacement::NewTab { label } = placement {
        return dispatch_spawn_tab(
            id,
            "spawn_codex_pane",
            "Codex pane",
            endpoint,
            &Request::SpawnTab {
                command: Some(command.clone()),
                id: name,
                label,
                role,
                cwd,
                from_pane: Some(caller_pane),
            },
            Some(&command),
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
            command: Some(command.clone()),
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
                Some(n) => format!("Spawned Codex pane id={n}. Launch command: {command}"),
                None => format!("Spawned Codex pane (id not reported). Launch command: {command}"),
            };
            ok_response(id, tool_text_result(&msg))
        }
        Ok(Response::Err { message, code }) => err_response(
            id,
            -32603,
            &format!(
                "renga refused spawn_codex_pane: {}",
                fmt_code(&message, &code)
            ),
        ),
        Ok(other) => err_response(id, -32603, &format!("unexpected renga response: {other:?}")),
        Err(e) => err_response(id, -32603, &format!("renga call failed: {e}")),
    }
}
