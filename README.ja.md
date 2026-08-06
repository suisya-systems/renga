# renga

*Language: [English](./README.md) / 日本語*

**複数の [Claude Code](https://docs.anthropic.com/en/docs/claude-code) と Codex エージェントを 1 つの TUI でオーケストレートするための AI ネイティブ・ターミナル基盤。mixed-client peer メッセージング、必要な部分だけ Claude 特化 UX、単一バイナリ。**

![renga スクリーンショット](screenshot.png)

## renga とは何か

renga は「ペイン自身が AI エージェントであることを知っている」ターミナルです。分割・タブ・フォーカスといった操作は他の TUI multiplexer と同じですが、内部ではそれぞれのペインを **第一級のエージェント・エンドポイント** として扱います。Claude Code が動いているペインは自動検出し、Codex ペインも同じ peer network に参加できるようにし、`spawn_claude_pane` / `spawn_codex_pane` / `set_pane_identity` / `new_tab` などのペイン制御 MCP ツールを提供します。peer のスコープは renga タブ単位 — ユーザーが文字通り同じタブに並べたペイン群 — で固定されており、cross-pane ルーティングがプロジェクトをまたいで衝突することはありません。

主なユースケースは **エージェントのオーケストレーション** — 「窓口（secretary）」ペインがタスクを「ワーカー」ペインに振り分ける構成、サブエージェントを別ペインで並走比較する構成、長時間セッションが軽い調べ物だけ別ペインに投げる構成、Claude と Codex を同じタブで役割分担させる構成、などです。エージェントを常に 1 つしか起動しないなら、renga が今のターミナルより優位な点は限られます。複数同時に動かすなら、peer チャネルと AI 認識ペインモデルがそのまま価値になります。

### 単体利用とスタック利用

renga には 2 つの正しい使い方があります。

- **単体利用** — Claude Code / Codex の複数ペインをローカルで協調させる AI ネイティブなターミナルとしてそのまま使う
- **[`claude-org`](https://github.com/suisya-systems/claude-org) の Layer 3 として使う** — Lead / Dispatcher / Curator / Worker の役割分担、タスクごとの working directory 境界、狭い権限コントラクト、知識のキュレーション、組織全体の suspend / resume を持つ上位運用モデルの下で、実行基盤として使う

考え方としては **renga が execution fabric、claude-org がその上の reference operating system** です。

### tmux / zellij との位置づけ

| | tmux / zellij | renga |
|---|---|---|
| ペインの抽象 | 汎用シェルセッション | **AI エージェント・エンドポイント**（安定 id / role / フォーカスフラグ付き） |
| ペイン間メッセージング | コピペ、手動 `send-keys`、外部 glue | 組み込み MCP `renga-peers` network。Claude は channel push、Codex は pane nudge を受けて `check_messages` で本文を読む |
| エージェントペイン起動 | ユーザーがクライアントごとの起動コマンドやフラグを手で管理 | `spawn_claude_pane` / `spawn_codex_pane` MCP ツール、Claude 用 `Alt+P` |
| IME / 日本語入力 | ホストターミナル任せ。Claude のストリーミング出力で候補窓が踊りがち | 専用 IME 合成 overlay。freeze-on-overlay + 周期 catch-up でキャレット直下に候補窓を固定 |
| 設定の表面積 | シェル glue / プラグイン / keytable | 小さな TUI バイナリ 1 本。レイアウト TOML でペイン構成と role を直接宣言 |

**やらないこと（non-goals）。** renga は **汎用 tmux 代替を目指していません**。tmux のセッション永続化、ネスト server モデル、プラグインエコシステム、スクリプタブルな自動化 API を網羅する意図はありません。ターミナルエミュレータでもありません（自前のフォント・グリフ描画は持たず、既存ターミナル上で動きます）。IDE プラグインでもチャット UI でもありません。狙いは狭く一点で、**「複数の Claude Code エージェントが 1 つのウィンドウで協調する」基盤として最良であること**、そして約 10 MB の単一バイナリで配布できるサイズに収めることです。

### 例: 窓口（secretary）+ ワーカー型オーケストレーション

[`claude-org`](https://github.com/suisya-systems/claude-org) / [`claude-org-ja`](https://github.com/suisya-systems/claude-org-ja) で実際に使われているレイアウト。窓口役の "secretary" ペインが renga-peers 経由でワーカーペインにタスクを振り分けます。

```
tab "project-X"
┌────────────────────┬────────────────────┐
│ secretary          │ worker-1           │
│ (claude, role=     │ (claude, role=     │
│  "secretary")      │  "worker")         │
│                    │                    │
│  send_message ────▶│  <channel ...> で  │
│   to_id="worker-1" │  受信              │
│                    │                    │
│◀── 返信 ───────────│                    │
└────────────────────┴────────────────────┘
```

secretary のチャットからワーカーを増やしてタスクを投げるのは MCP コール 2 回で完結します。シェルもコピペも不要です。ワーカーは次のターンで `<channel source="renga-peers" …>` タグとしてリクエストを受け取り、ユーザー入力ではなく peer メッセージだと判別（タグの `source` 属性が決め手）して作業し、同じ `send_message` で secretary に返信します。安定 name 解決があるので、呼び出し側は数値 id を追わずに `"secretary"` / `"worker-1"` で peer を指せます。

peer メッセージングの完全なワークフロー、2 ペイン例、トラブルシュート、ペイン操作ツール (`inspect_pane`, `send_keys`, `poll_events`, `set_pane_identity` など) は [`docs/peer-messaging.ja.md`](./docs/peer-messaging.ja.md) を参照してください。

## できること

- **ペイン分割** — 縦横に分割、各ペインは独立したシェル (PTY)
- **タブ** — プロジェクトごとに独立したワークスペースをタブで切替
- **mixed-client peer メッセージング** — 同じタブの Claude Code と Codex が `renga-peers` で協調。Claude は channel push、Codex は renga がペイン経由で配送。[`docs/peer-messaging.ja.md`](./docs/peer-messaging.ja.md) 参照
- **ファイルツリー** — アイコン付きサイドバー、展開/折りたたみ可能
- **プレビュー** — シンタックスハイライト付き、画像ファイルも表示
- **Claude Code 自動検出** — Claude Code が動いているペインは枠がオレンジになる
- **`cd` 追従** — ディレクトリ移動でファイルツリーとタブ名も自動切替
- **JP / CJK IME overlay** — 中央の合成ボックスと freeze-on-overlay + 周期 catch-up でキャレット直下に候補窓を固定。[`docs/ime.ja.md`](./docs/ime.ja.md) 参照
- **マウス操作** — クリックでフォーカス、ペイン外周や兄弟ペイン間の共有境界をダブルクリックで分割、境界ドラッグでリサイズ、ホイールで履歴スクロール
- **10,000 行のスクロールバック** (ペインごと)
- **ダークテーマ** (Claude 風カラースキーム)
- **Windows / macOS / Linux 対応**、単一バイナリ (約 8〜10 MB、追加ランタイム不要)

## インストール

### npm (おすすめ)

```bash
npm install -g @suisya-systems/renga
```

`npm update -g @suisya-systems/renga` でアップデート、キャッシュ pinning でスキップされる場合は `npm install -g @suisya-systems/renga@latest` で `@latest` を強制取得してください。`renga --version` で確認、[最新リリース](https://github.com/suisya-systems/renga/releases/latest) と照合できます。

> 旧 `ccmux-fork` を入れている場合は: `npm uninstall -g ccmux-fork && npm install -g @suisya-systems/renga`。上流 `ccmux-cli` も同じパターンです。

### バイナリを直接ダウンロード

[Releases](https://github.com/suisya-systems/renga/releases) から取得: `renga-windows-x64.exe` / `renga-macos-arm64` / `renga-macos-x64` / `renga-linux-x64`。

> **Windows:** コード署名していないため Microsoft Defender SmartScreen が警告を出すことがあります。「詳細情報」→「実行」で開いてください。
>
> **macOS / Linux:** ダウンロード後に `chmod +x renga-*` で実行権限を付けてください。

### ソースからビルド

```bash
git clone https://github.com/suisya-systems/renga.git
cd renga
cargo build --release
# 出来上がり: target/release/renga (Windows なら renga.exe)
```

[Rust](https://rustup.rs/) のツールチェインが必要です。PR を送る場合はクローン後に一度だけ `git config core.hooksPath .githooks` を実行しておくと、`cargo fmt --all -- --check` の整形漏れが CI ではなく手元で落ちます。

> **macOS ユーザーへ:** 既定の macOS ターミナルは `Option+<キー>` を奪うため、renga の `Alt+T` / `Alt+P` / `Alt+1..9` / `Alt+Left/Right` がそのままでは発火しません。1 行の設定で解決できます → [docs/keymap.ja.md → macOS: Option をメタキーにする](./docs/keymap.ja.md#macos-option-をメタキーにする) (WezTerm / iTerm2 / Alacritty / Ghostty / Kitty / Terminal.app 別に記載)。

## 使い方

```bash
renga
```

好きなディレクトリで起動してください。ファイルツリーにはそのディレクトリが表示されます。よく使うフラグは `--ime-freeze-panes` / `--ime-overlay-catchup-ms` / `--lang`、加えて分割サイズ制御の `--min-pane-width` / `--min-pane-height` です。完全な一覧は `renga --help`、canonical な TOML スキーマと CLI vs config の優先順位は [`docs/configuration.ja.md`](./docs/configuration.ja.md) に集約しています。

## Claude Code と Codex ペイン間のメッセージング

mixed-client peer メッセージングは renga の中心的な差別化ポイントです: 同じタブの Claude Code / Codex が `list_peers` / `send_message` / `check_messages` を呼び合って、調査の委譲、失敗の引き継ぎ、協調作業を、ユーザーが手で中継せずに進められます。Claude は `<channel source="renga-peers">` タグで push 受信、Codex は renga からペイン nudge を受けて `check_messages` で実本文を読みます。peer スコープは renga タブ単位 (権威的に、`cwd` / `PID` ヒューリスティックは無し) で、同じ Claude install 内で [`claude-peers-mcp`](https://github.com/happy-ryo/claude-peers-mcp) と共存できます — チャンネル名が衝突しません。

最短で試す手順:

```bash
renga mcp install --client claude
renga mcp install --client codex   # Codex peer も使う場合
# その後、renga のペイン内で Alt+P から Claude を起動、または plain `codex` で Codex を起動
```

完全なセットアップ、2 ペイン例、ペイン操作ツール (`inspect_pane`, `send_keys`, `poll_events`, `set_pane_identity`, `spawn_claude_pane`, `spawn_codex_pane` など)、トラブルシュートは [`docs/peer-messaging.ja.md`](./docs/peer-messaging.ja.md) を参照。canonical な MCP ツール表面 (パラメータ・返り値・エラーコード) は [`docs/api-surface-v1.0.md`](./docs/api-surface-v1.0.md) (英語のみ)。

## IME 合成 overlay

Claude のペインにフォーカスした状態で `Ctrl+;` を押すと、画面中央に複数行の合成ボックスが開きます。ホストターミナルの IME 候補窓は合成ボックス内のキャレットに吸着し、裏のペインは凍結 (周期 catch-up あり) されるので、ストリーミング出力が候補窓を踊らせることがありません。挙動ノブ (`freeze_panes_on_overlay`, `overlay_catchup_ms`)、overlay 内のキーマップ、プラットフォーム固有の癖 (WSL2 の `Alt+Enter` vs `Ctrl+Enter`、macOS の Option as Meta) は [`docs/ime.ja.md`](./docs/ime.ja.md) を参照。

## キーバインド チートシート

最初に覚えるキーだけを置いています。フル表 (ペイン / ファイルツリー / プレビュー / org サイドバー / マウス) と macOS Option-as-Meta 設定は [`docs/keymap.ja.md`](./docs/keymap.ja.md) を参照。

| キー | 動作 |
|-----|--------|
| `Ctrl+D` / `Ctrl+E` | 縦 / 横分割 |
| ペイン外周をダブルクリック | クリックした側へ分割 (上 / 左はクリック側に新ペイン、下 / 右は奥側に配置) |
| 共有境界をダブルクリック | 隣接ペインを分割し、2 つの兄弟ペインの境界上に新ペインを配置 (ドラッグは従来どおりリサイズ) |
| `Ctrl+Right` / `Ctrl+Left` | フォーカス巡回 (ペイン / サイドバー / プレビュー) |
| `Alt+T` / `Alt+1..9` | 新しいタブ / 番号 N のタブへ移動 |
| `Alt+P` | フォーカス中のペインに peer 対応 `claude …` 起動コマンドを入力 |
| `Ctrl+F` | ファイルツリー表示切替 |
| `Ctrl+B` | org サイドバー表示切替 — 全タブ / 全ペイン / Claude 稼働状態を 1 枚で表示 |
| `Ctrl+;` | IME 合成 overlay を開く (`Alt+;` / `Alt+I` がフォールバック) |
| `Ctrl+Q` | 終了 |

## ドキュメント

- [`docs/peer-messaging.ja.md`](./docs/peer-messaging.ja.md) — `renga-peers` MCP チャネルのセットアップ・ワークフロー・トラブルシュート
- [`docs/ime.ja.md`](./docs/ime.ja.md) — IME overlay の挙動、推奨上書き、overlay 内キーマップ
- [`docs/configuration.ja.md`](./docs/configuration.ja.md) — canonical な TOML スキーマ (`[ime]`, `[ui]`)、CLI フラグ、優先順位
- [`docs/keymap.ja.md`](./docs/keymap.ja.md) — フルキーバインド (ペイン / ファイルツリー / プレビュー / org サイドバー / マウス) と macOS Option as Meta
- [`docs/api-surface-v1.0.md`](./docs/api-surface-v1.0.md) — v1.0 wire-frozen コントラクト: MCP ツール / CLI / IPC / 設定・レイアウト・環境変数 (英語のみ)
- [`docs/semver-policy-2.0.md`](./docs/semver-policy-2.0.md) — **現行**の breaking / additive 変更ルール (2.0 系以降、英語のみ)
- [`docs/semver-policy.md`](./docs/semver-policy.md) — v1.0 freeze 前後の breaking / additive 変更ルール (英語のみ)。上記に置き換えられたが 1.0〜1.4 系の記録として保持
- [`BRANCHING.md`](./BRANCHING.md) — renga / 上流 ccmux の divergence と cherry-pick ポリシー (英語のみ)

## 構成

```
src/
├── main.rs       # エントリポイント、イベントループ、panic フック
├── app.rs        # ワークスペース / タブ / レイアウト、キー & マウス処理
├── pane.rs       # PTY 管理、vt100 エミュレーション、シェル検出
├── ui.rs         # ratatui 描画、テーマ、レイアウト
├── filetree.rs   # ファイルツリーのスキャンとナビゲーション
└── preview.rs    # プレビュー (シンタックスハイライト + 画像)
```

主な設計判断: ターミナルエミュレーションには `vt100` クレートを使用 (ANSI ストリップではない) — Claude Code のインタラクティブ UI のために必要。分割レイアウトは比率を持たせた二分木で再帰的に表現。PTY ごとに reader スレッドを立て、mpsc チャネルで main ループに流す。`cd` 追従は OSC 7。描画は dirty フラグ方式でアイドル時 CPU を最小化。

## 安定性

renga は v1.0 API freeze に近づいています。v1.0 が安定維持を約束するコントラクト (MCP ツール / CLI / IPC / 設定・レイアウト・環境変数) は [`docs/api-surface-v1.0.md`](./docs/api-surface-v1.0.md) に定義しています。breaking / additive 変更を支配する現行の semver ルールは [`docs/semver-policy-2.0.md`](./docs/semver-policy-2.0.md) (置き換えられた v1.0 系ポリシーは [`docs/semver-policy.md`](./docs/semver-policy.md) に保存)。pre-1.0 (`0.y.z`) リリースは未約束です。freeze 保証が必要な下流ツールは `>= 1.0` に pin してください。

## 技術スタック

- [ratatui](https://ratatui.rs/) + [crossterm](https://github.com/crossterm-rs/crossterm) — TUI フレームワーク
- [portable-pty](https://github.com/nickelc/portable-pty) — PTY 抽象化 (Windows では ConPTY)
- [vt100](https://crates.io/crates/vt100) — ターミナルエミュレーション
- [syntect](https://github.com/trishume/syntect) — シンタックスハイライト

## Claude Code の参考情報

Claude Code が初めてなら [Claude Code Academy](https://claude-code-academy.dev) のチュートリアルが参考になります。

## 経緯と謝辞

renga は 2026 年初頭に [Shin-sibainu/ccmux](https://github.com/Shin-sibainu/ccmux) から派生して始まり、その後は独立して進化してきました。peer メッセージング用の MCP チャネル、Claude 認識付きのペイン枠表示、IME 合成 overlay、layout TOML、日英バイリンガル UX といった機能はすべて renga 独自の実装です。両プロジェクトはもはやバージョンごとに追従する関係ではなく、renga は独自の semver 系列を独自のペースでリリースしています (詳細は [`BRANCHING.md`](./BRANCHING.md) の divergence policy を参照)。

ratatui ベースのペインツリー、vt100 を使ったターミナルエミュレーション、クロスプラットフォーム PTY レイヤーといった出発点を提供してくれた [Shin-sibainu](https://github.com/Shin-sibainu) 氏と上流 ccmux の作者陣に感謝します。上流由来のコミット履歴はリポジトリの git log にそのまま保存されており、`Shin-sibainu` の MIT 著作権表示はライセンス義務として [`LICENSE`](./LICENSE) に保持しています。

## ライセンス

MIT — [`LICENSE`](./LICENSE) を参照。上流 `Shin-sibainu/ccmux` の著作権表示はライセンス条項に従って保持されており、renga の追加実装も同じ MIT 条件で公開しています。
