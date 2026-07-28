# Changelog

All notable changes to renga are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and from v1.0 onward this project adheres to
[Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html) under the
rules in [`docs/semver-policy.md`](./docs/semver-policy.md).

## [Unreleased]

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
