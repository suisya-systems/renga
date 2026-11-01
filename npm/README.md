# @suisya-systems/renga

An AI-native terminal for orchestrating multiple [Claude Code](https://docs.anthropic.com/en/docs/claude-code) and Codex agents in one workspace — mixed-client peer messaging, pane orchestration via MCP, and an IME-aware composition overlay for JP/CJK input.

For people already running 2+ coding agents in parallel. If you only ever run one agent at a time, the value over a plain terminal is small.

## Install

```bash
npm install -g @suisya-systems/renga
```

Migrating from previous `ccmux-fork`:

```bash
npm uninstall -g ccmux-fork && npm install -g @suisya-systems/renga
```

Migrating from the upstream `ccmux-cli`:

```bash
npm uninstall -g ccmux-cli && npm install -g @suisya-systems/renga
```

## Usage

```bash
renga                    # Launch in current directory
renga /path/to/project   # Launch in specified directory
```

## Features

- Mixed-client peer messaging between Claude Code and Codex panes via the built-in `renga-peers` MCP channel
- Pane-control MCP tools (`spawn_claude_pane`, `spawn_codex_pane`, `set_pane_identity`, `new_tab`, `send_keys`, `inspect_pane`, ...)
- Centered IME composition overlay for JP / CJK input with pane freeze + draft restore on reopen
- Multi-pane splits, tab workspaces, and layout TOML
- File tree sidebar with syntax-highlighted preview
- Claude Code auto-detection (pane border turns orange)
- Mouse support (click, drag resize, text selection)
- Cross-platform (Windows, macOS, Linux)

## Links

- [Docs](https://github.com/suisya-systems/renga/tree/main/docs)
- [GitHub](https://github.com/suisya-systems/renga)
- [Full README (with peer messaging, IME overlay, keybindings, configuration)](https://github.com/suisya-systems/renga#readme)
- [claude-org reference stack built on renga](https://github.com/suisya-systems/claude-org)

## History

renga was originally derived from [Shin-sibainu/ccmux](https://github.com/Shin-sibainu/ccmux) and has since evolved independently — the AI-agent peer network, mixed-client orchestration flow, IME overlay, layout TOML, and the bilingual UX layer are renga-specific. See [`BRANCHING.md`](https://github.com/suisya-systems/renga/blob/main/BRANCHING.md) for the divergence policy.

## License

MIT — upstream `Shin-sibainu/ccmux` copyright is retained per the license terms.
