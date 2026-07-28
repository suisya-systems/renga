# IME composition overlay

renga ships a built-in IME composition overlay so JP / CJK candidate windows anchor to the caret instead of jumping across the screen as Claude streams. This page covers **what the overlay feels like** — recommended settings, the overlay's internal keymap, and troubleshooting.

For the **canonical TOML keys and CLI flags** (`[ime] mode` / `freeze_panes_on_overlay` / `overlay_catchup_ms` / `anchor_row_offset`, plus their `--ime-*` flag counterparts and precedence rules), see [`configuration.md`](./configuration.md). This doc deliberately does not restate the schema.

![Centered IME overlay with a Japanese conversion candidate window anchored right under the caret, Claude panes frozen behind it](../ime-overlay.png)

## What you get

1. **Overlay on demand.** Press `Ctrl+;` on a focused Claude pane — a centered multi-line composition box appears. The host-terminal IME candidate window anchors to the caret inside the box, so long JP words stop "jumping" around the screen mid-conversion ([Issue #25](https://github.com/suisya-systems/renga/issues/25)).
2. **Pane flicker stops.** While the overlay is open, renga freezes the pane underneath so Claude's thinking spinner and streaming tokens no longer force repaints that would flicker past your IME candidates. The freeze only takes effect while the overlay is open, so non-IME users never see a behavior change — that is why it ships on by default.
3. **Progress stays visible.** Every ~3 seconds renga unfreezes for a single frame so you can see Claude's streamed output advance. Tune the interval with `overlay_catchup_ms` (see [`configuration.md`](./configuration.md)): `0` for a pure freeze, `5000` if even 3 s feels busy.
4. **Multi-line drafts are first-class.** `Enter` inserts a newline; `Alt+Enter` (macOS `Option+Return`) sends the whole buffer on most hosts. On WSL2 / Windows Terminal where the host emulator binds `Alt+Enter` to *Toggle Fullscreen*, use `Ctrl+Enter` instead ([Issue #226](https://github.com/suisya-systems/renga/issues/226)).
5. **Temporary close is safe.** `Esc` / `Ctrl+C` closes the overlay so you can inspect the pane or use renga's pane-management shortcuts. Reopen the overlay on the same pane and the previous draft comes back.

## Overlay keymap

Inside the centered composition box:

| Key | Action |
|-----|--------|
| `Enter` | Insert newline (also `Shift+Enter`) |
| `Alt+Enter` | Send buffer to the pane and close (canonical commit on most terminals, incl. macOS `Option+Return` — see the macOS Option-as-Meta table in [`keymap.md`](./keymap.md) if Option doesn't fire). On WSL2 / Windows Terminal the host emulator binds this chord to *Toggle Fullscreen* and consumes it before renga sees it — use `Ctrl+Enter` instead ([Issue #226](https://github.com/suisya-systems/renga/issues/226)). |
| `Ctrl+Enter` | Send buffer — alternative commit for Windows Terminal / wezterm / VS Code / most Linux terminals. On WSL2 / Windows Terminal this is the recommended commit binding because the host swallows `Alt+Enter`; renga also accepts `Ctrl+J` (the LF byte 0x0A that the host's Ctrl+Enter actually delivers when extended-key reporting is off). |
| `Esc` / `Ctrl+C` | Close the overlay and keep the draft for the same pane; reopening restores it. |
| `←` `→` `↑` `↓` | Navigate |
| `Home` / `End` | Start / end of current line |
| `Ctrl+Home` / `Ctrl+End` | Start / end of whole buffer |
| `Backspace` | Delete char left of caret |

The overlay open / fallback chord set (`Ctrl+;` / `Alt+;` / `Alt+I`) is documented in the global Pane mode keymap — see [`keymap.md`](./keymap.md).

## Recommended overrides

Defaults are designed to be opt-out-friendly. Typical overrides:

```toml
# Live repaints during composition (no freeze).
[ime]
freeze_panes_on_overlay = false
```

```toml
# Pure freeze — no periodic catch-up frame.
[ime]
overlay_catchup_ms = 0
```

```toml
# Disable the overlay entirely. Ctrl+; is swallowed silently so it
# does not leak to the shell. Use this if your host terminal already
# places IME candidates correctly.
[ime]
mode = "off"
```

`--ime-*` CLI flags override `config.toml` for a single run. Full precedence rules are in [`configuration.md`](./configuration.md#precedence).

## Composing without the overlay (native IME anchoring)

You do not have to open the overlay — typing straight into a pane with the system IME works too, and renga still anchors the host's IME to your input position by parking the terminal caret there ([Issue #34](https://github.com/suisya-systems/renga/issues/34)).

Windows Terminal draws the pre-edit at that caret and opens the candidate window flush against the bottom of the same row, which left the popup butted straight into Claude's input line ([Issue #281](https://github.com/suisya-systems/renga/issues/281)). renga now drops the anchor one row so the popup — and the inline pre-edit — clear the input line:

```toml
# Anchor the native IME candidate window on the caret cell itself
# (pre-#281 behavior). Use this if you would rather have the visible
# caret sit exactly on your input cell than have the popup clear it.
[ime]
anchor_row_offset = 0
```

The trade-off is that the *visible* caret moves down with the anchor, since a terminal has only one cursor to give. The offset therefore applies only where a Windows IME really is anchored to the terminal caret: native Windows, or WSL under Windows Terminal (detected via the `WT_SESSION` / `WT_PROFILE_ID` that Windows Terminal forwards through `WSLENV`, and rejected when another emulator identifies itself through `TERM_PROGRAM` / `TERM`). SSH into a WSL distro, a Linux terminal under WSLg, and native Linux / macOS terminals all ignore the offset entirely and keep the caret exactly on the resolved cell.

> **Known limitation.** A Linux process cannot ask which emulator owns its tty. If you launch a terminal that identifies itself through neither `TERM_PROGRAM` nor `TERM` — bare `xterm`, `gnome-terminal` — *from* a Windows Terminal WSL shell, it inherits `WT_SESSION` and renga will read it as Windows Terminal. Set `anchor_row_offset = 0` in that session.

## Troubleshooting

- **`Ctrl+;` does nothing.** Some terminals swallow `Ctrl+;` before renga can see it (WSL under Windows Terminal, VS Code terminal on Linux, some tmux configs). Use the documented fallbacks `Alt+;` or `Alt+I`. macOS users may need to flip Option to act as Meta first — see [`keymap.md`](./keymap.md#macos-option-as-meta).
- **Candidate window still jumps.** Make sure the pane you intend to compose into is focused *before* opening the overlay; the overlay anchors to the focused pane's caret position. If you switched panes after opening, close (`Esc`) and reopen on the new pane.
- **Alt+Enter on WSL2 toggles fullscreen instead of sending.** Use `Ctrl+Enter` instead ([Issue #226](https://github.com/suisya-systems/renga/issues/226)). renga also accepts `Ctrl+J` as the same commit if your host swallows `Ctrl+Enter` too.
- **Streaming output disappears while composing.** That is freeze-on-overlay doing its job — set `overlay_catchup_ms` to a smaller value (e.g. `1500`) if you want progress to be more visible, or `freeze_panes_on_overlay = false` to disable the freeze entirely.

## See also

- [`configuration.md`](./configuration.md) — Canonical TOML keys and CLI flags.
- [`keymap.md`](./keymap.md) — Full keybindings including the `Ctrl+;` / `Alt+;` / `Alt+I` open chords and macOS Option as Meta.
