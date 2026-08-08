# Changelog

All notable changes to renga are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and from v1.0 onward this project adheres to
[Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html) under the
rules in [`docs/semver-policy-2.0.md`](./docs/semver-policy-2.0.md).

## [Unreleased]

### Added

- **`send_message` can deliver as a real user turn.** (#323)
  `send_message(to_id, message, deliver="channel" | "user_turn")`.
  `deliver="channel"` is the default and is unchanged byte-for-byte —
  it is omitted on the wire, so a channel request serializes exactly as
  a pre-#323 one did. `deliver="user_turn"` instead types the body into
  the recipient agent's composer and submits it, so instructions that
  only arm on a genuine user turn (`/loop`, `/clear`, slash commands
  generally) actually take effect. Previously this was reachable only
  by driving `send_keys` by hand — write the text, verify it landed,
  send Enter as a *separate* call — a discipline enforced by prose, and
  therefore one that kept breaking.

  renga now owns that sequence. It refuses rather than guesses: the
  target must present a positively identified, empty agent composer
  with the caret in it, so a permission prompt, a folder-trust dialog,
  a half-typed human draft, or any screen renga cannot read is
  **refused with zero bytes written** (`user_turn_not_ready`), and a
  mid-turn agent is refused too (`user_turn_busy`) rather than queued.
  The same refusal covers a pane the human has scrolled back — every
  screen read honors the scrollback offset, so renga would otherwise be
  judging history while a live modal sits underneath it — a pane whose
  agent has exited, and a pane with a Codex nudge still being typed
  into the same composer. The structural proof is re-run on every read
  *after* the body is written too, not only before it: a modal that
  appears during the settle window is not adopted as the draft, so the
  Enter that follows can never land on a permission menu.
  Body bytes and Enter are separate PTY writes with a settle and a
  stability check between them, and success is reported only once the
  draft is observed to be consumed; a delivery that wrote bytes without
  an observed submit reports `user_turn_stalled` and says so plainly
  instead of claiming success. Multi-line bodies go out as a bracketed
  paste, and are refused (`user_turn_invalid_body`) when the target has
  not enabled bracketed paste, since typing raw newlines would submit
  the first line and drive the UI with the rest. A body too long for a
  **Codex** composer's single row is refused too — renga can follow a
  wrapped Claude composer across its continuation rows but has no
  verified model of Codex's, and typing a body in that it then cannot
  observe or submit is worse than declining it. An identical user turn
  to the same pane within 5s is suppressed and reports
  `status: "duplicate_suppressed"` — a separate ledger from the channel
  dedupe window, so neither mode can swallow the other.

  On the wire: an additive `deliver` field on `peer_send`, gated on a
  new `peer_user_turn` capability token. Version skew fails closed —
  an older server would ignore the field and perform a channel send
  while answering `Ok`, so the client refuses instead of reporting a
  `/loop` that never armed.

  **`send_keys` is unchanged.** Dialog control (folder-trust Enter,
  permission `y`/`n`, `Shift+Tab`, `Ctrl+C`) is exactly the state where
  "did the text land in the input box?" has no meaning, and a readiness
  check there would either refuse the keystroke or delay it past the
  moment it was meant for. The split is by intent: `send_keys` for raw
  key input, `send_message(deliver="user_turn")` for "make this agent
  take this as a turn".

- **`subscribe` can scope itself to one pane's inbox.** (#306) The IPC
  `subscribe` request gained an optional `from_pane`. Naming a pane
  scopes the subscription: it receives the pane lifecycle events
  (`pane_started`, `pane_exited`, `events_dropped`, `heartbeat`) plus
  only the `peer_inbox` events whose `target_pane` is that pane. The
  bundled `renga mcp-peer` opts in — the pane it serves is the only one
  whose messages it was ever going to act on.

  **Behavior for existing subscribers is unchanged.** A `subscribe`
  that sends no `from_pane` receives exactly what it always received:
  every event, including every `peer_inbox` whatever its `target_pane`.
  `renga events` sends no `from_pane`, so it prints the same lines it
  has always printed, and a consumer that never learns this field
  exists never notices that it was added. That is a new optional input
  with a default that preserves prior behavior, which
  [`docs/semver-policy-2.0.md`](./docs/semver-policy-2.0.md) §3 lists
  under "a change is **not** breaking" — this ships in a minor, and
  nothing on the frozen surface changed meaning.

  What opting in buys is queue pressure. The server used to hand every
  `peer_inbox` to every open subscriber and let each client throw away
  the ones addressed elsewhere, so one peer message was copied into the
  bounded 256-event queue of every subscriber in the session, including
  subscribers that only ever wanted pane lifecycle. A scope is now
  applied *before* the send: an event a subscription declines is never
  offered to that channel at all, so it cannot fill the queue and
  cannot count toward that subscriber's `events_dropped` tally.
  Everything else is as it was — every other event type is broadcast to
  every subscriber, several subscriptions naming the same pane all
  receive that pane's messages, and clients keep their own
  `target_pane` check as a backstop.

  **Wire compatibility holds in both directions.** A subscription that
  names no pane serializes to exactly `{"cmd":"subscribe"}` —
  byte-identical to the pre-#306 request — so an existing client is
  untouched on the wire. A new client talking to an older server sends
  `{"cmd":"subscribe","from_pane":N}`; that server ignores the unknown
  key and broadcasts as it always did, and the client's own
  `target_pane` check keeps the result correct. New-client × old-server
  degrades to client-side filtering rather than erroring.

  Servers advertise a new **`subscribe_pane_scope`** capability token.
  It is **advertise-only**: no client gates on it, nothing refuses to
  run without it, and existing clients and downstream integrations need
  no changes. It exists so a client, an operator, or a test can read
  off the `hello` reply whether a `from_pane` sent on `subscribe` will
  actually be honored — the one thing that is otherwise only observable
  from traffic.

  Scoping is **defense in depth, not a boundary of any kind**. Any
  process running as this user can open the socket and name any pane id
  in its `subscribe`, so naming a pane is not authentication — the
  trust boundary is still OS-level user isolation, exactly as the IPC
  security model says
  ([`docs/content/en/ipc.mdx`](./docs/content/en/ipc.mdx) → Security
  model). What a scope removes is narrower and real: peer messages
  addressed elsewhere are no longer copied into a subscription that
  declared which pane it cares about, which cuts both unintended
  delivery to other panes and the queue pressure those copies caused.

## [2.0.0] — 2026-08-07

### Added

- **`spawn_*` can place workers in another tab — or a new background
  one.** (#290) The three spawn tools (`spawn_pane` /
  `spawn_claude_pane` / `spawn_codex_pane`) accept an optional tagged
  `tab` selector: `{name}` (exact display-name match; zero matches
  fail `tab_not_found`, several fail `tab_ambiguous` — never
  first-match), `{index}` (0-based, aligned with `list_peers`),
  `{pane_id}` (the owning tab — the stable anchor), or `{new: {name?}}`
  to spawn a fresh single-pane **background** tab: the tab the user is
  viewing does not change, the hidden tab's geometry (rects + PTY
  size) is finalized before the success reply, `pane_started` fires
  exactly once with name/role attached, and an omitted `cwd` inherits
  the caller pane's cwd. With an existing-tab selector the `target`
  resolves strictly inside the selected tab (`target_tab_mismatch`
  otherwise); with `tab.new`, `direction` / `target` are rejected
  rather than ignored. On the wire this is an optional
  `tab: TabSelector` on `split` plus a new `spawn_tab` request
  (replying `{id, tab}`), both additive. Version skew fails closed via
  a new `spawn_tab` hello capability: any tab-directed spawn against
  an older server errors with `[server_too_old]` instead of silently
  spawning into the caller's tab. `new_tab` keeps its create-and-focus
  contract, and tab creation now caps at MAX_TABS = 16 with a
  dedicated `tab_limit_reached` error (the api-surface doc wrongly
  listed `new_tab` under `split_refused`; corrected).

- **`server_info` — peers can now read the capability set instead of
  inferring it from `[server_too_old]` errors.** (#304) renga has
  negotiated capabilities since #288, but nothing exposed the token
  set: a client could only try a gated request and parse the failure
  string. That forced callers into static self-declaration —
  claude-org-runtime shipped a `--server-capability spawn_tab` flag,
  default off, that an operator had to set by hand after checking the
  renga version. The new tool reports `status`
  (`connected`/`detached`/`unreachable`), the running server's
  advertised tokens, this build's own tokens, and
  `effective_capabilities` (the intersection, and the field to gate
  on — an older mcp-peer against a newer server sees a token
  advertised truthfully but has no code to send the matching argument,
  which MCP would silently drop). It never returns a JSON-RPC error, so
  reading the answer never means parsing a failure again.
  Implementation is pure client-side plumbing: the `hello` handshake
  already carried the token list on every call and
  `client::converse` discarded it, so **no IPC protocol change was
  needed** — no new `Request`/`Response` variant, no new field, and no
  new capability token (one meaning "I can report my tokens" would be
  circular, and an old server could not advertise it anyway). We chose
  a dedicated tool over folding the list into the `list_peers` /
  `list_panes` envelope because both of those are themselves gated —
  on `cross_tab_peers` and `caller_scope` — so against exactly the old
  servers worth interrogating they fail before producing an envelope,
  putting the pre-flight surface behind the gate it exists to
  pre-flight. A new `Request` variant was rejected for the same reason
  in a different costume: old servers reject unknown variants with
  `protocol`, which is learning-by-failed-attempt. The probe completes
  only the handshake and sends no command, so it answers against every
  renga server ever shipped, including pre-#288 ones that advertise
  nothing — and `[server_too_old]` stays as the authoritative
  last-resort gate, since pre-flight is advisory, not a lease.

### Changed

- **BREAKING — `split` / `new_tab` now enforce the pane `name` rule the
  frozen API surface already documented, and every pane label refuses
  control characters.** This closes a conformance gap, not a contract:
  [`docs/api-surface-v1.0.md`](./docs/api-surface-v1.0.md) §1.6 has said
  since the v1.0 freeze that `name` "must satisfy `[A-Za-z0-9_-]`, not
  all-digits", and §7 lists `name_invalid` for `split` and `new_tab` —
  but only `set_pane_identity` and #290's `spawn_tab` ever called
  `validate_pane_name`. `split` (the three `spawn_*` MCP tools, `renga
  split --id`) and `new_tab` stored whatever they were handed.

  The gap was reachable, not cosmetic: a pane name is interpolated into
  the Codex peer nudge, which **types it into the target pane's PTY and
  presses Enter a second later**, and into the
  `notifications/claude/channel` banner a receiving Claude reads. A name
  carrying `\r` therefore submitted attacker-chosen text in another
  agent's composer, and a `\n` forged banner lines around content it did
  not own. #289 widened the blast radius from one tab to every tab.
  Concretely:

  1. `split` / `new_tab` now apply the documented rule — non-empty after
     trim, not all-digits (those collide with numeric pane ids), charset
     `[A-Za-z0-9_-]`. **Names with spaces, dots, or non-ASCII characters
     are now rejected with `name_invalid`** where the implementation
     previously accepted them; a name is also stored trimmed. Names that
     already match the charset are unaffected. It is flagged BREAKING
     because the observable behavior changes on a frozen-surface request
     ([`docs/semver-policy.md`](./docs/semver-policy.md) §3), even though
     no caller was ever entitled to the laxity under the written
     contract.
  2. `role` and the tab `label` keep their documented **free-form**
     contract — the same §1.6 table calls `role` a "Free-form label",
     so spaces and non-ASCII stay legal — but they now reject control
     characters (`name_invalid`) on `split`, `new_tab`, `spawn_tab` and
     `set_pane_identity`. #290 validated `spawn_tab`'s `name` while
     leaving its `role` and `label` verbatim, so those two kept the
     injection the check existed to close. Charset-restricting them
     instead would have narrowed a contract the freeze deliberately left
     open, and is unnecessary: control characters alone carry the
     injection.
  3. As a backstop for labels registered by an older build or a layout
     file, every site that renders a name / role / tab label into
     another agent's context or toward a PTY — the Codex nudge, the
     channel banner, `check_messages`, `list_peers`, `list_panes` —
     strips control characters at output. Message **bodies** are
     untouched; they are the payload and are legitimately multi-line.

  The stripped set is Unicode `Cc` (C0 including `\t` / `\r` / `\n`,
  DEL, and C1) — the characters that stop being decoration once a label
  reaches a terminal. Printable confusables such as RTL overrides are
  deliberately left alone: they can mislead a human reading the tab bar
  but cannot forge a line or drive a terminal, and refusing them would
  mangle legitimate non-ASCII labels.

- **`close_pane` / `set_pane_identity` now resolve `focused` and names
  against the *caller's* tab.** (#296) The two tools #288 left behind
  still resolved relative targets against the tab the **user was
  viewing**, so `close_pane(target: "focused")` from a background
  orchestrator terminated whatever pane the human was typing in — the
  #288 wrong-tab bug, on the one operation that cannot be undone.
  Both now use the same rule as the other seven pane tools: `focused`
  and stable names stay inside the calling pane's tab, while an
  explicit **numeric pane id still crosses tabs** (the deliberate
  escape hatch, unchanged), and name uniqueness is still judged per
  tab. On the wire this is an optional `from_pane` on the `close` and
  `set_pane_identity` requests; omitting it — which is what the
  `renga close` / `renga rename` CLI does — keeps their pre-existing
  all-workspace search exactly. Version skew fails closed through a
  new `caller_scope_close_identity` hello capability: a #290-era
  server would drop the unknown `from_pane` and close a pane in the
  visible tab, so the bundled mcp-peer answers `[server_too_old] …
  restart renga` instead.

- **BREAKING — peer messaging now crosses tabs.** (#289) `send_message`
  / `peer_send` deliver to panes in **any** tab when addressed by
  numeric pane id, and `list_peers` / `peer_list` enumerate **every**
  workspace (caller's tab first) instead of only the caller's. Three
  observable contract changes on the frozen v1.0 surface, called out
  per [`docs/semver-policy.md`](./docs/semver-policy.md) §3 ("cross-tab
  `peer_send` switching from silent no-op" is its named example of a
  breaking semantic change):
  1. cross-tab sends deliver instead of silently no-opping — the
     tab boundary no longer contains peer discovery or delivery, and
     the anti-enumeration property of the silent drop is gone (owner
     decision on #289: isolation belongs to the security layer around
     renga, so the flip ships without the §4 opt-in-flag window);
  2. an unresolvable target now fails with `pane_not_found` where it
     previously returned a fake success;
  3. `list_peers`'s empty-case string changed to `"No peers in any
     renga tab."`.
  Name targets still resolve only inside the *sender's* tab (names are
  unique per tab, not globally) — and now against the sender's tab
  even when the human is viewing another one, fixing a misroute for
  background-tab orchestrators. `PeerInfo` gains optional display-only
  `tab` / `tab_name` / `same_tab` fields (additive serde, compatible
  both directions). Version skew fails closed: the server advertises a
  new `cross_tab_peers` hello capability and the bundled mcp-peer
  refuses `list_peers` / `send_message` with `[server_too_old]`
  against a server that does not advertise it — including #288-era
  servers that advertise `caller_scope` but still drop cross-tab
  sends. Queued Codex nudges now also flush into background tabs
  (previously a single-pane background tab never received its nudge).

- **`Ctrl+W` now asks before closing.** A centered modal (`Close this
  pane? y / n`) holds every key, paste, and mouse event until you
  answer: `y` closes, `n` / `Esc` cancels, any other key is swallowed
  with the prompt left up, so nothing leaks into the shell behind it.
  `Ctrl+Q` remains an unconditional escape hatch. The confirmation
  pins the pane (or the tab plus its exact pane set) at the moment you
  press `Ctrl+W`, so focus moves, tab-index shifts, or a concurrent
  MCP `close_pane` / `split_pane` can never redirect the `y` onto a
  different target — the prompt expires instead. The MCP `close_pane`
  tool is deliberately **not** affected and still closes immediately.
  (#285)

## [1.4.0] — 2026-07-29

First minor release after the v1.3.x patch line. `inspect_pane` /
`renga inspect` can now reach past the visible screen into scrollback
history, and pane close on Windows finally reaps the processes it used
to leave behind. The frozen v1.0 API surface (MCP wire shape, CLI
flags, config keys, env vars) is unchanged — `inspect`'s existing
`lines` input grows a backward-compatible reach into scrollback (the
old height clamp and the scroll-position dependence were both
undocumented), so this bumps the minor per
[`docs/semver-policy.md`](./docs/semver-policy.md).

### Added

- **`inspect_pane` / `renga inspect` can now read scrollback history.**
  `lines` beyond the pane's visible height continues into the vt100
  scrollback (up to 2000 lines total), so an orchestrator can retrieve
  a worker's recent output even when small screens shrink every pane
  to a handful of rows. Scrollback rows are returned with negative
  `row` indices (`-1` = the line just above the visible top) and
  `screen.line_start` may be negative. Previously such requests were
  silently clamped to the pane height. Omitted `lines` and
  `N ≤ visible height` behave exactly as before. (#278)

### Changed

- Inspect reads are now pinned to the live tail: the result no longer
  depends on whether a human happens to be scrolling the target pane
  (previously undocumented — an inspect during a scroll-up returned
  the scrolled view), and the pane's scroll position is preserved
  across the call. (#278)

### Fixed

- **Closing a pane no longer leaves its background processes running
  (Windows).** The old `taskkill /F /T` walked live parent → child
  links only, so descendants whose intermediate parent had already
  exited — dev servers, detached background jobs — survived the close,
  and a shell that had already exited on its own skipped the tree kill
  entirely. Each pane's shell is now assigned to a kill-on-close
  Windows Job Object at spawn, and closing the pane terminates the job,
  reaping the whole tree regardless of its topology. `taskkill` stays
  as the fallback when job assignment fails or the kernel rejects the
  terminate, and the job's kill-on-close flag is the final backstop if
  renga itself goes away. (#268)
- **`renga mcp-peer` no longer outlives the Claude Code process that
  started it.** stdin EOF is not a reliable shutdown signal on Windows:
  handle inheritance can leak the write end of the stdin pipe into
  sibling children of the spawning client, and any survivor keeps EOF
  from ever arriving — observed as mcp-peer processes lingering for
  days after their parent was gone. The peer server now watches its
  parent directly (Windows: a thread blocked on a handle to the parent,
  with a creation-time guard against the startup PID-reuse window;
  Unix: a `getppid` poll) and exits within a few seconds of the parent
  disappearing. If the watchdog cannot be armed, the server keeps
  running on the previous stdin-EOF path rather than exiting. (#269)
- **The caret no longer flickers onto the spinner row while Claude is
  generating (Windows / WSL conpty).** With an IME composition active,
  the caret was resolved to the correct input row every frame, but
  ratatui re-shows the hardware cursor at frame end *before* moving it,
  so on conpty the caret became briefly visible at the last painted
  cell — the spinner row, which repaints every frame, turning the
  one-frame leak into continuous flicker. On conpty the resolved caret
  is now applied after the frame as `MoveTo` then `Show`, while the
  cursor is still hidden, so it only ever becomes visible at its final
  position. Non-conpty targets keep the previous in-frame path, so the
  plain-PTY caret behavior from 1.3.1 is unaffected. (#262)

## [1.3.2] — 2026-06-07

Patch release. Fixes caret freeze/drift in Claude Code panes on Windows
under Claude Code v2.1.x, which changed how Claude paints its caret.
The frozen v1.0 API surface (MCP wire shape, CLI flags, config keys,
env vars) is unchanged.

### Fixed

- **The caret in Claude Code panes tracks arrow keys and mid-line edits
  again under Claude Code v2.1.x.** Modern Claude Code (observed
  v2.1.168) stopped painting its caret as an inverse-video cell and
  instead parks the visible terminal hardware cursor directly on the
  edit cell. renga's caret resolver — built for legacy Claude where the
  inverse cell was authoritative — found no inverse cell, ignored the
  now-correct cursor, and pinned the drawn caret to end-of-input, so
  arrow keys appeared dead and mid-line edits landed away from the
  painted caret. When no inverse cell exists in the input block, the
  resolver now trusts the visible vt100 cursor if it sits inside the
  block (clamped for pending-autowrap); the in-block gate preserves the
  legacy protection against a cursor parked on spinner/streaming rows,
  and the inverse-cell tier stays first so legacy Claude is unaffected.
  The IME overlay's own caret-resolution copy gets the same
  hardware-cursor tier, so opening the overlay with the caret mid-line
  no longer snaps it to end-of-input. (#257)

## [1.3.1] — 2026-05-30

Patch release. Fixes caret/cursor desync on plain PTY panes — the
rendered caret could drift past the end of a line, and the conpty
cursor-leak guard only covered native Windows, leaving the WSL conpty
path exposed. The frozen v1.0 API surface (MCP wire shape, CLI flags,
config keys, env vars) is unchanged.

### Fixed

- **The caret no longer drifts past the line end on plain PTY panes.**
  When vt100 reports the pending-autowrap column (`col == cols`) at the
  right edge, renga now clamps the drawn cursor to `cols - 1` instead of
  rendering it one cell beyond the last column. New split panes are also
  seeded with their post-split geometry (minus the 1-cell border)
  instead of a fixed 10×40, so a fresh pane no longer takes a startup
  resize/reflow that contributed to the desync; the next render still
  resizes to the exact rect, so this is purely a better first frame.
  (#253)
- **The conpty cursor-leak guard now also covers the WSL conpty path.**
  The `Hide` that works around conpty's dropped `MoveTo` was gated to
  `#[cfg(windows)]` at compile time, so it never fired under WSL even
  when the conpty backend was in use. The guard is now a runtime check,
  so the WSL conpty path is protected too. (#253)

### Internal

- **The `gh pr create` PreToolUse hook is scoped to actual
  `gh pr create` calls.** The hook previously used an unsupported `if`
  field (silently ignored), so its `--repo` guard ran on every Bash
  command and blocked unrelated commands; it now exits early unless the
  command is a real `gh pr create`, preserving the upstream-PR
  protection. Repo tooling only — no effect on the renga binary. (#251)

## [1.3.0] — 2026-05-29

First minor release after the v1.2.x patch line. Adds click-to-split
panes: a pane can now be split by double-clicking its outer edge, and
the shared internal divider between two sibling panes can be
double-clicked to split the adjacent pane right on the divider. The
frozen v1.0 API surface (MCP wire shape, CLI flags, config keys, env
vars) is unchanged — this is a new pointer affordance in the input
handler, added backward-compatibly, so it bumps the minor per
[`docs/semver-policy.md`](./docs/semver-policy.md).

### Added

- **Double-click a pane's outer edge to split it in that direction.**
  A second click on the same outer-edge cell within 500 ms splits the
  pane: top / left clicks place the new pane on the clicked side, while
  bottom / right clicks place it on the trailing side (matching the
  historical Ctrl+D / Ctrl+E placement). Corner cells are ignored
  because their split direction is ambiguous, the double-click timer is
  scoped to the active tab so switching tabs within the window cannot
  be misread as the second click, and `MAX_PANES` / minimum pane
  width-height refusals are inherited from the existing split path.
  (#245, #246)
- **Double-click the shared internal divider between sibling panes to
  split the adjacent pane.** A second click on the same divider cell
  within 500 ms splits the bordering leaf right on the divider: a
  vertical divider splits its left leaf to the right, a horizontal
  divider splits its top leaf downward. A single click still arms the
  resize-drag and a plain click that never moves stays a no-op, so the
  resize affordance is preserved; junction cells where two dividers
  cross decline (like corner cells). The boundary hit-test now claims
  only the rows / cols a divider actually occupies, so a click that
  merely shares a nested divider's column but lands inside an unrelated
  pane no longer registers as a boundary press, and a nested divider's
  resize ratio is measured against the sub-region it slices rather than
  the whole pane. Hovering a shared divider tints it to match the
  file-tree / preview border affordance. (#247, #248)

### Documentation

- **Click-to-split is documented in the README and the Japanese
  keymap.** The README and `docs/keymap.ja.md` (JA) now describe the
  outer-edge and shared-divider double-click split gestures alongside
  the existing keyboard split bindings. (#249)

## [1.2.3] — 2026-05-14

Patch release. Fixes clipboard copy on WSL when the normal Linux
clipboard backend is unavailable and when pane-local applications such
as Claude Code emit OSC 52 clipboard sequences that the host terminal
does not accept directly. The frozen v1.0 API surface (MCP wire shape,
CLI flags, config keys, env vars) is unchanged.

### Fixed

- **Text selected with the mouse in renga now reaches the Windows
  clipboard on WSL even when the regular clipboard backend fails.**
  renga retries stale clipboard handles, then falls back to `clip.exe`
  under WSL so selected pane / preview text is still pasteable in
  renga, sibling panes, and Windows applications.
- **OSC 52 copy requests emitted by pane applications are bridged to
  renga's clipboard path.** Claude Code's terminal-copy path now works
  inside renga even when the outer terminal refuses OSC 52 directly:
  renga decodes BEL- and ST-terminated OSC 52 payloads, tolerates PTY
  chunk boundaries, and forwards the decoded text through the same
  WSL-aware clipboard fallback.

## [1.2.2] — 2026-05-13

Patch release. Reorganizes the README into a ~200-line landing-style
file with detail moved under `docs/`, documents the intentional Alt+P
silent-no-op gating for alt-screen / claude-titled panes so the refusal
isn't mistaken for a bug, and raises the `claude_monitor::context_limit()`
baseline to 500K only for Opus while Sonnet / Haiku / unknown stay at
200K so the status-bar token-usage ratio reflects each model's real
context window. The frozen v1.0 API surface (MCP wire shape, CLI flags,
config keys, env vars) is unchanged.

### Changed

- **`claude_monitor::context_limit()` returns 500K for Opus by default,
  keeping Sonnet / Haiku / unknown at the prior 200K.** Previously
  every model fell through to a single 200K baseline, which made the
  status-bar token-usage ratio over-report context pressure on Opus
  panes where the real window is 500K. The existing 1M overrides
  (`[1m]` suffix and `opus-4-6`) are preserved and continue to take
  precedence; only the per-model default changed. (#241)

### Documentation

- **README slimmed to ~200 lines; detail moved under `docs/`.**
  Keybindings now live in `docs/keymap.md`, configuration in
  `docs/configuration.md`, IME behavior in `docs/ime.md`, and peer
  messaging in `docs/peer-messaging.md`. The README now acts as a
  landing page with links into the topic docs instead of trying to
  cover everything inline. (#237)
- **Alt+P silent-no-op caveat documented in the keymap doc.** The
  Alt+P launch-line insertion is intentionally a no-op on panes that
  are in the alt-screen (e.g., running an interactive TUI) or whose
  pane title is `claude`, so the chord is never displayed in the
  rendered output of an already-claude pane and never overwrites the
  TUI buffer of an unrelated app. The keymap doc now calls this out
  next to the Alt+P row so users don't read the refusal as a bug.
  (#240, Closes #234)

## [1.2.1] — 2026-05-11

Patch release. Rebuilds the Linux npm release artifact with the musl
target so npm-installed `renga` no longer inherits the glibc version
from GitHub Actions' `ubuntu-latest` image.

### Fixed

- **Linux npm installs no longer fail on distributions older than the
  release runner's glibc with `GLIBC_2.39 not found`.** The release
  workflow now builds `renga-linux-x64` for
  `x86_64-unknown-linux-musl` and installs `musl-tools` only for that
  matrix entry, keeping the published filename and npm installer
  contract unchanged. (#235)

## [1.2.0] — 2026-05-10

First minor release after v1.1.x. Adds soft validation of
`spawn_claude_pane` `args[]` against the parsed `claude --help` output
so common flag typos are surfaced at MCP-call time rather than via a
cryptic Claude startup failure inside the spawned pane. The frozen v1.0
MCP wire shape is unchanged — input field types, required/optional
status, and error-code names are all preserved; the new validator only
adds a soft-fail path on previously-accepted-but-bogus flag spellings.

### Added

- **`spawn_claude_pane` soft-validates flag-like `args[]` entries
  against the parsed output of `claude --help`.** Help text is fetched
  once per process and cached for 5 minutes; if the parser fails or
  `claude --help` is unreachable the validator falls open so a
  transient `claude` outage cannot block pane spawns. Reserved-flag
  rejection (`--dangerously-load-development-channels`,
  `--permission-mode`, `--model` — already required for security
  routing because renga injects its own values for those) still runs
  first, so the soft validator never sees a reserved flag. Refs #229.
  (#230)

## [1.1.3] — 2026-05-09

### Fixed

- **IME composition overlay: `Ctrl+Enter` now commits on WSL2 / Windows
  Terminal, where the host emulator binds `Alt+Enter` to *Toggle
  Fullscreen* and consumes the chord before renga sees it.** The host
  delivers the user's Ctrl+Enter as a bare LF byte (0x0A) when extended
  key reporting is off, which crossterm decodes into `Ctrl+J`; renga's
  overlay commit predicate (`is_overlay_commit_key`) now accepts
  `Ctrl+J` as the WSL fallback for Ctrl+Enter, so the buffer commits to
  the target pane via the existing bracketed-paste path. `Alt+Enter`
  remains the canonical commit binding on hosts that don't shadow it,
  and the existing `Ctrl+Enter` event path (used by terminals that opt
  into kitty keyboard protocol or xterm modifyOtherKeys) is unchanged.
  README, README.ja, and the docs site keymap tables now call out the
  WSL caveat next to the `Alt+Enter` row. (#226)

## [1.1.2] — 2026-05-09

Patch release. Bracketed-paste events now route to the IME composition
overlay when it holds focus, so on WSL2 / Windows Terminal / WezTerm a
Ctrl+V no longer leaks pasted text through to the back-pane PTY
(typically Claude Code's input row) while the user is composing in the
overlay. Pasted CRLF from Windows clipboards is normalized to LF inside
the overlay buffer so stray `\r` no longer renders as a zero-width
control glyph and desyncs the rendered cursor from the buffer cursor,
and the normalization streams through an iterator bounded by the
existing 4096-char overlay cap so a megabyte-class hostile paste no
longer briefly allocates a megabyte just to drop all but 4096 chars.
The frozen v1.0 API surface is unchanged — this is a routing bug fix in
keyboard input handling.

### Fixed

- **Bracketed-paste (`Event::Paste`) is now spliced into the IME
  composition overlay buffer at the cursor when the overlay is open,
  instead of being unconditionally forwarded to the back-pane PTY.**
  WSL2 / Windows Terminal / WezTerm surface Ctrl+V as a terminal-level
  bracketed-paste, so the previous unconditional forward leaked pasted
  text through to whatever foreground client owned the PTY (typically
  Claude Code's input row) even while the user was actively composing
  in the overlay. `App::handle_paste` now centralizes the routing
  decision: if `self.overlay` is open the paste is spliced into the
  composition buffer (honoring the existing 4096-char cap by truncating
  the tail — a clipped paste is recoverable, a dropped paste is not),
  otherwise the existing PTY path is used. The PTY-echo paste cooldown
  is skipped on the overlay branch since there's no PTY echo to wait
  for, so the overlay redraw fires immediately. Pasted CRLF / bare CR
  from Windows clipboards is normalized to LF inside the overlay
  buffer so the wrap/render path (which only treats `\n` as a hard
  newline) no longer renders stray `\r` as a zero-width control glyph
  and desyncs the rendered cursor from the buffer cursor by one char
  per line. The CRLF normalization streams through an
  `impl Iterator<Item = char>` bounded by `take(remaining)` against the
  overlay cap, so a megabyte-class hostile paste short-circuits the
  state machine the moment the cap is reached instead of allocating
  the full normalized string upfront just to drop all but 4096 chars.
  (#224)

## [1.1.1] — 2026-05-08

Patch release. Peer channel notifications now carry a loud
`📡 PEER MESSAGE … NOT FROM USER` banner so operators can tell at a
glance that a `Human:` turn is renga peer chatter rather than user
input, and `handle_peer_send` dedupes duplicate `(target, from, body)`
re-sends within a 5-second window so a chatty dispatcher no longer
produces two phantom turns on the receiving pane. Also relocates the
`spawn_codex_pane` verifier entry from `[1.0.0]` to here, since the
fix actually shipped in 1.1.1 (the v1.1.0 binary did not contain
PR #220). The frozen v1.0 API surface is unchanged — the banner is a
presentation tweak in `notifications/claude/channel` and the dedupe /
verifier are bug fixes.

### Changed

- **Peer channel notifications now lead with a `📡 PEER MESSAGE …
  NOT FROM USER` banner.** Claude Code injects renga peer messages
  into a user-slot turn, which the transcript renders under a
  `Human:` heading. The banner is wrapped around the body in
  `notifications/claude/channel` so an operator scanning a long
  transcript can tell at a glance that the line is peer chatter
  rather than something the human typed. The original body is
  preserved verbatim after the banner. (#221, #222)
- **`spawn_codex_pane` now refuses to spawn when Codex's MCP config will
  not inject `RENGA_PEER_CLIENT_KIND=codex`.** Previously, if the user had
  not run `renga mcp install --client codex`, the freshly spawned codex
  pane registered as a `claude` (push) client and message delivery
  silently bifurcated. The handler now inspects `~/.codex/config.toml` for
  the `[mcp_servers.renga-peers.env] RENGA_PEER_CLIENT_KIND = "codex"`
  entry and fails the call with the new `[codex_not_installed]` error
  code, pointing the user at `renga mcp install --client codex`. The
  v1.0 freeze §6.2 entry tracking this as a follow-up has been removed
  and §1.8 / §5.1 are updated accordingly. Closes #203. (#220)

### Fixed

- **`handle_peer_send` now drops duplicate `(target, from, body)`
  re-sends inside a 5-second window.** Previously a chatty
  dispatcher / worker that fired the same payload twice in quick
  succession (duplicate acks, `PR_MERGE_WATCH_TIMEOUT` false fires)
  produced two phantom `Human:` turns on the receiving Claude pane.
  The dedupe key includes the sender, so two distinct peers sending
  the same text still both deliver. (#221, #222)

## [1.1.0] — 2026-05-07

First release after the v1.0 API surface freeze. Two new optional features
ship without touching the frozen surface, alongside four bug fixes.

### Added

- **`--fps` CLI flag and `[ui] fps` config key** for tuning the main
  event-loop target rate. Higher values reduce idle input latency at the
  cost of more wakeups; `0` is accepted and clamped to `1` at runtime to
  avoid a busy loop. The CLI flag overrides the config key; default
  behavior is unchanged when neither is set. Adds `--fps` to the frozen
  CLI surface as a new optional flag and `[ui] fps` to the frozen config
  schema as a new optional key (#213).
- **Ctrl+U IME overlay shortcut** that discards the entire composition
  buffer in one keystroke (multi-line, not just the current line). Footer
  hint updated; lowercase / Shift / empty-buffer paths covered by tests
  (#211).

### Fixed

- **Self-targeted peer sends now emit `Event::PeerInbox` instead of being
  silently dropped.** `handle_peer_send` previously returned `Ok` without
  emitting the event when `target_id == from_pane`, so JSON-RPC reported
  `Delivered` while the recipient never observed the message. The
  self-send guard is removed; cross-tab silence (the actual security
  boundary) is preserved (#215, #217).
- **Pane close now walks the descendant process tree on Windows.**
  `portable-pty`'s `Child::kill` only terminates the immediate shell, so
  grandchildren (e.g. `claude` / `node.exe` started via
  `spawn_claude_pane`'s queued startup command) survived close and kept
  open handles on the pane's working directory, blocking
  `git worktree remove --force` and `Remove-Item`. `Pane::kill` now
  invokes `taskkill /F /T /PID <pid>` before delegating to portable-pty,
  short-circuits when `try_wait()` shows the child has already exited
  (avoids redundant taskkill from `Drop` after an explicit close), and
  always calls `wait()` so the child is reaped on every exit pathway —
  no more zombies on natural Unix exit (#214, #216).
- **Cosmetic Claude/Codex pane indicators latch across OSC title
  rewrites.** The per-pane border accent, pane label, and status-bar tab
  title now key off the sticky `claude_ever_seen()` / `codex_ever_seen()`
  latches instead of the live title check, since both clients rewrite
  their OSC 0/2 window titles to in-flight task summaries that frequently
  drop the literal client name. Foreground-app gating
  (`shell_accepts_command_injection`, mouse protocol resolution,
  `codex_peer` fallback) keeps using the live signal where "is the client
  foreground right now?" is actually needed.
  `pane_expects_codex_peer_delivery` short-circuits on a registered
  Claude pane so transient "codex" mentions in a Claude task title cannot
  mis-route delivery (#209, #210).
- **Cargo.toml version-history comments and BRANCHING.md no longer
  reference the legacy `ccmux` binary name.** Comments for 0.9.0,
  0.10.0, 0.13.0, 0.14.0, 0.16.0, 0.17.0, the interprocess pin comment,
  the `~/.config/ccmux/.macos_tip_dismissed` path, and the
  `CCMUX_NO_MACOS_TIP` env var are updated to current `renga` naming.
  BRANCHING.md "renga と ccmux" is qualified as "upstream ccmux" to
  disambiguate now that this repo IS renga (#212).

## [1.0.0] — 2026-05-02

API surface freeze release. Defines the v1.0 frozen surface and adopts
formal semver for all subsequent changes. The Cargo.toml / `npm/package.json`
version bump was omitted at tag time and is reconciled in 1.1.0; the v1.0.0
git tag and GitHub Release remain the canonical marker for this surface
freeze.

### Added

- **API surface freeze.** [`docs/api-surface-v1.0.md`](./docs/api-surface-v1.0.md)
  defines the v1.0 frozen surface across the four boundaries: MCP tools, CLI,
  IPC protocol, and config / layout / env.
- **Semver policy.** [`docs/semver-policy.md`](./docs/semver-policy.md)
  formalizes what counts as a breaking change, the deprecation window, and
  how additive changes ship.
- **`RENGA_TOKEN` / `RENGA_SOCKET` / `RENGA_PANE_ID` / `RENGA_PEER_CLIENT_KIND`**
  are now part of the formal v1.0 contract (previously de-facto stable).
- **MCP `serverInfo.name = "renga-peers"`** is now part of the frozen
  contract; downstream tools (Claude Code's channel-source tag) may rely on
  this string.
- **Detached-mode ok-text fallback prefixes** for `list_peers` and
  `send_message` are now part of the wire ABI.

### Changed

- **`set_summary` is now implemented (was stub).** The input shape is
  unchanged; the tool now stores a per-pane summary string in-memory and
  surfaces it as `summary` on every `PaneInfo` / `PeerInfo` returned by
  `list_panes` / `list_peers`. Empty input clears the summary; input
  longer than 256 Unicode scalar values is rejected with the new
  `[summary_too_long]` error code. Closes #202.

### Documentation

- Added a *Stability* section to `README.md` linking the freeze and policy
  docs.

## Pre-1.0 history

Pre-1.0 release notes are preserved in the version-history comments in
`Cargo.toml` and the GitHub Releases page. From v1.0 onward they will be
maintained here.
