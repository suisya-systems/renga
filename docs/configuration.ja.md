# 設定ファイル

*Language: [English](./configuration.md) / 日本語*

renga は起動時にオプションの TOML 設定ファイルを読み込みます。このページは **canonical な設定キーリファレンス** です。各キーのデフォルト値、型、CLI フラグとの対応、優先順位を一箇所にまとめています。

挙動の説明（IME overlay の使い心地、推奨設定、トラブルシュート）は [`ime.ja.md`](./ime.ja.md) に分けています。本ページは設定キーの表面そのものだけを扱います。

## 設定ファイルの場所

| OS | パス |
|---|---|
| Linux | `$XDG_CONFIG_HOME/renga/config.toml` (なければ `~/.config/renga/config.toml`) |
| macOS | `~/Library/Application Support/renga/config.toml` |
| Windows | `%APPDATA%\renga\config.toml` |

ファイルが無い、書式が壊れている場合は stderr に警告を出してデフォルト値で起動します。設定ミスで renga が立ち上がらなくなることはありません。未知のセクション・キーは前方互換のため無視します。

## 優先順位

CLI フラグが用意されている各キーの解決順は次の通りです:

**CLI フラグ > 設定ファイル > 組み込みデフォルト**

`[ui] lang` の `auto` だけは、設定ファイルと英語フォールバックの間に「OS ロケール検出」のステップが入ります（下表参照）。

## `[ime]` — IME 合成 overlay

```toml
[ime]
mode = "hotkey"               # "hotkey" | "off"
freeze_panes_on_overlay = true
overlay_catchup_ms = 3000
anchor_row_offset = 1
```

| キー | 型 | デフォルト | CLI フラグ | 説明 |
|---|---|---|---|---|
| `mode` | `"hotkey" \| "off"` | `"hotkey"` | `--ime <hotkey\|off>` | `hotkey` は `Ctrl+;` (フォールバックに `Alt+;` / `Alt+I` — `Ctrl+;` を奪うターミナル向け) で overlay を開く。`off` は `Ctrl+;` を黙って飲み込む。ホスト端末側で IME 候補窓の位置が既に望ましく出ている人向け。 |
| `freeze_panes_on_overlay` | bool | `true` | `--ime-freeze-panes[=BOOL]` | overlay を開いている間、裏のペインの再描画を止める。Claude のストリーミング出力が IME 候補窓を踊らせるのを防ぐ。overlay を開かないユーザー (IME を使わない人) には影響しない。`=false` で生の再描画を維持。 |
| `overlay_catchup_ms` | u64 ms | `3000` | `--ime-overlay-catchup-ms <MS>` | freeze が有効な間、指定ミリ秒ごとに 1 フレームだけ再描画を挟む。`0` で完全凍結 (catch-up なし)。非ゼロ値は最低 `100` に丸める。 |
| `anchor_row_offset` | u16 行 | `1` | `--ime-anchor-row-offset <ROWS>` | overlay を使わず**ネイティブ IME** で直接入力するときに、変換窓が入力行にめり込まないよう、端末キャレットをフォーカスペインのキャレット行から何行下に置くか。conpty ホスト (Windows ネイティブ / Windows Terminal 上の WSL) でのみ適用され、他の端末はこの値を無視して従来どおりキャレット行そのものに置く。表示キャレットもアンカーと一緒に下がるため、入力セル上に戻したい場合は `0` を指定する。`4` を超える値は丸められ、キャレットは常にペイン内に収まる。 |

> 以前は `Claude` ペインにフォーカスするたびに overlay が自動で開く `"always"` モードがありました。実運用で不安定だったため削除済み。フォーカス直後から overlay を使いたい場合は `Ctrl+;` を 1 回押してください。

## `[ui]` — UI 言語とイベントループ rate

```toml
[ui]
lang = "auto"   # "auto" | "ja" | "en"
fps = 30
```

| キー | 型 | デフォルト | CLI フラグ | 説明 |
|---|---|---|---|---|
| `lang` | `"auto" \| "ja" \| "en"` | `"auto"` | `--lang <auto\|ja\|en>` | ステータスバー hint とプレビューのエラーメッセージの言語。`auto` は `sys-locale` (Unix は `nl_langinfo`、Windows は `GetUserDefaultLocaleName`) で OS ロケールを検出。`ja` 系で日本語、それ以外は英語にフォールバック。CLI でも TOML でも大小文字を区別しない。 |
| `fps` | u16 | `30` | `--fps <FPS>` | メインイベントループの目標 rate。アイドル時の crossterm poll タイムアウトを決め、入力レイテンシとアニメーションの滑らかさを上げる代わりに wakeup を増やす。`0` は実行時に `1` に丸めるので、設定ミスでもビジーループにはならない。 |

優先順位:

- `lang` — CLI > 設定 > OS ロケール検出 > 英語フォールバック
- `fps` — CLI > 設定 > デフォルト（最後に `0`→`1` の clamp を適用）

## 関連ドキュメント

- [`ime.ja.md`](./ime.ja.md) — IME overlay の挙動、推奨上書き、トラブルシュート
- [`keymap.ja.md`](./keymap.ja.md) — フルキーバインド (IME overlay 内のキーマップを含む)
- [`api-surface-v1.0.md`](./api-surface-v1.0.md) §4 — 設定 / レイアウト / 環境変数の wire-frozen 部分集合 (英語のみ)
