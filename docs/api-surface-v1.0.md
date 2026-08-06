# renga v1.0 — Frozen API Surface

> **Status**: design proposal. Defines the surface that v1.0 promises to keep
> stable under the rules in [`semver-policy.md`](./semver-policy.md).
>
> **Source-of-truth commit**: `main` at the time this doc lands; cross-checked
> against `Cargo.toml` `version = "0.18.5"` (the last pre-1.0 release).
>
> **Audience**: downstream callers (claude-org-ja workers, Codex peers,
> third-party tooling that integrates with `renga-peers`).

## Stability legend

- **stable** — frozen by v1.0. Breaking changes follow the deprecation window
  in [`semver-policy.md`](./semver-policy.md).
- **stable-stub** — frozen as a no-op for wire-compat. Caller-visible inputs
  and outputs are part of the contract; behavior can be upgraded additively
  (a stub becoming a real implementation is *not* a break).
- **deferred** — shipped today, intentionally **not** part of v1.0. May change
  shape in any minor release; downstream should not depend on it.

The four frozen surfaces are: **MCP tools** (§1), **CLI** (§2),
**IPC protocol** (§3), and **config / layout / env** (§4). Errors and
forward-compat rules are in §5.

---

## 1. MCP tools (`renga-peers` stdio MCP server)

Launched per pane as `renga mcp-peer`. Speaks MCP over stdio. Routes peer
traffic through the renga IPC server.

**Frozen contract — `serverInfo`**:

- `name` is `"renga-peers"` — **stable**. Claude Code derives the
  `<channel source="renga-peers">` notification tag from this string. Renaming
  it is a breaking change (Q8).
- `version` follows `CARGO_PKG_VERSION`. Not part of the wire contract beyond
  semver compliance.

**Frozen contract — detached mode**: when `RENGA_PANE_ID` / `RENGA_SOCKET` are
absent the server still handshakes and advertises the full tool list. Tools
return ok-text payloads with the prefixes documented per tool below; clients
must accept these instead of JSON-RPC errors.

### 1.1 `list_peers` — stable

| Field | Type | Notes |
|---|---|---|
| `scope` (in) | `"machine"\|"directory"\|"repo"` | Optional; **ignored**. Accepted for wire-compat with `claude-peers-mcp`. Results always span every renga tab (#289; before that, the caller's tab only). |

Result: text content listing `id`, `name`, `role`, `kind` (`claude`/`codex`),
`receive_mode` (`push`/`pull`), `cwd`, plus display-only tab metadata
(`tab` index, `tab_name`, `same_tab`) since #289. Every workspace is
enumerated, the caller's own tab first. Empty case: `"No peers in any renga
tab."` (#289; previously `"No peers in this tab."`).

**Detached fallback (frozen prefix)**: `"(no peers — renga not reachable from
this peer client: <reason>)"`. Downstream may match on this prefix; it is part
of the wire ABI.

### 1.2 `send_message` — stable

| Field | Type | Required | Notes |
|---|---|---|---|
| `to_id` | string | yes | Recipient pane id (numeric string) or stable name. All-digit strings are interpreted as ids (see §6.1). |
| `message` | string | yes | Body text. |

Result on success: `"Delivered to <to_id>."`.

**Detached fallback (frozen prefix)**: `"(message dropped — renga not
reachable: <reason>)"`.

**Cross-tab delivery (#289, supersedes the v1.0 Q5 silent no-op)**: a numeric
`to_id` reaches a pane in any tab and delivers normally. A *name* resolves
only inside the sender's own tab — pane names are unique per tab, not
globally, so an unqualified name can never address another tab. An
unresolvable target now fails with `pane_not_found` instead of silently
succeeding. This is a deliberate breaking semantic change (see
`semver-policy.md` §3) shipped without the §4 opt-in-flag window per the
owner decision on #289: the anti-enumeration rationale for the silent drop
moved to the security layer outside renga. Version skew fails closed via the
`cross_tab_peers` capability (§3.4): the bundled mcp-peer refuses `list_peers`
/ `send_message` against a server that does not advertise it.

**Same-payload dedupe (post-1.1)**: identical `(target, sender, body)` triples
arriving within a small dedupe window (~5s) are collapsed server-side to a
single delivery. The repeat call still returns `"Delivered to …"` so the
sender cannot probe the dedupe state; only one `Event::PeerInbox` reaches
the receiver. Two distinct senders sending the same body still both
deliver. See renga#221 for context.

**Push-mode body banner (post-1.1)**: for Claude (push) recipients renga
prepends a `📡 PEER MESSAGE — from {name} (id={id}) — NOT FROM USER` line
to the body before pushing it as `notifications/claude/channel`. The original
body is preserved verbatim after a blank line; pull-mode (Codex) deliveries
are unaffected. See renga#221.

Errors via `[code]`: `pane_not_found`, `pane_vanished`, `io_error`, plus the
shared `app_timeout` / `shutting_down` / `internal` set.

### 1.3 `set_summary` — stable (Q1)

| Field | Type | Required |
|---|---|---|
| `summary` | string | yes |

**v1.0 contract**: input shape is frozen; the tool is **implemented and
shipped in v1.0** (no longer a stub).

**Behavior**: the summary string is stored on the calling pane (resolved
from `RENGA_PANE_ID`) and surfaced as `summary` on every `PaneInfo` /
`PeerInfo` entry returned by `list_panes` and `list_peers`. Storage is
in-memory only — does not persist across renga restarts.

- An empty string clears the summary (round-trips to `Option::None` /
  omitted key on the wire).
- Repeated calls overwrite the previous value with the latest.
- Maximum length is 256 Unicode scalar values (`chars()`, not bytes);
  oversized input is rejected with `[summary_too_long]` before any
  state mutation.
- Pane exit drops the summary alongside the rest of the pane state.

Errors via `[code]`: `summary_too_long`, plus the shared
`pane_not_found` / `pane_vanished` / `app_timeout` / `shutting_down` /
`internal` set.

### 1.4 `check_messages` — stable

Input: `{}`.

Result: text + `structuredContent.messages[]` (each entry has `from_id`,
`from_name`, `from_kind`, `body`, `sent_at`) + `count`. Drains the local pull
inbox. Used primarily by Codex panes; the returned text intentionally instructs
the recipient to treat each body as an *instruction*, not transcript text.

### 1.5 `list_panes` — stable

Input: `{}`.

Result: text describing every pane in the **caller's tab** (Q4) — the tab the
calling pane lives in, which is not necessarily the tab on screen: `id`, `name`,
`role`, `focused`, geometry (`x`, `y`, `width`, `height`), `cwd`, `kind`,
`receive_mode`. Geometry fields are `0` before the first layout pass.

### 1.6 `spawn_pane` — stable

| Field | Type | Required | Notes |
|---|---|---|---|
| `direction` | `"vertical"\|"horizontal"` | conditional | `vertical` → new pane on the right; `horizontal` → new pane on the bottom. Required for every split — i.e. always, except with `tab: {new: …}`, where it is **forbidden** (a fresh tab has nothing to split). |
| `target` | string | no | Numeric id, name, or `"focused"`. Default `"focused"`. With a `tab` selector it resolves **inside the selected tab** (`"focused"` = that tab's focused pane); a numeric id owned by a different tab fails with `target_tab_mismatch`. Forbidden with `tab: {new: …}`. |
| `tab` | object | no | Tab placement selector (#290). Exactly one key: `{"name": "<label>"}` — exact display-name match, 0 matches → `tab_not_found`, several → `tab_ambiguous` (labels are not unique; never first-match); `{"index": N}` — 0-based tab index, the same index `list_peers` reports, out of range → `tab_not_found`; `{"pane_id": N}` — the tab owning that pane (stable anchor); `{"new": {}}` / `{"new": {"name": "<label>"}}` — create a fresh single-pane **background** tab: the visible tab does not change, geometry (rects + PTY size) is finalized before the success reply, and omitted `cwd` inherits the **caller pane's** cwd. Sending any `tab` requires the server to advertise `spawn_tab` (§3.4) — the MCP layer fails closed with `server_too_old` otherwise. |
| `command` | string | no | Startup command. **Bare `claude [...]` is auto-rewritten to the Alt+P peer-enabled form** — see contract note below (Q3). |
| `name` | string | no | Stable pane name, must satisfy `[A-Za-z0-9_-]`, not all-digits. |
| `role` | string | no | Free-form label. Non-unique. |
| `cwd` | string | no | Absolute or relative-to-caller. Validated **before** layout mutation; failure is `cwd_invalid`. |

Returns: text containing the new pane's numeric id (and, for `tab: {new: …}`,
the new tab's 0-based index).

**`command` rewrite contract (Q3)**: when `command` starts with the bare token
`claude` (no `--dangerously-load-development-channels`), renga injects the
peer-enabled launch flags so the new Claude pane joins the renga-peers
channel. An explicit `--dangerously-load-development-channels` is left alone.
This is **frozen behavior**; no opt-out flag in v1.0. Callers that want
verbatim execution should pick a different leading token (e.g. `bash -c
'claude ...'`).

Errors: `split_refused` (MAX_PANES = 16, or below `min_pane_width` /
`min_pane_height`), `cwd_invalid`, `pane_not_found`, `name_in_use`,
`name_invalid`, `io_error`; with a `tab` selector also `tab_not_found`,
`tab_ambiguous`, `target_tab_mismatch`, and (for `tab: {new: …}`)
`tab_limit_reached` (MAX_TABS = 16).

### 1.7 `spawn_claude_pane` — stable

Same envelope as `spawn_pane` minus `command`, plus structured Claude fields:

| Field | Type | Required | Notes |
|---|---|---|---|
| `direction`, `target`, `tab`, `name`, `role`, `cwd` | as in §1.6 | direction conditional | `tab` (#290) carries over unchanged, including the `{new: …}` background form. |
| `permission_mode` | string | no | Rendered as `--permission-mode <v>`. Not enum-validated server-side, so new Claude permission modes work without a renga release. |
| `model` | string | no | Rendered as `--model <v>`. |
| `args` | string[] | no | Appended after structured fields. Must NOT contain `--dangerously-load-development-channels` / `--permission-mode` / `--model` — rejected with JSON-RPC `-32602` invalid-params. |

POSIX shell quoting is applied server-side. Values containing single quotes may
not round-trip cleanly on PowerShell-fallback Windows hosts; callers should
restrict structured-field values to `[A-Za-z0-9_./:@+%=-]` for safety.

### 1.8 `spawn_codex_pane` — stable

Same envelope as `spawn_claude_pane` minus `permission_mode` / `model`. `args`
is appended after the literal `codex` token.

**Pre-condition**: the user must have run `renga mcp install --client codex`
so `RENGA_PEER_CLIENT_KIND=codex` is injected into Codex's MCP subprocess env.
The handler verifies this up front by inspecting `~/.codex/config.toml` for
`[mcp_servers.renga-peers.env] RENGA_PEER_CLIENT_KIND = "codex"`. If the file
is missing/unreadable, the renga-peers entry is absent, or the value differs
from `"codex"`, the call returns a JSON-RPC `-32603` whose message carries the
`[codex_not_installed]` marker and the remediation hint
`renga mcp install --client codex`. Issue #203 — replaces the prior
silent-bifurcation behavior recorded in v1.0.

### 1.9 `close_pane` — stable

Input: `{ target: string }` (required).

**Tab scoping (#296)**: caller-scoped like the other pane tools — `"focused"`
and stable names resolve inside the *calling pane's* tab, a numeric id may
address any tab. Before #296 the relative selectors resolved against the tab
the **user was viewing**, so a background orchestrator calling
`close_pane(target: "focused")` terminated a pane on the human's screen.
Requires the server to advertise `caller_scope_close_identity` (§3.4); the MCP
layer fails closed with `server_too_old` otherwise.

Errors: `pane_not_found`, `pane_vanished`, `last_pane` (only pane of only
remaining tab — surfaced as an error, not silenced), `io_error`.

### 1.10 `focus_pane` — stable

Input: `{ target: string }` (required). `"focused"` is a no-op (kept for
symmetry). Yanking focus is user-disruptive; doc explicitly tells callers to
use sparingly.

### 1.11 `new_tab` — stable

| Field | Type | Required | Notes |
|---|---|---|---|
| `command` | string | no | Same `claude` auto-rewrite as `spawn_pane`. |
| `name` | string | no | Stable pane name for the new tab's initial pane. |
| `label` | string | no | Tab label override (default: derived from cwd). |
| `role` | string | no | Free-form. |
| `cwd` | string | no | Absolute or relative-to-caller. Defaults to the renga server's cwd. |

Returns: numeric pane id of the new tab's initial pane. Focus switches to the
new tab.

Errors: `tab_limit_reached` (MAX_TABS = 16, #290), `cwd_invalid`, `io_error`.
For a **background** tab (no focus switch) use `spawn_pane` with
`tab: {new: …}` (§1.6) — `new_tab` itself keeps its create-and-focus contract.

### 1.12 `inspect_pane` — stable

| Field | Type | Required | Notes |
|---|---|---|---|
| `target` | string | yes | |
| `lines` | int ≥ 1 | no | Last N rendered lines ending at the live bottom (blank rows preserved). Since v1.4 (#278): N beyond the visible height continues into scrollback, capped at 2000 total; scrollback rows carry negative `row` indices and `screen.line_start` may be negative. Reads are pinned to the live tail regardless of the pane's user scroll position, which is preserved. N ≤ visible height and the omitted form are unchanged. |
| `include_cursor` | bool | no | Default `false`. |
| `format` | `"text"\|"grid"` | no | Default `"text"`. `"grid"` returns one JSON row object per line. `structuredContent` is always populated regardless of `format`. |

### 1.13 `send_keys` — stable

| Field | Type | Required | Notes |
|---|---|---|---|
| `target` | string | yes | |
| `text` | string | no | Sent before `keys`. |
| `keys` | string[] | no | See key vocabulary below. |
| `enter` | bool | no | Append CR after `text` + `keys`. |

**Frozen key vocabulary**: `Enter`/`Return`, `Tab`, `Shift+Tab`/`BackTab`,
`Esc`/`Escape`, `Backspace`, `Delete`/`Del`, `Up`/`Down`/`Left`/`Right`,
`Home`/`End`, `PageUp`/`PageDown`, `Space`, `Ctrl+<A-Z>`. Unknown names return
JSON-RPC `-32602` invalid-params **before** the IPC call (so detached-mode
rejection is also pre-IPC).

PTY-byte semantics — *not* a logical message. Whatever process is running in
the pane sees the bytes.

### 1.14 `set_pane_identity` — stable

| Field | Type | Required | Notes |
|---|---|---|---|
| `target` | string | no | Default `"focused"`. |
| `name` | string \| null | no | **Three-state**: omit = leave; `null` = clear; string = set. |
| `role` | string \| null | no | Same three-state semantics. |

**Tab scoping (#296)**: same rule as `close_pane` (§1.9) — `"focused"` and
names resolve inside the caller's tab, numeric ids cross tabs, and
`caller_scope_close_identity` (§3.4) is required.

Validation: `name` non-empty, not all-digits, `[A-Za-z0-9_-]` only, unique
within the **resolved pane's** tab (uniqueness is per tab, never global).

Errors: `name_in_use`, `name_invalid`, `pane_not_found`.

Returns the updated pane record so the caller doesn't need a `list_panes`
round-trip.

### 1.15 `poll_events` — stable (Q2)

| Field | Type | Required | Notes |
|---|---|---|---|
| `since` | string | no | Opaque cursor returned by a prior `next_since`. Omit → "start at now"; no historical replay. |
| `timeout_ms` | int ≥ 0 | no | Default 2000, hard cap 30000. `0` = non-blocking drain. |
| `types` | string[] | no | Filter list. Cursor advances past filtered-out events; non-matching arrival can early-return with `events: []` and an advanced cursor. |

Returns: `{ next_since: <cursor>, events: [<event obj>] }`.

Buffer cap: 4096 events per process; older entries evicted on overflow with an
`events_dropped` meta-event flowing through the buffered stream.

**Contract note (Q2)**: `poll_events` is the **MCP-side, opaque-cursor**
event interface — the right tool when a peer wants pull-style polling with
filters. The CLI `renga events` command (§2.2) is the **subscribe-stream**
counterpart for shell pipelines. Both are first-class in v1.0 and serve
different use cases; neither is deprecated.

**Counterintuitive but frozen**: a poll that filters out every buffered event
returns `events: []` with an **advanced** cursor. Callers must re-poll on
empty responses to make progress.

### 1.16 `server_info` — stable (#304)

Input: `{}`.

Reports the capability token set of the **running renga server** this pane is
attached to, without sending any capability-gated request. This is the
pre-flight surface: a client checks it before calling something gated (e.g.
the `tab` selector on the `spawn_*` tools needs `spawn_tab`) instead of
sending the call and reading the token out of a `[server_too_old]` failure.

Result: text + `structuredContent`:

| Field | Type | Notes |
|---|---|---|
| `status` | `"connected"\|"detached"\|"unreachable"` | The discriminant; read it first. |
| `reason` | string \| null | Why `status` is not `connected`. `null` exactly when connected. |
| `server.capabilities` | string[] \| **null** | What the running server advertised, verbatim (unknown/future tokens passed through). **`[]` means the server was asked and supports nothing; `null` means it was never asked.** Never conflate the two. |
| `server.pid` | int \| null | PID of the **server** process, not this client's. |
| `server.endpoint` | string \| null | Socket/pipe queried — disambiguates concurrent renga instances. Retained on `unreachable` (we know what we failed to reach); `null` only when `detached`. |
| `client.name` | string | `"renga-peers"`. |
| `client.version` | string | `CARGO_PKG_VERSION` of the **mcp-peer binary**, which is *not* the server's version — see below. |
| `client.pane_id` | int \| null | Caller's pane; `null` when detached. |
| `client.capabilities` | string[] | Tokens this build understands. **Never null** — a build always knows its own. |
| `effective_capabilities` | string[] \| null | `server.capabilities` ∩ `client.capabilities`, in `SERVER_CAPABILITIES` order. **This is the field to gate on.** `null` whenever `server.capabilities` is. |

Every key is always present, explicitly `null` when unknown, so a typed
consumer gets `None` rather than a silently-plausible default. Two
biconditionals are pinned by test: `server.capabilities != null` ⟺
`status == "connected"`, and `effective_capabilities != null` ⟺
`status == "connected"`.

**Gate on `effective_capabilities`, not `server.capabilities`.** The token
describes the last hop only; the real question is end-to-end. An older
mcp-peer talking to a newer server sees a token advertised truthfully, yet
its own handler never reads the corresponding argument — MCP arguments are
read by key and unknown keys are dropped — so the request would be silently
degraded. The intersection is what closes that.

**Never returns a JSON-RPC error**, in any state — a caller pre-flighting
capabilities must be able to read the answer out of a normal result, since
being forced back to parsing failure strings is the problem this tool exists
to remove. Ungated by construction: it completes only the `hello` handshake
and sends no `Request` beyond it, so it answers against every renga server
that has ever shipped, including pre-#288 ones that advertise nothing.

**Version skew, and why `client.version` is not the server's version**: renga
registers `renga mcp-peer` by absolute path, so upgrading the binary on disk
leaves the *old* server process running while newly spawned mcp-peers are the
*new* one. The two halves are reported separately and must not be conflated;
`client.version` describes the on-disk binary, never the process serving the
socket. This release exposes no server-side version field — capability tokens
are the contract, and a version→feature table maintained client-side rots.

**Pre-flight is advisory, not a lease.** The server can die between the probe
and the call, and every tool call opens a fresh connection. The
`send_request_requiring` gate and the `[server_too_old]` error remain the
authoritative check. This tool converts a guaranteed-wrong operator
configuration into a rare race; it does not remove the failure class.

**Absence is itself the answer** (#304 acceptance): a renga too old to have
this tool simply does not list it in `tools/list`, and a call yields
`-32601 unknown tool`. That is distinguishable from a server that merely
advertises fewer tokens, which returns `status: "connected"` with a short or
empty `capabilities` list.

### Common error wire format

JSON-RPC error `message` is `[<code>] <human message>` per `fmt_code`. Codes
are sourced from `renga::ipc::err_code` and are stable per its module-level
"Stability" doc-comment (deprecation-window contract — see §5).

JSON-RPC numeric codes (Q9): the renga MCP layer uses `-32602` for
client-side input validation faults (empty `to_id`, unknown `send_keys` key
name, conflicting `spawn_claude_pane.args` flag, unknown `inspect_pane.format`)
and `-32603` for everything else carrying a `[code]`. v1.0 freezes the
current usage but does **not** standardize a finer split — future minor
releases may narrow `-32603` cases to more specific numeric codes; downstream
must continue to read the `[code]` token for branching.

---

## 2. CLI surface (`renga` binary)

`renga [DIR] [flags] [SUBCOMMAND]`. With no subcommand the TUI launches; a
subcommand always exits without starting the TUI (dispatched over IPC to an
already-running renga server).

### 2.1 Top-level invocation — stable

| Arg / flag | Value | Notes |
|---|---|---|
| `DIR` (positional) | path | cwd to switch into before launching the TUI. |
| `--exec <CMD>` | string | Auto-run in initial pane. Conflicts with `--layout`. |
| `--layout <NAME>` | string | Load `./renga-layouts/<NAME>.toml` or `~/.config/renga/layouts/<NAME>.toml` (or `$RENGA_LAYOUTS_DIR`). Conflicts with `--exec`. |
| `--ime <hotkey\|off>` | enum | Overrides `[ime] mode` in config. |
| `--ime-freeze-panes[=BOOL]` | bool | Suppress repaints while IME overlay open. Default `true`. |
| `--ime-overlay-catchup-ms <MS>` | u64 | Periodic repaint interval while frozen. Default 3000, clamped ≥ 100; `0` = pure freeze. |
| `--lang <auto\|ja\|en>` | enum | UI language. |
| `--min-pane-width <COLS>` | u16 | Default 20. `0` clamps to 1. Process-global; not exposed per-call (see §6 *Out of scope*). |
| `--min-pane-height <ROWS>` | u16 | Default 5. `0` clamps to 1. Same scope as `--min-pane-width`. |
| `--no-macos-tip` / `--show-macos-tip` | bool | macOS Option-as-Meta first-launch banner override. Mutually exclusive. |
| `--version` / `-V` | bool | clap built-in. |
| `--help` / `-h` | bool | clap built-in. |

### 2.2 IPC subcommands — stable

Selector convention: exactly one of `--name` / `--id` / `--focused` per
command (clap `conflicts_with_all`). When no selector is given, the default is
`--focused`.

| Command | Args | Maps to IPC |
|---|---|---|
| `renga list` | — | `Request::List` |
| `renga send` | `--name\|--id\|--focused`, `--enter`, `<TEXT>` (positional) | `Request::Send { append_enter }` |
| `renga focus` | `--name\|--id` | `Request::Focus` |
| `renga close` | `--name\|--id` | `Request::Close` |
| `renga new-tab` | `--command`, `--id`, `--label`, `--role`, `--cwd` | `Request::NewTab` |
| `renga split` | `--target-name\|--target-id\|--target-focused`, `--direction <vertical\|horizontal>`, `--command`, `--id`, `--role`, `--cwd` | `Request::Split` |
| `renga inspect` | `--name\|--id\|--focused`, `--lines`, `--cursor` | `Request::Inspect` |
| `renga events` | `--timeout <humantime::Duration>`, `--count <usize>` | `Request::Subscribe` + stream |
| `renga rename` | `--name\|--id\|--focused`, `--to-name`/`--clear-name` (mutex), `--to-role`/`--clear-role` (mutex) | `Request::SetPaneIdentity` |
| `renga mcp-peer` | — | (not IPC) handed off to `mcp_peer::run` for the stdio MCP loop |
| `renga mcp install` | `--client <claude\|codex>` (default `claude`), `--force`, `--codex-auto-approve-peer-tools` | (writes Claude/Codex MCP config; not an IPC call) |
| `renga mcp uninstall` | `--client <claude\|codex>` | (config write) |
| `renga mcp status` | `--client <claude\|codex>` | (config read) |

**`renga rename` (Q6)**: same semantics as `set_pane_identity` (§1.14) —
three-state via `--to-X` / `--clear-X` flags. Frozen in v1.0.

**`renga events` vs `poll_events` (Q2)**: see §1.15. The CLI form streams a
connection (good for shell pipelines and `tail -F`-style tooling); the MCP
form cursor-paginates (good for cooperative pull from peer agents). Both are
frozen.

### 2.3 Environment variables — stable (Q7)

These were de-facto stable; v1.0 makes them part of the formal contract.

| Var | Direction | Purpose |
|---|---|---|
| `RENGA_SOCKET` | published by parent renga, read by children | Path to the IPC endpoint (Unix socket on Unix; Named Pipe path on Windows). |
| `RENGA_TOKEN` | published by parent, read by children | Per-instance session token. Not a secret (same-user trust model); used as a PID-reuse defense. |
| `RENGA_PANE_ID` | published per-PTY by renga, read by `renga mcp-peer` | Numeric pane id the MCP subprocess belongs to. Absent → MCP runs in **detached mode**. |
| `RENGA_PEER_CLIENT_KIND` | injected by `renga mcp install --client codex` into Codex's MCP subprocess env | `"claude"` or `"codex"`. Defaults to `claude`. Selects the receive mode (`push` vs `pull`). |
| `RENGA_LAYOUTS_DIR` | read by CLI | Override layout search root. |
| `RENGA_NO_MACOS_TIP` | read by `macos_tip` | Set non-empty → suppress macOS first-launch banner. macOS-only. |

The historical `CCMUX_NO_MACOS_TIP` from the 0.10.0 release notes is **not**
part of the v1.0 contract; renamed to `RENGA_NO_MACOS_TIP` in the 0.18.x
sweep.

---

## 3. IPC protocol (Unix socket / Named Pipe)

Local-only, same-user, newline-delimited JSON. Not an authentication or
secrecy boundary — same-user processes are inside the trust boundary.

### 3.1 Endpoint naming — stable

- **Unix**: `$XDG_RUNTIME_DIR/renga/renga-<pid>.sock`. Fallback:
  `$TMPDIR/renga-<uid>/renga-<pid>.sock`, then `/tmp/renga-<uid>/renga-<pid>.sock`.
  Parent dir is forced to `0o700`. `<uid>` is the **real** OS uid (`getuid()`).
- **Windows**: `\\.\pipe\renga-<pid>` (Named Pipe). Default session-scoped ACL.

### 3.2 Connection lifecycle — stable

Short-lived per request:

1. Client opens connection, sends `Hello { client_pid }`.
2. Server replies `Response::Hello { server_pid, session_token }`. Client
   verifies `session_token == $RENGA_TOKEN`; mismatch → reject (PID-reuse
   defense).
3. Client sends exactly one `Request`.
4. Server replies one `Response`.
5. Server closes its side.

Exception: `Request::Subscribe` switches the connection to event-stream mode —
server replies `Response::Subscribed`, then emits `Event` JSON Lines until the
client disconnects. No further `Request`s are accepted on that connection.

Server budgets: 5 s `APP_REPLY_TIMEOUT` (server → app event loop) +
5 s `CLIENT_MARGIN` → 10 s `RESPONSE_TIMEOUT` from the client's perspective.

### 3.3 Request envelope — stable

`#[serde(tag = "cmd", rename_all = "snake_case")]`.

| Variant | Fields | Notes |
|---|---|---|
| `hello` | `client_pid: u32` | Required first message. |
| `list` | `from_pane?: usize` | `from_pane` added in #288; optional, omitted on the wire when absent, so `{"cmd":"list"}` is unchanged. |
| `send` | `target: PaneRef`, `data: string`, `append_enter: bool` (default false), `from_pane?: usize` | |
| `split` | `target: PaneRef`, `direction: vertical\|horizontal`, `command?`, `id?`, `role?`, `cwd?`, `from_pane?: usize`, `tab?: TabSelector` | Relative `cwd` resolves against the **target** pane, not `from_pane`. `tab` (#290) is omitted on the wire when absent; senders must gate it on `spawn_tab` (§3.4). `{new: …}` is not valid here — that is `spawn_tab`'s job. |
| `focus` | `target: PaneRef`, `from_pane?: usize` | Resolving outside the visible tab switches the visible tab. |
| `close` | `target: PaneRef`, `from_pane?: usize` | `from_pane` added in #296; optional and omitted on the wire when absent, so `{"cmd":"close","target":"focused"}` is unchanged. Unlike the #288 five, the **legacy** (`from_pane` absent) branch searches every workspace — `renga close --id/--name` has always been cross-tab. Senders must gate `from_pane` on `caller_scope_close_identity` (§3.4). |
| `new_tab` | `command?`, `id?`, `label?`, `role?`, `cwd?` | Creates **and focuses** — unchanged by #290. |
| `spawn_tab` | `command?`, `id?`, `label?`, `role?`, `cwd?`, `from_pane?: usize` | #290. Creates a single-pane tab **in the background**: the active tab is untouched, geometry (rects + PTY size) is finalized before the reply, exactly one `pane_started` is emitted after name/role attach. Relative / omitted `cwd` follows the **caller pane** (falls back to the server cwd without `from_pane`). Replies `{ id, tab }` with the new pane id and 0-based tab index. Senders must gate on `spawn_tab` (§3.4). |
| `subscribe` | — | Switches to event-stream mode after ack. |
| `inspect` | `target: PaneRef`, `lines?`, `include_cursor: bool` (default false) | `lines` beyond the visible height reads scrollback since v1.4 (#278) — see §1.12. |
| `peer_list` | `from_pane: usize` | |
| `peer_send` | `from_pane: usize`, `target: PaneRef`, `body: string` | Cross-tab ids deliver since #289; names resolve in the sender's tab only; unresolvable targets fail `pane_not_found`. |
| `peer_register_client` | `pane_id: usize`, `kind: claude\|codex` | Posted by `renga mcp-peer` on startup. |
| `set_pane_identity` | `target: PaneRef`, `name?`, `role?` (three-state: missing / null / value), `from_pane?: usize` | Uses serde `double_option`. `from_pane` (#296) behaves exactly as on `close`. |
| `set_summary` | `from_pane: usize`, `summary: string` | Empty `summary` clears. >256 `chars` rejected with `summary_too_long`. |

`PaneRef` = `{ id: usize } | { name: string } | "focused"`.

`TabSelector` (#290) = `{ name: string } | { index: usize } | { pane_id: usize }
| { new: { name?: string } }`. Externally tagged like `PaneRef`. `name` is an
exact display-name match (0 → `tab_not_found`, >1 → `tab_ambiguous`; never
first-match), `index` is the 0-based tab index `list_peers` reports, `pane_id`
selects the owning tab (the stable anchor), `new` creates a background tab. A
tagged object rather than an overloaded string so a tab literally named "new"
stays addressable.

### 3.4 Response envelope — stable

`#[serde(tag = "status", rename_all = "snake_case")]`.

| Variant | Fields | When |
|---|---|---|
| `ok` | `data: Value` (request-specific shape) | Most success paths. |
| `hello` | `server_pid: u32`, `session_token: string`, `capabilities?: string[]` | Reply to `Hello` only. `capabilities` is `default` + `skip_serializing_if = "Vec::is_empty"`, so a pre-#288 server omits the key entirely and it decodes as `[]`. Exposed to peers by the `server_info` MCP tool (§1.16). |
| `subscribed` | — | Ack of `Subscribe`; event lines follow on same connection. |
| `err` | `message: string`, `code?: string` | Failure. `code` is `Option<String>` with `skip_serializing_if = "Option::is_none"`. |

`PaneInfo` payload (used by `list` data, `set_pane_identity` ok data, embedded
in `peer_list` data):
`{ id, name?, role?, focused, x, y, width, height, cwd?, kind?, receive_mode?, summary? }`.

`PeerInfo` = `PaneInfo` minus the focused flag and geometry (purposefully
hidden from cross-pane callers), plus — since #289 — optional display-only
tab metadata: `tab?` (workspace index; shifts when tabs close, never an
address), `tab_name?` (display label), `same_tab?` (whether the pane shares
the caller's tab, i.e. is addressable by bare name). All three are additive
serde (`default` + `skip_serializing_if`), so both old-client × new-server
and new-client × old-server decode cleanly.

Servers advertising cross-tab peer messaging include `cross_tab_peers` in the
`hello` capability list (#289), alongside `caller_scope` (#288). The bundled
mcp-peer requires `cross_tab_peers` for `list_peers` / `send_message` and
fails closed (`server_too_old`) when it is absent — a #288 server advertises
`caller_scope` yet still silently drops cross-tab sends, so the two tokens
are deliberately distinct.

Servers supporting tab-directed spawning additionally advertise `spawn_tab`
(#290). Any `spawn_*` call carrying a `tab` selector — including one that
resolves to the caller's own tab — is sent with `spawn_tab` required and
fails closed (`server_too_old`) when it is absent: `Request` tolerates
unknown fields, so a #289-era server would silently drop the selector and
split in the caller's tab, the wrong-tab accident again. Calls without `tab`
keep requiring only `caller_scope`, so pre-#290 behavior is untouched.

Servers that also scope `close` / `set_pane_identity` to the caller advertise
`caller_scope_close_identity` (#296). The bundled mcp-peer sends `from_pane`
on those two requests and requires this token, failing closed
(`server_too_old`) when it is absent: a #290-era server advertises the three
earlier tokens yet drops the unknown `from_pane` and closes a pane in the
visible tab — irreversibly. A token of its own, for the same reason
`cross_tab_peers` and `spawn_tab` are separate.

### 3.5 Event envelope — stable

`#[serde(tag = "type", rename_all = "snake_case")]`.

| Variant | Fields | Notes |
|---|---|---|
| `pane_started` | `id`, `name?`, `role?`, `ts_ms` | One per pane creation. |
| `pane_exited` | `id`, `name?`, `role?`, `ts_ms` | Exactly once per pane id. |
| `events_dropped` | `count: u64`, `ts_ms` | Synthesized when a slow subscriber missed events. Per-subscriber. |
| `heartbeat` | `ts_ms` | Periodic; only purpose is to detect half-closed connections. Buffer cap 256/subscriber. |
| `peer_inbox` | `target_pane: usize`, `from_pane: usize`, `from_name?`, `from_kind?`, `body`, `ts_ms` | May originate from any tab since #289 (previously intra-tab by construction). Subscribers filter on `target_pane`; pane ids are session-unique so the filter needs no tab awareness. |

**`heartbeat` audience (Q10)**: emitted into the subscribe-stream
(`renga events` / `Request::Subscribe`). The MCP-side `poll_events` consumes
heartbeats internally as a half-close detector and does **not** surface them
to callers. v1.0 freezes this asymmetry.

**Forward-compat rule**: clients **must** ignore unknown `type` tags rather
than abort the stream. New variants are additive (see §5).

---

## 4. Layout / config files

### 4.1 `~/.config/renga/config.toml` — stable

(Windows: `%APPDATA%/renga/config.toml`.)

```toml
[ime]
mode = "hotkey"           # "hotkey" (default) | "off"
freeze_panes_on_overlay = true   # default true
overlay_catchup_ms = 3000        # default 3000; non-zero clamped >= 100; 0 = pure freeze

[ui]
lang = "auto"             # "auto" (default) | "ja" | "en"; case-insensitive
org_sidebar = "coexist"   # "coexist" (default) | "replace" | "off"; case-insensitive
```

Missing or malformed file → warning to stderr, defaults apply (never fails
startup). Extra keys are ignored — additive forward-compat.

### 4.2 Layout TOML — stable (`version = 1`)

Search order: `$RENGA_LAYOUTS_DIR/<NAME>.toml` → `./renga-layouts/<NAME>.toml`
→ `~/.config/renga/layouts/<NAME>.toml`.

Top-level:

```toml
version = 1                # SUPPORTED_VERSION = 1; mismatch fails parse
name = "my-layout"         # non-empty
[root]
type = "pane" | "split"
# ... node schema below
```

Node — `type = "pane"`:

| Field | Type | Required | Notes |
|---|---|---|---|
| `id` | string | yes | Unique within layout. `[A-Za-z0-9_-]` only; non-empty. |
| `command` | string | no | Run after shell ready. |
| `role` | string | no | Free-form, may repeat. |
| `cwd` | string | no | Abs or relative-to-CLI-invocation. Falls back to parent pane's cwd; root leaf falls back to renga server cwd. |

Node — `type = "split"`:

| Field | Type | Required | Notes |
|---|---|---|---|
| `direction` | `"vertical"` \| `"horizontal"` | yes | |
| `ratio` | f32 | yes | Range `0.1..=0.9`; finite; otherwise reject. |
| `first` | node | yes | Recursive. |
| `second` | node | yes | Recursive. |

Caps: total pane count ≤ 16 (`MAX_PANES`).

The `version` integer is precisely the contract — any breaking schema change
ships as `version = 2` and the v1 parser continues to accept v1 files. The
parser already rejects unknown versions.

---

## 5. Errors, codes, and forward-compat

### 5.1 Error code catalog (`renga::ipc::err_code`) — stable

Wire ABI per the module's "Stability" doc-comment. The MCP layer surfaces
these as `[<code>] <human message>` in JSON-RPC error message strings.

| Code | Where | Meaning |
|---|---|---|
| `shutting_down` | every request | Server is shutting down. |
| `app_timeout` | every request | App event loop didn't reply within budget. |
| `parse` | every request | Request JSON failed to parse. |
| `protocol` | every request | Protocol violation (wrong message at wrong time). |
| `internal` | every request | Server invariant violation. |
| `pane_not_found` | pane-targeted requests | `PaneRef` did not resolve. |
| `pane_vanished` | pane-targeted requests | Resolved then disappeared mid-flight. Rare. |
| `split_refused` | `split`, `spawn_*` (and layout TOML apply); `spawn_tab` only for a terminal below the layout threshold | MAX_PANES = 16, or below `min_pane_width` / `min_pane_height`. Corrected in #290: `new_tab` never returned this — its capacity failure is `tab_limit_reached`. |
| `io_error` | requests with PTY side-effects | OS-level write/spawn failure. |
| `last_pane` | `close` | Refused to remove the only pane of the only tab. |
| `cwd_invalid` | `split`, `new_tab`, `spawn_tab` | `cwd` missing or not a directory. Pre-mutation rejection — no half-mutated layout. |
| `name_in_use` | `split`, `new_tab`, `set_pane_identity` | Another pane in the same tab holds the requested name. |
| `name_invalid` | `split`, `new_tab`, `set_pane_identity`, `spawn_tab` | Name empty / all-digits / non-`[A-Za-z0-9_-]`. `spawn_tab` rejects **before** creating the tab (#290). |
| `summary_too_long` | `set_summary` | Summary input exceeds 256 Unicode scalar values. Pre-mutation rejection. |
| `tab_not_found` | `split` with `tab` | Selector's display name matched no tab, or the 0-based index is out of range. Pre-mutation rejection. |
| `tab_ambiguous` | `split` with `tab` | `{name}` matched several tabs. Labels are not unique; the server never first-matches — re-address via `{index}` or `{pane_id}`. |
| `target_tab_mismatch` | `split` with `tab` | Numeric `target` lives in a different tab than the selector picked. The request contradicts itself; refused instead of following either half. |
| `tab_limit_reached` | `new_tab`, `spawn_tab` | MAX_TABS = 16 tabs already open. Deliberately not `split_refused`, which is about pane capacity inside one tab. |
| `codex_not_installed` | `spawn_codex_pane` | Codex's `~/.codex/config.toml` is missing the renga-peers entry, the file is unreadable, or the `RENGA_PEER_CLIENT_KIND=codex` env-var passthrough is absent. Surfaced from the MCP layer (not `renga::ipc::err_code`); branch on the `[code]` token same as the others. Run `renga mcp install --client codex` to remediate. |

### 5.2 JSON-RPC numeric codes (Q9)

- `-32602` invalid-params — input validation failures (empty `to_id`, unknown
  `send_keys` key name, conflicting `spawn_claude_pane.args` flag, unknown
  `inspect_pane.format`).
- `-32603` internal error — everything else, **including** renga-side errors
  carrying a `[code]` token. By design.

v1.0 does not split `-32603` further. Future minor releases may move specific
classes into more specific numeric codes; this is **not** a breaking change
because downstream is required to read the `[code]` token for branching.

### 5.3 Forward-compat rules — stable

- **Unknown event `type` tags**: ignore, do not abort the stream.
- **Unknown JSON keys** in config/layout/IPC payloads: ignored on read.
- **Unknown `[code]` tokens**: treat as the equivalent of `internal`.

These rules let renga add fields and variants additively without bumping the
major version.

---

## 6. Global rules and out-of-scope

### 6.1 Global rules (apply across all surfaces)

- **All-digit `name` ↔ id rule**: a string consisting entirely of digits is
  always interpreted as a numeric pane id. `set_pane_identity` and layout TOML
  reject all-digit names; pane lookup interprets all-digit `target` strings as
  ids. This is a global lookup invariant.
- **`PaneRef::Focused` defaulting**: CLI subcommands default to `--focused`
  when no selector is given. MCP tools that accept a `target` default to
  `"focused"` only where documented (`spawn_*`, `set_pane_identity`); other
  tools require an explicit `target`.
- **Tab scoping (Q4)**: `list_panes`, `focus_pane`, `send_message`,
  `inspect_pane`, `send_keys`, `spawn_pane`, `spawn_claude_pane`,
  `spawn_codex_pane`, `close_pane`, `set_pane_identity`, and `peer_send` are
  **scoped to the caller's tab** — the tab the calling pane lives in, *not*
  whichever tab the user is currently looking at (Issue #288; before that fix
  these resolved against the visible tab, which silently misdirected every
  call made from a background tab. `close_pane` / `set_pane_identity` were
  the two stragglers, fixed in #296).
  Relative selectors (`"focused"`, a stable name) never leave the caller's tab.
  An explicit **numeric pane id** may address a pane in another tab — the
  deliberate escape hatch, and the one thing #296 preserved unchanged.
  For `send_message` / `peer_send` the numeric-id escape hatch actually
  *delivers* across tabs since #289 (see Q5 below); the relative-selector
  rule is unchanged. `focus_pane` additionally switches the visible tab
  whenever the resolved pane is not in it — focus the keyboard cannot reach
  would not be focus.

  Wire-level: the five stable IPC requests (`list`, `send`, `split`, `focus`,
  `inspect`) carry an **optional** `from_pane`; `close` and `set_pane_identity`
  gained the same field in #296. Omitting it preserves each request's
  pre-existing semantics exactly, which is what the `renga` CLI sends — note
  that "pre-existing" differs between the two groups: active-tab-only for the
  #288 five, all-workspace search for the #296 two.
  Servers advertise a `caller_scope` capability in the `hello` reply (plus
  `caller_scope_close_identity` since #296); clients that depend on caller
  scoping refuse to run against a server that does not advertise it rather
  than degrade silently.
- **Cross-tab `peer_send` delivers (Q5, revised by #289)**: v1.0 froze the
  cross-tab silent no-op; #289 removed it. A numeric-id target in another tab
  delivers; an unresolvable target fails `pane_not_found`; `peer_list` spans
  every tab. Callers must no longer rely on the tab boundary to contain peer
  discovery or delivery — the anti-enumeration property moved to the security
  layer outside renga (owner decision on #289). Version skew is fenced by the
  `cross_tab_peers` capability token (§3.4).
- **Detached-mode ok-text fallbacks**: `list_peers` and `send_message` return
  the documented ok-text prefixes (§1.1, §1.2) instead of JSON-RPC errors when
  the renga IPC server is unreachable. The prefixes are part of the wire ABI.

### 6.2 Out of scope for v1.0

The following are **not** part of the v1.0 frozen surface. They may exist in
the codebase but downstream must not depend on them; they may change in any
minor release.

- **Cross-tab selectors** for `list_panes` / `focus_pane` etc. (Q4 → v1.1+).
  `send_message` gained cross-tab delivery by numeric id in #289; for the
  remaining tab-scoped tools, workers needing cross-tab coordination continue
  to use numeric-id escape hatches or the "all workers in one tab" pattern.
- **`spawn_pane.command` opt-out flag** for the `claude → claude
  --dangerously-load-...` rewrite (Q3). Callers who need verbatim execution
  must use a non-`claude` leading token (e.g. `bash -c '…'`).
- **Per-call `min_pane_width` / `min_pane_height`** on `spawn_pane` /
  `spawn_claude_pane` / `spawn_codex_pane`. Process-global only.
- **`peer_*` IPC variant naming as a stable surface**. The peer-routing
  subset of `Request` is reachable from downstream only via the MCP layer
  (§1) and the CLI (§2); the Rust-level variant names are not promised.
- **`SetPaneIdentity` `double_option` Rust encoding**. Wire JSON behavior
  (omit / `null` / value) is frozen; the Rust serde helper is not.
- **JSON-RPC `-32603` granularity** (Q9). Branch on `[code]`, not on the
  numeric.
- **`heartbeat` event in `poll_events`** (Q10). Internally consumed; not
  surfaced to MCP callers.
- **`CCMUX_*` legacy environment variables**. Retired in the rename sweep;
  not part of v1.0.

---

## Appendix — surface count

| Section | Count |
|---|---|
| MCP tools (§1) | 16 |
| CLI top-level flags (§2.1) | 11 |
| CLI IPC subcommands (§2.2) | 13 |
| Env vars (§2.3) | 6 |
| IPC `Request` variants (§3.3) | 15 |
| IPC `Response` variants (§3.4) | 4 |
| IPC `Event` variants (§3.5) | 5 |
| Error codes (§5.1) | 19 |
| Config schema sections (§4.1) | 2 |
| Layout TOML node types (§4.2) | 2 |
