# IME 合成 overlay

*Language: [English](./ime.md) / 日本語*

renga には JP / CJK の IME 候補窓をキャレット直下に固定するための合成 overlay が組み込まれています。本ページは **overlay の使い心地** を扱います — 推奨設定、overlay 内のキーマップ、トラブルシュート。

**canonical な TOML キーと CLI フラグ**（`[ime] mode` / `freeze_panes_on_overlay` / `overlay_catchup_ms` / `anchor_row_offset` と対応する `--ime-*` フラグ、優先順位ルール）は [`configuration.ja.md`](./configuration.ja.md) に集約しています。本ページは設定スキーマを再掲しません。

![ターミナル中央に開いた IME overlay。日本語の変換候補窓がキャレット直下に表示され、背後の Claude ペインは凍結している](../ime-overlay.png)

## どんな体験になるか

1. **overlay は必要なときだけ開く。** Claude のペインで `Ctrl+;` を押すと、画面中央に複数行の入力ボックスが出ます。IME の候補窓が入力ボックス内のキャレットに吸着するので、長い日本語を変換している最中に候補窓が画面を跳ね回ることがなくなります ([Issue #25](https://github.com/suisya-systems/renga/issues/25))。
2. **背後のちらつきが止まる。** overlay が開いている間は裏のペインを凍結するため、Claude の Thinking スピナーや流れてくるトークンが再描画を起こしません。入力に集中できます。freeze が効くのは overlay を開いている間だけなので、overlay を開かないユーザーには既存と同じ挙動 — これが「デフォルト ON」を許容できる理由です。
3. **それでも進捗は見える。** 約 3 秒ごとに 1 フレームだけ凍結を解除するので、Claude の出力がどこまで進んでいるかは確認できます。`overlay_catchup_ms` ([`configuration.ja.md`](./configuration.ja.md) 参照) で間隔を調整できます。完全凍結なら `0`、3 秒でも落ち着かないなら `5000`。
4. **複数行のドラフトをそのまま書ける。** `Enter` は改行、送信は `Alt+Enter` (macOS は `Option+Return`)。WSL2 / Windows Terminal ではホストが `Alt+Enter` を *Toggle Fullscreen* に bind して飲み込んでしまうため、`Ctrl+Enter` を使ってください ([Issue #226](https://github.com/suisya-systems/renga/issues/226))。
5. **一旦閉じても下書きは残る。** `Esc` / `Ctrl+C` で overlay を閉じれば、ペインの様子を見たり renga のペイン操作キーを使ったりできます。同じペインで開き直せば、直前のドラフトがそのまま戻ります。

## Overlay 内のキーマップ

中央の合成ボックスの中で:

| キー | 動作 |
|-----|--------|
| `Enter` | 改行 (`Shift+Enter` でも同じ) |
| `Alt+Enter` | 送信して overlay を閉じる (主要ターミナルでのカノニカル commit、macOS では `Option+Return` — Option が効かない場合は [`keymap.ja.md`](./keymap.ja.md) の「macOS: Option をメタキーにする」を参照)。WSL2 / Windows Terminal ではホストが *Toggle Fullscreen* に bind しているため renga まで届きません。代わりに `Ctrl+Enter` を使ってください ([Issue #226](https://github.com/suisya-systems/renga/issues/226))。 |
| `Ctrl+Enter` | 送信 — Windows Terminal / wezterm / VS Code / 多くの Linux ターミナル向け代替 commit。WSL2 / Windows Terminal ではホストが `Alt+Enter` を奪うため、こちらが推奨。extended-key reporting が無効なホストが Ctrl+Enter を素の LF バイト (0x0A) として送ってきた場合の同義扱いとして `Ctrl+J` も commit として受け付けます。 |
| `Esc` / `Ctrl+C` | overlay を閉じる。同じペインで開き直すとドラフトを復元 |
| `←` `→` `↑` `↓` | カーソル移動 |
| `Home` / `End` | 現在行の先頭 / 末尾 |
| `Ctrl+Home` / `Ctrl+End` | バッファ全体の先頭 / 末尾 |
| `Backspace` | キャレット左の 1 文字を削除 |

overlay を **開く** ための chord set (`Ctrl+;` / `Alt+;` / `Alt+I`) は [`keymap.ja.md`](./keymap.ja.md) のペインモード表に記載しています。

## 推奨上書き

デフォルトは「IME を使う人だけが効果を感じる」設計です。よくある上書き:

```toml
# 合成中も生の再描画を見たい (freeze 無効)。
[ime]
freeze_panes_on_overlay = false
```

```toml
# 完全凍結 — 周期 catch-up なし。
[ime]
overlay_catchup_ms = 0
```

```toml
# overlay 自体を無効化。Ctrl+; を黙って飲み込むのでシェルには漏れない。
# ホスト端末側で IME 候補窓の位置が既に望ましいユーザー向け。
[ime]
mode = "off"
```

`--ime-*` CLI フラグは 1 回の起動だけ `config.toml` を上書きします。完全な優先順位ルールは [`configuration.ja.md`](./configuration.ja.md#優先順位) を参照。

## overlay を使わない場合（ネイティブ IME のアンカー）

overlay を開かず、ペインへ直接 OS の IME で入力することもできます。その場合も renga は端末キャレットを入力位置に置くことでホスト側 IME をそこへアンカーします（[Issue #34](https://github.com/suisya-systems/renga/issues/34)）。

Windows Terminal は未確定文字列をそのキャレット位置に描画し、変換窓を同じ行の直下にぴったり付けて開くため、変換窓が Claude の入力行に食い込んでいました（[Issue #281](https://github.com/suisya-systems/renga/issues/281)）。現在はアンカーを 1 行下げ、変換窓と未確定文字列が入力行を避けるようにしています。

```toml
# ネイティブ IME のアンカーをキャレットのセルそのものに戻す（#281 以前の挙動）。
# 変換窓が入力行を避けることより、表示キャレットが入力セル上にあることを
# 優先したい場合はこちら。
[ime]
anchor_row_offset = 0
```

トレードオフとして、端末のカーソルは 1 つしかないため **表示キャレットもアンカーと一緒に下がります**。そのため、この offset は Windows の IME が実際に端末キャレットへ吸着するホスト — Windows ネイティブ、および Windows Terminal 上の WSL（Windows Terminal が `WSLENV` 経由で渡す `WT_SESSION` / `WT_PROFILE_ID` で判定）— でのみ適用されます。WSL への SSH、WSLg 上の Linux 端末、ネイティブの Linux / macOS 端末では完全に無視され、キャレットは従来どおりの位置に留まります。

## うまく動かないとき

- **`Ctrl+;` が反応しない。** 一部のターミナルは `Ctrl+;` を renga が見る前に奪います (WSL + Windows Terminal、Linux 上の VS Code ターミナル、一部の tmux 設定など)。フォールバックの `Alt+;` か `Alt+I` を使ってください。macOS では Option をメタキー化が必要なことがあります — [`keymap.ja.md`](./keymap.ja.md#macos-option-をメタキーにする) を参照。
- **候補窓がやはり踊る。** overlay を開く前に、入力したいペインに **フォーカス済み** か確認してください。overlay はフォーカス中ペインのキャレット位置に吸着します。途中でペインを切り替えた場合は `Esc` で閉じてから新ペインで開き直してください。
- **WSL2 で Alt+Enter がフルスクリーン切替になる。** `Ctrl+Enter` を使ってください ([Issue #226](https://github.com/suisya-systems/renga/issues/226))。ホストが `Ctrl+Enter` も奪う環境では `Ctrl+J` も commit として受け付けます。
- **入力中にストリーミング出力が止まって見える。** freeze-on-overlay の意図通りの挙動です。進捗を多めに見たいなら `overlay_catchup_ms` を `1500` 等に下げる、もしくは `freeze_panes_on_overlay = false` で freeze 自体を無効化してください。

## 関連ドキュメント

- [`configuration.ja.md`](./configuration.ja.md) — canonical な TOML キー / CLI フラグ
- [`keymap.ja.md`](./keymap.ja.md) — フルキーバインド (`Ctrl+;` / `Alt+;` / `Alt+I` の open chord と macOS Option as Meta を含む)
