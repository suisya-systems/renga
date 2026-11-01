# renga — Claude Code Multiplexer

> Renamed from `ccmux` (Issue #102, 2026-04). Historical references to the prior name are preserved in the upstream-fork notes below and in version-history comments in `Cargo.toml`.

## Overview
Rust TUI tool for managing multiple Claude Code instances in split panes.

## Tech Stack
- Rust (stable), ratatui + crossterm, portable-pty, vt100

## Build & Run
```bash
cargo build          # Debug build
cargo build --release # Release build
cargo test           # Run tests
cargo run            # Run the app
```

## Architecture
- `main.rs` — Entry point, terminal setup, event loop
- `app.rs` — App state, event dispatching, layout tree
- `pane.rs` — PTY management, vt100 terminal emulation, shell detection
- `ui.rs` — ratatui rendering, layout calculation, theme
- `filetree.rs` — File tree sidebar
- `preview.rs` — File preview panel

## Key Design Decisions
- **vt100 crate** for terminal emulation (not ANSI stripping) — needed for Claude Code's interactive UI
- **Binary tree layout** for recursive pane splitting
- **Per-PTY reader threads** with mpsc channel to main event loop
- PTY resize via both `master_pty.resize()` and `vt100_parser.set_size()`

## Shell Detection Priority
- Windows: Git Bash → PowerShell
- Unix: $SHELL → /bin/sh

## Release Process
1. `Cargo.toml` と `npm/package.json` のバージョンを同じ値に揃えて上げる
2. PR で `main` に merge (main は PR 必須)
3. `git tag vX.Y.Z && git push origin vX.Y.Z`
4. CI (`.github/workflows/release.yml`) が自動で実行:
   - 4プラットフォーム (Windows x64, macOS x64/arm64, Linux x64) のリリースビルド
   - GitHub Release 作成 + checksums.txt 生成
   - npm publish (Trusted Publishing)

### タグ命名
- 通常は **`vX.Y.Z` (plain semver)** を使う。これで GitHub Release が stable、npm dist-tag が `latest` になる。
- 先行公開したい場合だけ `vX.Y.Z-rc.N` / `-beta.N` / `-alpha.N` 等を使う。workflow が `ref_name` に `-` を含むかで自動的に prerelease + npm `next` に振り分ける。
- 過去に `vX.Y.Z-fork.N` suffix で全リリースを prerelease 扱いしていたが、フォーク識別子はパッケージ名 (現在 `@suisya-systems/renga`、旧 `ccmux-fork`) とリポジトリ名で既に確保されており、実運用中のバージョンを "pre" として出す意味がなかったため v0.5.7-fork.3 以降廃止。

### やってはいけない
- **手動で `npm publish` や `gh release create` しないこと** — バージョン衝突の原因になる

## Fork & Branching (Divergence Policy)
renga was forked from `Shin-sibainu/ccmux` but is now developed as an **independent main line**. There is no periodic / weekly upstream sync; useful upstream changes are pulled in only as ad-hoc, per-commit cherry-picks when explicitly requested. `BRANCHING.md` is the source of truth — see it for the full policy.

Key points:
- `main` — renga's mainline (default branch, release target). Evolves independently of upstream.
- `master` — snapshot mirror of `upstream/master`, FF-updated only when needed as a base for cherry-picks. Never carries renga-specific commits.
- Upstream changes enter renga only via cherry-pick of specific commits the user has named. No blanket sync.
- `.claude/skills/upstream-sync/` skill assists with those ad-hoc cherry-pick and reverse-PR procedures. It must NOT fire on generic "sync the fork" / "merge in upstream" requests — push back to the user instead.

**Do not propose or open reverse PRs against upstream unless the user explicitly asks for one.** It is fine to label individual features as "potentially generalizable upstream" on an umbrella issue, but do not queue "open an upstream PR" as a follow-up task on your own — renga's progress should not be gated on upstream's review cadence.

## Intentional `ccmux` References (post-rename)
Issue #102 renamed the project from `ccmux` to `renga`. The following residual references to `ccmux` are intentional and should NOT be swept:

- **Upstream attribution** — the project is a fork of upstream `Shin-sibainu/ccmux`. Mentions of upstream by its name in `BRANCHING.md` and `README*` are preserved.
- **Version-history comments in `Cargo.toml`** — pre-rename release notes describe past versions accurately; rewriting them would falsify history.
- **`.claude/` agent and skill files** — worker tooling, not user-facing product surface; outside the rename scope.
- **`.github/workflows/release.yml` historical mention** — none deliberately retained; if any remain they should be flagged.

## Workflow Rules
- **Every implementation must be reviewed by the evaluator agent** before reporting done. This is a Rust TUI app, so Playwright MCP is not available — the evaluator should perform static review (diff analysis, edge cases, logic correctness, key conflict checks, layout math consistency).
- **Run `cargo fmt --all` before committing.** CI's `rustfmt` job fails fast on unformatted code, so an unformatted commit costs an extra push-and-wait cycle. The repo ships a `.githooks/pre-commit` that enforces this; enable it with `git config core.hooksPath .githooks` once after cloning.
