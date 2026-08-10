# Claude Code と Codex ペイン間のメッセージング

*Language: [English](./peer-messaging.md) / 日本語*

同じ renga セッションに並べた Claude Code と Codex のペイン同士が、`renga-peers` MCP サーバ経由でメッセージを送り合えるようになります (Issue #289 以降はタブをまたいで届きます)。片方のエージェントに「これを調べておいて」と頼んだり、失敗したテストの原因追いを引き継いだりを、ユーザーが手で中継しなくても進められます。Claude は `<channel source="renga-peers">` タグで受け取り、Codex には renga が PTY 経由で `check_messages` を促す nudge を送り、実本文は MCP inbox から読みます。

本ページは **運用ワークフロー** を扱います — セットアップ、起動、2 ペイン例、トラブルシュート。**canonical な MCP ツール一覧、パラメータスキーマ、エラーコード、frozen-prefix 文字列**は [`api-surface-v1.0.md`](./api-surface-v1.0.md) §1 (英語のみ) にあります。本ページではそのコントラクトを再掲しません。

> **[`claude-peers-mcp`](https://github.com/happy-ryo/claude-peers-mcp) との違い** — 両者ともツール表面はほぼ同じですが、`claude-peers-mcp` は `cwd` / `git_root` / `PID` からスコープを推測 (ヒューリスティック、衝突しうる) します。renga-peers は **renga セッション** を権威スコープとして使う — ユーザーが文字通りこの renga インスタンスに置いたペイン群が、全タブ横断で対象です (`list_peers` は自分のタブを先頭に列挙、他タブ宛は数値 pane id で指定)。両者は同じ Claude install 内で共存できます (チャンネル名が衝突しない: `server:renga-peers` vs `server:claude-peers`)。

## セットアップ (1 回だけ)

```bash
renga mcp install --client claude
renga mcp install --client codex   # Codex peer も使う場合
```

いま走っている `renga` バイナリを `renga-peers` という名前で、選択した client のユーザー設定に登録します。同じ登録の重複実行は冪等で、renga アップグレード後に上書きしたいときは `--force` を付けてください。`renga mcp uninstall --client …` と `renga mcp status --client …` がそれぞれ逆操作・状態確認です。

Codex では、既定の install は client CLI の正規登録経路を尊重しつつ、peer messaging に必要な最小限の `env_vars` passthrough だけを補正します。`check_messages` と `send_message` も auto-approve 寄りにしたい場合は、明示的に opt-in してください:

```bash
renga mcp install --client codex --codex-auto-approve-peer-tools
```

このフラグは `send_keys` や pane 操作系のような、より強いツールまでは自動承認しません。

## peer チャネル付きで起動する

配送方式は client ごとに違います。

- **Claude Code** は MCP の experimental channel 機能を使うので、起動時に毎回 `--dangerously-load-development-channels server:renga-peers` が必要です。
- **Codex** は `renga mcp install --client codex` で入れた MCP 登録を使います。これが入っていれば plain `codex` 起動で足ります。非フォーカスの worker pane が PTY 入力を受けられる状態になったら renga が `check_messages` を促す nudge を送り、実際の peer 本文は `check_messages` で読みます。対象の Codex pane がフォーカス中なら、PTY 注入を即座にせずローカル通知 overlay を表示します。

Claude の起動フラグを毎回手で打たなくて済むように、renga 側から 2 つの経路を用意しています:

- **`Alt+P`** — フォーカス中のペインに `claude --dangerously-load-development-channels server:renga-peers ` を入力 (末尾にスペース、**Enter は押されない**)。そのまま Enter で起動してもいいし、追加引数を続けて書いてから Enter でも OK。シェルの種類を問わず動作します。
- **`renga split --role claude`** / **`renga new-tab --role claude`** — 新しいペインを開いて、上記フラグ付きの Claude Code を自動起動。`--command "..."` を明示したらそちらが優先されるので、カスタム起動の逃げ道は残ります。

Codex の登録が済んでいれば、orchestrator ペインは会話の中から `spawn_codex_pane(direction, …)` で Codex ワーカーを起動できます。

## 2 ペインでのやり取り

```
タブ A                         タブ B
┌──────────┬──────────┐        ┌──────────┐
│ claude-1 │ claude-2 │        │ claude-3 │
│          │          │        │          │
│  peers ──┼──▶ ✓     │        │    ▲     │
│  send ◀──┼── msg    │──id=3───────┘     │  ← 数値 id で届く (#289)
└──────────┴──────────┘        └──────────┘
```

Claude A の会話で:

```
> list_peers を呼んで
# → id=2 (同じタブの相方 — id でも名前でも指定可)
#    id=3 [tab 1] (別タブ — 数値 id で指定する)

> send_message を to_id=2, message="src/app.rs の handle_split を読んで要約して" で呼んで
```

Issue #289 以降、`list_peers` は全タブを列挙し (自分のタブが先頭)、`send_message` は宛先が**数値 pane id** ならタブをまたいで配送します。**名前**は今も自分のタブ内でしか解決されません (pane 名はタブ内でのみ一意) 。解決できない宛先は偽の `Delivered` を返さず `pane_not_found` エラーになります。

Claude B の次のターンのコンテキストに `<channel source="renga-peers">src/app.rs の handle_split を読んで…</channel>` タグで届き、Claude B は「ユーザーではなく相方からの依頼」と判別 (タグの `source` 属性が決め手) して要件を処理 → 同じ `send_message` で返信します。

安定 name 解決があるので、orchestrator は同じタブの peer なら数値 id を追いかけずに `"secretary"` / `"worker-1"` で指せます (名前はタブを越えて解決されません)。途中で名前を付け替えたい場合は `set_pane_identity` を使います。push される本文には `📡 PEER MESSAGE … NOT FROM USER` バナーが付くので、トランスクリプトを眺める運用者から見ても「`Human:` 風に表示されているターンが peer 由来か user 由来か」を一目で見分けられます。同一本文の数秒以内の連投はサーバ側で 1 通に畳まれるので、トランスクリプトに幻の重複ターンが現れません ([renga#221](https://github.com/suisya-systems/renga/issues/221))。

## ペイン操作を組み合わせる

ワーカーが対話プロンプトで止まった場合も、オーケストレータは会話の中で完結できます:

- `inspect_pane(target="worker-1", lines=20)` でワーカー自身に画面状態を語らせずにスナップショット
- `send_keys(target="worker-1", text="y", enter=true)` (もしくは `Esc`、矢印、`Ctrl+C` のような名前付きキー) でプロンプトに応答
- `poll_events` の cursor をターン間で持ち回ると、毎回タブ全体を `list_panes` し直さずに `pane_started` / `pane_exited` を追える

ペイン操作系ツール (`list_panes` / `spawn_pane` / `spawn_claude_pane` / `spawn_codex_pane` / `close_pane` / `focus_pane` / `new_tab` / `inspect_pane` / `send_keys` / `set_pane_identity` / `poll_events`) でオーケストレータが必要とする表面はほぼ揃います。各ツールのパラメータスキーマ、返り値の形、エラーコードの完全な一覧は [`api-surface-v1.0.md`](./api-surface-v1.0.md) §1 (英語) を参照してください。


> **タブスコープ。** `list_panes` / `spawn_pane` / `spawn_claude_pane` / `spawn_codex_pane` / `focus_pane` / `inspect_pane` / `send_keys` / `close_pane` / `set_pane_identity` の 9 つでいう「現在のタブ」は、**呼び出し元ペイン自身が属するタブ**であって、ユーザーがたまたま表示しているタブではありません。相対指定 (`target="focused"`、安定名) は自分のタブから出ず、数値のペイン id を明示した場合のみ他タブのペインに届きます。これは*アドレッシング*の話で、そこは変わっていません — Issue #329 で広がったのは*列挙*のほうで、`list_panes` にオプションの `tab` 引数が付きました (後述)。`tab` を省略すれば従来どおり自分のタブだけを返します。加えて `focus_pane` は解決先が表示中のタブに無い場合、ユーザーが**見ているタブ自体を切り替えます** — キーボードが届かない focus は focus ではないためです。`close_pane` にも別の鋭さがあります: 解決先がそのタブで唯一のペインで、かつ他にタブが残っている場合、renga は**そのタブごと閉じて** success を返します (拒否 `last_pane` になるのは唯一のタブの最後のペインだけです)。
>
> 9 つのうち 7 つの修正が Issue #288、残る `close_pane` と `set_pane_identity` が Issue #296 です。修正前は表示中のタブを対象にしていたため、バックグラウンドタブで動くオーケストレータの `send_keys` がユーザーの切り替え先タブに黙って入り込み、`close_pane(target="focused")` はユーザーが今まさに触っているペインを終了させていました。ツールが `[server_too_old] ... restart renga` を返す場合、ディスク上のバイナリは新しくても renga の**プロセス**が修正前のものです。renga を再起動してください。

> **`claude` 自動アップグレード。** `spawn_pane` / `new_tab` / `renga split` / `renga new-tab`、および layout TOML の `command = "claude"` 指定は、peer 対応の起動コマンドに自動で書き換えられます。各呼び出し側で `--dangerously-load-development-channels server:renga-peers` を覚えていなくても、新ペインが renga-peers ネットワークに参加します。orchestrator が Claude を起動したい場合は `spawn_pane(command="claude ...")` より `spawn_claude_pane` を推奨 — launch policy が renga 側に集約され、`args[]` に予約済みフラグが混入したら `invalid-params` で拒否されます。

> **ペインの `cwd`。** `spawn_pane` / `new_tab` / `renga split --cwd` / `renga new-tab --cwd` / layout TOML `cwd = "..."` で新ペインの作業ディレクトリを指定できます。絶対パスはそのまま、相対パスは呼び出し元ペインの cwd (MCP)、シェルの cwd (CLI)、renga プロセスの cwd (layout TOML) を基準に解決されます。無効なパス (存在しない、アクセスできない、ディレクトリでない) はレイアウト変更前に `cwd_invalid` で失敗するため、half-mutated なレイアウトになりません。`claude` 自動アップグレードは `command` の先頭の空白区切りトークンがちょうど `claude` のときだけ発火する (`claudex` / `claude-mobile` / `./claude` は意図的に対象外) ため、`cd <dir> && ...` を書くと効かなくなります。`cwd` フィールドで指定してください。

> **タブ配置 (`tab`、Issue #290)。** 3 つの `spawn_*` ツールはオプションの `tab` セレクタを受け付け、新ペインを**どのタブに**作るかを明示できます (上記の数値 id による暗黙の cross-tab とは別の、明示的な機構です)。キーはちょうど 1 つ: `{"name": "workers"}` (表示名の完全一致。0 件は `tab_not_found`、複数件は `tab_ambiguous` — タブ名は一意ではないため renga は推測しません)、`{"index": 2}` (0 始まり。`list_peers` が報告する index と同じ)、`{"pane_id": 17}` (そのペインが属するタブ。id はタブの close や rename でずれない安定アンカー)、`{"new": {}}` / `{"new": {"name": "workers"}}` (単一ペインの**バックグラウンド**新規タブを作成。ユーザーが見ているタブは切り替わらず、新規タブには split 対象が無いため `direction` / `target` の指定は拒否されます)。既存タブのセレクタでは `target` は選択したタブの**内側で**解決され、別タブの数値 target は `target_tab_mismatch` で失敗します。`tab.new` で `cwd` を省略すると呼び出し元ペインの cwd を継承します。`tab` を使うにはサーバが `spawn_tab` capability を広告している必要があり、古い renga プロセスに対しては呼び出し元のタブへ黙って spawn する代わりに `[server_too_old]` で fail closed します。`new_tab` は従来どおり「作成してフォーカス」のままです。またタブ数は MAX_TABS = 16 で上限され、超過は `tab_limit_reached` になります。

> **タブ横断の列挙 (`tab`、Issue #329)。** `list_panes` は読み取り側で同じ `tab` セレクタを受け付けます — `{"name": "workers"}` / `{"index": 2}` / `{"pane_id": 17}` (サーバ側の解決処理は spawn 側と共通で、`tab_not_found` / `tab_ambiguous` / `pane_not_found` の意味も同じ) に加えて、全タブを返す `{"all": true}` (自分のタブが先頭、以降は index 順)。`{"new": …}` は読み取りには意味がないため存在しません。`tab` を省略した場合は #329 以前の挙動がバイト単位でそのまま残り、自分のタブだけを返します。全タブ形は、オーケストレータが **id を保持していない**ペイン — バックグラウンドタブに置いた worker を含む — を列挙するための経路です。#329 以前はそうしたペインが監視母集団からも容量会計からも落ち、生存している worker が退役され、spawn が過剰方向にずれていました。レコードには `tab` (0 始まりの index) と `tab_name` (表示ラベル) が加わりますが、どちらも**表示用メタデータのみ**です (index はタブを閉じるとずれ、ラベルは一意ではありません)。さらに `same_tab` が付きますが、これは複数タブにまたがりうる応答 (ペインから `tab` セレクタ付きで呼んだ場合) にだけ含まれます。タブを跨いで安定するアドレスは今も数値 `id` だけです。独立した 2 つの orchestration が別タブで動くと、どちらにも `dispatcher` と `worker-<task_id>` が実在するため `name` では判別できません — 判別材料は `cwd` です。`tab` を使うにはサーバが `cross_tab_list` capability を広告している必要があります。#329 以前のプロセスは未知のフィールドを捨てて自分のタブだけを返し、それは正しい答えと見分けのつかない well-formed な `Ok` になってしまうため、黙って狭い集合を返す代わりに `[server_too_old]` で fail closed します。CLI の `renga list` には新しいレコードのフィールドが出ますが、CLI 側のタブセレクタは見送りです。

## うまく動かないとき

- **`list_peers` が "renga not reachable from this peer client" を返す** — client が renga の外で起動されたか、renga ペインの環境変数を引き継げていません。renga のペイン内から起動し直してください（Claude は `Alt+P` / `renga split --role claude`、Codex は `renga mcp install --client codex` 後の plain `codex` または `spawn_codex_pane`）。
- **相手に送ったメッセージが `<channel>` タグで表示されない** — 起動時のフラグ `--dangerously-load-development-channels server:renga-peers` を付け忘れています。`claude` と打つ代わりに `Alt+P` を使えばフラグ付きのコマンドが挿入されるので事故りにくくなります。
- **Codex に送ったのに反応がない** — renga は Codex ペインが PTY 入力を安全に受けられる状態で、かつ非フォーカスになってから `check_messages` を促す nudge を流し込みます。フォーカス中に届いたメッセージは、会話を汚さないように通知 overlay へ回します。`Alt+Enter` / `Ctrl+Enter` で `check_messages` を呼ぶための文面だけ挿入し、`Esc` なら無視、Enter を押して実行するかどうかは人間が決めます。overlay を放置してもリクエストは MCP inbox に残り、フォーカスを外せば worker と同じ deferred nudge に戻ります。実際の依頼本文は MCP inbox 側にあり、`check_messages` の返り値が真実です。
- **新しい Codex pane で `check_messages` / `send_message` の承認がまた出る** — Codex の承認は pane-local に振る舞うことがあります。`renga mcp install --client codex --codex-auto-approve-peer-tools` で安全な peer messaging 系の承認を事前設定できますが、Codex のバージョンや実行形態によっては、新しい pane で一度だけ warm-up 承認が必要です。
- **`spawn_codex_pane` が `[codex_not_installed]` で失敗する** — Codex の MCP 設定 (`~/.codex/config.toml`) に renga-peers エントリがない、ファイルが読めない、もしくは `[mcp_servers.renga-peers.env]` に `RENGA_PEER_CLIENT_KIND=codex` が登録されていません。`renga mcp install --client codex` を 1 回実行してください。env 値だけが欠けた既存エントリも install 経路で self-heal します。
- **`send_keys` が効いていないように見える** — `send_keys` は target ペインの PTY に生の入力バイトを書き込むだけで、帯域外の「承認」操作ではありません。まず `inspect_pane(target=…, lines=20)` で本当に入力待ちか確認し、レイアウトが動く運用ではフォーカス推測ではなく安定した pane `name` を target に使ってください。
- **`poll_events` が想定より早く `events: []` を返す** — `types=[…]` フィルタは返却結果を絞るだけで、非一致イベントでも long-poll は解除されて `next_since` は前進します。返ってきた cursor でそのまま再 poll してください。`events_dropped` が来た場合は全タブのビューで再同期してください。`poll_events` はプロセス全体が対象で**全タブ**のペインライフサイクルが届くので、素の `list_panes` (自分のタブだけ) では他タブで落ちたイベントを埋められません。母集団全体を取り直せるのは `list_peers` か `list_panes(tab={"all": true})` (Issue #329) です。`list_peers` は自分自身のペインを含まないため、自分のタブも込みで欲しいときは全タブ形の `list_panes` を使ってください。
- **renga をアップグレードしたら** — `renga mcp install --client claude --force` / `renga mcp install --client codex --force` を実行し直し、登録済みの各 client が新しいバイナリを指すようにしてください。

## 関連ドキュメント

- [`api-surface-v1.0.md`](./api-surface-v1.0.md) — MCP ツール / パラメータ / 返り値 / エラーコードの canonical wire-frozen リスト (英語のみ)
- [`keymap.ja.md`](./keymap.ja.md) — フルキーバインド (`Alt+P` peer-launch と file-tree の `c` / `v` 分割起動を含む)
- [`configuration.ja.md`](./configuration.ja.md) — TOML 設定キー (MCP / ペイン操作の表面とは分離)
