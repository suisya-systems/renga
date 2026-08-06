# renga

*Read this in other languages: [日本語](./README.ja.md)*

**An AI-native terminal substrate for orchestrating multiple [Claude Code](https://docs.anthropic.com/en/docs/claude-code) and Codex agents in one TUI — mixed-client peer messaging, Claude-specific UX where it matters, single binary.**

![renga screenshot](screenshot.png)

## What renga is

renga is a terminal where the panes know they are AI agents. Splits, tabs, and focus work like any TUI multiplexer, but the substrate underneath treats each pane as a first-class agent endpoint: it detects which panes are running Claude Code, lets Codex panes participate in the same peer network, and exposes pane-control MCP tools (`spawn_claude_pane`, `spawn_codex_pane`, `set_pane_identity`, `new_tab`, …). Peer scope is authoritative at the renga-tab level — panes the user literally put in the same tab — so cross-pane routing never collides across projects.

The target use case is **agent orchestration**: a "secretary" pane dispatching tasks to "worker" panes, sub-agents comparing approaches in parallel, a long-running session reaching out to a sibling for a quick lookup, or a mixed Claude / Codex tab where each client is used for what it is best at. If you only ever run one agent at a time, renga's value over your current terminal is small. If you run several, the peer channel and the AI-aware pane model are the point.

### Standalone or as part of a stack

renga works in two valid modes:

- **Standalone** — use it directly as an AI-native terminal for coordinating multiple Claude Code / Codex panes locally.
- **As Layer 3 under [`claude-org`](https://github.com/suisya-systems/claude-org)** — use it as the execution fabric beneath a higher-level operating model with Lead / Dispatcher / Curator / Worker roles, per-task working-directory boundaries, narrow permission contracts, knowledge curation, and organization suspend / resume.

The right mental model is: **renga is the execution fabric; claude-org is the reference operating system built on top of it.**

### Positioning vs. tmux / zellij

| | tmux / zellij | renga |
|---|---|---|
| Pane model | Generic shell sessions | First-class **AI agent endpoints** with stable id, role, focus flag |
| Inter-pane messaging | Copy-paste, manual `send-keys`, or external glue | Built-in MCP `renga-peers` network; Claude receives channel pushes, Codex gets a pane-local nudge and reads the queued body via `check_messages` |
| Spawning agent panes | User hand-wires shell commands and client flags | `spawn_claude_pane` / `spawn_codex_pane` MCP tools, plus `Alt+P` for Claude |
| IME / CJK | Host terminal handles it; candidate windows often jump as Claude streams | Built-in IME composition overlay with freeze-on-overlay + periodic catch-up so candidates anchor to the caret |
| Configuration surface | Shell glue, plugins, keytables | A small TUI binary; layout TOMLs declare panes/roles directly |

**Non-goals.** renga is *not* a generic tmux replacement — it does not aim to match tmux's session persistence, nested-server model, plugin ecosystem, or scripted automation surface. It does not try to be a terminal emulator (no font/glyph rendering of its own; it runs inside your existing terminal). It is not an IDE plugin or chat UI. The bet is narrower: be the best substrate for **multiple Claude Code agents collaborating in one window**, and stay small enough to ship as a single ~10 MB binary.

### Example: secretary + workers orchestration

A typical layout used by [`claude-org`](https://github.com/suisya-systems/claude-org) / [`claude-org-ja`](https://github.com/suisya-systems/claude-org-ja) — one "secretary" pane dispatches tasks to one or more "worker" panes via the renga-peers channel:

```
tab "project-X"
┌────────────────────┬────────────────────┐
│ secretary          │ worker-1           │
│ (claude, role=     │ (claude, role=     │
│  "secretary")      │  "worker")         │
│                    │                    │
│  send_message ────▶│  receives as       │
│   to_id="worker-1" │  <channel ...>     │
│                    │                    │
│◀── reply ──────────│                    │
└────────────────────┴────────────────────┘
```

From the secretary's chat, growing the team and dispatching a task is two MCP calls — no shell, no copy-paste. The worker sees the request as a `<channel source="renga-peers" …>` tag in its next turn, recognises it as a peer message (not user input — the tag's `source` attribute makes that distinction), does the work, and replies via `send_message` back to the secretary. Stable names mean callers can address peers as `"secretary"` / `"worker-1"` instead of chasing numeric ids.

The full peer-messaging workflow, two-pane example, troubleshooting, and pane-control toolset (`inspect_pane`, `send_keys`, `poll_events`, `set_pane_identity`, …) are documented in [`docs/peer-messaging.md`](./docs/peer-messaging.md).

## Features

- **Multi-pane terminal** — Split vertically/horizontally, run independent PTY shells
- **Tab workspaces** — Multiple project tabs with click-to-switch
- **Mixed-client peer messaging** — Same-tab Claude Code and Codex instances talk to each other via `renga-peers`; Claude receives channel pushes, Codex peers are driven through their pane by renga itself. See [`docs/peer-messaging.md`](./docs/peer-messaging.md).
- **File tree sidebar** — Browse project files with icons, expand/collapse directories
- **Syntax-highlighted preview** — View file contents with language-aware coloring
- **Claude Code detection** — Pane border turns orange when Claude Code is running
- **cd tracking** — File tree and tab name auto-update when you change directories
- **JP / CJK IME overlay** — Centered composition box with freeze-on-overlay + periodic catch-up so candidate windows anchor to the caret. See [`docs/ime.md`](./docs/ime.md).
- **Mouse support** — Click to focus, double-click a pane's outer edge or a shared border to split, drag borders to resize, scroll history
- **Scrollback** — 10,000 lines of terminal history per pane
- **Dark theme** — Claude-inspired color scheme
- **Cross-platform** — Windows, macOS, Linux
- **Single binary** — ~8–10 MB depending on platform, no runtime dependencies

## Install

### Via npm (recommended)

```bash
npm install -g @suisya-systems/renga
```

Update with `npm update -g @suisya-systems/renga`; if that no-ops because of a pinned cache, force-pull the newest version with `npm install -g @suisya-systems/renga@latest`. Verify with `renga --version` against the [latest release](https://github.com/suisya-systems/renga/releases/latest).

> Previously installed `ccmux-fork`? Migrate with: `npm uninstall -g ccmux-fork && npm install -g @suisya-systems/renga`. The same pattern works for the upstream `ccmux-cli`.

### Download binary

Download the latest binary from [Releases](https://github.com/suisya-systems/renga/releases): `renga-windows-x64.exe`, `renga-macos-arm64`, `renga-macos-x64`, `renga-linux-x64`.

> **Windows:** Microsoft Defender SmartScreen may show a warning because the binary is not code-signed. Click "More info" → "Run anyway" to proceed.
>
> **macOS/Linux:** `chmod +x renga-*` after downloading.

### From source

```bash
git clone https://github.com/suisya-systems/renga.git
cd renga
cargo build --release
# Binary at target/release/renga (or renga.exe on Windows)
```

Requires the [Rust](https://rustup.rs/) toolchain. If you plan to send PRs, enable the repo's git hooks once after cloning with `git config core.hooksPath .githooks` so a `cargo fmt --all -- --check` miss fails locally instead of on CI.

> **macOS users:** the default macOS terminal swallows `Option+<key>`, so renga's `Alt+T` / `Alt+P` / `Alt+1..9` / `Alt+Left/Right` shortcuts won't fire out of the box. See [docs/keymap.md → macOS: Option as Meta](./docs/keymap.md#macos-option-as-meta) for the one-line fix per terminal (WezTerm / iTerm2 / Alacritty / Ghostty / Kitty / Terminal.app).

## Usage

```bash
renga
```

Launch from any directory. The file tree shows the current working directory. The most-used flags are `--ime-freeze-panes` / `--ime-overlay-catchup-ms` / `--lang`, plus `--min-pane-width` / `--min-pane-height` for split sizing. Run `renga --help` for the full list; the canonical TOML schema and CLI-vs-config precedence are documented in [`docs/configuration.md`](./docs/configuration.md).

## Peer messaging between Claude Code and Codex panes

Mixed-client peer messaging is renga's headline differentiator: Claude Code and Codex instances in the same tab call `list_peers` / `send_message` / `check_messages` to delegate research, hand off failures, or coordinate without the user relaying every message manually. Claude peers receive `<channel source="renga-peers">` pushes; Codex peers get a pane-local nudge from renga and read the actual queued body with `check_messages`. Peer scope is the renga tab (authoritative, no `cwd` / `PID` heuristics), and `renga-peers` coexists with [`claude-peers-mcp`](https://github.com/happy-ryo/claude-peers-mcp) in the same install — channel names don't collide.

The shortest path to try it:

```bash
renga mcp install --client claude
renga mcp install --client codex   # optional, if you want Codex peers
# then launch Claude in a renga pane with Alt+P, or Codex with a plain `codex` line
```

Full setup, two-pane workflow, pane-control tools (`inspect_pane`, `send_keys`, `poll_events`, `set_pane_identity`, `spawn_claude_pane`, `spawn_codex_pane`, …), and troubleshooting are in [`docs/peer-messaging.md`](./docs/peer-messaging.md). The canonical MCP tool surface — parameter schemas, return shapes, error codes — lives in [`docs/api-surface-v1.0.md`](./docs/api-surface-v1.0.md).

## IME composition overlay

Press `Ctrl+;` on a focused Claude pane to open a centered multi-line composition box; the host terminal's IME candidate window anchors to the caret inside the box, and the pane underneath is frozen with a periodic catch-up so streaming output doesn't flicker past your candidates. Behavior knobs (`freeze_panes_on_overlay`, `overlay_catchup_ms`), the overlay's internal keymap, and platform-specific quirks (WSL2 `Alt+Enter` vs `Ctrl+Enter`, macOS Option as Meta) are documented in [`docs/ime.md`](./docs/ime.md).

## Keybindings cheat sheet

The first keys to learn. Full tables (Pane / File tree / Preview / Org sidebar / Mouse) and the macOS Option-as-Meta setup are in [`docs/keymap.md`](./docs/keymap.md).

| Key | Action |
|-----|--------|
| `Ctrl+D` / `Ctrl+E` | Split vertically / horizontally |
| Double-click pane outer edge | Split toward the clicked side (top/left spawns the new pane on the clicked side, bottom/right behind it) |
| Double-click shared border | Split the adjacent pane, dropping the new pane on the border between the two siblings (drag still resizes) |
| `Ctrl+Right` / `Ctrl+Left` | Cycle focus (panes, sidebar, preview) |
| `Alt+T` / `Alt+1..9` | New tab / jump to tab N |
| `Alt+P` | Insert peer-enabled `claude …` launch into the focused pane |
| `Ctrl+F` | Toggle file tree sidebar |
| `Ctrl+B` | Toggle the org sidebar — all tabs / panes / Claude status in one panel |
| `Ctrl+;` | Open IME composition overlay (`Alt+;` / `Alt+I` as fallbacks) |
| `Ctrl+Q` | Quit |

## Documentation

- [`docs/peer-messaging.md`](./docs/peer-messaging.md) — Setup, workflow, troubleshooting for the `renga-peers` MCP channel.
- [`docs/ime.md`](./docs/ime.md) — IME overlay behavior, recommended overrides, overlay keymap.
- [`docs/configuration.md`](./docs/configuration.md) — Canonical TOML schema (`[ime]`, `[ui]`), CLI flags, precedence.
- [`docs/keymap.md`](./docs/keymap.md) — Full keybindings (Pane / File tree / Preview / Org sidebar / Mouse) plus macOS Option as Meta.
- [`docs/api-surface-v1.0.md`](./docs/api-surface-v1.0.md) — Wire-frozen v1.0 contract: MCP tools, CLI, IPC, config/layout/env.
- [`docs/semver-policy-2.0.md`](./docs/semver-policy-2.0.md) — **Current** semver rules for breaking vs. additive changes (2.0 line onward).
- [`docs/semver-policy.md`](./docs/semver-policy.md) — Semver rules for breaking vs. additive changes around the v1.0 freeze. Superseded by the above; kept for the 1.0–1.4 line.
- [`BRANCHING.md`](./BRANCHING.md) — renga / upstream-ccmux divergence and cherry-pick policy.

## Architecture

```
src/
├── main.rs       # Entry point, event loop, panic hook
├── app.rs        # Workspace/tab state, layout tree, key/mouse handling
├── pane.rs       # PTY management, vt100 emulation, shell detection
├── ui.rs         # ratatui rendering, theme, layout
├── filetree.rs   # File tree scanning, navigation
└── preview.rs    # File preview with syntax highlighting
```

Key design decisions: `vt100` crate for terminal emulation (not ANSI stripping) — needed for Claude Code's interactive UI; binary tree layout for recursive splits with variable ratios; per-PTY reader threads with mpsc channel to the main event loop; OSC 7 detection for automatic cd tracking; dirty-flag rendering for minimal CPU usage when idle.

## Stability

renga's API freeze shipped in v1.0.0. The contract it keeps stable — across MCP tools, CLI, IPC, and config/layout/env — is defined in [`docs/api-surface-v1.0.md`](./docs/api-surface-v1.0.md), which tracks `main`; the `v1.0` in that filename records the freeze it began as, not the version it describes. The semver rules that govern breaking and additive changes are in [`docs/semver-policy-2.0.md`](./docs/semver-policy-2.0.md) (the v1.0-line policy it supersedes is preserved at [`docs/semver-policy.md`](./docs/semver-policy.md)). Pre-1.0 releases (`0.y.z`) never made these promises. Downstream tooling should pin to a single major range rather than an open-ended `>= 1.0`, since a major bump is precisely where this contract is allowed to change.

## Tech Stack

- [ratatui](https://ratatui.rs/) + [crossterm](https://github.com/crossterm-rs/crossterm) — TUI framework
- [portable-pty](https://github.com/nickelc/portable-pty) — PTY abstraction (ConPTY on Windows)
- [vt100](https://crates.io/crates/vt100) — Terminal emulation
- [syntect](https://github.com/trishume/syntect) — Syntax highlighting

## Learn Claude Code

New to Claude Code? Check out [Claude Code Academy](https://claude-code-academy.dev) for tutorials and guides.

## History & Acknowledgments

renga was originally derived from [Shin-sibainu/ccmux](https://github.com/Shin-sibainu/ccmux) in early 2026 and has since evolved independently — the peer-messaging MCP channel, Claude-aware pane detection, the IME composition overlay, layout TOMLs, and the bilingual UX layer are all renga-specific work. The two projects no longer track version-for-version; renga ships its own semver line on its own cadence (see [`BRANCHING.md`](./BRANCHING.md) for the divergence policy).

Many thanks to [Shin-sibainu](https://github.com/Shin-sibainu) for the original ccmux foundation, which gave renga its starting point for the ratatui pane tree, vt100-based terminal emulation, and the cross-platform PTY layer. The full upstream commit history is preserved in the repo's git log, and the `Shin-sibainu` MIT copyright is retained in [`LICENSE`](./LICENSE) per the license terms.

## License

MIT — see [`LICENSE`](./LICENSE). Copyright is retained for the original `Shin-sibainu/ccmux` author per the upstream license; renga's additional contributions are released under the same MIT terms.
