# renga semver policy (v1.0+)

> **Status**: design proposal, scoped to v1.0 and onward.
>
> **Companion doc**: [`api-surface-v1.0.md`](./api-surface-v1.0.md) — defines
> the surface this policy promises to keep stable.
>
> **Superseded by**: [`semver-policy-2.0.md`](./semver-policy-2.0.md) — the
> live policy from the 2.0 line onward. This document is preserved at its
> final v1.0-line state: the rules below are unedited and continue to
> describe releases 1.0.0 through 1.4.x. The successor inherits §1–§6 and
> states explicitly what it changed.

renga follows [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html).
This document specifies *what* counts as the public API for the purposes of
that spec, *how* breaking changes are deprecated, and *when* the version
components bump.

## 1. What is the public API

The renga public API is exactly the set of items listed in
[`api-surface-v1.0.md`](./api-surface-v1.0.md) at stability **stable** or
**stable-stub**. These are the four frozen surfaces:

1. **MCP tools** (`renga-peers` server) — tool names, input schemas, result
   shapes, error code tokens, and the documented ok-text fallback prefixes.
2. **CLI** (`renga` binary) — top-level flags, subcommand names, subcommand
   flags, and the env vars listed in §2.3 of the surface doc.
3. **IPC protocol** — endpoint naming, handshake, request/response/event
   envelopes and variants, error code catalog.
4. **Config and layout files** — `config.toml` schema and layout TOML
   `version = 1` schema.

Items marked **deferred** in the surface doc are explicitly *not* part of the
public API. They may change in any minor release. This includes Rust-level
`peer_*` IPC variant naming, the `double_option` serde helper, the
`spawn_codex_pane` env-detection behavior, and anything else listed under
"Out of scope for v1.0".

Internal Rust APIs (anything under `src/` that is not on a frozen surface),
test fixtures, build scripts, and documentation prose are likewise not part of
the public API.

## 2. Version bumps

Given a version `MAJOR.MINOR.PATCH`:

- **MAJOR** bumps when the public API changes incompatibly (see §3 for what
  counts).
- **MINOR** bumps when functionality is added in a backward-compatible
  manner. Examples: new MCP tool, new CLI subcommand, new optional flag with a
  backward-compatible default, new IPC `Request` variant, new IPC `Event`
  variant, new error code token, new config key, new layout node field with a
  backward-compatible default.
- **PATCH** bumps for backward-compatible bug fixes only.

Pre-1.0 versions (`0.y.z`) followed the pre-1.0 spec: anything could change in
any release, with `MINOR` reserved for user-visible behavior changes by
convention. That convention is replaced by this policy at 1.0.0.

## 3. What counts as a breaking change

A change is breaking if any of the following hold for a frozen surface item:

- **Removal** — tool, subcommand, flag, env var, IPC variant, error code
  token, or config key disappears.
- **Rename** — the wire identifier (tool name, JSON tag, error code token,
  CLI flag long form, env var name, config key name) changes.
- **Required-input addition** — a previously optional input becomes required,
  or a new required field is added.
- **Type narrowing of an input** — a string input becomes an enum that
  rejects previously accepted values; an integer range tightens; a flag
  becomes mutually exclusive with another that previously composed.
- **Output shape removal or rename** — a documented output field is removed
  or renamed; a documented output prefix string changes.
- **Semantic change** — the same input produces materially different output
  in a way callers can observe (e.g. cross-tab `peer_send` switching from
  silent no-op to error; the all-digit-name rule changing direction;
  `spawn_pane.command` rewrite condition broadening).
- **Endpoint or path change** — IPC socket / pipe naming convention changes
  in a way that breaks `endpoint_from_env` discovery for an existing client.
- **Layout `version = 1` schema break** — see §6.

A change is **not** breaking if:

- A new optional input is added with a default that preserves prior behavior.
- A new tool / subcommand / variant / error code / event / field is added.
- A `stable-stub` becomes a real implementation while the documented input
  and output shape stays valid.
- An undocumented or deferred behavior changes.
- Internal Rust types are refactored without changing wire output.
- Detached-mode ok-text *suffix* (the `<reason>` portion) changes wording —
  only the documented prefix is frozen.

## 4. Deprecation window

Renga adopts the same deprecation discipline that today already governs the
`renga::ipc::err_code` module ("Stability" doc-comment), and extends it to all
frozen surfaces.

The window for any breaking removal or rename is:

1. **One full minor release** during which the item is marked deprecated and
   continues to work. Deprecation must be announced in:
   - The CHANGELOG entry for that minor release (under a `### Deprecated`
     subheading).
   - Inline doc-comments / `--help` text for code surfaces.
   - The `api-surface-v1.0.md` entry for the item (added "deprecated since
     vX.Y").
2. **Removal in the next major release**. Until that major release ships the
   item must continue to function with a runtime warning to stderr (CLI / IPC
   server) or to the JSON-RPC error path with a `deprecated_*` code prefix
   where applicable.

A breaking *semantic* change that cannot be expressed as add-the-new /
remove-the-old (e.g. flipping `peer_send` cross-tab from silent to error)
must:

- Ship the new behavior behind an opt-in flag in a minor release first.
- Make the new behavior the default only in the next major release.
- Provide the prior behavior under an opt-out flag for at least one full minor
  release after the default flip.

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
- New config key — minor. Existing readers ignore unknown keys.
- New layout TOML field with a default — minor. A schema change that is not
  expressible additively in v1 forces `version = 2` (see §6).

## 6. Layout TOML versioning

The `version` integer in layout files is the layout schema's own version
contract:

- `version = 1` is frozen by renga 1.0.
- Adding fields with defaults to v1 nodes is a minor renga release.
- Any change that an existing v1 file would no longer parse cleanly under
  forces `version = 2`. The v2 parser is added in a minor release; the v1
  parser stays.
- The v1 parser may only be removed in a major renga release (and only after
  the deprecation window in §4).

## 7. Pre-1.0 → 1.0 release procedure

When the design in `api-surface-v1.0.md` and this doc has been reviewed and
adopted:

1. Verify the surface doc against `main` (one final inventory pass; reconcile
   anything that drifted during review).
2. Roll the CHANGELOG: collapse the `## [Unreleased]` working section into
   `## [1.0.0] - YYYY-MM-DD`. Include:
   - A pointer to `docs/api-surface-v1.0.md` as the freeze definition.
   - A pointer to `docs/semver-policy.md` for the rules.
   - The `set_summary` implementation note (was `stable-stub`, becoming
     real per the follow-up).
   - Any additional behavior changes shipped against the same release.
3. Bump `Cargo.toml` and `npm/package.json` to `1.0.0` in the same commit
   (per the existing release process in `CLAUDE.md`).
4. Open the release PR. Squash on merge per repo convention.
5. After merge: `git tag v1.0.0 && git push origin v1.0.0`. CI publishes the
   binaries and the npm package.
6. GitHub Release notes link to `api-surface-v1.0.md`, `semver-policy.md`,
   and the CHANGELOG entry.

After 1.0.0 ships, this doc is the source of truth. Future major lines
(2.0.0+) supersede it via a successor doc; the 1.0 doc is preserved at its
final state for reference.

---

> **Superseded.** The successor doc the paragraph above calls for is
> [`semver-policy-2.0.md`](./semver-policy-2.0.md), adopted for the 2.0 line.
> This document is now frozen at its final v1.0-line state — the text above
> is unedited and still describes releases 1.0.0 through 1.4.x. For the
> current rules, including what §1–§6 carry over unchanged, what the 2.0 line
> changed, and the deprecation-window ledger for the 2.0 breaking changes,
> see the successor.
