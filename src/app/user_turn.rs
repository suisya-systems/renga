//! `deliver="user_turn"` — delivering a peer message as a real user
//! turn (Issue #323).
//!
//! A channel message is *shown* to the recipient without taking its
//! turn. Some instructions only take effect as a genuine user turn:
//! `/loop`, `/clear` and slash commands generally are not armed by a
//! `<channel>` tag. Reaching that today means driving `send_keys` by
//! hand — write the text, check it landed, send Enter as a *separate*
//! call — a ritual enforced by prose, and therefore a ritual that
//! eventually breaks.
//!
//! This module owns that ritual instead, in one place:
//!
//! 1. **Prove readiness** ([`App::user_turn_readiness`]). Positive
//!    identification only: an agent renga recognizes, an empty composer
//!    framed the way that agent frames it, caret evidence inside it, and
//!    no "interrupt" affordance on screen. Anything unproven is refused
//!    with zero bytes written — a modal must fail closed, never be typed
//!    into (see the design note on #323).
//! 2. **Write the body** — once, without Enter.
//! 3. **Settle, then confirm** the draft is on screen and stable, so a
//!    submit can't fire into a composer somebody else just touched.
//! 4. **Enter as a separate PTY write**, then observe that the draft was
//!    actually consumed before reporting success.
//!
//! Nothing here touches `send_keys`. Dialog control (folder-trust Enter,
//! permission `y`/`n`, `Shift+Tab`, `Ctrl+C`) is precisely the state
//! where "did the text land in the input box?" has no meaning, and a
//! readiness check there would either refuse the keystroke or delay it
//! past the moment it was meant for.

use super::codex_peer::{
    codex_prompt_allows_peer_nudge_on_screen, screen_has_visible_text, write_input_to_pane,
};
use super::*;
use crate::ui::{find_prompt_row, resolve_input_row_last};

/// How long to wait after writing the body before reading the composer
/// back. Covers the agent's render round-trip: the PTY echo, the TUI's
/// own repaint, and renga's next `drain_pty_events`.
pub(crate) const USER_TURN_SETTLE_DELAY: Duration = Duration::from_millis(300);

/// Gap between two identical composer reads required before Enter. A
/// single read can catch a half-painted frame; requiring the same
/// content twice means the draft has stopped moving.
pub(crate) const USER_TURN_CONFIRM_DELAY: Duration = Duration::from_millis(120);

/// How many times the confirm stage may accept a *changed* composer and
/// restart its stability window.
///
/// A couple of restarts absorb a slow multi-frame repaint. The bound
/// matters because the restart re-anchors onto whatever is on screen:
/// if a human edits the draft and pauses for one confirm window, renga
/// submits the body *plus* their edit. Structural re-verification
/// (`composer_block_text`) rules out submitting into a dialog, and the
/// `empty` guard rules out a stray bare Enter, but two writers typing
/// into one composer is not something renga can disentangle — it can
/// only keep the window short.
const USER_TURN_CONFIRM_MAX_RESTARTS: u8 = 3;

/// Whole-delivery budget, measured from the moment the App accepts the
/// command. Deliberately well inside [`crate::ipc::APP_REPLY_TIMEOUT`]
/// (5s) so a slow delivery reports `user_turn_stalled` — which says
/// something specific about the target — instead of `app_timeout`,
/// which claims renga itself is wedged.
pub(crate) const USER_TURN_DEADLINE: Duration = Duration::from_millis(3000);

/// Window during which an identical `(target, from, body)` user turn is
/// suppressed. Mirrors [`super::codex_peer::PEER_SEND_DEDUPE_TTL`] in
/// size but is a **separate** ledger: a channel report and a user turn
/// are different intentional operations, so neither may swallow the
/// other.
pub(crate) const USER_TURN_DEDUPE_TTL: Duration = Duration::from_secs(5);

/// Largest body renga will type as a user turn.
///
/// The body write happens while the parser lock is held (see
/// [`App::snapshot_and_write_user_turn`]), which parks the pane's PTY
/// reader thread for the duration. That is safe only because the write
/// cannot block: a full PTY input buffer would stall it, the reader
/// could not drain the child's output, and a child blocked writing
/// stdout would stop reading stdin — a cycle. Reaching it needs the
/// child to fill its whole output buffer inside a sub-millisecond
/// critical section, which an agent readiness just proved idle will
/// not do; this cap removes the other half of the cycle by keeping the
/// write far below any platform's buffer. A user turn is a prompt, not
/// a file transfer, so the limit is generous in practice.
const USER_TURN_MAX_BODY_BYTES: usize = 4096;

/// Minimum non-blank cells a row needs before it can count as one of
/// the composer's frame rows. Keeps a dialog's `│ … │` side borders —
/// two non-blank cells on an otherwise empty line — from passing as a
/// horizontal rule.
const COMPOSER_FRAME_MIN_CELLS: usize = 4;

/// Box-drawing glyphs a composer frame row may be built from. Covers
/// both shapes Claude Code has shipped: the bordered input box
/// (`╭──╮` / `╰──╯`) and the current pair of plain horizontal rules.
/// Deliberately excludes the T-junctions `├` / `┤`: those delimit rows
/// *inside* a widget (a menu separator, a table rule), and accepting
/// them let a highlighted list entry pass as a framed composer.
const FRAME_GLYPHS: &[&str] = &[
    "─", "━", "│", "┃", "╭", "╮", "╰", "╯", "┌", "┐", "└", "┘", "═", "╌", "┄", "┈",
];

/// Substrings that mean "this agent is mid-turn". Scanned only in the
/// status rows *below* the composer, never in transcript text, so a
/// message that happens to discuss interrupts cannot fake a busy pane.
///
/// This is the one string-shaped signal in the predicate, and it can
/// only ever turn a *delivery* into a *refusal* — the composer proof
/// above it is what authorizes writing. If a future Claude Code drops
/// or translates these, the failure is "a turn is delivered to a busy
/// agent, which queues it", not "a turn is typed into a dialog".
const BUSY_MARKERS: &[&str] = &["to interrupt", "tab to queue"];

/// Which agent renga believes owns a pane's input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnAgent {
    Claude,
    Codex,
}

/// Outcome of the readiness predicate. Every non-`Ready` value
/// guarantees that no bytes were written to the pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnReadiness {
    /// An empty, focused composer was positively identified.
    Ready,
    /// A composer was identified but the agent is mid-turn.
    Busy,
    /// No empty composer could be proven — a draft is present, a modal
    /// is up, or the screen is simply not one renga recognizes.
    NotReady,
    /// The pane is not running an agent that takes turns at all.
    Unsupported,
}

impl TurnReadiness {
    fn into_error(self, pane_id: usize) -> Option<ipc::CodedError> {
        match self {
            TurnReadiness::Ready => None,
            TurnReadiness::Busy => Some(ipc::CodedError::new(
                ipc::err_code::USER_TURN_BUSY,
                format!(
                    "pane {pane_id} is mid-turn; nothing was written. Retry once it goes idle, \
                     or interrupt it with send_keys first."
                ),
            )),
            TurnReadiness::NotReady => Some(ipc::CodedError::new(
                ipc::err_code::USER_TURN_NOT_READY,
                format!(
                    "pane {pane_id} is not accepting a turn: no empty agent composer is on \
                     screen (a draft, a permission prompt or another modal is up, or the UI is \
                     one renga cannot read). Nothing was written — resolve it with send_keys \
                     and retry."
                ),
            )),
            TurnReadiness::Unsupported => Some(ipc::CodedError::new(
                ipc::err_code::USER_TURN_UNSUPPORTED_TARGET,
                format!(
                    "pane {pane_id} is not running an agent that takes user turns (deliver=\
                     \"user_turn\" needs a Claude or Codex pane). Use deliver=\"channel\", or \
                     send_keys for raw input."
                ),
            )),
        }
    }
}

/// Stage of an accepted delivery. The body has already been written by
/// the time any of these exist.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)] // every stage genuinely awaits something
pub(crate) enum UserTurnStage {
    /// Waiting out [`USER_TURN_SETTLE_DELAY`], then for the composer to
    /// stop reading as `empty` — the snapshot taken just *before* the
    /// body was written.
    ///
    /// The reference snapshot is load-bearing. An empty composer is not
    /// blank text: it renders as its prompt glyph (`❯`), so "the block
    /// is non-empty" would be true before a single byte arrived and the
    /// machine would submit into an empty composer.
    AwaitDraft { ready_at: Instant, empty: String },
    /// Draft seen; waiting for a second identical read before Enter.
    AwaitConfirm {
        ready_at: Instant,
        /// The pre-write snapshot, carried through: if the composer
        /// reads as this again, our body left it before we submitted —
        /// somebody else pressed Enter, or cleared it. Re-anchoring onto
        /// it and pressing Enter would fire a stray submit into a pane
        /// that is now doing something else.
        empty: String,
        draft: String,
        restarts: u8,
    },
    /// Enter written; waiting for the draft to be consumed.
    AwaitSubmit { draft: String },
}

/// What [`step_user_turn`] wants the caller to do with a delivery.
///
/// Splitting the decision from the I/O keeps every timing and
/// observation rule — settle, stability, restart budget, deadline,
/// "was the draft consumed?" — in one pure function that tests can
/// drive with explicit clocks and explicit screens, instead of with
/// sleeps against a live PTY.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum UserTurnStep {
    /// Nothing to do this frame.
    Wait,
    /// Adopt the new stage; no bytes to write.
    Advance(UserTurnStage),
    /// Write Enter, then adopt the new stage.
    Submit(UserTurnStage),
    /// Terminal: the draft was consumed.
    Submitted,
    /// Terminal: bytes were written but submission was not observed.
    Stalled(&'static str),
}

/// What one look at the target's screen produced.
///
/// The distinction is load-bearing at the submit stage: a composer that
/// structurally vanished from a screen we *can* read means our draft
/// was consumed (`/clear` repaints the whole screen), while a pane we
/// cannot read at all means we observed nothing — and reporting that as
/// a submission would be a claim about a turn nobody watched land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ComposerRead {
    /// The screen was read. `None` means no composer is on it.
    Readable(Option<String>),
    /// The pane could not be read: it exited, the human scrolled it
    /// back, its agent identity is gone, or its parser is poisoned.
    Unreadable,
}

impl ComposerRead {
    #[cfg(test)]
    pub(crate) fn text(s: &str) -> Self {
        ComposerRead::Readable(Some(s.to_string()))
    }

    #[cfg(test)]
    pub(crate) fn gone() -> Self {
        ComposerRead::Readable(None)
    }

    fn as_text(&self) -> Option<&str> {
        match self {
            ComposerRead::Readable(v) => v.as_deref(),
            ComposerRead::Unreadable => None,
        }
    }
}

/// One step of the delivery state machine.
pub(crate) fn step_user_turn(
    stage: &UserTurnStage,
    composer: &ComposerRead,
    now: Instant,
    deadline: Instant,
) -> UserTurnStep {
    let expired = now >= deadline;
    // Everything below runs after the body was written, so a screen we
    // cannot read is an uncertain outcome, never a success.
    if matches!(composer, ComposerRead::Unreadable) {
        return UserTurnStep::Stalled(
            "the target became unreadable mid-delivery, so nothing could be observed",
        );
    }
    let composer = composer.as_text();
    match stage {
        UserTurnStage::AwaitDraft { ready_at, empty } => {
            // Expiry wins over progress here. Advancing at the deadline
            // would park the delivery in a stage that has no budget left
            // to confirm or submit from, so the reply would cost another
            // frame — past `APP_REPLY_TIMEOUT` at low fps — for an
            // outcome already decided.
            if expired {
                return UserTurnStep::Stalled(
                    "the delivery budget ran out before the body appeared in the composer",
                );
            }
            if now < *ready_at {
                return UserTurnStep::Wait;
            }
            match composer {
                Some(text) if text != empty => UserTurnStep::Advance(UserTurnStage::AwaitConfirm {
                    ready_at: now + USER_TURN_CONFIRM_DELAY,
                    empty: empty.clone(),
                    draft: text.to_string(),
                    restarts: 0,
                }),
                _ if expired => {
                    UserTurnStep::Stalled("the body never appeared in the target's composer")
                }
                _ => UserTurnStep::Wait,
            }
        }
        UserTurnStage::AwaitConfirm {
            ready_at,
            empty,
            draft,
            restarts,
        } => {
            if expired {
                return UserTurnStep::Stalled(
                    "the delivery budget ran out before the draft could be submitted",
                );
            }
            if now < *ready_at {
                return UserTurnStep::Wait;
            }
            let Some(current) = composer else {
                return UserTurnStep::Stalled("the composer disappeared before submit");
            };
            if current == empty {
                // Our body is gone and we never pressed Enter. Somebody
                // else submitted or cleared it; pressing Enter now would
                // land a bare submit on whatever they started.
                return UserTurnStep::Stalled(
                    "the body left the composer before renga could submit it",
                );
            }
            if current == draft {
                // Reached only while budget remains (checked above):
                // submitting costs at least one more frame to observe,
                // and spending the last of it on a write nobody watches
                // would report `app_timeout` for a turn that did land.
                return UserTurnStep::Submit(UserTurnStage::AwaitSubmit {
                    draft: draft.clone(),
                });
            }
            if *restarts >= USER_TURN_CONFIRM_MAX_RESTARTS {
                return UserTurnStep::Stalled(
                    "the composer kept changing, so the draft was never confirmed — another \
                     writer may own it",
                );
            }
            UserTurnStep::Advance(UserTurnStage::AwaitConfirm {
                ready_at: now + USER_TURN_CONFIRM_DELAY,
                empty: empty.clone(),
                draft: current.to_string(),
                restarts: restarts.saturating_add(1),
            })
        }
        UserTurnStage::AwaitSubmit { draft } => {
            // Submission is "our draft is gone", not "the screen
            // changed": a spinner repaint is not a submit, and a slash
            // command that repaints the whole screen is. Reached only
            // for a readable screen — an unreadable one stalled above.
            let consumed = composer.is_none_or(|current| current != draft);
            if consumed {
                UserTurnStep::Submitted
            } else if expired {
                UserTurnStep::Stalled("Enter was written but the draft is still in the composer")
            } else {
                UserTurnStep::Wait
            }
        }
    }
}

/// One in-flight user-turn delivery, parked in
/// [`App::pending_user_turns`] with the IPC reply it still owes.
#[derive(Debug)]
pub(crate) struct PendingUserTurn {
    target_pane: usize,
    reply: Option<oneshot::Sender<std::result::Result<serde_json::Value, ipc::CodedError>>>,
    stage: UserTurnStage,
    deadline: Instant,
}

impl PendingUserTurn {
    fn answer(&mut self, outcome: std::result::Result<serde_json::Value, ipc::CodedError>) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(outcome);
        }
    }
}

fn submitted_payload(target_id: usize) -> serde_json::Value {
    serde_json::json!({
        "delivery": "user_turn",
        "status": "submitted",
        "target_id": target_id,
    })
}

fn duplicate_payload(target_id: usize) -> serde_json::Value {
    serde_json::json!({
        "delivery": "user_turn",
        "status": "duplicate_suppressed",
        "target_id": target_id,
    })
}

fn stalled_error(pane_id: usize, detail: &str) -> ipc::CodedError {
    ipc::CodedError::new(
        ipc::err_code::USER_TURN_STALLED,
        format!(
            "user turn to pane {pane_id} was written but submission was not observed ({detail}). \
             The body may be sitting in the composer — inspect the pane before retrying."
        ),
    )
}

// ── screen reading ────────────────────────────────────────────

fn cell_text(screen: &vt100::Screen, row: u16, col: u16) -> String {
    screen
        .cell(row, col)
        .map(|c| c.contents().to_string())
        .unwrap_or_default()
}

fn row_text(screen: &vt100::Screen, row: u16) -> String {
    let cols = screen.size().1;
    let mut line = String::with_capacity(cols as usize);
    for col in 0..cols {
        line.push_str(&cell_text(screen, row, col));
    }
    line.trim_end().to_string()
}

/// Whether `row` looks like one of the composer's horizontal frame
/// rows: enough non-blank cells to be a rule, and every one of them a
/// box-drawing glyph.
fn row_is_composer_frame(screen: &vt100::Screen, row: u16) -> bool {
    if row >= screen.size().0 {
        return false;
    }
    let cols = screen.size().1;
    let mut seen = 0usize;
    for col in 0..cols {
        let s = cell_text(screen, row, col);
        let s = s.trim();
        if s.is_empty() {
            continue;
        }
        if !FRAME_GLYPHS.contains(&s) {
            return false;
        }
        seen += 1;
    }
    seen >= COMPOSER_FRAME_MIN_CELLS
}

/// Column of the prompt glyph on `row`, searching the same leading
/// columns [`row_starts_with_prompt`] does.
fn prompt_glyph_col(screen: &vt100::Screen, row: u16) -> Option<u16> {
    let cols = screen.size().1.min(crate::ui::CLAUDE_PROMPT_SCAN_COLS);
    (0..cols).find(|&col| {
        let s = cell_text(screen, row, col);
        crate::ui::CLAUDE_PROMPT_GLYPHS.contains(&s.as_str())
    })
}

/// Whether the composer at `prompt_row` holds nothing.
///
/// Scans the **whole** row, not just the part right of the glyph: the
/// only cells allowed to be non-blank are the prompt glyph itself and
/// the box's own borders. Skipping the columns left of the glyph would
/// let a list widget's `2. no` sit there unseen, and the input block
/// must not extend onto wrapped rows either.
fn composer_is_empty(screen: &vt100::Screen, prompt_row: u16) -> bool {
    let Some(glyph_col) = prompt_glyph_col(screen, prompt_row) else {
        return false;
    };
    if resolve_input_row_last(screen, prompt_row) != prompt_row {
        return false;
    }
    let cols = screen.size().1;
    for col in 0..cols {
        if col == glyph_col {
            continue;
        }
        let s = cell_text(screen, prompt_row, col);
        let s = s.trim();
        if s.is_empty() || FRAME_GLYPHS.contains(&s) {
            continue;
        }
        return false;
    }
    true
}

/// Caret evidence that the composer, and not something else on screen,
/// currently owns input. Accepts both eras of Claude Code rendering:
/// the visible hardware cursor parked on the prompt row (2.x), and the
/// painted inverse caret cell older builds used.
fn caret_is_in_composer(screen: &vt100::Screen, prompt_row: u16) -> bool {
    caret_is_in_composer_block(screen, prompt_row, prompt_row)
}

/// As [`caret_is_in_composer`], but over a possibly wrapped input block
/// spanning `first..=last` — what a typed draft occupies.
fn caret_is_in_composer_block(screen: &vt100::Screen, first: u16, last: u16) -> bool {
    if !screen.hide_cursor() && (first..=last).contains(&screen.cursor_position().0) {
        return true;
    }
    let cols = screen.size().1;
    (first..=last)
        .any(|row| (0..cols).any(|col| screen.cell(row, col).is_some_and(|c| c.inverse())))
}

/// Scan the status rows around the composer for an "interrupt"
/// affordance.
///
/// `rows_above` extends the scan upward: Claude Code puts its footer
/// below the composer (0), Codex puts its working indicator directly
/// above it (1). The bound is the point — see [`BUSY_MARKERS`]. Scanning
/// the whole screen, as the Codex *nudge* path does, means a peer
/// message that merely quotes "esc to interrupt" sits in the transcript
/// and pins the pane at `user_turn_busy` for as long as it stays
/// visible, which on an idle pane is forever.
fn busy_near_composer(screen: &vt100::Screen, prompt_row: u16, rows_above: u16) -> bool {
    let rows = screen.size().0;
    let first = prompt_row.saturating_sub(rows_above);
    let mut haystack = String::new();
    for row in first..rows {
        if row == prompt_row {
            // The composer itself holds user text, not chrome.
            continue;
        }
        haystack.push_str(&row_text(screen, row).to_lowercase());
        haystack.push('\n');
    }
    BUSY_MARKERS.iter().any(|m| haystack.contains(m))
}

/// The current contents of the agent's input block, used as the draft
/// fingerprint.
///
/// This re-proves the *structure* on every read, and that is the whole
/// point of it. Readiness runs once, before the body is written; a
/// modal can appear during the settle window that follows, and a
/// permission menu's `❯ 1. Yes` row satisfies `find_prompt_row` just as
/// well as a composer does. A reader that only found "the bottom-most
/// prompt glyph" would adopt that row as the draft, watch it hold still
/// for 120ms, and press Enter on it — auto-approving the dialog. So the
/// framing is checked here too, and an unrecognized screen returns
/// `None`, which the state machine treats as "the composer disappeared"
/// and reports honestly rather than submitting into.
///
/// Unlike [`composer_is_empty`] it makes no demand of the *content* — a
/// draft is exactly what it expects to find.
pub(crate) fn composer_block_text(screen: &vt100::Screen, agent: TurnAgent) -> Option<String> {
    if screen.alternate_screen() {
        return None;
    }
    match agent {
        TurnAgent::Claude => {
            let prompt_row = find_prompt_row(screen)?;
            let last = resolve_input_row_last(screen, prompt_row);
            // Same sandwich the readiness predicate requires, but around
            // the whole (possibly wrapped) input block.
            if prompt_row == 0 || !row_is_composer_frame(screen, prompt_row.saturating_sub(1)) {
                return None;
            }
            if !row_is_composer_frame(screen, last.saturating_add(1)) {
                return None;
            }
            if !caret_is_in_composer_block(screen, prompt_row, last) {
                return None;
            }
            let mut out = String::new();
            for row in prompt_row..=last {
                out.push_str(&row_text(screen, row));
                out.push('\n');
            }
            Some(out)
        }
        TurnAgent::Codex => {
            let prompt_row = codex_prompt_row(screen)?;
            // Same reasoning as the Claude branch. `codex_prompt_row`
            // only rejects a *boxed* dialog (its rows start with `│`);
            // an unboxed `› 1. Yes` option list would pass. Requiring
            // the caret to sit in the composer's edit region is what
            // separates a draft renga typed from a menu selection.
            if screen.hide_cursor() {
                return None;
            }
            let (cursor_row, cursor_col) = screen.cursor_position();
            if cursor_row != prompt_row || cursor_col < CODEX_EDIT_COL {
                return None;
            }
            Some(format!("{}\n", row_text(screen, prompt_row)))
        }
    }
}

/// Dispatch the agent-specific readiness predicate for one screen.
pub(crate) fn turn_readiness_on_screen(agent: TurnAgent, screen: &vt100::Screen) -> TurnReadiness {
    match agent {
        TurnAgent::Claude => claude_turn_readiness(screen),
        TurnAgent::Codex => codex_turn_readiness(screen),
    }
}

/// Readiness for a Claude pane.
///
/// The composer is proven *positively*: a prompt glyph row sandwiched
/// between two frame rows, empty, with the caret in it. Claude Code
/// keeps that composer on screen while it works, so emptiness alone
/// does not mean idle — the interrupt affordance below it is what
/// separates "accepting a turn" from "mid-turn".
pub(crate) fn claude_turn_readiness(screen: &vt100::Screen) -> TurnReadiness {
    if screen.alternate_screen() {
        return TurnReadiness::Unsupported;
    }
    if !screen_has_visible_text(screen) {
        return TurnReadiness::NotReady;
    }
    let Some(prompt_row) = find_prompt_row(screen) else {
        return TurnReadiness::NotReady;
    };
    // A permission menu's `❯ 1. Yes` row is also "a row starting with a
    // prompt glyph". It fails here twice over: its neighbours are not
    // rules, and it is not empty.
    if prompt_row == 0 || !row_is_composer_frame(screen, prompt_row.saturating_sub(1)) {
        return TurnReadiness::NotReady;
    }
    if !row_is_composer_frame(screen, prompt_row.saturating_add(1)) {
        return TurnReadiness::NotReady;
    }
    if !composer_is_empty(screen, prompt_row) {
        return TurnReadiness::NotReady;
    }
    if !caret_is_in_composer(screen, prompt_row) {
        return TurnReadiness::NotReady;
    }
    if busy_near_composer(screen, prompt_row, 0) {
        return TurnReadiness::Busy;
    }
    TurnReadiness::Ready
}

/// Readiness for a Codex pane.
///
/// Reuses the structural half of the existing nudge gate
/// ([`codex_prompt_allows_peer_nudge_on_screen`]: bottom-most `›`
/// composer, visible cursor, cursor at the empty edit position) but
/// deliberately drops that path's `"enter to send"` / `"ready for
/// input"` string fallback. A nudge that lands late is a cosmetic
/// nuisance; a user turn typed into an unproven screen is not.
pub(crate) fn codex_turn_readiness(screen: &vt100::Screen) -> TurnReadiness {
    if screen.alternate_screen() {
        return TurnReadiness::Unsupported;
    }
    if !screen_has_visible_text(screen) {
        return TurnReadiness::NotReady;
    }
    let Some(prompt_row) = codex_prompt_row(screen) else {
        return TurnReadiness::NotReady;
    };
    // Codex paints its working indicator directly above the composer,
    // so the scan reaches one row up — but no further, so transcript
    // text cannot pin the pane at busy.
    if busy_near_composer(screen, prompt_row, 1) || codex_working_line_above(screen, prompt_row) {
        return TurnReadiness::Busy;
    }
    // `codex_prompt_allows_peer_nudge_on_screen` proves the caret is at
    // the composer's edit position, which it treats as proof of an
    // empty composer — but only when the caret is on the prompt row at
    // all. A human who moved the caret home mid-draft leaves the text
    // in place, so check the text itself too.
    if !codex_composer_is_empty(screen, prompt_row) {
        return TurnReadiness::NotReady;
    }
    // The nudge gate accepts a cursor on any row at or above the
    // prompt, which is fine for a nudge but not for authorizing a
    // write: an empty composer can stay painted while a dialog or
    // another widget owns input, and the caret is what says which.
    if screen.hide_cursor() {
        return TurnReadiness::NotReady;
    }
    let (cursor_row, cursor_col) = screen.cursor_position();
    if cursor_row != prompt_row || cursor_col > CODEX_EDIT_COL {
        return TurnReadiness::NotReady;
    }
    match codex_prompt_allows_peer_nudge_on_screen(screen) {
        Some(true) => TurnReadiness::Ready,
        _ => TurnReadiness::NotReady,
    }
}

/// Codex's status line (`• Working (2s • esc to interrupt)`) is followed
/// by blank spacer rows before the composer, so the 1-row scan misses
/// it (#364). Anchoring on the leading bullet keeps transcript text that
/// merely quotes the marker from pinning the pane.
fn codex_working_line_above(screen: &vt100::Screen, prompt_row: u16) -> bool {
    (prompt_row.saturating_sub(4)..prompt_row).any(|row| {
        let t = row_text(screen, row).to_lowercase();
        t.trim_start().starts_with('\u{2022}') && BUSY_MARKERS.iter().any(|m| t.contains(m))
    })
}

/// First editable column of Codex's composer — the glyph plus its
/// separating space. Mirrors the column
/// [`codex_prompt_allows_peer_nudge_on_screen`] treats as "empty".
const CODEX_EDIT_COL: u16 = 2;

/// Bottom-most row carrying Codex's `›` composer glyph, searched the
/// same way [`codex_prompt_allows_peer_nudge_on_screen`] searches.
fn codex_prompt_row(screen: &vt100::Screen) -> Option<u16> {
    let rows = screen.size().0;
    (0..rows)
        .rev()
        .find(|&row| row_text(screen, row).trim_start().starts_with('\u{203A}'))
}

/// Whether Codex's composer holds nothing: the `›` glyph and nothing
/// after it. Codex paints its idle hint ("Ask Codex to do anything") in
/// the dim attribute right in the edit position; typed text is never
/// dim, so dim cells are not a draft.
fn codex_composer_is_empty(screen: &vt100::Screen, prompt_row: u16) -> bool {
    (0..screen.size().1).all(|col| {
        screen.cell(prompt_row, col).is_none_or(|c| {
            let s = c.contents();
            c.dim() || s.trim().is_empty() || s == "\u{203A}"
        })
    })
}

// ── body handling ─────────────────────────────────────────────

/// Normalize a user-turn body, or explain why it cannot be typed.
///
/// Line endings collapse to `\n` so the multi-line decision downstream
/// sees one representation. Control characters are refused outright:
/// this string is written into another agent's PTY, so an `\x1b` in it
/// is a live escape sequence in somebody else's terminal, not a display
/// glitch — the same reasoning as `ipc::sanitized_label`. Channel
/// delivery is untouched by any of this and still accepts any body.
pub(crate) fn normalize_user_turn_body(body: &str) -> std::result::Result<String, ipc::CodedError> {
    // U+2028 / U+2029 are line separators that `char::is_control()` does
    // not catch (they are Zl/Zp, not Cc), so folding them here is what
    // keeps them from slipping past the multi-line branch below as if
    // they were ordinary text.
    let normalized = body
        .replace("\r\n", "\n")
        .replace(['\r', '\u{2028}', '\u{2029}'], "\n");
    // A trailing newline is what a heredoc, `$(cat file)` or a generated
    // string carries incidentally. Keeping it would make `/clear\n`
    // "multi-line" and refuse it with a reason that is false for it.
    // Only newlines: trimming whitespace generally would silently
    // reshape the delivered message, and would strip a trailing tab
    // past the rule below that exists to refuse it.
    let normalized = normalized.trim_end_matches('\n').to_string();
    if normalized.trim().is_empty() {
        return Err(ipc::CodedError::new(
            ipc::err_code::USER_TURN_INVALID_BODY,
            "deliver=\"user_turn\" needs a non-empty message".to_string(),
        ));
    }
    if normalized.len() > USER_TURN_MAX_BODY_BYTES {
        return Err(ipc::CodedError::new(
            ipc::err_code::USER_TURN_INVALID_BODY,
            format!(
                "message is {} bytes; deliver=\"user_turn\" types at most \
                 {USER_TURN_MAX_BODY_BYTES} into another agent's composer. Send a shorter body, \
                 or use deliver=\"channel\", which has no such limit.",
                normalized.len()
            ),
        ));
    }
    if let Some(bad) = normalized
        .chars()
        .find(|c| c.is_control() && *c != '\n' && *c != '\t')
    {
        return Err(ipc::CodedError::new(
            ipc::err_code::USER_TURN_INVALID_BODY,
            format!(
                "message contains the control character {bad:?}, which would reach the target's \
                 terminal as an escape sequence rather than as text"
            ),
        ));
    }
    Ok(normalized)
}

/// Bytes to write for `body`, or an error when the pane cannot accept
/// them.
///
/// A multi-line body typed raw submits at its first newline and drives
/// the UI with the remainder, so it goes out as a bracketed paste — and
/// only when the application has actually enabled bracketed paste,
/// which is renga's existing signal for "this app treats a paste as
/// composer content" (see `App::paste_to_pane`). Single-line bodies are
/// written verbatim so a leading `/` still reads as a slash command.
/// Whether `body` fits on one row of a Codex composer.
///
/// renga can fingerprint a wrapped Claude input block (`ui.rs` already
/// walks its continuation rows for caret rendering) but has no verified
/// model of how Codex lays a wrapped composer out — and guessing one
/// here would mean guessing which rows a bare Enter is about to submit.
/// So an over-long Codex body is refused before anything is written
/// rather than typed in and abandoned.
fn codex_body_fits_on_one_row(body: &str, pane: &Pane) -> bool {
    let cols = {
        let Ok(parser) = pane.parser.lock() else {
            return false;
        };
        parser.screen().size().1
    };
    let available = cols.saturating_sub(CODEX_EDIT_COL + 1) as usize;
    unicode_width::UnicodeWidthStr::width(body) <= available
}

fn user_turn_payload(
    body: &str,
    pane: &Pane,
    pane_id: usize,
    agent: TurnAgent,
) -> std::result::Result<Vec<u8>, ipc::CodedError> {
    // Codex composers are read one row at a time (see
    // `codex_body_fits_on_one_row`), and a hard newline makes a second
    // row just as surely as an over-long line wraps into one.
    if agent == TurnAgent::Codex && (body.contains('\n') || !codex_body_fits_on_one_row(body, pane))
    {
        return Err(ipc::CodedError::new(
            ipc::err_code::USER_TURN_INVALID_BODY,
            format!(
                "message needs more than one row of pane {pane_id}'s Codex composer, which is \
                 all renga can read back; it would be typed in and never submitted. Send a \
                 shorter single-line body, or use deliver=\"channel\"."
            ),
        ));
    }
    if !body.contains('\n') {
        // Raw bytes are keystrokes, and Tab is a bound key in both
        // agents (completion / queue-message), not composer text —
        // renga's own send_keys vocabulary lowers `Tab` to this byte.
        // Inside a bracketed paste below it is literal, so this refusal
        // is specific to the unwrapped path.
        if body.contains('\t') {
            return Err(ipc::CodedError::new(
                ipc::err_code::USER_TURN_INVALID_BODY,
                "message contains a tab, which the recipient reads as a Tab keypress rather \
                 than as text. Use spaces, or send it as a multi-line body so it goes out as \
                 a paste."
                    .to_string(),
            ));
        }
        return Ok(body.as_bytes().to_vec());
    }
    if !pane.is_bracketed_paste_enabled() {
        return Err(ipc::CodedError::new(
            ipc::err_code::USER_TURN_INVALID_BODY,
            format!(
                "message is multi-line but pane {pane_id} has not enabled bracketed paste; \
                 typing it raw would submit the first line and drive the UI with the rest. \
                 Send a single-line body, or use deliver=\"channel\"."
            ),
        ));
    }
    let mut out = Vec::with_capacity(body.len() + 12);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(body.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    Ok(out)
}

impl App {
    /// Which agent, if any, owns `pane_id`'s input.
    ///
    /// Registration is authoritative when present and the live OSC
    /// title is the fallback — the same precedence
    /// [`App::pane_expects_codex_peer_delivery`] uses, and for the same
    /// reason in reverse: an agent rewrites its window title to the
    /// in-flight task, so requiring the live title would make a
    /// registered pane unaddressable exactly while it is working.
    /// Neither signal authorizes writing on its own; the composer proof
    /// does that.
    pub(crate) fn user_turn_agent(&self, ws_index: usize, pane_id: usize) -> Option<TurnAgent> {
        match self.peer_client_kinds.get(&pane_id) {
            Some(PeerClientKind::Claude) => return Some(TurnAgent::Claude),
            Some(PeerClientKind::Codex) => return Some(TurnAgent::Codex),
            None => {}
        }
        let pane = self.workspaces.get(ws_index)?.panes.get(&pane_id)?;
        if pane.is_codex_running() {
            Some(TurnAgent::Codex)
        } else if pane.is_claude_running() {
            Some(TurnAgent::Claude)
        } else {
            None
        }
    }

    /// Full readiness predicate for one pane.
    pub(crate) fn user_turn_readiness(&self, ws_index: usize, pane_id: usize) -> TurnReadiness {
        let Some(pane) = self
            .workspaces
            .get(ws_index)
            .and_then(|w| w.panes.get(&pane_id))
        else {
            return TurnReadiness::NotReady;
        };
        // A pane whose startup command has not been flushed yet has no
        // agent behind it: the shell prompt hasn't even been observed.
        if pane.pending_startup.is_some() {
            return TurnReadiness::NotReady;
        }
        // A dead agent leaves its last frame painted, OSC title and all,
        // so the screen still "proves" an idle composer. `write_input`
        // on an exited pane silently writes nothing, which would be
        // reported as `user_turn_stalled` — "the body WAS typed" — about
        // a pane that received nothing at all.
        if pane.exited {
            return TurnReadiness::Unsupported;
        }
        // Every read below goes through `Screen::cell`, which honors the
        // scrollback offset — so a pane the human wheel-scrolled up is
        // judged on HISTORY. The live screen underneath may be showing
        // the very permission prompt this predicate exists to refuse.
        // Renga must not scroll the pane back down to look (that is the
        // human's view), so refuse instead.
        if pane.is_scrolled_back() {
            return TurnReadiness::NotReady;
        }
        let Some(agent) = self.user_turn_agent(ws_index, pane_id) else {
            return TurnReadiness::Unsupported;
        };
        let Ok(parser) = pane.parser.lock() else {
            return TurnReadiness::NotReady;
        };
        match agent {
            TurnAgent::Claude => claude_turn_readiness(parser.screen()),
            TurnAgent::Codex => codex_turn_readiness(parser.screen()),
        }
    }

    /// Re-prove the composer and write the body **in one critical
    /// section**, returning the pre-write snapshot the settle stage
    /// compares against.
    ///
    /// Readiness ran a few statements earlier, and the PTY reader
    /// thread can paint between then and now — a modal appearing in
    /// that window would otherwise be typed into. Holding the parser
    /// lock across the write closes the gap: the reader cannot paint
    /// while we hold it, so the screen we proved is the screen the
    /// bytes land on. A composer that cannot be proven here refuses
    /// with nothing written, exactly like the readiness check itself.
    fn snapshot_and_write_user_turn(
        &mut self,
        ws_index: usize,
        pane_id: usize,
        agent: TurnAgent,
        bytes: &[u8],
    ) -> std::result::Result<String, ipc::CodedError> {
        let pane = self
            .workspaces
            .get_mut(ws_index)
            .and_then(|w| w.panes.get_mut(&pane_id))
            .ok_or_else(|| {
                ipc::CodedError::new(
                    ipc::err_code::PANE_VANISHED,
                    format!("pane {pane_id} vanished before delivery"),
                )
            })?;
        if pane.exited {
            return Err(ipc::CodedError::new(
                ipc::err_code::USER_TURN_UNSUPPORTED_TARGET,
                format!("pane {pane_id}'s process has exited; nothing was written"),
            ));
        }
        // Cloning the handle rather than borrowing `pane.parser` is what
        // lets the guard outlive the immutable borrow and coexist with
        // the `&mut pane` the write needs.
        let parser = std::sync::Arc::clone(&pane.parser);
        let guard = parser.lock().map_err(|_| {
            TurnReadiness::NotReady
                .into_error(pane_id)
                .expect("NotReady always carries an error")
        })?;
        // The *full* predicate, not merely "a composer is readable":
        // `composer_block_text` accepts a draft and ignores busy chrome
        // on purpose, because after the write a draft is exactly what
        // it expects. Before the write, a draft or a busy footer that
        // appeared since the outer check must still refuse — otherwise
        // the body is appended to somebody's half-typed sentence and
        // the confirm stage submits the pair.
        if let Some(e) = turn_readiness_on_screen(agent, guard.screen()).into_error(pane_id) {
            return Err(e);
        }
        let Some(empty) = composer_block_text(guard.screen(), agent) else {
            return Err(TurnReadiness::NotReady
                .into_error(pane_id)
                .expect("NotReady always carries an error"));
        };
        write_input_to_pane(pane, bytes, false)?;
        if pane.exited {
            return Err(ipc::CodedError::new(
                ipc::err_code::USER_TURN_UNSUPPORTED_TARGET,
                format!(
                    "pane {pane_id}'s process exited while renga was writing to it; the write \
                     did not land"
                ),
            ));
        }
        drop(guard);
        #[cfg(test)]
        self.user_turn_writes.push((pane_id, bytes.to_vec()));
        Ok(empty)
    }

    /// Re-prove the draft and write Enter **in one critical section**.
    ///
    /// The mirror of [`Self::snapshot_and_write_user_turn`], and needed
    /// for the same reason with more at stake: the composer read that
    /// decided to submit happened a few statements earlier, and the PTY
    /// reader thread can replace the composer with a permission menu in
    /// between. A bare `\r` arriving there answers the menu. Holding the
    /// parser lock across the check and the write means the draft
    /// proved is the draft Enter lands on; anything else refuses and
    /// leaves the body sitting in the composer for a human to look at.
    pub(crate) fn submit_user_turn_enter(
        &mut self,
        ws_index: usize,
        pane_id: usize,
        agent: TurnAgent,
        expected: &str,
    ) -> std::result::Result<(), ipc::CodedError> {
        let pane = self
            .workspaces
            .get_mut(ws_index)
            .and_then(|w| w.panes.get_mut(&pane_id))
            .ok_or_else(|| stalled_error(pane_id, "the pane vanished before submit"))?;
        if pane.exited {
            return Err(stalled_error(
                pane_id,
                "the pane's process exited before submit",
            ));
        }
        let parser = std::sync::Arc::clone(&pane.parser);
        let guard = parser
            .lock()
            .map_err(|_| stalled_error(pane_id, "the pane's screen became unreadable"))?;
        match composer_block_text(guard.screen(), agent) {
            Some(current) if current == expected => {}
            _ => {
                return Err(stalled_error(
                    pane_id,
                    "the composer stopped holding the confirmed draft, so Enter was withheld",
                ))
            }
        }
        write_input_to_pane(pane, b"\r", false)?;
        if pane.exited {
            return Err(stalled_error(
                pane_id,
                "the pane's process exited while renga was submitting",
            ));
        }
        drop(guard);
        #[cfg(test)]
        self.user_turn_writes.push((pane_id, b"\r".to_vec()));
        Ok(())
    }

    /// Return true when an identical user turn was accepted within
    /// [`USER_TURN_DEDUPE_TTL`], recording the new one otherwise.
    /// Deliberately mirrors `is_duplicate_peer_send` — including the
    /// timestamp refresh and the TTL sweep — over its own map.
    fn is_duplicate_user_turn(&mut self, target: usize, from: usize, body: &str) -> bool {
        let now = Instant::now();
        self.recent_user_turn_sends
            .retain(|_, ts| now.duration_since(*ts) < USER_TURN_DEDUPE_TTL);
        let key = (target, from, body.to_string());
        match self.recent_user_turn_sends.get(&key).copied() {
            // Deliberately does NOT refresh the timestamp on a hit,
            // unlike `is_duplicate_peer_send`. Refreshing keeps a chatty
            // channel sender collapsed, which is what that window wants;
            // here it would mean a caller retrying a `user_turn_stalled`
            // every few seconds pushes the expiry back on every attempt
            // and can never get through.
            Some(prev) if now.duration_since(prev) < USER_TURN_DEDUPE_TTL => true,
            _ => false,
        }
    }

    fn record_user_turn(&mut self, target: usize, from: usize, body: &str) {
        self.recent_user_turn_sends
            .insert((target, from, body.to_string()), Instant::now());
    }

    /// Accept (or refuse) a `deliver="user_turn"` peer send.
    ///
    /// Everything that can fail without touching the target's PTY fails
    /// here, synchronously, and answers `reply` immediately. Once the
    /// body has been written the delivery is parked in
    /// [`Self::pending_user_turns`] and `reply` is answered later by
    /// [`Self::flush_pending_user_turns`].
    ///
    /// Never emits `Event::PeerInbox`: a user turn must not also show up
    /// as a `<channel>` tag or a Codex `check_messages` body.
    pub(crate) fn handle_peer_send_user_turn(
        &mut self,
        from_pane: usize,
        target: &PaneRef,
        body: String,
        reply: oneshot::Sender<std::result::Result<serde_json::Value, ipc::CodedError>>,
    ) {
        let mut reply = Some(reply);
        let mut answer = |outcome: std::result::Result<serde_json::Value, ipc::CodedError>| {
            if let Some(tx) = reply.take() {
                let _ = tx.send(outcome);
            }
        };

        let Some((sender_ws, _)) = self.resolve_pane_across_workspaces(&PaneRef::Id(from_pane))
        else {
            answer(Err(ipc::CodedError::new(
                ipc::err_code::PANE_NOT_FOUND,
                format!("sender pane {from_pane} not found"),
            )));
            return;
        };
        let Some((target_ws, target_id)) = self.resolve_target_from(sender_ws, target) else {
            answer(Err(ipc::CodedError::new(
                ipc::err_code::PANE_NOT_FOUND,
                format!(
                    "peer target not found: {target:?} (names only resolve inside the sender's \
                     tab; use the numeric pane id from list_peers for other tabs)"
                ),
            )));
            return;
        };

        let normalized = match normalize_user_turn_body(&body) {
            Ok(v) => v,
            Err(e) => {
                answer(Err(e));
                return;
            }
        };

        // A second delivery to the same composer while the first is
        // mid-flight would interleave two drafts and submit their
        // concatenation. Refuse rather than race.
        if self
            .pending_user_turns
            .iter()
            .any(|p| p.target_pane == target_id)
        {
            answer(Err(ipc::CodedError::new(
                ipc::err_code::USER_TURN_NOT_READY,
                format!(
                    "a user-turn delivery to pane {target_id} is already in flight; nothing was \
                     written. Retry once it settles."
                ),
            )));
            return;
        }

        // Same hazard, different writer: a queued Codex nudge types its
        // own text into that composer from `flush_pending_codex_peer_messages`,
        // which runs on the same frames this delivery does. Two writers,
        // one composer, one Enter — the submitted turn would be the
        // concatenation. Let the nudge finish first.
        if self.pending_codex_peer_messages.contains_key(&target_id) {
            answer(Err(ipc::CodedError::new(
                ipc::err_code::USER_TURN_NOT_READY,
                format!(
                    "pane {target_id} has a peer nudge still being typed into its composer; \
                     nothing was written. Retry once it has been delivered."
                ),
            )));
            return;
        }

        if self.is_duplicate_user_turn(target_id, from_pane, &normalized) {
            // Unlike the channel path, say so out loud. A caller that
            // just got `user_turn_stalled` needs to know its retry was
            // collapsed rather than delivered — the whole point of the
            // window is that an uncertain `/clear` is not fired twice.
            answer(Ok(duplicate_payload(target_id)));
            return;
        }

        if let Some(e) = self
            .user_turn_readiness(target_ws, target_id)
            .into_error(target_id)
        {
            answer(Err(e));
            return;
        }

        // Readiness already proved this resolves; unwrapping to Claude
        // would silently mis-read a Codex composer, so carry the real
        // answer forward.
        let Some(agent) = self.user_turn_agent(target_ws, target_id) else {
            answer(Err(TurnReadiness::Unsupported
                .into_error(target_id)
                .expect("Unsupported always carries an error")));
            return;
        };
        let Some(pane) = self
            .workspaces
            .get_mut(target_ws)
            .and_then(|w| w.panes.get_mut(&target_id))
        else {
            answer(Err(ipc::CodedError::new(
                ipc::err_code::PANE_VANISHED,
                format!("pane {target_id} vanished before delivery"),
            )));
            return;
        };
        let payload = match user_turn_payload(&normalized, pane, target_id, agent) {
            Ok(v) => v,
            Err(e) => {
                answer(Err(e));
                return;
            }
        };

        // Past this line bytes may be on the wire, so the dedupe entry
        // has to exist before the write — not after it, and not before
        // the refusals above, which must stay freely retryable.
        self.record_user_turn(target_id, from_pane, &normalized);

        let empty = match self.snapshot_and_write_user_turn(target_ws, target_id, agent, &payload) {
            Ok(v) => v,
            Err(e) => {
                // Nothing was written, so the ledger entry recorded a
                // moment ago would suppress a legitimate retry for
                // nothing. Take it back — the entry exists to cover
                // *uncertain* writes, not refused or failed ones.
                self.recent_user_turn_sends
                    .remove(&(target_id, from_pane, normalized.clone()));
                answer(Err(e));
                return;
            }
        };

        let now = Instant::now();
        self.pending_user_turns.push(PendingUserTurn {
            target_pane: target_id,
            reply: reply.take(),
            stage: UserTurnStage::AwaitDraft {
                ready_at: now + USER_TURN_SETTLE_DELAY,
                empty,
            },
            deadline: now + USER_TURN_DEADLINE,
        });
        self.dirty = true;
    }

    /// Read the target's composer, distinguishing "no composer on a
    /// screen we can read" from "we cannot read this pane".
    fn read_composer(&self, ws_index: usize, pane_id: usize) -> ComposerRead {
        let Some(agent) = self.user_turn_agent(ws_index, pane_id) else {
            return ComposerRead::Unreadable;
        };
        let Some(pane) = self
            .workspaces
            .get(ws_index)
            .and_then(|w| w.panes.get(&pane_id))
        else {
            return ComposerRead::Unreadable;
        };
        // A scrolled-back pane shows history, and an exited one shows a
        // frozen final frame; neither says anything about what the live
        // screen is doing now.
        if pane.exited || pane.is_scrolled_back() {
            return ComposerRead::Unreadable;
        }
        match pane.parser.lock() {
            Ok(parser) => ComposerRead::Readable(composer_block_text(parser.screen(), agent)),
            Err(_) => ComposerRead::Unreadable,
        }
    }

    /// Panes with a user-turn delivery in flight.
    ///
    /// `flush_pending_codex_peer_messages` consults this before typing
    /// a nudge: it runs earlier in the same frame, so without it a
    /// channel message that arrives *during* the settle window would
    /// type its nudge into a composer already holding our body, and the
    /// confirm stage would then submit the concatenation. The handler's
    /// own pre-flight check covers the opposite order.
    pub(crate) fn panes_with_user_turn_in_flight(&self) -> HashSet<usize> {
        self.pending_user_turns
            .iter()
            .map(|p| p.target_pane)
            .collect()
    }

    /// Advance every in-flight user turn by one frame. Called from the
    /// main event loop next to `flush_pending_codex_peer_messages`;
    /// cheap and a no-op when nothing is in flight.
    ///
    /// This never sleeps: each stage either makes progress against the
    /// current screen or leaves the delivery parked for the next frame.
    pub(crate) fn flush_pending_user_turns(&mut self) {
        self.flush_pending_user_turns_at(Instant::now());
    }

    /// [`Self::flush_pending_user_turns`] with the clock supplied, so
    /// tests can drive settle / confirm / deadline transitions without
    /// sleeping through them.
    pub(crate) fn flush_pending_user_turns_at(&mut self, now: Instant) {
        if self.pending_user_turns.is_empty() {
            return;
        }
        let mut still_pending = Vec::with_capacity(self.pending_user_turns.len());
        for mut pending in std::mem::take(&mut self.pending_user_turns) {
            match self.advance_user_turn(&mut pending, now) {
                Some(outcome) => {
                    pending.answer(outcome);
                    self.dirty = true;
                }
                None => still_pending.push(pending),
            }
        }
        self.pending_user_turns = still_pending;
    }

    /// Advance one delivery by a frame, performing the I/O
    /// [`step_user_turn`] asks for. `Some(_)` is terminal.
    fn advance_user_turn(
        &mut self,
        pending: &mut PendingUserTurn,
        now: Instant,
    ) -> Option<std::result::Result<serde_json::Value, ipc::CodedError>> {
        let target_id = pending.target_pane;
        let Some((ws_index, _)) = self.resolve_pane_across_workspaces(&PaneRef::Id(target_id))
        else {
            return Some(Err(ipc::CodedError::new(
                ipc::err_code::PANE_VANISHED,
                format!(
                    "pane {target_id} closed mid-delivery; the body had already been written, so \
                     whether it was submitted is unknown"
                ),
            )));
        };
        let composer = self.read_composer(ws_index, target_id);

        match step_user_turn(&pending.stage, &composer, now, pending.deadline) {
            UserTurnStep::Wait => None,
            UserTurnStep::Advance(stage) => {
                pending.stage = stage;
                None
            }
            UserTurnStep::Submit(stage) => {
                // Enter goes out as its own write, deliberately: the
                // agent has to have taken the body as input before the
                // submit key arrives, which is exactly what a combined
                // write does not guarantee.
                let UserTurnStage::AwaitSubmit { draft } = &stage else {
                    return Some(Err(stalled_error(target_id, "internal: bad submit stage")));
                };
                let Some(agent) = self.user_turn_agent(ws_index, target_id) else {
                    return Some(Err(stalled_error(
                        target_id,
                        "the pane stopped being an agent pane before submit",
                    )));
                };
                let draft = draft.clone();
                if let Err(e) = self.submit_user_turn_enter(ws_index, target_id, agent, &draft) {
                    return Some(Err(e));
                }
                pending.stage = stage;
                None
            }
            UserTurnStep::Submitted => Some(Ok(submitted_payload(target_id))),
            UserTurnStep::Stalled(detail) => Some(Err(stalled_error(target_id, detail))),
        }
    }
}
