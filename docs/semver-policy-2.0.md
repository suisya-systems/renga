# renga semver policy (v2.0+)

> **Status**: adopted. This is the live policy for the 2.0 line and onward.
>
> **Supersedes**: [`semver-policy.md`](./semver-policy.md) — the v1.0-line
> policy, preserved at its final state. Everything in this document that is
> not marked as a change from it is inherited unchanged, with one systematic
> exception: capability tokens (§7) are threaded through §1, §3 and §5 as a
> new frozen-surface item class, §3 gains a "newly imposed limit" predicate,
> and §3's semantic-change examples were replaced by the 2.0 ledger in §8.
>
> **Companion doc**: [`api-surface-v1.0.md`](./api-surface-v1.0.md) — defines
> the surface this policy promises to keep stable. See §1.1 on the filename.

renga follows [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html).
This document specifies *what* counts as the public API for the purposes of
that spec, *how* breaking changes are deprecated, and *when* the version
components bump.

Sections §1–§6 keep the numbering and the substance of the 1.0 policy, so that
existing citations — including the live doc-comment at `src/ipc/mod.rs` that
cites "`docs/semver-policy.md` §3" — continue to resolve to the same rule.
Sections §7 onward are new in the 2.0 line.

## 1. What is the public API

The renga public API is exactly the set of items listed in the companion
surface doc at stability **stable** or **stable-stub**. These are the four
frozen surfaces:

1. **MCP tools** (`renga-peers` server) — tool names, input schemas, result
   shapes, error code tokens, and the documented ok-text fallback prefixes.
2. **CLI** (`renga` binary) — top-level flags, subcommand names, subcommand
   flags, and the env vars listed in §2.3 of the surface doc.
3. **IPC protocol** — endpoint naming, handshake, request/response/event
   envelopes and variants, error code catalog, and the capability tokens
   advertised in the `hello` response (§7).
4. **Config and layout files** — `config.toml` schema and layout TOML
   `version = 1` schema.

Items marked **deferred** in the surface doc are explicitly *not* part of the
public API. They may change in any minor release. This includes Rust-level
`peer_*` IPC variant naming, the `double_option` serde helper, and anything
else listed under the surface doc's out-of-scope section.

> **Changed from 1.0.** The 1.0 policy also listed the `spawn_codex_pane`
> env-detection behavior as deferred. That is no longer true: #203 / #220 gave
> it a documented contract and a `codex_not_installed` error code, and the
> surface doc's out-of-scope section no longer lists it. The 1.0 policy text
> was never updated to match; this document drops the stale item.

Internal Rust APIs (anything under `src/` that is not on a frozen surface),
test fixtures, build scripts, and documentation prose are likewise not part of
the public API.

### 1.1 On the companion doc's filename

The companion doc is still named `api-surface-v1.0.md`. **The filename is
historical; the contents are live.** It has been amended repeatedly since the
1.0 freeze — #202/#207 (which landed inside 1.0.0 itself), #203/#220 and #221
(1.1.1), #278/#279 (1.4.0), and #291, #288, #289, #290, #296 (unreleased,
shipping in 2.0.0) — so it is not, and has not been for some time, a snapshot
of what v1.0 shipped. The true v1.0 text is recoverable from git history at
the `v1.0.0` tag — not at the freeze commit, which predates the #202/#207
amendment and is therefore a pre-release draft.

The name is retained deliberately rather than renamed, because fourteen
inbound links across `README.md`, `README.ja.md`, `docs/peer-messaging.md`,
`docs/peer-messaging.ja.md`, `docs/configuration.md` and
`docs/configuration.ja.md` point at it, and because `CHANGELOG.md`'s `[1.0.0]`
entry links to it as a historical record that must keep resolving.

Renaming it to a version-neutral `docs/api-surface.md` is a reasonable
follow-up, and is the only clean permanent fix for the naming. It was not
bundled into this document because it is link churn across two locales with no
behavioral content, and it is better reviewed on its own.

**What this means in practice**: the frozen surface for a given release of the
2.0 line is the companion doc *as of that release's tag*.

## 2. Version bumps

Given a version `MAJOR.MINOR.PATCH`:

- **MAJOR** bumps when the public API changes incompatibly (see §3 for what
  counts).
- **MINOR** bumps when functionality is added in a backward-compatible
  manner. Examples: new MCP tool, new CLI subcommand, new optional flag with a
  backward-compatible default, new IPC `Request` variant, new IPC `Event`
  variant, new error code token, new capability token, new config key, new
  layout node field with a backward-compatible default.
- **PATCH** bumps for backward-compatible bug fixes only.

For the pre-1.0 (`0.y.z`) convention and how it was retired at 1.0.0, see the
1.0 policy §2. It is history and is not restated here.

## 3. What counts as a breaking change

A change is breaking if any of the following hold for a frozen surface item:

- **Removal** — tool, subcommand, flag, env var, IPC variant, error code
  token, capability token, or config key disappears.
- **Rename** — the wire identifier (tool name, JSON tag, error code token,
  capability token, CLI flag long form, env var name, config key name)
  changes.
- **Required-input addition** — a previously optional input becomes required,
  or a new required field is added.
- **Type narrowing of an input** — a string input becomes an enum that
  rejects previously accepted values; an integer range tightens; a flag
  becomes mutually exclusive with another that previously composed.
- **Output shape removal or rename** — a documented output field is removed
  or renamed; a documented output prefix string changes.
- **Semantic change** — the same input produces materially different output
  in a way callers can observe. The 2.0 line shipped three: cross-tab
  `peer_send` moving from silent no-op to delivery-or-error (#289), pane-tool
  target resolution moving from the active tab to the caller's tab (#288,
  #296), and `new_tab` gaining a `MAX_TABS = 16` ceiling that a previously
  uncapped call can now hit (#290). See §8.
- **Newly imposed limit** — an operation that was unbounded acquires a
  ceiling, quota, or rate limit that a previously-succeeding call can now
  hit. This is called out separately from *semantic change* because #290's
  tab cap was filed as an addition and the distinction was not obvious at
  review time.
- **Endpoint or path change** — IPC socket / pipe naming convention changes
  in a way that breaks `endpoint_from_env` discovery for an existing client.
- **Layout `version = 1` schema break** — see §6.

A change is **not** breaking if:

- A new optional input is added with a default that preserves prior behavior.
- A new tool / subcommand / variant / error code / capability token / event /
  field is added.
- A `stable-stub` becomes a real implementation while the documented input
  and output shape stays valid.
- An undocumented or deferred behavior changes.
- Internal Rust types are refactored without changing wire output.
- Detached-mode ok-text *suffix* (the `<reason>` portion) changes wording —
  only the documented prefix is frozen.

### 3.1 Wrong-behavior corrections

> **New in 2.0.** The 1.0 policy had no bug-fix carve-out, which made #296 —
> stopping `close_pane(target: "focused")` from terminating a pane the user
> was typing in — a policy violation on paper.

A semantic change is **not** treated as breaking when all four hold:

1. The prior behavior contradicted the surface doc, the tool's own
   description, or an explicit design invariant — it was a defect, not a
   contract.
2. No caller could reasonably have depended on it *on purpose*. Destroying
   the wrong pane, delivering to the wrong tab, and silently discarding a
   message all qualify; a merely surprising default does not.
3. The fix is announced in the CHANGELOG with the defect described, not just
   the new behavior.
4. Where version skew can reintroduce the defect, the fix is capability-gated
   so that a mismatched client fails closed rather than silently getting the
   old behavior (§7).

Invoking this carve-out is a judgment call and must be stated explicitly in
the CHANGELOG entry. It is not a general licence to relabel breaks as fixes:
if condition 2 is arguable, the change is breaking.

## 4. Deprecation window

Renga adopts the same deprecation discipline that governs the
`renga::ipc::err_code` module ("Stability" doc-comment), and extends it to all
frozen surfaces.

The window for any breaking removal or rename is:

1. **One full minor release** during which the item is marked deprecated and
   continues to work. Deprecation must be announced in:
   - The CHANGELOG entry for that minor release (under a `### Deprecated`
     subheading).
   - Inline doc-comments / `--help` text for code surfaces.
   - The companion surface doc entry for the item (added "deprecated since
     vX.Y").
2. **Removal in the next major release**. Until that major release ships the
   item must continue to function with a runtime warning to stderr (CLI / IPC
   server) or to the JSON-RPC error path with a `deprecated_*` code prefix
   where applicable.

A breaking *semantic* change that cannot be expressed as add-the-new /
remove-the-old must take one of two paths:

- **The flag path** (inherited from 1.0): ship the new behavior behind an
  opt-in flag in a minor release, make it the default only in the next major,
  and provide the prior behavior under an opt-out flag for at least one full
  minor release after the default flip.
- **The capability path** (new in 2.0, §7): flip the behavior in a major
  release and gate it on a new capability token, so that a client built
  against either side of the flip fails closed with `server_too_old` instead
  of silently getting the behavior it did not ask for.

> **Why the capability path exists.** The flag path assumes the risk is a
> caller who wants the old behavior. renga's actual risk is different: the
> binary on disk gets upgraded while the old server process keeps running, so
> a new client can meet an old server within one user session. A flag cannot
> detect that; a capability token can, and it is what renga shipped four times
> over in the 2.0 line. The capability path is only available for changes
> where fail-closed is an acceptable outcome — it converts a silent wrong
> action into a loud refusal, which is the right trade for destructive or
> misrouted operations and the wrong trade for cosmetic ones.

In practice this means breaking changes accumulate against an unreleased
`MAJOR + 1` line and ship together, not piecemeal.

## 5. Additive changes

Adding to a frozen surface is a minor bump. Specifically:

- New MCP tool — minor.
- New input field on an existing MCP tool, optional, with a backward-compatible
  default — minor.
- New CLI subcommand — minor.
- New CLI flag, optional, with a backward-compatible default — minor.
- New IPC `Request` variant — minor. Existing servers must continue to reject
  unknown variants with `protocol`; clients must treat that rejection as
  "feature not supported on this server version".
- New IPC `Response` variant — major (clients can't assume forward-compat on
  status discriminants without an explicit ignore rule, and we do not have one
  for `Response`). Adding a new field to an existing variant is minor.
- New IPC `Event` variant — minor. Per the forward-compat rule, clients
  ignore unknown `type` tags.
- New `err_code` token — minor. Per the forward-compat rule, clients treat
  unknown tokens as `internal`.
- **New capability token — minor** (new in 2.0). The token list on
  `Response::Hello` is additive by construction: clients match tokens they
  know and ignore the rest. Adding a token is never breaking even when the
  *behavior* it advertises is.
- New config key — minor. Existing readers ignore unknown keys.
- New layout TOML field with a default — minor. A schema change that is not
  expressible additively in v1 forces `version = 2` (see §6).

## 6. Layout TOML versioning

The `version` integer in layout files is the layout schema's own version
contract:

- `version = 1` is frozen. It remains the only value renga accepts —
  `SUPPORTED_VERSION` is still `1` in `src/layout_config.rs`.
- Adding fields with defaults to v1 nodes is a minor renga release.
- Any change that an existing v1 file would no longer parse cleanly under
  forces `version = 2`. The v2 parser is added in a minor release; the v1
  parser stays.
- The v1 parser may only be removed in a major renga release (and only after
  the deprecation window in §4).

> **Note for 2.0.0.** This is the first major release at which the last rule
> becomes exercisable. renga 2.0 does **not** exercise it: the v1 parser stays,
> and no `version = 2` schema exists. Layout files written for renga 1.x load
> unchanged under 2.x.

## 7. Capability tokens and version skew

> **New in 2.0.** The 1.0 policy did not describe this mechanism; the 2.0 line
> depends on it.

The `hello` handshake carries a `capabilities` list. Each token names a
behavior a client may need to know about before it sends a request that
depends on it. The tokens this build advertises are defined in
`src/ipc/mod.rs` as `SERVER_CAPABILITIES`; as of 2.0.0 they are
`caller_scope` (#288), `cross_tab_peers` (#289), `spawn_tab` (#290), and
`caller_scope_close_identity` (#296).

Rules:

1. **Capability tokens are a frozen surface.** Their string values are wire
   ABI. Removing or renaming a token is breaking (§3); adding one is minor
   (§5).
2. **Tokens are per-behavior, not per-release.** `caller_scope` and
   `caller_scope_close_identity` are deliberately separate because a
   #288-era server advertises the first while still resolving `close_pane`
   against the wrong tab. Do not fold tokens together to save a string.
3. **Absent token ⇒ fail closed.** A client that needs a behavior must gate on
   its token and refuse with `server_too_old` rather than send the request and
   hope. `Request` does not use `deny_unknown_fields`, so an older server
   silently *drops* an unknown field and performs the old behavior — which is
   precisely the wrong-tab accident these tokens exist to prevent.
4. **Version skew is a live scenario, not a theoretical one.** A renga binary
   can be upgraded on disk while the old server process keeps running, so a
   newly spawned `renga mcp-peer` can talk to an old server within one user
   session. `[server_too_old] … restart renga` is the expected, correct
   outcome in that window.

Note that the capability set is readable directly out of the `hello`
handshake by any IPC client — that is how the bundled client gates its
requests — but it is not surfaced to MCP peers, which today can observe it
only by attempting a gated request and reading the error. Exposing it to peers
is tracked as a follow-up (#304) and is not part of this policy.

## 8. The 2.0.0 ledger

> **New in 2.0.** This section exists because the honest answer to "did the
> 2.0 breaking changes serve the §4 deprecation window?" is **no**, and a
> policy document that quietly implies otherwise is worse than no document.

**No deprecation has ever been announced in this project.** `CHANGELOG.md`
contains zero `### Deprecated` subheadings across every release from 1.0.0
through 1.4.0, and the companion surface doc carries no "deprecated since
vX.Y" marks. §4's announcement machinery has therefore never run. Every
breaking change in 2.0.0 reached the major boundary without a deprecation
minor preceding it.

| Change | Filed as | Window served? | Mitigation |
|---|---|---|---|
| #289 cross-tab peer messaging | BREAKING | No — waived on the record by the owner, on the grounds that the anti-enumeration rationale for the silent drop belongs to the security layer outside renga | `cross_tab_peers` capability, fail closed |
| #288 caller-tab scoping (7 tools) | *(no CHANGELOG entry of its own)* | No, and no waiver was recorded | `caller_scope` capability, fail closed |
| #296 caller-tab scoping (`close_pane`, `set_pane_identity`) | Changed, not labelled BREAKING | No | `caller_scope_close_identity` capability, fail closed; qualifies under §3.1 |
| #290 `MAX_TABS = 16` | Added | No | None — a 17th `new_tab` now returns `tab_limit_reached` where it previously succeeded |

Two consequences for how this policy is read going forward:

- **2.0.0 is the major release that legitimises #288, #289, #290's tab cap and
  #296.** They are declared here rather than pretended away. §3.1 was written
  to cover #296 honestly; the other three are accepted breaks.
- **The §4 window is now expected to actually run.** The reason it never has
  is that no removal or rename has been attempted yet — only semantic changes
  and one newly imposed limit. None of them took §4's flag path, and of the
  four only #289 has a waiver on the record; #288, #290 and #296 skipped the
  window with nothing recorded at all. From 2.0.0 onward, a breaking change
  that skips its window needs an explicit waiver recorded in the CHANGELOG
  entry, naming who decided and why. "It is a bug fix" is §3.1 and must
  satisfy §3.1's four conditions.

Two further gaps the 2.0.0 release should close, both surfaced by running the
§9 step 1 inventory pass: the companion doc is missing the `--fps` CLI flag
and `[ui] fps` config key (shipped in 1.1.0), the `server_too_old` error code
(emitted today and referenced five times in its own prose), and the
`capabilities` field on `Response::Hello`. Its appendix item counts are stale
accordingly.

## 9. Major release procedure

> **Changed from 1.0.** The 1.0 policy's §7 was a one-shot checklist for
> getting *to* 1.0. This is its general form, corrected against what CI
> actually does.

1. **Reconcile the surface doc against `main`.** One full inventory pass;
   fix anything that drifted since the last release. This step is what
   catches the gaps listed at the end of §8 — it is the cheapest step in this
   list and the one most worth not skipping.
2. **Roll the CHANGELOG**: collapse `## [Unreleased]` into
   `## [MAJOR.0.0] - YYYY-MM-DD`. Include a pointer to the companion surface
   doc, a pointer to this policy, every breaking change with its §3 predicate
   named, and any waiver required by §8.
3. **Bump the version in three places in the same commit**: `Cargo.toml`,
   `npm/package.json`, **and `Cargo.lock`**. CI builds with `--locked` in
   clippy, test, and release, so a forgotten lockfile fails every job.
4. **Open the release PR.** `main` is PR-only; squash on merge per repo
   convention.
5. **After merge**: `git tag vMAJOR.0.0 && git push origin vMAJOR.0.0`. CI
   builds four targets (Windows x64, macOS x64/arm64, Linux x64 musl),
   generates `checksums.txt`, creates the GitHub Release, and publishes to npm
   via Trusted Publishing.
6. **Edit the GitHub Release body** to link the companion surface doc, this
   policy, and the CHANGELOG entry. The workflow sets
   `generate_release_notes: true`, so these links do not appear on their own.

Constraints the procedure depends on:

- **Tag naming drives release classification.** A tag containing `-` is
  published as a prerelease with npm dist-tag `next`; a plain `vX.Y.Z` is a
  stable release with dist-tag `latest`. The workflow decides this purely from
  whether `ref_name` contains a hyphen.
- **The tag and `npm/package.json` must agree exactly.** The npm postinstall
  script builds its download URL from the package version as
  `…/releases/download/v${VERSION}`. Tagging `v2.0.0` while `package.json`
  says `2.0.0-rc.1` publishes a package whose install 404s. This matters most
  on a prerelease-heavy major line.
- **Never run `npm publish` or `gh release create` by hand.** Both are CI's
  job; doing them manually causes version collisions.
- **`cargo fmt --all` before committing.** CI's rustfmt job fails fast.

## 10. What this policy deliberately does not cover

The four frozen surfaces in §1 are machine-facing contracts. Interactive TUI
behavior — keybindings, modals, layout rendering, colors — is **not** among
them, and changing it is never a MAJOR bump under this policy.

That is a deliberate scoping choice, not an oversight, but it has a
consequence worth stating plainly: **"not breaking" does not mean "not
disruptive."** The most visible change in 2.0.0 for a human user is that
`Ctrl+W` now asks before closing a pane (#285), which this policy classifies
as non-breaking because it touches no frozen surface. Release notes should
lead with what users will notice, not with what the policy classifies as
breaking.

---

This document is the source of truth for the 2.0 line. Future major lines
(3.0.0+) supersede it via a successor doc, following the same pattern this
document establishes: the superseded doc stays at its path with its body
intact, gains a forward pointer, and the successor states explicitly what it
inherits and what it changes.
