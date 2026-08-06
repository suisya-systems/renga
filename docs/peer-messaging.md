# Peer messaging between Claude Code and Codex panes

Mixed Claude Code and Codex instances running under the same renga session exchange structured messages through the `renga-peers` MCP server — across every tab since Issue #289 — so one agent can ask a sibling to research something, hand off a test failure, or coordinate without the user relaying every message manually. Claude peers receive `<channel source="renga-peers">` tags; Codex peers get a pane-local nudge from renga, then drain the actual queued message body with `check_messages`.

This page covers the **operational workflow** — setup, launch, the two-pane example, and troubleshooting. The **canonical MCP tool list, parameter schemas, error codes, and frozen-prefix strings** live in [`api-surface-v1.0.md`](./api-surface-v1.0.md) §1; this doc deliberately does not restate that contract.

> **Why this is different from [`claude-peers-mcp`](https://github.com/happy-ryo/claude-peers-mcp)** — both offer the same tool surface, but `claude-peers-mcp` infers peer scope from `cwd` / `git_root` / `PID` (heuristic, can collide). renga-peers uses the **renga session** as the authoritative scope — the panes the user literally put into this renga instance, across all of its tabs (`list_peers` lists your own tab first; other tabs are addressed by numeric pane id). The two can coexist in the same Claude install; channel names don't collide (`server:renga-peers` vs `server:claude-peers`).

## Setup — one-time

```bash
renga mcp install --client claude
renga mcp install --client codex   # if you want Codex peers too
```

Registers the running `renga` binary as the `renga-peers` MCP server in each selected client's user config. Re-running is idempotent; pass `--force` to overwrite after a renga upgrade. `renga mcp uninstall --client …` and `renga mcp status --client …` are the inverse and introspection commands.

For Codex, the default install keeps the client CLI as the primary registration path and only patches the minimum `env_vars` passthrough needed for peer messaging. If you also want renga to preconfigure `check_messages` and `send_message` to auto-approve where Codex supports it, opt in explicitly:

```bash
renga mcp install --client codex --codex-auto-approve-peer-tools
```

That flag intentionally does not auto-approve riskier tools such as `send_keys` or pane-control actions.

## Launching with the peer channel

Peer delivery is client-specific:

- **Claude Code** uses the MCP experimental channel protocol, so it needs `--dangerously-load-development-channels server:renga-peers` at startup.
- **Codex** uses the MCP registration installed by `renga mcp install --client codex`; once that is in place, a plain `codex` launch is enough. renga will nudge non-focused worker panes when they look ready, and Codex reads the actual peer request body with `check_messages`. If the target Codex pane is currently focused, renga shows a local notification overlay instead of injecting PTY input immediately.

renga gives you two shortcuts so you don't have to type the Claude launch flag by hand:

- **`Alt+P`** — Inserts `claude --dangerously-load-development-channels server:renga-peers ` into the focused pane (trailing space, *no* Enter). Review, optionally tack on args, press Enter to run. Works in any pane, any shell.
- **`renga split --role claude`** / **`renga new-tab --role claude`** — Creates a new pane and auto-launches Claude Code with the flag already applied. Explicit `--command` wins if you pass one, so the flag path stays an escape hatch you can override.

Once Codex is registered, orchestrator panes can also launch it in-band with `spawn_codex_pane(direction, …)`.

## Two-pane workflow

```
tab A                          tab B
┌──────────┬──────────┐        ┌──────────┐
│ claude-1 │ claude-2 │        │ claude-3 │
│          │          │        │          │
│  peers ──┼──▶ ✓     │        │    ▲     │
│  send ◀──┼── msg    │───id=3──────┘     │  ← reachable by numeric id (#289)
└──────────┴──────────┘        └──────────┘
```

In Claude A's chat:

```
> call list_peers
# returns: id=2 (same-tab sibling, addressable by id or name)
#          id=3 [tab 1] (other tab — address it by numeric id)

> call send_message with to_id=2 and message="can you read src/app.rs:handle_split and summarise?"
```

Since Issue #289 `list_peers` spans every tab (your own tab listed first) and `send_message` delivers across tabs when the target is a **numeric pane id**. A *name* still resolves only inside your own tab — pane names are unique per tab, not globally — and an unresolvable target returns a `pane_not_found` error instead of a fake `Delivered`.

Claude B sees a `<channel source="renga-peers">can you read src/app.rs...</channel>` tag in its next turn, recognises it as a peer request (not user input, thanks to the tag source), does the work, and replies back the same way.

Stable name lookups mean the orchestrator can address same-tab peers as `"secretary"` / `"worker-1"` instead of chasing numeric ids (names never resolve across tabs); `set_pane_identity` lets it (re)assign a pane's name mid-session if needed. The pushed body is prefixed with a `📡 PEER MESSAGE … NOT FROM USER` banner so an operator scrolling the transcript can tell at a glance that a `Human:`-rendered turn came from a peer rather than the user, and identical re-sends within a few seconds are collapsed server-side to keep the transcript free of phantom duplicate turns ([renga#221](https://github.com/suisya-systems/renga/issues/221)).

## Pane control alongside peer messaging

When a worker lands on an interactive prompt, the orchestrator can stay in-band:

- `inspect_pane(target="worker-1", lines=20)` to snapshot the visible state without asking the worker to describe itself.
- `send_keys(target="worker-1", text="y", enter=true)` (or named keys like `Esc`, arrows, `Ctrl+C`) to answer the prompt.
- `poll_events` gives you a cursor you can keep between turns so you notice `pane_started` / `pane_exited` without rescanning the full tab every time.

The pane-control tools (`list_panes`, `spawn_pane`, `spawn_claude_pane`, `spawn_codex_pane`, `close_pane`, `focus_pane`, `new_tab`, `inspect_pane`, `send_keys`, `set_pane_identity`, `poll_events`) round out the surface used by an orchestrator. Their full parameter schemas, return shapes, and error codes are listed in [`api-surface-v1.0.md`](./api-surface-v1.0.md) §1.


> **Tab scope.** For `list_panes`, `spawn_pane`, `spawn_claude_pane`, `spawn_codex_pane`, `focus_pane`, `inspect_pane`, `send_keys`, `close_pane` and `set_pane_identity`, "the current tab" means **the tab your own pane lives in**, not whichever tab the user happens to be looking at. Relative selectors (`target="focused"`, a stable pane name) never leave your tab; an explicit numeric pane id may reach a pane in another tab. `focus_pane` additionally switches the tab the user is *viewing* whenever the resolved pane is not already in it, because focus the keyboard cannot reach is not focus. `close_pane` has the other sharp edge: when the pane it resolves is the only pane in its tab and other tabs remain, renga **closes that whole tab** and reports success (only the last pane of the *only* tab is refused with `last_pane`).
>
> Seven of the nine were fixed in Issue #288 and the last two — `close_pane` and `set_pane_identity` — in Issue #296. Before those fixes they resolved against the visible tab, so an orchestrator running in a background tab would quietly `send_keys` into whatever the user had switched to, and `close_pane(target="focused")` would terminate a pane the user was typing in. If a tool now answers `[server_too_old] ... restart renga`, the renga *process* predates the fix even though the binary on disk does not — restart renga.

> **`claude` auto-upgrade.** `spawn_pane` / `new_tab` / `renga split` / `renga new-tab`, and layout-TOML `command = "claude"` entries, are auto-rewritten to the peer-enabled launch line so the new pane joins the renga-peers network without each caller having to remember `--dangerously-load-development-channels server:renga-peers`. Prefer `spawn_claude_pane` over `spawn_pane(command="claude ...")` when an orchestrator wants Claude — it keeps launch policy in renga and rejects reserved flags inside `args[]` with `invalid-params`.

> **Pane `cwd`.** `spawn_pane` / `new_tab` / `renga split --cwd` / `renga new-tab --cwd` / layout TOML `cwd = "..."` all accept a working directory for the new pane. Absolute paths are used as-is; relative paths resolve against the caller pane's cwd (MCP), the shell cwd (CLI), or the renga process cwd (layout TOML). Invalid paths (missing, inaccessible, or not a directory) fail with error code `cwd_invalid` **before** any layout mutation. Prefer this over embedding `cd <dir> && ...` inside `command` — the `claude` auto-upgrade only fires when `command`'s leading whitespace-delimited token is exactly `claude` (`claudex`, `claude-mobile` and `./claude` are deliberately left alone).

> **Tab placement (`tab`, Issue #290).** The three `spawn_*` tools accept an optional `tab` selector that says **which tab** hosts the new pane — an explicit mechanism, unlike the numeric-id escape hatch above. Exactly one key: `{"name": "workers"}` (exact display-name match; zero matches fail `tab_not_found`, several fail `tab_ambiguous` — labels are not unique, so renga never guesses), `{"index": 2}` (0-based, the same index `list_peers` reports), `{"pane_id": 17}` (the tab owning that pane — the stable anchor, since ids never shift when tabs close or get renamed), or `{"new": {}}` / `{"new": {"name": "workers"}}` to spawn a fresh single-pane **background** tab: the tab the user is looking at does not change, and `direction` / `target` must be omitted (a fresh tab has nothing to split). With an existing-tab selector, `target` resolves *inside* the selected tab and a numeric target from a different tab fails `target_tab_mismatch`. Omitted `cwd` on `tab.new` inherits the caller pane's cwd. Any use of `tab` requires a server advertising the `spawn_tab` capability; against an older renga process the call fails closed with `[server_too_old]` instead of quietly spawning into the caller's tab. `new_tab` is unaffected — it keeps creating **and focusing** a tab, and tab creation now caps at MAX_TABS = 16 (`tab_limit_reached`).

## Troubleshooting

- **`list_peers` reports "renga not reachable from this peer client"** — The client was launched outside a renga pane, or without inheriting the pane env. Re-launch from inside renga (`Alt+P` / `renga split --role claude` for Claude, or a normal `codex` / `spawn_codex_pane` launch after `renga mcp install --client codex`).
- **Peer messages don't render as `<channel>` tags** — You probably forgot the `--dangerously-load-development-channels server:renga-peers` flag. Prefer `Alt+P` over typing `claude` directly.
- **A message sent to Codex seems to do nothing** — renga only injects the `check_messages` nudge when the target Codex pane looks ready to accept PTY input and is not currently focused. If the message arrives while that pane is focused, renga shows a notification overlay instead: `Alt+Enter` / `Ctrl+Enter` inserts the `check_messages` prompt into the composer, `Esc` ignores it, and pressing Enter is still your decision. If you leave the pane focused alone, the request stays in the MCP inbox; if you move focus away, the worker-style deferred nudge path takes over. The actual request body still lives in the MCP inbox, so run `check_messages` and treat that result as the source of truth.
- **A new Codex pane asks for `check_messages` / `send_message` approval again** — Codex approvals can still behave pane-locally. `renga mcp install --client codex --codex-auto-approve-peer-tools` preconfigures the safe peer-messaging approvals, but a brand-new pane may still need one warm-up approval depending on the Codex version and runtime.
- **`spawn_codex_pane` fails with `[codex_not_installed]`** — Codex's MCP config (`~/.codex/config.toml`) is missing the renga-peers entry, the file is unreadable, or `RENGA_PEER_CLIENT_KIND=codex` is absent from its `[mcp_servers.renga-peers.env]` subtable. Run `renga mcp install --client codex` once; the install path self-heals an existing entry that is missing the env var.
- **`send_keys` seems to do nothing** — `send_keys` writes raw bytes to the target pane's PTY; it does not grant approval out-of-band. Snapshot first with `inspect_pane(target=…, lines=20)` to confirm the pane is actually waiting for input, and prefer a stable pane `name` over guessing by focus in changing layouts.
- **`poll_events` returns `events: []` before the timeout you expected** — A `types=[…]` filter only narrows what is returned; a non-matching event can still wake the long-poll and advance `next_since`. Re-issue the call with the returned cursor. If you receive `events_dropped`, re-sync with `list_peers`: `poll_events` is process-wide and delivers pane lifecycle from *every* tab, and `list_peers` is the matching all-tab view — `list_panes` returns your own tab alone and cannot reconcile events dropped elsewhere. `list_peers` omits your own pane, so pair it with `list_panes` when you need your tab in full.
- **Upgrading renga?** — Re-run `renga mcp install --client claude --force` and/or `renga mcp install --client codex --force` so each registered client points at your new binary.

## See also

- [`api-surface-v1.0.md`](./api-surface-v1.0.md) — Canonical, wire-frozen list of MCP tools, parameters, return shapes, and error codes.
- [`keymap.md`](./keymap.md) — Full keybindings, including the `Alt+P` peer-launch chord and file-tree `c` / `v` split-and-queue shortcuts.
- [`configuration.md`](./configuration.md) — TOML config keys (separate from the MCP / pane-control surface).
