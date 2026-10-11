# Branching & Divergence Policy

renga (`suisya-systems/renga`) は [`Shin-sibainu/ccmux`](https://github.com/Shin-sibainu/ccmux) をもとに始まりましたが、現在は **独立した本流 (`main`) として開発しています**。
ミラーブランチの定期同期 (毎週の upstream sync など) は行いません。バージョン番号も上流と揃えません。上流に有用なバグ修正や小さな改善があるときだけ、その commit を個別に `main` へ cherry-pick します。上流の MIT 著作権表示は `LICENSE` に残しています。

## ブランチ構成

| ブランチ | 役割 | push 権限 |
|---|---|---|
| `main` | **renga の本流** (default branch、リリース対象) | PR のみ。force-push 禁止 |
| `master` | **上流ミラー (任意保守)**。`upstream/master` を必要なときだけ FF 追従するスナップショット用 | force-push 許可 (上流が rebase する場合に備え) |
| `feat/*`, `fix/*`, `chore/*` | 通常の機能ブランチ。`main` から切る | PR で `main` にマージ |
| `upstream-pr/*` | 上流に還元したい変更があれば `master` から切る (基本は使わない) | `Shin-sibainu/ccmux` へ PR |

### `master` を残しておく理由

- 過去の上流 commit と現在の renga の差分を git で直接比較したいときに便利
- 上流から個別に cherry-pick する場合のベースになる
- アクティブに追従しているわけではないので、`master` 自体には機能を足さない

## Remote

```bash
git remote add upstream https://github.com/Shin-sibainu/ccmux.git
git fetch upstream
```

`upstream` remote の追加は任意です。renga の通常開発には不要で、上流の変更を覗きたい / cherry-pick したいときにだけ使います。

## 日常運用

### 通常の機能開発

```bash
git checkout main
git pull
git checkout -b feat/xxx
# ... 実装 ...
gh pr create --base main
```

これが基本フロー。renga の開発は `main` 中心で完結します。

### 上流から cherry-pick する (任意・必要時のみ)

定期的な「上流 sync」は行いません。renga と upstream ccmux はもう独立した実装系列です。
ただし、上流に明らかに有用なバグ修正や小さな改善があれば、その単発 commit を選んで取り込むことはあります。

```bash
git fetch upstream

# 1. master を upstream/master に FF 追従 (記録用スナップショット)
git checkout master
git merge --ff-only upstream/master
git push origin master

# 2. 取り込みたい commit を main に cherry-pick
git checkout main
git pull
git checkout -b chore/cherry-pick-<topic>
git cherry-pick <upstream commit>
gh pr create --base main --title "chore: cherry-pick <topic> from upstream ccmux"
```

renga 側の実装と衝突することが多いので、コンフリクトが大きい場合は cherry-pick せず renga 流で書き直すのが基本方針です。

### 上流に PR を返す (基本ユーザーから明示指示があった場合のみ)

renga の独自実装から汎用的に切り出せるものを上流に還元したいケースは稀ですが、出す場合は `master` を base にした別ブランチで送ります。
`main` から直接 PR すると無関係の renga 独自変更が混入するので厳禁。

```bash
git fetch upstream
git checkout master
git merge --ff-only upstream/master
git checkout -b upstream-pr/foo
git cherry-pick <main の commit>
git push origin upstream-pr/foo
gh pr create --repo Shin-sibainu/ccmux --base master
```

ユーザーから明示の指示がない限り、上流 PR は提案・実行しません — 上流の受け入れタイミングに renga の進捗が縛られないようにするための方針です。

## リリース・npm パッケージ

- **Git tag**: 通常の semver (`vX.Y.Z`)。renga は ccmux のバージョン番号と同期せず、独自に進めます。
- **Prerelease が必要な場合** (大規模変更の先行公開など): `vX.Y.Z-rc.N` / `-beta.N` 等を使用。`contains(github.ref_name, '-')` で workflow が自動的に prerelease 扱い + npm dist-tag `next` に振り分け (`.github/workflows/release.yml`)。
- **npm パッケージ名**: `@suisya-systems/renga` (scoped)。元々 `ccmux-fork` で publish しており、Issue #102 / PR #152・#153 の流れで `@suisya-systems/renga` に rename しました (PR #152 で一旦 `renga-fork` に書き換えたが publish 前に PR #153 で scope 付き名に再変更したため、npm 上に存在する旧名は `ccmux-fork` のみ)。`renga` (unscoped) は他者が先行 publish 済みのため scope 付きで確保しています。
- 過去の `v0.5.7-fork.1〜3` では `-fork.N` 接尾辞で全リリースを prerelease 扱いしていましたが、フォーク識別子はパッケージ名とリポジトリ名で既に確保されており suffix は誤った "pre" 信号にしかならなかったため v0.5.7-fork.3 以降廃止しました。

## ブランチ保護

GitHub 側で以下を設定:

- `main`: PR 必須 / CI 必須 / force-push 禁止 / 直 push 禁止
- `master`: 管理者のみ push 可 (上流スナップショット専用)

## 関連

- リリース手順の詳細は `CLAUDE.md` の Release Process を参照
- (内部) `.claude/skills/upstream-sync/` Skill は本ドキュメントの divergence policy に追随しており、ad-hoc な cherry-pick / 逆 PR 手順の補助に用途を絞っています。本 BRANCHING.md が常に正本です
