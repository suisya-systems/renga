//! `deliver="user_turn"` (Issue #323).
//!
//! Two halves, deliberately kept apart:
//!
//! - the **readiness predicate**, driven against synthetic vt100
//!   screens copied from what real Claude Code / Codex panes actually
//!   render (captured with `inspect_pane` against live panes), and
//!
//! - the **delivery state machine**, driven through the pure
//!   [`step_user_turn`] with explicit clocks and explicit composer
//!   contents, so none of it depends on sleeping next to a live PTY.

use super::super::user_turn::{
    claude_turn_readiness, codex_turn_readiness, normalize_user_turn_body, step_user_turn,
    ComposerRead, TurnAgent, TurnReadiness, UserTurnStage, UserTurnStep, USER_TURN_CONFIRM_DELAY,
    USER_TURN_DEADLINE, USER_TURN_SETTLE_DELAY,
};
use super::super::*;

/// Paint `bytes` onto the focused pane's vt100 screen without going
/// near its PTY, and hand back the pane id.
fn seed_focused_pane_screen(app: &mut App, bytes: &[u8]) -> usize {
    let pane_id = app.ws().focused_pane_id;
    let pane = app
        .ws_mut()
        .panes
        .get_mut(&pane_id)
        .expect("focused pane exists");
    let mut parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
    parser.process(bytes);
    drop(parser);
    pane_id
}

/// Run a screen through the Claude predicate in isolation.
fn claude_readiness_of(bytes: &[u8], rows: u16, cols: u16) -> TurnReadiness {
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(bytes);
    claude_turn_readiness(parser.screen())
}

fn codex_readiness_of(bytes: &[u8], rows: u16, cols: u16) -> TurnReadiness {
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(bytes);
    codex_turn_readiness(parser.screen())
}

/// The composer as Claude Code 2.x actually draws it: a horizontal
/// rule, the prompt glyph alone on its own row with the hardware cursor
/// parked just after it, another rule, then the mode footer.
///
/// Captured from a live pane — this exact shape (not the older
/// `╭──╮` box) is what the predicate has to accept.
fn claude_idle_screen(footer: &str) -> Vec<u8> {
    let rule = "─".repeat(40);
    // \x1b[?25h keeps the cursor visible; the final CUP parks it on the
    // prompt row at the first edit cell, exactly like Claude does.
    format!(
        "\x1b[2J\x1b[H\x1b[?25hsome transcript text\r\n\r\n{rule}\r\n\u{276F}\r\n{rule}\r\n{footer}\x1b[4;3H"
    )
    .into_bytes()
}

// ── readiness: Claude ─────────────────────────────────────────

#[test]
fn claude_idle_composer_is_ready() {
    assert_eq!(
        claude_readiness_of(
            &claude_idle_screen("\u{23F5}\u{23F5} auto mode on (shift+tab to cycle)"),
            8,
            40
        ),
        TurnReadiness::Ready
    );
}

/// Claude Code keeps an *empty* composer on screen while it works —
/// verified against three live busy panes — so emptiness alone says
/// nothing about idleness. The interrupt affordance in the footer is
/// what separates "accepting a turn" from "mid-turn", and getting this
/// wrong means every delivery races a permission dialog.
#[test]
fn claude_busy_footer_is_busy_not_ready() {
    assert_eq!(
        claude_readiness_of(
            &claude_idle_screen("\u{23F5}\u{23F5} auto mode on \u{00B7} esc to interrupt"),
            8,
            40
        ),
        TurnReadiness::Busy
    );
}

/// A permission menu's option row also "starts with a prompt glyph".
/// It must not be mistaken for a composer — that is the exact failure
/// the #323 design note calls out in herdr's `agent.prompt`.
#[test]
fn claude_permission_dialog_is_not_ready() {
    let screen = "\x1b[2J\x1b[H\x1b[?25h\
         \u{256D}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{256E}\r\n\
         \u{2502} Bash command   \u{2502}\r\n\
         \u{2502}                \u{2502}\r\n\
         \u{2502} \u{276F} 1. Yes       \u{2502}\r\n\
         \u{2502}   2. No        \u{2502}\r\n\
         \u{2570}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{256F}\x1b[4;4H";
    assert_eq!(
        claude_readiness_of(screen.as_bytes(), 8, 40),
        TurnReadiness::NotReady
    );
}

/// A folder-trust / "load development channel?" style dialog has the
/// same shape and must be refused the same way — answering it stays
/// `send_keys`' job.
#[test]
fn claude_trust_dialog_is_not_ready() {
    let screen = "\x1b[2J\x1b[H\x1b[?25hDo you trust the files in this folder?\r\n\r\n\
         \u{276F} 1. Yes, proceed\r\n  2. No, exit\x1b[3;3H";
    assert_eq!(
        claude_readiness_of(screen.as_bytes(), 8, 40),
        TurnReadiness::NotReady
    );
}

/// A human's half-typed draft owns the composer. Delivering into it
/// would submit their words concatenated with ours.
#[test]
fn claude_composer_holding_a_draft_is_not_ready() {
    let rule = "─".repeat(40);
    let screen = format!(
        "\x1b[2J\x1b[H\x1b[?25h\r\n{rule}\r\n\u{276F} half-typed thought\r\n{rule}\r\n? for shortcuts\x1b[3;22H"
    );
    assert_eq!(
        claude_readiness_of(screen.as_bytes(), 8, 40),
        TurnReadiness::NotReady
    );
}

/// The frame rows are half the structural proof. Without them any
/// bottom-most `>` on screen — a shell prompt, a quoted line of
/// transcript — would read as a composer.
#[test]
fn claude_prompt_glyph_without_frame_rows_is_not_ready() {
    let screen = "\x1b[2J\x1b[H\x1b[?25hsome output\r\n\u{276F}\r\nmore output\x1b[2;3H";
    assert_eq!(
        claude_readiness_of(screen.as_bytes(), 8, 40),
        TurnReadiness::NotReady
    );
}

/// The composer must also own the caret. A cursor parked elsewhere
/// means something else is taking input.
#[test]
fn claude_caret_outside_composer_is_not_ready() {
    let rule = "─".repeat(40);
    // Same idle composer, but the cursor is left up in the transcript.
    let screen = format!(
        "\x1b[2J\x1b[H\x1b[?25htranscript\r\n{rule}\r\n\u{276F}\r\n{rule}\r\nfooter\x1b[1;1H"
    );
    assert_eq!(
        claude_readiness_of(screen.as_bytes(), 8, 40),
        TurnReadiness::NotReady
    );
}

/// A hidden cursor is fine when the older inverse-video caret cell is
/// painted in the composer instead — Claude Code has shipped both.
#[test]
fn claude_inverse_caret_satisfies_the_caret_check() {
    let rule = "─".repeat(40);
    let screen = format!(
        "\x1b[2J\x1b[H\x1b[?25ltranscript\r\n{rule}\r\n\u{276F} \x1b[7m \x1b[0m\r\n{rule}\r\nfooter"
    );
    assert_eq!(
        claude_readiness_of(screen.as_bytes(), 8, 40),
        TurnReadiness::Ready
    );
}

/// A full-screen TUI (vim, lazygit) has taken the terminal; there is no
/// composer to be found and retrying will not change that.
#[test]
fn claude_alternate_screen_is_unsupported() {
    let mut bytes = b"\x1b[?1049h".to_vec();
    bytes.extend_from_slice(&claude_idle_screen("footer"));
    assert_eq!(
        claude_readiness_of(&bytes, 8, 40),
        TurnReadiness::Unsupported
    );
}

/// A blank screen is a pane that has not painted yet, not a refusal to
/// take turns — the caller should retry.
#[test]
fn claude_blank_screen_is_not_ready() {
    assert_eq!(
        claude_readiness_of(b"\x1b[2J\x1b[H", 8, 40),
        TurnReadiness::NotReady
    );
}

// ── readiness: Codex ──────────────────────────────────────────

#[test]
fn codex_ready_prompt_is_ready() {
    assert_eq!(
        codex_readiness_of(b"\x1b[2J\x1b[H\x1b[?25h\xE2\x80\xBA \x1b[1;3H", 8, 40),
        TurnReadiness::Ready
    );
}

/// Codex paints its working indicator on the row directly above the
/// composer, which is why the busy scan reaches one row up there and
/// not for Claude.
#[test]
fn codex_busy_banner_is_busy() {
    assert_eq!(
        codex_readiness_of(
            b"\x1b[2J\x1b[H\x1b[?25hworking\xE2\x80\xA6 esc to interrupt\r\n\xE2\x80\xBA \x1b[2;3H",
            8,
            40
        ),
        TurnReadiness::Busy
    );
}

/// ...and no further up. A peer message that merely quotes the phrase
/// sits in the transcript; treating that as "mid-turn" would refuse
/// every user turn to that pane until the text scrolls off, which on an
/// idle pane never happens.
#[test]
fn codex_transcript_quoting_a_busy_marker_does_not_pin_the_pane() {
    assert_eq!(
        codex_readiness_of(
            b"\x1b[2J\x1b[H\x1b[?25hpeer said: if it hangs, esc to interrupt\r\n\r\n\xE2\x80\xBA \x1b[3;3H",
            8,
            40
        ),
        TurnReadiness::Ready
    );
}

/// The nudge path accepts a bare `ready for input` banner as good
/// enough. A user turn does not: a late nudge is a nuisance, a turn
/// typed into an unproven screen is damage. This pins the difference.
#[test]
fn codex_ready_for_input_string_alone_is_not_enough() {
    assert_eq!(
        codex_readiness_of(b"\x1b[2J\x1b[Hready for input", 8, 40),
        TurnReadiness::NotReady
    );
    // ...while the existing nudge gate still accepts it, unchanged.
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = seed_focused_pane_screen(&mut app, b"\x1b[2J\x1b[Hready for input");
    let pane = app.ws().panes.get(&pane_id).expect("pane");
    assert!(App::codex_peer_delivery_ready(true, pane));
    app.shutdown();
}

// ── agent resolution ──────────────────────────────────────────

/// Registration is authoritative, matching
/// `pane_expects_codex_peer_delivery`. Requiring the live OSC title
/// instead would make a pane unaddressable exactly while it works,
/// because agents rewrite that title to the in-flight task.
#[test]
fn registered_kind_beats_the_window_title() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    app.peer_client_kinds
        .insert(pane_id, PeerClientKind::Claude);
    assert_eq!(app.user_turn_agent(0, pane_id), Some(TurnAgent::Claude));

    app.peer_client_kinds.insert(pane_id, PeerClientKind::Codex);
    assert_eq!(app.user_turn_agent(0, pane_id), Some(TurnAgent::Codex));
    app.shutdown();
}

/// An unregistered plain shell is not a turn-taking target.
#[test]
fn unregistered_shell_pane_has_no_turn_agent() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    assert_eq!(app.user_turn_agent(0, pane_id), None);
    assert_eq!(
        app.user_turn_readiness(0, pane_id),
        TurnReadiness::Unsupported
    );
    app.shutdown();
}

// ── body normalization ────────────────────────────────────────

#[test]
fn body_normalization_collapses_line_endings() {
    assert_eq!(
        normalize_user_turn_body("a\r\nb\rc").expect("normalizes"),
        "a\nb\nc"
    );
}

#[test]
fn empty_body_is_rejected() {
    let err = normalize_user_turn_body("   \n  ").expect_err("empty body refused");
    assert_eq!(err.code, Some(ipc::err_code::USER_TURN_INVALID_BODY));
}

/// The body is written into somebody else's PTY, so an escape byte in
/// it is a live control sequence in their terminal, not a display
/// glitch — the same reasoning as `ipc::sanitized_label`.
#[test]
fn control_characters_in_body_are_rejected() {
    let err = normalize_user_turn_body("hello\x1b[31mworld").expect_err("escape refused");
    assert_eq!(err.code, Some(ipc::err_code::USER_TURN_INVALID_BODY));
    // Tab and newline are ordinary composer content and stay allowed.
    assert!(normalize_user_turn_body("a\tb\nc").is_ok());
}

// ── delivery state machine ────────────────────────────────────

fn t0() -> Instant {
    Instant::now()
}

/// What an empty Claude composer's input block reads as: the prompt
/// glyph and nothing else. Not blank — which is the whole reason the
/// machine carries a pre-write reference instead of testing for blank.
const EMPTY: &str = "\u{276F}\n";

/// An empty Claude composer renders as its prompt glyph, not as blank
/// text — so "the block is non-empty" is true before a single byte
/// arrives. The settle stage compares against the pre-write snapshot
/// instead; without that, the machine submits into an empty composer.
#[test]
fn settle_stage_waits_for_the_composer_to_differ_from_the_pre_write_snapshot() {
    let now = t0();
    let stage = UserTurnStage::AwaitDraft {
        ready_at: now,
        empty: EMPTY.to_string(),
    };
    let deadline = now + USER_TURN_DEADLINE;

    // Composer still reads exactly as it did before the write.
    assert_eq!(
        step_user_turn(&stage, &ComposerRead::text(EMPTY), now, deadline),
        UserTurnStep::Wait
    );

    // Draft visible → move to confirmation.
    match step_user_turn(
        &stage,
        &ComposerRead::text("\u{276F} /loop\n"),
        now,
        deadline,
    ) {
        UserTurnStep::Advance(UserTurnStage::AwaitConfirm {
            draft, restarts, ..
        }) => {
            assert_eq!(draft, "\u{276F} /loop\n");
            assert_eq!(restarts, 0);
        }
        other => panic!("expected AwaitConfirm, got {other:?}"),
    }
}

#[test]
fn settle_stage_holds_until_the_settle_delay_elapses() {
    let now = t0();
    let stage = UserTurnStage::AwaitDraft {
        ready_at: now + USER_TURN_SETTLE_DELAY,
        empty: EMPTY.to_string(),
    };
    assert_eq!(
        step_user_turn(
            &stage,
            &ComposerRead::text("\u{276F} hi\n"),
            now,
            now + USER_TURN_DEADLINE
        ),
        UserTurnStep::Wait
    );
}

#[test]
fn a_stable_draft_is_submitted() {
    let now = t0();
    let stage = UserTurnStage::AwaitConfirm {
        ready_at: now,
        empty: EMPTY.to_string(),
        draft: "\u{276F} /loop\n".to_string(),
        restarts: 0,
    };
    match step_user_turn(
        &stage,
        &ComposerRead::text("\u{276F} /loop\n"),
        now,
        now + USER_TURN_DEADLINE,
    ) {
        UserTurnStep::Submit(UserTurnStage::AwaitSubmit { draft }) => {
            assert_eq!(draft, "\u{276F} /loop\n")
        }
        other => panic!("expected Submit, got {other:?}"),
    }
}

/// A draft still being repainted restarts the stability window rather
/// than being submitted half-drawn.
#[test]
fn a_changing_draft_restarts_the_stability_window() {
    let now = t0();
    let stage = UserTurnStage::AwaitConfirm {
        ready_at: now,
        empty: EMPTY.to_string(),
        draft: "\u{276F} /lo\n".to_string(),
        restarts: 0,
    };
    match step_user_turn(
        &stage,
        &ComposerRead::text("\u{276F} /loop\n"),
        now,
        now + USER_TURN_DEADLINE,
    ) {
        UserTurnStep::Advance(UserTurnStage::AwaitConfirm {
            draft,
            restarts,
            ready_at,
            ..
        }) => {
            assert_eq!(draft, "\u{276F} /loop\n");
            assert_eq!(restarts, 1);
            assert_eq!(ready_at, now + USER_TURN_CONFIRM_DELAY);
        }
        other => panic!("expected a restarted AwaitConfirm, got {other:?}"),
    }
}

/// A composer that never settles is somebody else's — a human typing,
/// most likely. Give up rather than submit whatever they paused on.
#[test]
fn an_endlessly_changing_draft_stalls_instead_of_submitting() {
    let now = t0();
    let stage = UserTurnStage::AwaitConfirm {
        ready_at: now,
        empty: EMPTY.to_string(),
        draft: "\u{276F} a\n".to_string(),
        restarts: 3,
    };
    assert!(matches!(
        step_user_turn(
            &stage,
            &ComposerRead::text("\u{276F} ab\n"),
            now,
            now + USER_TURN_DEADLINE
        ),
        UserTurnStep::Stalled(_)
    ));
}

#[test]
fn a_consumed_draft_counts_as_submitted() {
    let now = t0();
    let stage = UserTurnStage::AwaitSubmit {
        draft: "\u{276F} /loop\n".to_string(),
    };
    // Composer back to empty — the ordinary case.
    assert_eq!(
        step_user_turn(
            &stage,
            &ComposerRead::text("\u{276F}\n"),
            now,
            now + USER_TURN_DEADLINE
        ),
        UserTurnStep::Submitted
    );
    // `/clear` repaints the screen out from under us; the composer may
    // not be findable at all. The draft is still gone.
    assert_eq!(
        step_user_turn(&stage, &ComposerRead::gone(), now, now + USER_TURN_DEADLINE),
        UserTurnStep::Submitted
    );
}

/// A spinner repaint is not a submit. Only the draft's disappearance
/// counts, so an unchanged composer keeps waiting and eventually
/// stalls — it never reports success it did not observe.
#[test]
fn an_unconsumed_draft_waits_then_stalls() {
    let now = t0();
    let stage = UserTurnStage::AwaitSubmit {
        draft: "\u{276F} /loop\n".to_string(),
    };
    assert_eq!(
        step_user_turn(
            &stage,
            &ComposerRead::text("\u{276F} /loop\n"),
            now,
            now + USER_TURN_DEADLINE
        ),
        UserTurnStep::Wait
    );
    assert!(matches!(
        step_user_turn(&stage, &ComposerRead::text("\u{276F} /loop\n"), now, now),
        UserTurnStep::Stalled(_)
    ));
}

// ── handler-level behavior ────────────────────────────────────

fn user_turn_result(
    app: &mut App,
    from: usize,
    target: ipc::PaneRef,
    body: &str,
) -> std::result::Result<serde_json::Value, ipc::CodedError> {
    let (tx, rx) = oneshot::channel();
    app.handle_peer_send_user_turn(from, &target, body.to_string(), tx);
    rx.try_recv().expect("handler answered synchronously")
}

/// Refusals must be provably byte-free — that is what makes them
/// retryable — and must never leak the body out as a channel tag
/// instead.
#[test]
fn refusal_writes_nothing_and_emits_no_peer_inbox() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    // Bound to the pane the refused turn addresses. A leaked
    // `PeerInbox` could only carry `target_pane == pane_id`, so this is
    // the narrowest receiver that would still catch one — and the one
    // that catches it with no other pane's traffic mixed in. An
    // unscoped subscription would work too, just noisily.
    let (_sub_id, rx) = app
        .event_bus
        .subscribe_scoped(ipc::EventScope::PaneInbox(pane_id));
    while rx.try_recv().is_ok() {}

    // Plain shell pane: no turn-taking agent behind it.
    let err = user_turn_result(&mut app, pane_id, ipc::PaneRef::Id(pane_id), "/loop")
        .expect_err("unsupported target");
    assert_eq!(err.code, Some(ipc::err_code::USER_TURN_UNSUPPORTED_TARGET));

    while let Ok(ev) = rx.try_recv() {
        assert!(
            !matches!(ev, ipc::Event::PeerInbox { .. }),
            "a user turn must never also arrive as a channel tag: {ev:?}"
        );
    }
    assert!(app.pending_user_turns.is_empty());
    app.shutdown();
}

#[test]
fn unknown_target_is_pane_not_found() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    let err = user_turn_result(&mut app, pane_id, ipc::PaneRef::Id(9999), "hi")
        .expect_err("unknown target");
    assert_eq!(err.code, Some(ipc::err_code::PANE_NOT_FOUND));
    app.shutdown();
}

/// A refused delivery records nothing in the dedupe ledger, so the
/// caller's retry after clearing the blocker is not swallowed.
#[test]
fn a_refused_user_turn_leaves_no_dedupe_trace() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    let _ = user_turn_result(&mut app, pane_id, ipc::PaneRef::Id(pane_id), "/loop");
    assert!(
        app.recent_user_turn_sends.is_empty(),
        "a refusal must stay freely retryable"
    );
    app.shutdown();
}

/// Paint an idle Claude composer onto the focused pane, sized to that
/// pane's *actual* PTY geometry — a pane is smaller than the terminal
/// (status bar, borders), so a fixed-width rule would wrap and shift
/// every row the predicate looks at.
fn seed_claude_idle_pane(app: &mut App, prefix: &[u8]) -> usize {
    let pane_id = app.ws().focused_pane_id;
    let (rows, cols) = {
        let pane = app.ws().panes.get(&pane_id).expect("focused pane exists");
        let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
        parser.screen().size()
    };
    assert!(rows >= 5 && cols >= 8, "test pane too small: {rows}x{cols}");
    let rule = "─".repeat(cols.saturating_sub(1) as usize);
    let mut bytes = prefix.to_vec();
    bytes.extend_from_slice(
        format!(
            "\x1b[2J\x1b[H\x1b[?25htranscript\
             \x1b[{};1H{rule}\
             \x1b[{};1H\u{276F}\
             \x1b[{};1H{rule}\
             \x1b[{};1H\u{23F5}\u{23F5} auto mode on (shift+tab to cycle)\
             \x1b[{};3H",
            rows - 3,
            rows - 2,
            rows - 1,
            rows,
            rows - 2,
        )
        .as_bytes(),
    );
    seed_focused_pane_screen(app, &bytes)
}

/// Repaint the focused pane's composer so it holds `draft`.
fn seed_claude_draft_pane(app: &mut App, draft: &str) {
    let pane_id = app.ws().focused_pane_id;
    let (rows, cols) = {
        let pane = app.ws().panes.get(&pane_id).expect("focused pane exists");
        let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
        parser.screen().size()
    };
    let rule = "─".repeat(cols.saturating_sub(1) as usize);
    let col = draft.chars().count() as u16 + 3;
    let bytes = format!(
        "\x1b[2J\x1b[H\x1b[?25htranscript\
         \x1b[{};1H{rule}\
         \x1b[{};1H\u{276F} {draft}\
         \x1b[{};1H{rule}\
         \x1b[{};1H\u{23F5}\u{23F5} auto mode on (shift+tab to cycle)\
         \x1b[{};{col}H",
        rows - 3,
        rows - 2,
        rows - 1,
        rows,
        rows - 2,
    );
    seed_focused_pane_screen(app, bytes.as_bytes());
}

/// Paint a Claude permission dialog — the shape whose option row also
/// starts with a prompt glyph.
fn seed_claude_dialog_pane(app: &mut App) {
    let pane_id = app.ws().focused_pane_id;
    let rows = {
        let pane = app.ws().panes.get(&pane_id).expect("focused pane exists");
        let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
        parser.screen().size().0
    };
    let bytes = format!(
        "\x1b[2J\x1b[H\x1b[?25h\
         \x1b[{};1H\u{256D}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{256E}\
         \x1b[{};1H\u{2502} Bash command   \u{2502}\
         \x1b[{};1H\u{2502} \u{276F} 1. Yes       \u{2502}\
         \x1b[{};1H\u{2502}   2. No        \u{2502}\
         \x1b[{};1H\u{2570}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{256F}\
         \x1b[{};4H",
        rows - 4,
        rows - 3,
        rows - 2,
        rows - 1,
        rows,
        rows - 2,
    );
    seed_focused_pane_screen(app, bytes.as_bytes());
}

/// Wait for the pane's real login shell to stop painting.
///
/// `App::new` spawns `$SHELL --login` on a genuine PTY whose reader
/// thread writes into the same parser these tests seed — and for
/// bash/zsh renga also injects a setup line ending in `clear`. A seed
/// laid down while that is still arriving gets overwritten between the
/// seed and the assertion, which is a flake with a ~1-in-4 rate under
/// a parallel `cargo test`. Waiting for two identical screen reads
/// costs a few tens of milliseconds once per test and removes the race
/// rather than narrowing it.
fn wait_for_pane_quiet(app: &App, pane_id: usize) {
    let read = || {
        let pane = app.ws().panes.get(&pane_id).expect("pane");
        let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
        let screen = parser.screen();
        let (rows, cols) = screen.size();
        let mut out = String::with_capacity((rows as usize) * (cols as usize));
        for row in 0..rows {
            for col in 0..cols {
                out.push_str(
                    &screen
                        .cell(row, col)
                        .map(|c| c.contents().to_string())
                        .unwrap_or_default(),
                );
            }
        }
        out
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut previous = read();
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(40));
        let current = read();
        if current == previous {
            return;
        }
        previous = current;
    }
}

/// Stand up a pane that the predicate will accept: registered as
/// Claude, painting an idle composer.
fn app_with_ready_claude_pane() -> (App, usize) {
    let mut app = App::new(40, 120).expect("App::new");
    wait_for_pane_quiet(&app, app.ws().focused_pane_id);
    let pane_id = seed_claude_idle_pane(&mut app, b"");
    app.peer_client_kinds
        .insert(pane_id, PeerClientKind::Claude);
    (app, pane_id)
}

/// The happy path defers: the handler writes the body and parks the
/// reply rather than answering, because "did this submit?" cannot be
/// known yet.
#[test]
fn an_accepted_user_turn_is_parked_not_answered() {
    let (mut app, pane_id) = app_with_ready_claude_pane();
    assert_eq!(app.user_turn_readiness(0, pane_id), TurnReadiness::Ready);

    let (tx, rx) = oneshot::channel();
    app.handle_peer_send_user_turn(pane_id, &ipc::PaneRef::Id(pane_id), "/loop".into(), tx);

    assert!(
        rx.try_recv().is_err(),
        "an accepted delivery must not answer before it has observed a submit"
    );
    assert_eq!(app.pending_user_turns.len(), 1);
    assert_eq!(
        app.recent_user_turn_sends.len(),
        1,
        "the dedupe entry must exist before the bytes, not after"
    );
    app.shutdown();
}

/// Two drafts in one composer would submit their concatenation.
#[test]
fn a_second_delivery_while_one_is_in_flight_is_refused() {
    let (mut app, pane_id) = app_with_ready_claude_pane();
    let (tx, _rx) = oneshot::channel();
    app.handle_peer_send_user_turn(pane_id, &ipc::PaneRef::Id(pane_id), "/loop".into(), tx);
    assert_eq!(app.pending_user_turns.len(), 1);

    let err = user_turn_result(&mut app, pane_id, ipc::PaneRef::Id(pane_id), "different")
        .expect_err("concurrent delivery refused");
    assert_eq!(err.code, Some(ipc::err_code::USER_TURN_NOT_READY));
    app.shutdown();
}

/// An identical retry inside the window is collapsed — and says so,
/// unlike the channel path's indistinguishable `Ok`. A caller that just
/// got `user_turn_stalled` has to be able to tell "your retry was
/// swallowed" from "your retry was delivered", or it will keep firing
/// `/clear` at a pane it already cleared.
#[test]
fn an_identical_user_turn_within_the_window_is_suppressed() {
    let (mut app, pane_id) = app_with_ready_claude_pane();
    let (tx, _rx) = oneshot::channel();
    app.handle_peer_send_user_turn(pane_id, &ipc::PaneRef::Id(pane_id), "/loop".into(), tx);
    // Retire the in-flight delivery so the retry reaches the dedupe
    // check rather than the concurrency guard.
    app.pending_user_turns.clear();

    let out = user_turn_result(&mut app, pane_id, ipc::PaneRef::Id(pane_id), "/loop")
        .expect("duplicate reports success");
    assert_eq!(
        out.get("status").and_then(|v| v.as_str()),
        Some("duplicate_suppressed")
    );
    assert!(
        app.pending_user_turns.is_empty(),
        "a suppressed retry must not write a second time"
    );
    app.shutdown();
}

/// A multi-line body typed raw would submit at its first newline and
/// drive the UI with the rest. Without bracketed paste there is no safe
/// encoding, so it is refused before anything is written.
#[test]
fn a_multiline_body_without_bracketed_paste_is_refused() {
    let (mut app, pane_id) = app_with_ready_claude_pane();
    let err = user_turn_result(
        &mut app,
        pane_id,
        ipc::PaneRef::Id(pane_id),
        "line one\nline two",
    )
    .expect_err("multi-line without bracketed paste refused");
    assert_eq!(err.code, Some(ipc::err_code::USER_TURN_INVALID_BODY));
    assert!(app.pending_user_turns.is_empty());
    assert!(
        app.recent_user_turn_sends.is_empty(),
        "a refusal must stay freely retryable"
    );
    app.shutdown();
}

/// With bracketed paste enabled the same body is accepted, because the
/// application has told us it treats a paste as composer content.
#[test]
fn a_multiline_body_is_accepted_when_bracketed_paste_is_on() {
    let mut app = App::new(40, 120).expect("App::new");
    // `\x1b[?2004h` is the application declaring it handles pastes.
    let pane_id = seed_claude_idle_pane(&mut app, b"\x1b[?2004h");
    app.peer_client_kinds
        .insert(pane_id, PeerClientKind::Claude);

    let (tx, rx) = oneshot::channel();
    app.handle_peer_send_user_turn(
        pane_id,
        &ipc::PaneRef::Id(pane_id),
        "line one\nline two".into(),
        tx,
    );
    assert!(rx.try_recv().is_err(), "accepted, so deferred");
    assert_eq!(app.pending_user_turns.len(), 1);
    app.shutdown();
}

/// Drain `rx` and return just the `PeerInbox` events it held, as
/// `(target_pane, body)`. Every subscriber also sees pane lifecycle
/// traffic from `App::new`, and none of that is under test here.
fn drained_peer_inboxes(rx: &Receiver<ipc::Event>) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let ipc::Event::PeerInbox {
            target_pane, body, ..
        } = event
        {
            out.push((target_pane, body));
        }
    }
    out
}

/// The two delivery modes against the #306 routing, pinned together.
///
/// Both halves live in one test on purpose. "A user turn emits no
/// `PeerInbox`" is only worth anything next to evidence that this very
/// harness *does* observe one when the other mode runs: if pane-scoped
/// routing regressed into never matching, or the subscriber were bound
/// to the wrong pane, a `UserTurn`-only test would keep passing for
/// entirely the wrong reason. Running the channel send first through the
/// same bus, the same target pane and the same receiver removes that
/// vacuous-pass mode.
///
/// The unscoped subscriber earns its keep on both halves. On the channel
/// half it is the guard on #306 being non-breaking: a subscription that
/// names no pane must still see the `PeerInbox`, exactly as it did
/// before the issue. On the user-turn half it makes "no `PeerInbox` for
/// anyone" literal — a user turn is typed into the composer, and must
/// never *also* arrive as a channel tag, no matter how a subscriber
/// registered.
#[test]
fn channel_delivery_reaches_the_bound_subscriber_and_a_user_turn_emits_no_peer_inbox() {
    let (mut app, pane_id) = app_with_ready_claude_pane();
    let (_bound_id, bound) = app
        .event_bus
        .subscribe_scoped(ipc::EventScope::PaneInbox(pane_id));
    let (_unscoped_id, unscoped) = app.event_bus.subscribe();

    // Channel delivery, unchanged by #306: the subscriber that bound
    // itself to the target pane gets it, which is what opting in buys.
    app.handle_peer_send(
        pane_id,
        &ipc::PaneRef::Id(pane_id),
        "channel body".to_string(),
    )
    .expect("channel send accepted");
    assert_eq!(
        drained_peer_inboxes(&bound),
        vec![(pane_id, "channel body".to_string())],
        "a channel send must still reach the subscriber bound to its target pane"
    );
    // And so does the one that named no pane: that subscription keeps
    // the pre-#306 stream verbatim, which is the guarantee that makes
    // #306 a minor rather than a break. Draining here also keeps the
    // second half's absence assertion honest.
    assert_eq!(
        drained_peer_inboxes(&unscoped),
        vec![(pane_id, "channel body".to_string())],
        "a subscription that named no pane must still receive every channel send, as before #306"
    );

    // User-turn delivery: accepted (bytes written, reply parked) and
    // yet no PeerInbox for anyone.
    let (tx, rx) = oneshot::channel();
    app.handle_peer_send_user_turn(pane_id, &ipc::PaneRef::Id(pane_id), "/loop".into(), tx);
    assert!(
        rx.try_recv().is_err(),
        "the delivery must have been accepted and parked, not refused"
    );
    assert_eq!(
        app.user_turn_writes,
        vec![(pane_id, b"/loop".to_vec())],
        "the body must have gone to the composer"
    );
    assert!(
        drained_peer_inboxes(&bound).is_empty(),
        "a user turn must not also arrive as a channel tag on the target's own subscriber"
    );
    assert!(
        drained_peer_inboxes(&unscoped).is_empty(),
        "nor on a subscription that named no pane, which just saw the channel send arrive"
    );
    app.shutdown();
}

/// The user-turn ledger is separate from the channel one: a `<channel>`
/// report must not suppress a later, deliberately different, real user
/// turn carrying the same text.
#[test]
fn channel_dedupe_does_not_suppress_a_later_user_turn() {
    let mut app = App::new(40, 80).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    app.handle_peer_send(pane_id, &ipc::PaneRef::Id(pane_id), "/loop".to_string())
        .expect("channel send");
    assert_eq!(app.recent_peer_sends.len(), 1);
    assert!(
        app.recent_user_turn_sends.is_empty(),
        "the channel ledger must not stand in for the user-turn one"
    );
    app.shutdown();
}

// ── the guarantees the refusals rest on ───────────────────────

/// "Refused with zero bytes written" is what makes every refusal safe
/// to retry, and it is the one claim a real PTY cannot be asked about —
/// the bytes are gone. `App::user_turn_writes` records them instead, so
/// this is an assertion rather than a promise.
#[test]
fn every_refusal_writes_nothing_to_the_pty() {
    // Busy: the composer is there, the footer says it is mid-turn.
    let (mut app, pane_id) = app_with_ready_claude_pane();
    let (rows, _) = {
        let pane = app.ws().panes.get(&pane_id).expect("pane");
        let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
        parser.screen().size()
    };
    seed_focused_pane_screen(
        &mut app,
        format!(
            "\x1b[{rows};1H\u{23F5}\u{23F5} auto mode on \u{00B7} esc to interrupt\x1b[{};3H",
            rows - 2
        )
        .as_bytes(),
    );
    assert_eq!(app.user_turn_readiness(0, pane_id), TurnReadiness::Busy);
    let err = user_turn_result(&mut app, pane_id, ipc::PaneRef::Id(pane_id), "/loop")
        .expect_err("busy refusal");
    assert_eq!(err.code, Some(ipc::err_code::USER_TURN_BUSY));
    assert!(
        app.user_turn_writes.is_empty(),
        "a busy refusal wrote {:?}",
        app.user_turn_writes
    );
    app.shutdown();

    // NotReady: a permission dialog owns the screen.
    let (mut app, pane_id) = app_with_ready_claude_pane();
    seed_claude_dialog_pane(&mut app);
    assert_eq!(app.user_turn_readiness(0, pane_id), TurnReadiness::NotReady);
    let err = user_turn_result(&mut app, pane_id, ipc::PaneRef::Id(pane_id), "/loop")
        .expect_err("dialog refusal");
    assert_eq!(err.code, Some(ipc::err_code::USER_TURN_NOT_READY));
    assert!(
        app.user_turn_writes.is_empty(),
        "a dialog refusal wrote {:?} — that text went into the dialog",
        app.user_turn_writes
    );
    assert!(app.recent_user_turn_sends.is_empty());
    app.shutdown();

    // Unsupported: a plain shell.
    let mut app = App::new(40, 120).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    let err = user_turn_result(&mut app, pane_id, ipc::PaneRef::Id(pane_id), "/loop")
        .expect_err("shell refusal");
    assert_eq!(err.code, Some(ipc::err_code::USER_TURN_UNSUPPORTED_TARGET));
    assert!(app.user_turn_writes.is_empty());
    app.shutdown();
}

/// An accepted delivery writes the body — and only the body, with no
/// Enter riding along in the same call.
#[test]
fn an_accepted_delivery_writes_the_body_without_enter() {
    let (mut app, pane_id) = app_with_ready_claude_pane();
    let (tx, _rx) = oneshot::channel();
    app.handle_peer_send_user_turn(pane_id, &ipc::PaneRef::Id(pane_id), "/loop".into(), tx);
    assert_eq!(app.user_turn_writes, vec![(pane_id, b"/loop".to_vec())]);
    app.shutdown();
}

/// A human scrolled the pane up to re-read output. Every screen read
/// goes through the scrollback offset, so the predicate would be
/// judging history — while the live screen underneath may be showing
/// the permission prompt it exists to refuse.
#[test]
fn a_scrolled_back_pane_is_never_ready() {
    let (mut app, pane_id) = app_with_ready_claude_pane();
    assert_eq!(app.user_turn_readiness(0, pane_id), TurnReadiness::Ready);

    // Scrolling only moves if there is history to move into.
    seed_focused_pane_screen(&mut app, "line\r\n".repeat(80).as_bytes());
    seed_claude_idle_pane(&mut app, b"");
    app.ws().panes.get(&pane_id).expect("pane").scroll_up(3);
    assert!(app
        .ws()
        .panes
        .get(&pane_id)
        .expect("pane")
        .is_scrolled_back());
    assert_eq!(app.user_turn_readiness(0, pane_id), TurnReadiness::NotReady);

    app.ws().panes.get(&pane_id).expect("pane").scroll_reset();
    assert_eq!(app.user_turn_readiness(0, pane_id), TurnReadiness::Ready);
    app.shutdown();
}

/// A dead agent leaves its last frame painted, composer and all, so the
/// screen still "proves" readiness. Writing there silently succeeds
/// while delivering nothing.
#[test]
fn an_exited_pane_is_unsupported() {
    let (mut app, pane_id) = app_with_ready_claude_pane();
    app.ws_mut().panes.get_mut(&pane_id).expect("pane").exited = true;
    assert_eq!(
        app.user_turn_readiness(0, pane_id),
        TurnReadiness::Unsupported
    );
    let err = user_turn_result(&mut app, pane_id, ipc::PaneRef::Id(pane_id), "/loop")
        .expect_err("exited pane refused");
    assert_eq!(err.code, Some(ipc::err_code::USER_TURN_UNSUPPORTED_TARGET));
    assert!(app.user_turn_writes.is_empty());
    app.shutdown();
}

/// A queued Codex nudge types into the same composer from the same
/// frames this delivery runs on. Two writers, one composer, one Enter —
/// the submitted turn would be the concatenation.
#[test]
fn a_pending_codex_nudge_blocks_a_user_turn_to_that_pane() {
    let (mut app, pane_id) = app_with_ready_claude_pane();
    app.pending_codex_peer_messages
        .entry(pane_id)
        .or_default()
        .push_back(PendingCodexPeerDelivery::Draft(PendingCodexPeerMessage {
            from_pane: 99,
            from_name: None,
            from_kind: None,
        }));
    let err = user_turn_result(&mut app, pane_id, ipc::PaneRef::Id(pane_id), "/loop")
        .expect_err("nudge in flight");
    assert_eq!(err.code, Some(ipc::err_code::USER_TURN_NOT_READY));
    assert!(app.user_turn_writes.is_empty());
    app.shutdown();
}

// ── the delivery machine, end to end through the real adapter ──

/// The full accepted path: body written, settle, confirm, Enter as its
/// own write, then success only once the draft is observed gone.
///
/// Each flush is preceded by a fresh paint of the screen it is meant to
/// read. The pane's real login shell shares this parser, so the last
/// write before the read has to be the test's, not the shell's.
#[test]
fn the_full_delivery_writes_body_then_enter_and_reports_submitted() {
    let (mut app, pane_id) = app_with_ready_claude_pane();
    let (tx, rx) = oneshot::channel();
    let t0 = Instant::now();
    app.handle_peer_send_user_turn(pane_id, &ipc::PaneRef::Id(pane_id), "/loop".into(), tx);
    assert_eq!(app.user_turn_writes, vec![(pane_id, b"/loop".to_vec())]);

    // Before the settle window elapses nothing moves.
    app.flush_pending_user_turns_at(t0);
    assert_eq!(app.pending_user_turns.len(), 1);
    assert_eq!(app.user_turn_writes.len(), 1);

    // Draft on screen and stable -> settle, confirm, Enter.
    for step in 1..=6 {
        seed_claude_draft_pane(&mut app, "/loop");
        app.flush_pending_user_turns_at(t0 + USER_TURN_SETTLE_DELAY * step);
        if app.user_turn_writes.len() > 1 {
            break;
        }
    }
    assert_eq!(
        app.user_turn_writes,
        vec![(pane_id, b"/loop".to_vec()), (pane_id, b"\r".to_vec())],
        "the body and Enter must be two writes, in that order"
    );
    assert!(
        rx.try_recv().is_err(),
        "still deferred until a submit is seen"
    );

    // The agent consumes the draft -> submitted.
    for step in 7..=12 {
        seed_claude_idle_pane(&mut app, b"");
        app.flush_pending_user_turns_at(t0 + USER_TURN_SETTLE_DELAY * step);
        if app.pending_user_turns.is_empty() {
            break;
        }
    }
    let out = rx.try_recv().expect("answered").expect("submitted");
    assert_eq!(
        out.get("status").and_then(|v| v.as_str()),
        Some("submitted")
    );
    assert_eq!(
        app.user_turn_writes.len(),
        2,
        "no extra keystrokes after the submit"
    );
    assert!(
        app.pending_user_turns.is_empty(),
        "a resolved delivery must not leak a pending entry"
    );
    app.shutdown();
}

/// The blocker this feature exists to avoid. Readiness passes, the body
/// is written, and *then* a permission dialog replaces the composer.
/// A reader that only looked for "the bottom-most prompt glyph" would
/// adopt the dialog's `❯ 1. Yes` row as the draft, watch it hold still,
/// and press Enter on it — auto-approving the prompt.
#[test]
fn a_dialog_appearing_after_the_write_never_receives_enter() {
    let (mut app, pane_id) = app_with_ready_claude_pane();
    let (tx, rx) = oneshot::channel();
    let t0 = Instant::now();
    app.handle_peer_send_user_turn(pane_id, &ipc::PaneRef::Id(pane_id), "/loop".into(), tx);
    assert_eq!(app.user_turn_writes.len(), 1);

    seed_claude_dialog_pane(&mut app);
    // Drive it well past the deadline: the machine must never find
    // anything on this screen it is willing to submit into.
    for step in 1..=12 {
        app.flush_pending_user_turns_at(t0 + USER_TURN_SETTLE_DELAY * step);
    }

    assert_eq!(
        app.user_turn_writes.len(),
        1,
        "Enter reached a dialog: {:?}",
        app.user_turn_writes
    );
    let err = rx
        .try_recv()
        .expect("answered")
        .expect_err("must not claim success");
    assert_eq!(err.code, Some(ipc::err_code::USER_TURN_STALLED));
    assert!(app.pending_user_turns.is_empty());
    app.shutdown();
}

/// The human presses Enter while our body sits in the composer. The
/// draft is gone and we never submitted it — pressing Enter now would
/// land a bare submit on whatever they just started.
#[test]
fn a_draft_that_leaves_the_composer_before_submit_never_gets_enter() {
    let (mut app, pane_id) = app_with_ready_claude_pane();
    let (tx, rx) = oneshot::channel();
    let t0 = Instant::now();
    app.handle_peer_send_user_turn(pane_id, &ipc::PaneRef::Id(pane_id), "/loop".into(), tx);

    // The draft is seen...
    for step in 1..=6 {
        seed_claude_draft_pane(&mut app, "/loop");
        app.flush_pending_user_turns_at(t0 + USER_TURN_SETTLE_DELAY * step);
        if !matches!(rx.try_recv(), Err(oneshot::TryRecvError::Empty)) {
            panic!("must not resolve while the draft is on screen");
        }
        if app.user_turn_writes.len() > 1 {
            panic!("Enter must not fire before the confirm window closes");
        }
        // Stop once the machine has taken the draft into confirmation.
        if step >= 2 {
            break;
        }
    }

    // ...then the human submits it themselves and the composer empties.
    for step in 3..=9 {
        seed_claude_idle_pane(&mut app, b"");
        app.flush_pending_user_turns_at(t0 + USER_TURN_SETTLE_DELAY * step);
        if app.pending_user_turns.is_empty() {
            break;
        }
    }

    assert_eq!(app.user_turn_writes.len(), 1, "no stray Enter");
    let err = rx
        .try_recv()
        .expect("answered")
        .expect_err("outcome is uncertain");
    assert_eq!(err.code, Some(ipc::err_code::USER_TURN_STALLED));
    app.shutdown();
}

/// A delivery whose target never shows the draft must still answer —
/// an unanswered reply channel would block the IPC server for the full
/// `APP_REPLY_TIMEOUT` and be reported as `app_timeout`, blaming renga
/// for a target that simply never echoed.
#[test]
fn a_delivery_that_never_progresses_stalls_at_the_deadline() {
    let (mut app, pane_id) = app_with_ready_claude_pane();
    let (tx, rx) = oneshot::channel();
    let t0 = Instant::now();
    app.handle_peer_send_user_turn(pane_id, &ipc::PaneRef::Id(pane_id), "/loop".into(), tx);

    // The screen never changes.
    app.flush_pending_user_turns_at(t0 + USER_TURN_SETTLE_DELAY * 2);
    assert_eq!(app.pending_user_turns.len(), 1);
    app.flush_pending_user_turns_at(t0 + USER_TURN_DEADLINE * 2);

    let err = rx.try_recv().expect("answered").expect_err("stalled");
    assert_eq!(err.code, Some(ipc::err_code::USER_TURN_STALLED));
    assert!(app.pending_user_turns.is_empty());
    assert_eq!(app.user_turn_writes.len(), 1, "no Enter was ever written");
    app.shutdown();
}

/// The budget must not be spent on a write nobody watches: submitting
/// with no time left to observe it reports `app_timeout` for a turn
/// that did land.
#[test]
fn confirm_refuses_to_submit_once_the_budget_is_gone() {
    let now = Instant::now();
    let stage = UserTurnStage::AwaitConfirm {
        ready_at: now,
        empty: EMPTY.to_string(),
        draft: "\u{276F} /loop\n".to_string(),
        restarts: 0,
    };
    assert!(matches!(
        step_user_turn(&stage, &ComposerRead::text("\u{276F} /loop\n"), now, now),
        UserTurnStep::Stalled(_)
    ));
}

/// The dedupe window must not slide forward on every retry, or a caller
/// politely re-sending a stalled turn can never get through.
#[test]
fn the_dedupe_window_does_not_extend_on_repeated_retries() {
    let (mut app, pane_id) = app_with_ready_claude_pane();
    let (tx, _rx) = oneshot::channel();
    app.handle_peer_send_user_turn(pane_id, &ipc::PaneRef::Id(pane_id), "/loop".into(), tx);
    app.pending_user_turns.clear();
    let first = *app
        .recent_user_turn_sends
        .values()
        .next()
        .expect("ledger entry");

    for _ in 0..3 {
        let out = user_turn_result(&mut app, pane_id, ipc::PaneRef::Id(pane_id), "/loop")
            .expect("suppressed");
        assert_eq!(
            out.get("status").and_then(|v| v.as_str()),
            Some("duplicate_suppressed")
        );
    }
    let after = *app
        .recent_user_turn_sends
        .values()
        .next()
        .expect("ledger entry");
    assert_eq!(
        first, after,
        "a suppressed retry must not push the expiry back"
    );
    app.shutdown();
}

// ── body handling ─────────────────────────────────────────────

/// Tab is a bound key in both agents (completion / queue-message), not
/// composer text — renga's own send_keys vocabulary lowers `Tab` to
/// this byte.
#[test]
fn a_tab_in_a_single_line_body_is_refused() {
    let (mut app, pane_id) = app_with_ready_claude_pane();
    let err = user_turn_result(
        &mut app,
        pane_id,
        ipc::PaneRef::Id(pane_id),
        "rerun\tcargo test",
    )
    .expect_err("tab refused");
    assert_eq!(err.code, Some(ipc::err_code::USER_TURN_INVALID_BODY));
    assert!(app.user_turn_writes.is_empty());
    app.shutdown();
}

/// A trailing newline is what a heredoc or a generated string carries
/// incidentally. Treating it as "multi-line" refused `/clear\n` with a
/// reason that was false for it.
#[test]
fn a_trailing_newline_does_not_make_a_body_multi_line() {
    assert_eq!(
        normalize_user_turn_body("/clear\n").expect("normalizes"),
        "/clear"
    );
    let (mut app, pane_id) = app_with_ready_claude_pane();
    let (tx, _rx) = oneshot::channel();
    app.handle_peer_send_user_turn(pane_id, &ipc::PaneRef::Id(pane_id), "/clear\n".into(), tx);
    assert_eq!(
        app.user_turn_writes,
        vec![(pane_id, b"/clear".to_vec())],
        "a slash command must reach the composer verbatim"
    );
    app.shutdown();
}

/// U+2028 / U+2029 are line separators `char::is_control()` does not
/// catch, so without folding they would ride the single-line path and
/// skip the bracketed-paste precondition entirely.
#[test]
fn unicode_line_separators_are_folded_into_newlines() {
    assert_eq!(
        normalize_user_turn_body("stop\u{2028}/clear").expect("normalizes"),
        "stop\n/clear"
    );
    assert_eq!(
        normalize_user_turn_body("a\u{2029}b").expect("normalizes"),
        "a\nb"
    );
}

// ── predicate scope regressions ───────────────────────────────

/// The busy scan is bounded to the status rows for a reason: widening
/// it to the whole screen means a pane whose transcript quotes the
/// phrase is refused forever, since an idle pane never scrolls it off.
#[test]
fn a_busy_marker_in_the_transcript_does_not_make_claude_busy() {
    let rule = "─".repeat(40);
    let screen = format!(
        "\x1b[2J\x1b[H\x1b[?25hwe discussed esc to interrupt earlier\r\n{rule}\r\n\u{276F}\r\n{rule}\r\n\u{23F5}\u{23F5} auto mode on\x1b[3;3H"
    );
    assert_eq!(
        claude_readiness_of(screen.as_bytes(), 8, 40),
        TurnReadiness::Ready
    );
}

/// A menu row highlighted with inverse video, sitting between two
/// separator rules, must not prove as an empty focused composer: the
/// emptiness scan covers the whole row, not just what is right of the
/// glyph, and the T-junctions those separators use are not frame
/// glyphs.
#[test]
fn a_highlighted_menu_row_between_separators_is_not_a_composer() {
    let bar = "\u{251C}".to_string() + &"\u{2500}".repeat(38) + "\u{2524}";
    let screen = format!(
        "\x1b[2J\x1b[H\x1b[?25lChoose:\r\n{bar}\r\n\x1b[7m\u{276F}\x1b[0m\r\n{bar}\r\n  2. no"
    );
    assert_eq!(
        claude_readiness_of(screen.as_bytes(), 8, 40),
        TurnReadiness::NotReady
    );
}

/// Codex's composer emptiness is checked against the text, not only
/// against the caret column: a human who moves the caret home mid-draft
/// leaves their words in place.
#[test]
fn a_codex_draft_with_the_caret_at_home_is_not_ready() {
    assert_eq!(
        codex_readiness_of(
            "\x1b[2J\x1b[H\x1b[?25hdone\r\n\u{203A} please review the P\x1b[2;3H".as_bytes(),
            8,
            40
        ),
        TurnReadiness::NotReady
    );
}

/// The whole reason `user_turn_stalled` exists instead of `app_timeout`
/// is that the delivery budget finishes first. The two constants live
/// in different modules with nothing linking them.
#[test]
fn the_delivery_budget_fits_inside_the_ipc_reply_budget() {
    assert!(
        USER_TURN_DEADLINE < crate::ipc::APP_REPLY_TIMEOUT,
        "a delivery that outlives the IPC budget is reported as app_timeout, \
         which blames renga for a slow target"
    );
    assert!(USER_TURN_SETTLE_DELAY + USER_TURN_CONFIRM_DELAY < USER_TURN_DEADLINE);
}

/// Codex gets the same post-write revalidation Claude does. Its `›`
/// composer glyph is not exclusive to the composer: an unboxed option
/// list wears it too, and without a caret check the machine would adopt
/// a menu row as the draft and press Enter on it.
#[test]
fn a_codex_menu_row_is_not_adopted_as_a_draft() {
    let mut app = App::new(40, 120).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    app.peer_client_kinds.insert(pane_id, PeerClientKind::Codex);

    // A real Codex draft: caret parked in the composer's edit region.
    seed_focused_pane_screen(
        &mut app,
        "\x1b[2J\x1b[H\x1b[?25hwork\r\n\u{203A} /loop 5m\x1b[2;11H".as_bytes(),
    );
    let has_draft = {
        let pane = app.ws().panes.get(&pane_id).expect("pane");
        let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
        super::super::user_turn::composer_block_text(parser.screen(), TurnAgent::Codex)
    };
    assert!(
        has_draft.is_some(),
        "a genuine Codex draft must be readable"
    );

    // A menu whose selected row also starts with the composer glyph,
    // with the caret nowhere near an edit position.
    seed_focused_pane_screen(
        &mut app,
        "\x1b[2J\x1b[H\x1b[?25hApprove?\r\n\u{203A} 1. Yes\r\n  2. No\x1b[1;1H".as_bytes(),
    );
    let menu = {
        let pane = app.ws().panes.get(&pane_id).expect("pane");
        let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
        super::super::user_turn::composer_block_text(parser.screen(), TurnAgent::Codex)
    };
    assert!(
        menu.is_none(),
        "a menu row must not read as a composer: {menu:?}"
    );
    app.shutdown();
}

/// `Pane::write_input` answers `Ok(())` whether it wrote or gave up,
/// flipping `exited` on failure. Trusting its return value made a
/// failed Enter look successful — and an exited pane's composer then
/// reads as gone, which the submit stage scores as "consumed". That is
/// a false success about a turn nobody received.
#[test]
fn a_pane_that_dies_mid_delivery_is_never_reported_as_submitted() {
    let (mut app, pane_id) = app_with_ready_claude_pane();
    let (tx, rx) = oneshot::channel();
    let t0 = Instant::now();
    app.handle_peer_send_user_turn(pane_id, &ipc::PaneRef::Id(pane_id), "/loop".into(), tx);
    assert_eq!(app.user_turn_writes.len(), 1);

    // One flush takes the draft into confirmation; Enter cannot fire in
    // the same call, because confirmation needs a second read a
    // stability window later.
    seed_claude_draft_pane(&mut app, "/loop");
    app.flush_pending_user_turns_at(t0 + USER_TURN_SETTLE_DELAY * 4);
    assert_eq!(app.user_turn_writes.len(), 1, "Enter has not fired yet");

    // The agent dies before the Enter goes out.
    app.ws_mut().panes.get_mut(&pane_id).expect("pane").exited = true;
    app.flush_pending_user_turns_at(t0 + USER_TURN_SETTLE_DELAY * 5);

    let err = rx
        .try_recv()
        .expect("answered")
        .expect_err("must not claim the turn was submitted");
    // `stalled` is the honest answer here and the one that matters:
    // the body genuinely was written before the pane died, so the
    // outcome is uncertain — but it is never reported as `submitted`,
    // which is what an exited pane's vanished composer would otherwise
    // score as "the draft was consumed".
    assert_eq!(err.code, Some(ipc::err_code::USER_TURN_STALLED), "{err:?}");
    assert_eq!(app.user_turn_writes.len(), 1, "the Enter never landed");
    assert!(app.pending_user_turns.is_empty());
    app.shutdown();
}

/// A body write that demonstrably never landed must not leave a dedupe
/// entry behind — the window exists to cover *uncertain* writes.
#[test]
fn a_failed_body_write_leaves_no_dedupe_trace() {
    let (mut app, pane_id) = app_with_ready_claude_pane();
    // Readiness is evaluated against the screen; kill the PTY after it
    // passes by flipping the flag the writer checks.
    let readiness = app.user_turn_readiness(0, pane_id);
    assert_eq!(readiness, TurnReadiness::Ready);
    app.ws_mut()
        .panes
        .get_mut(&pane_id)
        .expect("pane")
        .writer_fail_for_test();

    let err = user_turn_result(&mut app, pane_id, ipc::PaneRef::Id(pane_id), "/loop")
        .expect_err("write failed");
    assert_eq!(err.code, Some(ipc::err_code::USER_TURN_UNSUPPORTED_TARGET));
    assert!(
        app.recent_user_turn_sends.is_empty(),
        "a write that never landed must stay retryable"
    );
    assert!(app.pending_user_turns.is_empty());
    app.shutdown();
}

/// An empty Codex composer can stay painted while a dialog owns input.
/// The caret is what says which, so the nudge gate's "cursor anywhere
/// at or above the prompt" is not enough to authorize a write.
#[test]
fn a_codex_prompt_without_the_caret_is_not_ready() {
    // Caret parked up in the transcript.
    assert_eq!(
        codex_readiness_of(
            "\x1b[2J\x1b[H\x1b[?25hApprove this command?\r\n\u{203A} \x1b[1;1H".as_bytes(),
            8,
            40
        ),
        TurnReadiness::NotReady
    );
    // Caret in the composer's edit cell: ready.
    assert_eq!(
        codex_readiness_of(
            "\x1b[2J\x1b[H\x1b[?25hdone\r\n\u{203A} \x1b[2;3H".as_bytes(),
            8,
            40
        ),
        TurnReadiness::Ready
    );
}

/// The handler's pre-flight check catches a nudge queued *before* the
/// turn. This is the other order: a channel message arriving while the
/// body settles. The nudge flush runs first in the frame, so without a
/// guard it would type into a composer already holding our body and the
/// confirm stage would submit the concatenation.
#[test]
fn a_nudge_queued_mid_delivery_is_not_typed_into_the_composer() {
    let (mut app, pane_id) = app_with_ready_claude_pane();
    let (tx, _rx) = oneshot::channel();
    app.handle_peer_send_user_turn(pane_id, &ipc::PaneRef::Id(pane_id), "/loop".into(), tx);
    assert_eq!(app.pending_user_turns.len(), 1);

    // A channel message lands for the same pane, mid-delivery.
    app.pending_codex_peer_messages
        .entry(pane_id)
        .or_default()
        .push_back(PendingCodexPeerDelivery::Draft(PendingCodexPeerMessage {
            from_pane: 99,
            from_name: None,
            from_kind: None,
        }));
    app.flush_pending_codex_peer_messages();

    assert!(
        app.panes_with_user_turn_in_flight().contains(&pane_id),
        "the delivery is still in flight"
    );
    assert!(
        app.pending_codex_peer_messages.contains_key(&pane_id),
        "the nudge must stay queued, not be typed into our composer"
    );
    app.shutdown();
}

/// Normalization removes the incidental trailing newline and nothing
/// else: trimming whitespace generally would silently reshape the
/// delivered message, and would strip a trailing tab past the rule
/// that exists to refuse it.
#[test]
fn normalization_strips_only_the_trailing_newline() {
    assert_eq!(
        normalize_user_turn_body("/clear\n\n").expect("normalizes"),
        "/clear"
    );
    assert_eq!(
        normalize_user_turn_body("keep me  ").expect("normalizes"),
        "keep me  ",
        "intentional trailing spaces are part of the message"
    );
    // A trailing tab must still reach the rule that refuses it rather
    // than being quietly trimmed away.
    let (mut app, pane_id) = app_with_ready_claude_pane();
    let err = user_turn_result(&mut app, pane_id, ipc::PaneRef::Id(pane_id), "foo\t")
        .expect_err("trailing tab refused");
    assert_eq!(err.code, Some(ipc::err_code::USER_TURN_INVALID_BODY));
    app.shutdown();
}

/// A pane that becomes unreadable after Enter — it exited, the human
/// scrolled it back, its parser is poisoned — proves nothing about
/// whether the turn landed. Only a *readable* screen whose composer
/// structurally vanished counts as a consumed draft.
#[test]
fn an_unreadable_pane_is_never_scored_as_submitted() {
    let now = Instant::now();
    let stage = UserTurnStage::AwaitSubmit {
        draft: "\u{276F} /loop\n".to_string(),
    };
    assert!(
        matches!(
            step_user_turn(
                &stage,
                &ComposerRead::Unreadable,
                now,
                now + USER_TURN_DEADLINE
            ),
            UserTurnStep::Stalled(_)
        ),
        "an unobservable pane must not be reported as submitted"
    );
    // A readable screen with no composer on it is the `/clear` case,
    // and that one really is a consumed draft.
    assert_eq!(
        step_user_turn(&stage, &ComposerRead::gone(), now, now + USER_TURN_DEADLINE),
        UserTurnStep::Submitted
    );
}

/// The window between "readiness passed" and "the body is written" is
/// the PTY reader thread's to paint into. If a modal lands there, the
/// pre-write proof must refuse with nothing written rather than
/// treating an unreadable composer as an empty one.
#[test]
fn a_modal_between_readiness_and_the_write_refuses_with_nothing_written() {
    let (mut app, pane_id) = app_with_ready_claude_pane();
    assert_eq!(app.user_turn_readiness(0, pane_id), TurnReadiness::Ready);
    // Stand in for the reader thread painting after the check.
    seed_claude_dialog_pane(&mut app);

    let err = user_turn_result(&mut app, pane_id, ipc::PaneRef::Id(pane_id), "/loop")
        .expect_err("refused");
    assert_eq!(err.code, Some(ipc::err_code::USER_TURN_NOT_READY));
    assert!(
        app.user_turn_writes.is_empty(),
        "wrote into a dialog: {:?}",
        app.user_turn_writes
    );
    assert!(
        app.recent_user_turn_sends.is_empty(),
        "a refusal must stay freely retryable"
    );
    app.shutdown();
}

/// The pre-write recheck runs the *full* predicate, not merely "is a
/// composer readable". `composer_block_text` accepts a draft and
/// ignores busy chrome on purpose — after the write, a draft is what it
/// expects — so relying on it alone would append the body to a human's
/// half-typed sentence that appeared during the readiness race.
#[test]
fn a_draft_appearing_between_readiness_and_the_write_refuses() {
    let (mut app, pane_id) = app_with_ready_claude_pane();
    assert_eq!(app.user_turn_readiness(0, pane_id), TurnReadiness::Ready);
    seed_claude_draft_pane(&mut app, "half-typed thought");

    let err = user_turn_result(&mut app, pane_id, ipc::PaneRef::Id(pane_id), "/loop")
        .expect_err("refused");
    assert_eq!(err.code, Some(ipc::err_code::USER_TURN_NOT_READY));
    assert!(
        app.user_turn_writes.is_empty(),
        "appended to somebody's draft: {:?}",
        app.user_turn_writes
    );
    app.shutdown();
}

/// A Codex body long enough to wrap moves the caret off the composer
/// row, and renga has no verified model of how Codex lays a wrapped
/// composer out. Typing it in and never submitting it is the worst
/// outcome, so it is refused before anything is written.
#[test]
fn an_over_long_codex_body_is_refused_before_writing() {
    let mut app = App::new(40, 120).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    app.peer_client_kinds.insert(pane_id, PeerClientKind::Codex);
    let cols = {
        let pane = app.ws().panes.get(&pane_id).expect("pane");
        let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
        parser.screen().size().1
    };
    seed_focused_pane_screen(
        &mut app,
        "\x1b[2J\x1b[H\x1b[?25hdone\r\n\u{203A} \x1b[2;3H".as_bytes(),
    );
    assert_eq!(app.user_turn_readiness(0, pane_id), TurnReadiness::Ready);

    let long = "x".repeat(cols as usize + 10);
    let err = user_turn_result(&mut app, pane_id, ipc::PaneRef::Id(pane_id), &long)
        .expect_err("too long for one row");
    assert_eq!(err.code, Some(ipc::err_code::USER_TURN_INVALID_BODY));
    assert!(app.user_turn_writes.is_empty());

    // A body that does fit is still accepted.
    let (tx, _rx) = oneshot::channel();
    app.handle_peer_send_user_turn(pane_id, &ipc::PaneRef::Id(pane_id), "/loop".into(), tx);
    assert_eq!(app.user_turn_writes, vec![(pane_id, b"/loop".to_vec())]);
    app.shutdown();
}

/// Walk **every** refusal the handler can produce and assert each one
/// wrote nothing.
///
/// The individual tests above each cover one path; this covers the set,
/// so a new refusal added later without a byte-freedom assertion shows
/// up here rather than silently widening the hole. "Refused with zero
/// bytes written" is what makes a refusal safe to retry — a partial
/// guarantee is not one.
#[test]
fn no_refusal_path_anywhere_writes_a_byte() {
    let long = "x".repeat(9000);
    // (label, body, how to make the pane refuse, expected code)
    #[allow(clippy::type_complexity)]
    let cases: Vec<(&str, &str, Box<dyn Fn(&mut App, usize)>, &'static str)> = vec![
        (
            "empty body",
            "   ",
            Box::new(|_, _| {}),
            ipc::err_code::USER_TURN_INVALID_BODY,
        ),
        (
            "control character",
            "hi\x1b[31m",
            Box::new(|_, _| {}),
            ipc::err_code::USER_TURN_INVALID_BODY,
        ),
        (
            "oversized body",
            &long,
            Box::new(|_, _| {}),
            ipc::err_code::USER_TURN_INVALID_BODY,
        ),
        (
            "tab in a single-line body",
            "a\tb",
            Box::new(|_, _| {}),
            ipc::err_code::USER_TURN_INVALID_BODY,
        ),
        (
            "multi-line without bracketed paste",
            "one\ntwo",
            Box::new(|_, _| {}),
            ipc::err_code::USER_TURN_INVALID_BODY,
        ),
        (
            "agent mid-turn",
            "/loop",
            Box::new(|app, pane_id| {
                let rows = {
                    let pane = app.ws().panes.get(&pane_id).expect("pane");
                    let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
                    parser.screen().size().0
                };
                seed_focused_pane_screen(
                    app,
                    format!(
                        "\x1b[{rows};1Hauto mode on \u{00B7} esc to interrupt\x1b[{};3H",
                        rows - 2
                    )
                    .as_bytes(),
                );
            }),
            ipc::err_code::USER_TURN_BUSY,
        ),
        (
            "permission dialog",
            "/loop",
            Box::new(seed_claude_dialog_pane_for),
            ipc::err_code::USER_TURN_NOT_READY,
        ),
        (
            "human draft in the composer",
            "/loop",
            Box::new(|app, _| seed_claude_draft_pane(app, "half-typed")),
            ipc::err_code::USER_TURN_NOT_READY,
        ),
        (
            "pane scrolled back",
            "/loop",
            Box::new(|app, pane_id| {
                seed_focused_pane_screen(app, "line\r\n".repeat(80).as_bytes());
                seed_claude_idle_pane(app, b"");
                app.ws().panes.get(&pane_id).expect("pane").scroll_up(3);
            }),
            ipc::err_code::USER_TURN_NOT_READY,
        ),
        (
            "delivery already in flight",
            "/loop",
            Box::new(|app, pane_id| {
                let (tx, _rx) = oneshot::channel();
                app.handle_peer_send_user_turn(
                    pane_id,
                    &ipc::PaneRef::Id(pane_id),
                    "other".into(),
                    tx,
                );
                app.user_turn_writes.clear();
            }),
            ipc::err_code::USER_TURN_NOT_READY,
        ),
        (
            "codex nudge queued for the same composer",
            "/loop",
            Box::new(|app, pane_id| {
                app.pending_codex_peer_messages
                    .entry(pane_id)
                    .or_default()
                    .push_back(PendingCodexPeerDelivery::Draft(PendingCodexPeerMessage {
                        from_pane: 99,
                        from_name: None,
                        from_kind: None,
                    }));
            }),
            ipc::err_code::USER_TURN_NOT_READY,
        ),
        (
            "agent exited",
            "/loop",
            Box::new(|app, pane_id| {
                app.ws_mut().panes.get_mut(&pane_id).expect("pane").exited = true;
            }),
            ipc::err_code::USER_TURN_UNSUPPORTED_TARGET,
        ),
        (
            "not an agent pane",
            "/loop",
            Box::new(|app, pane_id| {
                app.peer_client_kinds.remove(&pane_id);
                seed_focused_pane_screen(app, b"\x1b[2J\x1b[H$ ");
            }),
            ipc::err_code::USER_TURN_UNSUPPORTED_TARGET,
        ),
    ];

    for (label, body, arrange, expected) in cases {
        let (mut app, pane_id) = app_with_ready_claude_pane();
        arrange(&mut app, pane_id);
        let err =
            user_turn_result(&mut app, pane_id, ipc::PaneRef::Id(pane_id), body).expect_err(label);
        assert_eq!(err.code, Some(expected), "{label}: {err:?}");
        assert!(
            app.user_turn_writes.is_empty(),
            "{label} wrote to the PTY: {:?}",
            app.user_turn_writes
        );
        app.shutdown();
    }

    // An unresolvable target never reaches a pane at all.
    let (mut app, pane_id) = app_with_ready_claude_pane();
    let err = user_turn_result(&mut app, pane_id, ipc::PaneRef::Id(9999), "/loop")
        .expect_err("unknown target");
    assert_eq!(err.code, Some(ipc::err_code::PANE_NOT_FOUND));
    assert!(app.user_turn_writes.is_empty());
    app.shutdown();
}

fn seed_claude_dialog_pane_for(app: &mut App, _pane_id: usize) {
    seed_claude_dialog_pane(app);
}

/// The Enter write gets the same atomic prove-and-write the body does.
/// A modal replacing the composer between the decision to submit and
/// the keystroke would otherwise have the bare `\r` answer it — the
/// worst outcome this feature has, arriving through the half of the
/// path that was not covered by the lock.
#[test]
fn enter_is_withheld_when_the_draft_stops_matching() {
    let (mut app, pane_id) = app_with_ready_claude_pane();
    let empty = {
        let pane = app.ws().panes.get(&pane_id).expect("pane");
        let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
        super::super::user_turn::composer_block_text(parser.screen(), TurnAgent::Claude)
            .expect("idle composer")
    };
    let agent = TurnAgent::Claude;

    // Confirmed draft is one thing; the screen now shows another.
    seed_claude_dialog_pane(&mut app);
    let err = app
        .submit_user_turn_enter(0, pane_id, agent, "\u{276F} /loop\n")
        .expect_err("Enter must be withheld");
    assert_eq!(err.code, Some(ipc::err_code::USER_TURN_STALLED));
    assert!(
        app.user_turn_writes.is_empty(),
        "Enter reached a dialog: {:?}",
        app.user_turn_writes
    );

    // With the screen still holding the confirmed draft, it goes out.
    seed_claude_idle_pane(&mut app, b"");
    app.submit_user_turn_enter(0, pane_id, agent, &empty)
        .expect("matching draft submits");
    assert_eq!(app.user_turn_writes, vec![(pane_id, b"\r".to_vec())]);
    app.shutdown();
}

/// A hard newline makes a second Codex composer row just as surely as
/// an over-long line wraps into one, and the width check never saw it.
#[test]
fn a_multiline_codex_body_is_refused() {
    let mut app = App::new(40, 120).expect("App::new");
    let pane_id = app.ws().focused_pane_id;
    app.peer_client_kinds.insert(pane_id, PeerClientKind::Codex);
    seed_focused_pane_screen(
        &mut app,
        "\x1b[?2004h\x1b[2J\x1b[H\x1b[?25hdone\r\n\u{203A} \x1b[2;3H".as_bytes(),
    );
    assert_eq!(app.user_turn_readiness(0, pane_id), TurnReadiness::Ready);

    // Short enough by width, but two rows once rendered.
    let err = user_turn_result(&mut app, pane_id, ipc::PaneRef::Id(pane_id), "a\nb")
        .expect_err("multi-line Codex body refused");
    assert_eq!(err.code, Some(ipc::err_code::USER_TURN_INVALID_BODY));
    assert!(app.user_turn_writes.is_empty());
    app.shutdown();
}
