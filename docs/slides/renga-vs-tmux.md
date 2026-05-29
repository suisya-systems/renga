---
marp: true
title: renga vs tmux — 導入すべき理由・インストール・最低限の操作
description: Rust 製 AI ネイティブ・ターミナルマルチプレクサ renga を tmux と比較する
author: renga
lang: ja
paginate: true
size: 16:9
theme: default
style: |
  section {
    font-family: "Noto Sans CJK JP", "Hiragino Kaku Gothic ProN", "Yu Gothic", "Meiryo", sans-serif;
    font-size: 26px;
    line-height: 1.45;
    background: #1f1d1a;
    color: #e8e4dd;
    padding: 56px 64px;
  }
  h1 { color: #e07b39; font-size: 50px; }
  h2 { color: #e07b39; border-bottom: 2px solid #5a544c; padding-bottom: 8px; }
  h3 { color: #d9b08c; }
  strong { color: #f0a866; }
  a { color: #6fb3d2; }
  code { background: #2c2925; color: #f0a866; padding: 1px 6px; border-radius: 4px; }
  table { font-size: 21px; border-collapse: collapse; width: 100%; }
  th { background: #2c2925; color: #e07b39; }
  th, td { border: 1px solid #5a544c; padding: 7px 11px; text-align: left; vertical-align: top; }
  blockquote { border-left: 4px solid #e07b39; color: #c9c3b8; padding-left: 16px; }
  section.lead { text-align: center; }
  section.lead h1 { font-size: 62px; }
  ul { margin-top: 4px; }
  li { margin: 3px 0; }
  footer { color: #8a847a; font-size: 14px; }
footer: 'renga v1.3.0 — AI-native terminal for multi-agent coding'
---

<!-- _class: lead -->
<!-- _paginate: false -->

# renga vs tmux

### 複数の AI エージェントを 1 つのターミナルで束ねる

導入すべき理由 / インストール方法 / 最低限覚える操作

<br>

`renga v1.3.0` — Rust 製 AI ネイティブ・ターミナルマルチプレクサ

---

## このスライドのゴール

3 つだけ持ち帰ってください。

1. **なぜ renga か** — tmux と何が違い、どちらをいつ使うべきか
2. **どう入れるか** — npm / バイナリ / ソースビルドの OS 別手順
3. **最低限の操作** — 起動・分割・タブ・マウス・日本語入力

> renga は **「ペイン自身が AI エージェントだと知っている」ターミナル**。
> 一方 tmux は実績豊富な汎用マルチプレクサ。両者は競合というより **守備範囲が違う**。

---

## renga とは何か

**Rust 製の単一バイナリ TUI ターミナルマルチプレクサ**。分割・タブ・フォーカスは
他の multiplexer と同じだが、内部では各ペインを **第一級の AI エージェント・エンドポイント** として扱う。

- 各ペインは独立シェル (PTY)。`vt100` クレートでフル端末エミュレーション (ANSI ストリップではない)
- **Claude Code が動くペインを自動検出**し、枠をオレンジ表示
- 同じタブの Claude Code / Codex が **`renga-peers` MCP チャネル**で相互メッセージング
- **単一バイナリ 約 8〜10 MB**、追加ランタイム不要。Windows / macOS / Linux 対応
- ペインごと **10,000 行**のスクロールバック、`cd` 追従 (OSC 7)、日本語 IME overlay

> 主眼は **複数の Claude Code / Codex を 1 ウィンドウで協調させる基盤**であること。
> エージェントを常に 1 つしか動かさないなら、renga の優位点は限られる。

---

## なぜ今これが問題になるのか

AI コーディングエージェントを **複数同時に走らせる**運用が当たり前になってきた。

- 「窓口」役が複数の「ワーカー」役にタスクを振り分ける
- サブエージェントを別ペインで並走させて結果を比較する
- 長時間セッションが軽い調べ物を別ペインへ投げる
- Claude と Codex をタブ内で役割分担させる

**従来の multiplexer での痛み:**

- ペイン間連携が **コピペ・手動 `send-keys`・外部 glue** 頼み
- どのペインがどのエージェントかは **人間の記憶**にしかない
- Claude のストリーミング出力で **日本語 IME 候補窓が踊る**

→ renga はこの「複数エージェント協調」を一級市民として設計している。

---

## 比較表 — tmux / zellij と renga

| 観点 | tmux / zellij | renga |
|---|---|---|
| ペインの抽象 | 汎用シェルセッション | **AI エージェント端点** (安定 id / role / focus) |
| ペイン間連携 | コピペ・手動 `send-keys`・外部 glue | 組み込み **`renga-peers` MCP** で相互送受信 |
| エージェント起動 | 起動コマンドを手で管理 | `spawn_claude_pane` / `spawn_codex_pane`・`Alt+P` |
| 日本語 IME | ホスト端末任せ。候補窓が踊りがち | 専用 **IME 合成 overlay** で候補窓を固定 |
| 設定の表面積 | シェル glue / プラグイン / keytable | 小さな単一バイナリ + レイアウト TOML |
| 配布 | OS パッケージ等 | **単一バイナリ 約 8〜10 MB** |

---

## renga の差別化ポイント

repo で裏取りした「tmux に無い・薄い」核心。

- **mixed-client peer メッセージング** — 同じ renga タブ内の Claude / Codex が
  `list_peers` / `send_message` / `check_messages` で直接連携。
  Claude は `<channel source="renga-peers">` で push 受信、Codex はペイン nudge → `check_messages`
- **ペイン制御 MCP ツール** — `spawn_claude_pane` / `spawn_codex_pane` /
  `set_pane_identity` / `new_tab` / `send_keys` / `inspect_pane` / `poll_events`
- **Claude Code 自動検出** — 該当ペインの枠がオレンジに
- **IME 合成 overlay** — freeze-on-overlay + 周期 catch-up で候補窓をキャレット直下に固定
- **peer スコープは renga タブ単位**に固定 → プロジェクト跨ぎの誤ルーティングが起きない

> 窓口役からワーカーを増やしてタスクを投げるのは **MCP コール 2 回**。シェルもコピペも不要。

---

## tmux の強み（公正に）

renga は **汎用 tmux 代替を目指していない**（README の non-goals に明記）。
次の領域では tmux が依然として強い。

- **セッション永続 (detach / attach)** — renga には無い。SSH が切れても生き続ける作業セッションは tmux の独擅場
- **SSH 越し運用** — リモートに常駐させ、ローカルから付け外しする定番ワークフロー
- **スクリプタビリティ** — 成熟した制御コマンド / フックによる自動化 API
- **巨大なプラグインエコシステム** — tpm, resurrect, continuum など
- **長年の実績**と幅広い OS / 環境カバレッジ

> renga にも `renga list / send / focus / split / new-tab` などの IPC サブコマンドはあるが、
> tmux の自動化 API ほど網羅的ではない。

---

## いつ renga / いつ tmux

| こういうときは renga | こういうときは tmux |
|---|---|
| 複数の Claude Code / Codex を**並走・協調**させたい | 1 つのシェルセッションを**永続**させたい |
| エージェント間メッセージングを**手中継なし**で回したい | **SSH 越し**に detach / attach したい |
| 日本語入力しながら AI と長文をやり取りする | シェルスクリプトで**多重自動化**したい |
| 役割 (窓口 / ワーカー) を持つ**オーケストレーション** | 既存の**プラグイン資産**を活かしたい |

> 両方使ってもよい。renga はターミナルエミュレータ自体を置き換えないので、
> **既存ターミナル + tmux + renga** の併用も成立する。

---

## インストール — 3 つの方法

| 方法 | 向いている人 | 一言 |
|---|---|---|
| **npm (おすすめ)** | 手早く入れたい全員 | `npm install -g @suisya-systems/renga` |
| **バイナリ直接DL** | Node を入れたくない | GitHub Releases + `checksums.txt` で検証 |
| **ソースからビルド** | Rust 開発者 / 改造したい | `cargo build --release` |

```bash
# おすすめ: npm 経由（Node が必要）
npm install -g @suisya-systems/renga
renga --version   # 最新リリースと照合
```

> アップデートは `npm update -g`。pinning でスキップされる時は `@latest` を明示。

---

## インストール — OS 別（バイナリ直接DL）

[GitHub Releases](https://github.com/suisya-systems/renga/releases) から v1.3.0 のバイナリを取得。

| OS | アセット名 |
|---|---|
| Windows x64 | `renga-windows-x64.exe` |
| macOS (Apple Silicon) | `renga-macos-arm64` |
| macOS (Intel) | `renga-macos-x64` |
| Linux x64 | `renga-linux-x64` |

```bash
# 改ざん検証（推奨）: checksums.txt と突き合わせる
sha256sum -c checksums.txt        # Linux
shasum -a 256 -c checksums.txt    # macOS

chmod +x renga-*                  # macOS / Linux は実行権限を付与
```

> **Windows:** 未署名のため SmartScreen が警告 →「詳細情報」→「実行」。

---

## インストール — ソースからビルド（cargo）

[Rust ツールチェイン](https://rustup.rs/) が必要。

```bash
git clone https://github.com/suisya-systems/renga.git
cd renga
cargo build --release
# 成果物: target/release/renga （Windows なら renga.exe）
```

PR を送るなら、クローン直後に 1 度だけ:

```bash
git config core.hooksPath .githooks   # fmt 漏れを手元で検出
```

> peer メッセージングを使うなら、起動後に MCP を登録:
> `renga mcp install --client claude` / `renga mcp install --client codex`

---

## 起動と主要フラグ

```bash
renga                 # カレントディレクトリで起動
renga ~/work/project  # 指定ディレクトリで起動
renga --help          # 全フラグ一覧
```

| フラグ | 既定値 | 用途 |
|---|---|---|
| `--min-pane-width <COLS>` | `20` | 縦分割後に各ペインが保つ最小桁数 |
| `--min-pane-height <ROWS>` | `5` | 横分割後に各ペインが保つ最小行数 |
| `--lang <auto\|ja\|en>` | `auto` | UI 言語（OS ロケール検出） |
| `--ime <hotkey\|off>` | `hotkey` | IME overlay のモード |
| `--ime-overlay-catchup-ms <MS>` | `3000` | overlay 凍結中の再描画間隔 |
| `--fps <FPS>` | `30` | イベントループの目標 rate |

> `--exec "<CMD>"` で初期ペインにコマンド自動実行、`--layout <NAME>` で複数ペイン構成を読込。

---

## 最低限のキーバインド（ペイン操作）

これだけ覚えれば動かせる。

| キー | 動作 |
|---|---|
| `Ctrl+D` | **縦**分割 |
| `Ctrl+E` | **横**分割 |
| `Ctrl+W` | ペインを閉じる（最後の 1 つならタブごと） |
| `Ctrl+Right` / `Ctrl+Left` | フォーカス移動（ペイン / サイドバー / プレビュー） |
| `Ctrl+F` | ファイルツリー表示切替 |
| `Ctrl+;` | IME 合成 overlay を開く（`Alt+;` / `Alt+I` がフォールバック） |
| `Alt+P` | フォーカス中ペインに peer 対応 `claude` 起動コマンドを入力 |
| `Ctrl+Q` | renga を終了 |

---

## 最低限の操作 — タブ

プロジェクトごとにワークスペースをタブで分ける。

| キー / 操作 | 動作 |
|---|---|
| `Alt+T` / `Ctrl+T` | 新しいタブ |
| `Alt+1` 〜 `Alt+9` | 指定番号のタブへ移動 |
| `Alt+Left` / `Alt+Right` | 前 / 次のタブ |
| `Alt+R` | タブ名を変更（セッション内のみ） |
| タブをクリック | タブ切替 |
| タブをダブルクリック | タブ名変更 |
| `+` をクリック | 新しいタブ |

> **macOS 注意:** 既定で `Option` が Unicode 入力に取られ `Alt+*` が届かない。
> 端末側で「Option をメタキー」に 1 行設定すれば解決（keymap.ja.md 参照）。

---

## 最低限の操作 — マウス（v1.3.0 の目玉）

クリックとダブルクリックだけで分割・フォーカスが完結。

| 操作 | 動作 |
|---|---|
| ペインをクリック | フォーカス移動 |
| **ペイン外周をダブルクリック** | クリックした側へ分割（上 / 左は手前、下 / 右は奥） |
| **共有境界をダブルクリック** | 隣接ペインを分割し、新ペインを境界上に配置 |
| 境界をドラッグ | パネルのリサイズ |
| ホイールスクロール | 履歴 / ツリー / プレビューをスクロール |

> ダブルクリック判定は **500 ms / アクティブタブ内**。角・接合セルは曖昧なので無視。
> `min-pane-width/height` やペイン上限に反する分割は拒否（v1.3.0 で追加 — #245〜#248）。

---

## 日本語入力 — IME 合成 overlay

Claude のストリーミング出力で候補窓が踊る問題への renga の回答。

- フォーカス中ペインで **`Ctrl+;`** → 画面中央に複数行の合成ボックスが開く
- ホスト端末の **IME 候補窓が合成ボックス内のキャレットに吸着**
- 裏のペインは **凍結 (freeze-on-overlay)** され、出力が候補窓を乱さない
- 完全凍結だと進捗が見えないので **周期 catch-up**（既定 3000 ms ごとに 1 フレーム再描画）

```toml
# config.toml
[ime]
mode = "hotkey"               # "hotkey" | "off"
freeze_panes_on_overlay = true
overlay_catchup_ms = 3000     # 0 で完全凍結
```

> `Ctrl+;` を端末に奪われる環境（WSL + Windows Terminal 等）は `Alt+;` / `Alt+I` で代替。

---

## エージェント連携の実例 — 窓口 + ワーカー

```
tab "project-X"
┌────────────────────┬────────────────────┐
│ secretary          │ worker-1           │
│ (claude, role=     │ (claude, role=     │
│  "secretary")      │  "worker")         │
│   send_message ───▶│  <channel ...> 受信 │
│ ◀── 返信 ──────────│                    │
└────────────────────┴────────────────────┘
```

- ワーカーは次ターンで `<channel source="renga-peers" …>` として受信し、
  **ユーザー入力ではなく peer メッセージ**だと判別して作業 → 同じ `send_message` で返信
- 安定 name 解決があるので、数値 id ではなく `"secretary"` / `"worker-1"` で指せる
- これが `claude-org` のオーケストレーション運用の実行基盤になっている

---

## まとめ

- **renga = AI ネイティブな multiplexer**。複数の Claude Code / Codex を
  1 ウィンドウで協調させる基盤。peer メッセージング・ペイン自動検出・IME overlay が核心
- **tmux = 実績ある汎用 multiplexer**。セッション永続・SSH 越し・スクリプタビリティ・
  プラグイン資産で依然強い。renga は代替を狙わず、守備範囲が違う
- **入れる:** `npm install -g @suisya-systems/renga`（または Releases バイナリ / ソースビルド）
- **覚える:** `Ctrl+D`/`Ctrl+E` 分割、`Alt+T`・`Alt+1..9` タブ、外周/境界ダブルクリック分割、
  `Ctrl+;` で日本語 IME、`Ctrl+Q` 終了

<br>

### 複数エージェントを走らせ始めたら、renga を試す価値がある。

> Docs: README.ja.md / docs/keymap.ja.md / docs/configuration.ja.md / docs/peer-messaging.ja.md
