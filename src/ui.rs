use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use crate::app::{App, DragTarget, FocusTarget, SplitDirection};

// ─── Theme (terminal palette) ─────────────────────────────
const BG: Color = Color::Reset;
const PANEL_BG: Color = Color::Reset;
const BORDER: Color = Color::DarkGray;
const FOCUS_BORDER: Color = Color::LightBlue;
const TEXT: Color = Color::Reset;
const TEXT_DIM: Color = Color::DarkGray;
const ACCENT_GREEN: Color = Color::Green;
const ACCENT_BLUE: Color = Color::Blue;
const ACCENT_CLAUDE: Color = Color::Yellow;
const ACCENT_CODEX: Color = Color::Cyan;
const HEADER_BG: Color = Color::Reset;
const ACTIVE_TAB_BG: Color = Color::Reset;
const LINE_NUM_COLOR: Color = Color::DarkGray;
const SCROLL_BG: Color = Color::Reset;
const SCROLL_THUMB: Color = Color::Gray;

const MIN_TERMINAL_WIDTH: u16 = 40;
const MIN_TERMINAL_HEIGHT: u16 = 10;
const MIN_PANE_AREA_WIDTH: u16 = 20;

// ─── File type icons ──────────────────────────────────────
fn file_icon(name: &str) -> (&'static str, Color) {
    let ext = name.rsplit('.').next().unwrap_or("");
    match ext {
        "rs" => ("\u{1f980} ", Color::Yellow),
        "toml" => ("\u{2699}\u{fe0f} ", TEXT_DIM),
        "lock" => ("\u{1f512} ", TEXT_DIM),
        "md" => ("\u{1f4c4} ", ACCENT_BLUE),
        "json" => ("{ ", Color::Yellow),
        "yaml" | "yml" => ("~ ", Color::Yellow),
        "js" => ("\u{26a1} ", Color::Yellow),
        "ts" | "tsx" => ("\u{26a1} ", ACCENT_BLUE),
        "jsx" => ("\u{26a1} ", Color::Cyan),
        "py" => ("\u{1f40d} ", ACCENT_BLUE),
        "sh" | "bash" | "zsh" => ("$ ", ACCENT_GREEN),
        "css" | "scss" => ("# ", Color::Magenta),
        "html" => ("< ", Color::Red),
        "gitignore" => ("\u{2022} ", Color::Red),
        _ => ("\u{2022} ", TEXT_DIM),
    }
}

fn syntax_rgb_to_ansi_color(r: u8, g: u8, b: u8) -> Color {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    if max.saturating_sub(min) < 32 {
        return if max < 96 {
            Color::DarkGray
        } else {
            Color::Gray
        };
    }

    let intense = max >= 192;
    let substantial = |channel: u8| (u16::from(channel) * 20) >= (u16::from(max) * 11);
    let close = |channel: u8| (u16::from(channel) * 4) > (u16::from(max) * 3);
    let low = |channel: u8| u16::from(channel) * 5 <= u16::from(max) * 3;

    let color = if r == max && substantial(g) && low(b) {
        Color::Yellow
    } else if r == max && close(b) && g < b {
        Color::Magenta
    } else if g == max && close(b) && r < b {
        Color::Cyan
    } else if r == max {
        Color::Red
    } else if g == max {
        Color::Green
    } else {
        Color::Blue
    };

    if intense {
        match color {
            Color::Red => Color::LightRed,
            Color::Green => Color::LightGreen,
            Color::Yellow => Color::LightYellow,
            Color::Blue => Color::LightBlue,
            Color::Magenta => Color::LightMagenta,
            Color::Cyan => Color::LightCyan,
            _ => color,
        }
    } else {
        color
    }
}

// ─── Main render ──────────────────────────────────────────

pub fn render(app: &mut App, frame: &mut Frame) {
    let area = frame.area();
    app.last_term_size = (area.width, area.height);
    // Cleared every frame; re-populated below only on conpty when a caret is
    // resolved. Keeps a stale position from a prior frame out of the deferred
    // apply (see the field doc on `App::deferred_caret` and #260).
    app.deferred_caret = None;

    if area.width < MIN_TERMINAL_WIDTH || area.height < MIN_TERMINAL_HEIGHT {
        let msg = Paragraph::new("Terminal too small")
            .style(Style::default().fg(TEXT_DIM).bg(BG))
            .alignment(Alignment::Center);
        frame.render_widget(msg, area);
        return;
    }

    let bg_block = Block::default().style(Style::default().bg(BG));
    frame.render_widget(bg_block, area);

    let show_status = app.status_bar_visible || app.rename_input.is_some();
    let status_h = if show_status { 1 } else { 0 };
    let show_macos_tip = app.macos_tip_visible;
    // Two rows: line 1 is the warning + what the user needs to fix,
    // line 2 is the README URL + dismiss hint. We don't shrink to
    // one row on narrow terminals because the URL is the whole point
    // — better to let line 2 truncate horizontally than drop it.
    let macos_tip_h: u16 = if show_macos_tip { 2 } else { 0 };
    let show_overlay = app.overlay.is_some();
    let show_codex_peer_notification =
        !show_overlay && app.visible_codex_peer_notification().is_some();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),           // tab bar
            Constraint::Min(1),              // main area
            Constraint::Length(macos_tip_h), // first-launch macOS tip
            Constraint::Length(status_h),    // status bar
        ])
        .split(area);

    render_tab_bar(app, frame, chunks[0]);
    let pane_caret = render_main_area(app, frame, chunks[1]);
    if show_macos_tip {
        render_macos_tip(app, frame, chunks[2]);
    }
    if show_status {
        render_status_bar(app, frame, chunks[3]);
    }
    // The IME composition overlay is drawn last so its centered box
    // and its caret anchor land on top of the pane content without
    // having claimed a layout slot. Using the full terminal `area`
    // (not `chunks[1]`) keeps it visually centered on the whole
    // window even when the status bar is visible.
    let overlay_caret = if show_overlay {
        render_ime_overlay(app, frame, area)
    } else if show_codex_peer_notification {
        render_codex_peer_notification(app, frame, area);
        None
    } else {
        None
    };

    // Final hardware-caret position for this frame. The overlay caret wins
    // over the pane caret when the overlay is open, reproducing the historical
    // last-write-wins ordering (the overlay previously called
    // `set_cursor_position` after the panes).
    let caret = overlay_caret.or(pane_caret);

    // Caret delivery. On conpty (Windows / WSL) defer the caret so the main
    // loop applies MoveTo->Show after `terminal.draw`, while the cursor is
    // still hidden. Letting ratatui set the frame cursor would instead make it
    // re-show the caret at its stale post-paint position before moving it,
    // leaking a one-frame flicker onto Claude's spinner row (#260). On every
    // other target keep the original `set_cursor_position` path (#253).
    if crate::hide_cursor_during_draw() {
        app.deferred_caret = caret;
    } else if let Some(pos) = caret {
        frame.set_cursor_position(pos);
    }
}

// ─── IME composition overlay ──────────────────────────────

/// Minimum inside-box dimensions for the centered IME overlay. Below
/// these the box collapses to nothing so we don't render a widget so
/// small the caret can't fit.
const OVERLAY_MIN_INNER_WIDTH: u16 = 20;
const OVERLAY_MIN_INNER_HEIGHT: u16 = 1;
/// Visible height inside the box — content rows, not including
/// borders. The overlay opens at this height immediately so the user
/// sees the full editing area on the first frame; buffers longer than
/// this cap scroll instead of resizing the box.
const OVERLAY_MAX_INNER_HEIGHT: u16 = 10;
/// Target width as a percentage of the terminal columns, clamped
/// below to 40 cols and above to 100 cols so the box is readable on
/// both narrow and ultra-wide terminals.
const OVERLAY_TARGET_WIDTH_PCT: u16 = 60;
const OVERLAY_MAX_WIDTH: u16 = 100;
const OVERLAY_MIN_WIDTH: u16 = 42;

fn render_codex_peer_notification(app: &mut App, frame: &mut Frame, area: Rect) {
    let Some(notification) = app.visible_codex_peer_notification() else {
        return;
    };

    let box_w = area.width.min(68);
    let box_h = area.height.min(7);
    if box_w < 44 || box_h < 5 {
        return;
    }
    let box_x = area.x + (area.width.saturating_sub(box_w)) / 2;
    let box_y = area.y + (area.height.saturating_sub(box_h)) / 2;
    let box_rect = Rect::new(box_x, box_y, box_w, box_h);
    frame.render_widget(ratatui::widgets::Clear, box_rect);

    let title = Line::from(vec![
        Span::styled(" PEER ▷ ", Style::default().fg(ACCENT_CODEX)),
        Span::styled(
            "pending Codex messages",
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
    ]);
    let sender = match (
        &notification.message.from_name,
        notification.message.from_kind,
    ) {
        (Some(name), Some(kind)) => {
            let kind = match kind {
                crate::ipc::PeerClientKind::Claude => "claude",
                crate::ipc::PeerClientKind::Codex => "codex",
            };
            format!("from {name} (id={} {kind})", notification.message.from_pane)
        }
        (Some(name), None) => format!("from {name} (id={})", notification.message.from_pane),
        (None, Some(kind)) => {
            let kind = match kind {
                crate::ipc::PeerClientKind::Claude => "claude",
                crate::ipc::PeerClientKind::Codex => "codex",
            };
            format!("from id={} {kind}", notification.message.from_pane)
        }
        (None, None) => format!("from id={}", notification.message.from_pane),
    };
    let noun = if notification.pending_count == 1 {
        "message"
    } else {
        "messages"
    };
    let lines = vec![
        Line::from(Span::styled(
            format!(
                "{} pending peer {} in the MCP inbox.",
                notification.pending_count, noun
            ),
            Style::default().fg(TEXT),
        )),
        Line::from(Span::styled(sender, Style::default().fg(TEXT_DIM))),
        Line::from(Span::styled(
            "Alt+Enter/Ctrl+Enter inserts the check_messages prompt. Press Enter yourself to send it.",
            Style::default().fg(TEXT_DIM),
        )),
    ];
    let hint = Line::from(Span::styled(
        " Alt+Enter/Ctrl+Enter insert nudge · Esc ignore ",
        Style::default().fg(TEXT_DIM),
    ));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT_CODEX))
        .style(Style::default().bg(PANEL_BG))
        .title(title)
        .title_bottom(hint);
    frame.render_widget(Paragraph::new(lines).block(block), box_rect);
}

/// Returns the caret anchor the overlay wants, or `None` when it does not
/// render (no overlay, or the terminal is too small to fit the box). The
/// caller decides how to deliver the caret (see `render`'s delivery policy).
fn render_ime_overlay(app: &mut App, frame: &mut Frame, area: Rect) -> Option<(u16, u16)> {
    let overlay = app.overlay.as_ref()?;

    // Box width: 60% of the terminal, clamped. Plus-2 for the border
    // columns. Collapse if the terminal is smaller than the minimum.
    let pct_w = area.width.saturating_mul(OVERLAY_TARGET_WIDTH_PCT) / 100;
    let box_w = pct_w.clamp(OVERLAY_MIN_WIDTH, OVERLAY_MAX_WIDTH);
    let box_w = box_w.min(area.width);
    if box_w < OVERLAY_MIN_INNER_WIDTH + 2 {
        return None;
    }
    let inner_w = box_w - 2;

    // Split the buffer into logical lines, then wrap each at inner_w
    // display columns. Cursor position in wrapped space is tracked
    // alongside so the caret anchor lands in the right visual cell.
    let (wrapped, cursor_row_in_wrapped, cursor_col_in_wrapped) =
        wrap_overlay_buffer(&overlay.buffer, overlay.cursor, inner_w as usize);

    // Box height: always open at the configured maximum so the user
    // sees the full editing area on the first frame, instead of a
    // 1-row box that grows as they type. Buffers longer than the cap
    // scroll (see scroll_y below) rather than resizing the box.
    // `area.height` still bounds the box so a tiny terminal collapses
    // gracefully — `OVERLAY_MIN_INNER_HEIGHT + 2` remains the floor
    // below which the overlay simply refuses to render.
    let inner_h_wanted = OVERLAY_MAX_INNER_HEIGHT;
    let box_h = (inner_h_wanted + 2).min(area.height);
    if box_h < OVERLAY_MIN_INNER_HEIGHT + 2 {
        return None;
    }
    let inner_h = box_h - 2;

    // Vertical scroll: if the cursor row sits below the visible
    // window, scroll down so the caret stays in view. Keep at least
    // one context row above the cursor when possible.
    let scroll_y = if cursor_row_in_wrapped >= inner_h as usize {
        cursor_row_in_wrapped + 1 - inner_h as usize
    } else {
        0
    };

    // Center the box on the whole terminal area.
    let box_x = area.x + (area.width - box_w) / 2;
    let box_y = area.y + (area.height.saturating_sub(box_h)) / 2;
    let box_rect = Rect::new(box_x, box_y, box_w, box_h);

    // Clear the area underneath so the frozen pane content doesn't
    // bleed through the border/padding.
    frame.render_widget(ratatui::widgets::Clear, box_rect);

    let title = Line::from(vec![
        Span::styled(" IME \u{276f} ", Style::default().fg(ACCENT_CLAUDE)),
        Span::styled(
            "compose",
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
    ]);
    let hint = Line::from(Span::styled(
        " Alt/Ctrl+Enter send \u{00b7} Enter newline \u{00b7} Ctrl+U clear \u{00b7} Esc cancel ",
        Style::default().fg(TEXT_DIM),
    ));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT_CLAUDE))
        .style(Style::default().bg(PANEL_BG))
        .title(title)
        .title_bottom(hint);
    let visible: Vec<Line> = wrapped
        .iter()
        .skip(scroll_y)
        .take(inner_h as usize)
        .map(|row| {
            Line::from(Span::styled(
                row.clone(),
                Style::default().fg(TEXT).bg(PANEL_BG),
            ))
        })
        .collect();
    let para = Paragraph::new(visible).block(block);
    frame.render_widget(para, box_rect);

    // Caret anchor. The host terminal's IME candidate window follows
    // wherever we park the hardware cursor — the whole reason this
    // widget exists.
    let caret_row_visible = cursor_row_in_wrapped.saturating_sub(scroll_y) as u16;
    let caret_x = box_x + 1 + cursor_col_in_wrapped as u16;
    let caret_y = box_y + 1 + caret_row_visible;
    // Clamp inside the box interior just in case the wrap math ever
    // drifts; better to place the caret on the last visible cell
    // than to leak it outside the border.
    let caret_x = caret_x.min(box_x + box_w.saturating_sub(2));
    let caret_y = caret_y.min(box_y + box_h.saturating_sub(2));
    Some((caret_x, caret_y))
}

/// Split `buffer` into display rows that fit within `inner_w` cols.
/// Each `\n` in the buffer starts a new row; within a logical line,
/// characters wrap once their cumulative display width reaches
/// `inner_w`. Returns the wrapped rows, the wrapped-space row
/// containing the cursor, and the cursor's column inside that row.
///
/// When `cursor_chars` sits exactly on a soft-wrap boundary (the
/// previous char filled the row to `inner_w`, and the next char
/// would wrap), the caret is reported at `(next_row, 0)` rather than
/// at `(current_row, inner_w)` — that column is past the visible
/// content area and would leak outside the border, and it would also
/// mislead users about where the next inserted character lands.
fn wrap_overlay_buffer(
    buffer: &str,
    cursor_chars: usize,
    inner_w: usize,
) -> (Vec<String>, usize, usize) {
    let inner_w = inner_w.max(1);
    let mut rows: Vec<String> = vec![String::new()];
    let mut current_col: usize = 0; // display columns in the current row
    let mut cur_row = 0usize;
    let mut cur_col = 0usize;
    let mut found_cursor = false;

    for (i, ch) in buffer.chars().enumerate() {
        // Look ahead: will this char trigger a soft wrap? Newlines
        // are handled separately, so `would_wrap` only applies to
        // visible chars.
        let w = if ch == '\n' {
            0
        } else {
            unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1)
        };
        let would_wrap = ch != '\n' && current_col + w > inner_w && current_col > 0;

        if i == cursor_chars {
            if would_wrap {
                // Cursor is exactly at a wrap boundary → report on
                // the next row's col 0, where the next insertion
                // will actually land.
                cur_row = rows.len(); // the row we are about to push
                cur_col = 0;
            } else {
                cur_row = rows.len() - 1;
                cur_col = current_col;
            }
            found_cursor = true;
        }

        if ch == '\n' {
            rows.push(String::new());
            current_col = 0;
            continue;
        }
        if would_wrap {
            rows.push(String::new());
            current_col = 0;
        }
        rows.last_mut().unwrap().push(ch);
        current_col += w;
    }
    if !found_cursor {
        cur_row = rows.len() - 1;
        cur_col = current_col;
    }
    (rows, cur_row, cur_col)
}

// ─── Tab bar ──────────────────────────────────────────────

fn render_tab_bar(app: &mut App, frame: &mut Frame, area: Rect) {
    let mut spans = Vec::new();
    let mut tab_rects = Vec::new();
    let mut x = area.x;

    // Logo
    spans.push(Span::styled(
        " \u{25c8} ",
        Style::default()
            .fg(ACCENT_CLAUDE)
            .bg(HEADER_BG)
            .add_modifier(Modifier::BOLD),
    ));
    x += 3;

    for (i, ws) in app.workspaces.iter().enumerate() {
        let is_active = i == app.active_tab;
        let renaming = is_active && app.rename_input.is_some();

        let label = if renaming {
            let buf = app.rename_input.as_deref().unwrap_or("");
            // Block cursor at end; placeholder when empty keeps the tab visible.
            format!(" {}\u{2588} ", buf)
        } else {
            format!(" {} ", ws.display_name())
        };
        let label_width = unicode_width::UnicodeWidthStr::width(label.as_str()) as u16;

        if renaming {
            spans.push(Span::styled(
                label.clone(),
                Style::default()
                    .fg(TEXT)
                    .bg(ACTIVE_TAB_BG)
                    .add_modifier(Modifier::BOLD),
            ));
        } else if is_active {
            // Active tab: identified via underline + bold + default fg vs DarkGray for inactive.
            spans.push(Span::styled(
                label.clone(),
                Style::default()
                    .fg(TEXT)
                    .bg(ACTIVE_TAB_BG)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            ));
        } else {
            spans.push(Span::styled(
                label.clone(),
                Style::default().fg(TEXT_DIM).bg(HEADER_BG),
            ));
        }

        tab_rects.push((i, Rect::new(x, area.y, label_width, 1)));
        x += label_width;

        spans.push(Span::styled(" ", Style::default().bg(HEADER_BG)));
        x += 1;
    }

    // [+] button
    let plus_label = " + ";
    spans.push(Span::styled(
        plus_label,
        Style::default().fg(ACCENT_GREEN).bg(HEADER_BG),
    ));
    let plus_rect = Rect::new(x, area.y, plus_label.len() as u16, 1);
    x += plus_label.len() as u16;

    // Fill remaining
    let remaining = area.width.saturating_sub(x - area.x);
    if remaining > 0 {
        spans.push(Span::styled(
            " ".repeat(remaining as usize),
            Style::default().bg(HEADER_BG),
        ));
    }

    app.last_tab_rects = tab_rects;
    app.last_new_tab_rect = Some(plus_rect);

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ─── Main area ────────────────────────────────────────────

/// Returns the focused pane's caret anchor, threaded up from
/// `render_terminal_content` so `render` can apply the caret-delivery policy
/// once at the top level.
fn render_main_area(app: &mut App, frame: &mut Frame, area: Rect) -> Option<(u16, u16)> {
    let tree_width = app.file_tree_width;
    let preview_width = app.preview_width;

    let mut has_tree = app.ws().file_tree_visible;
    let mut has_preview = app.ws().preview.is_active();

    let needed = MIN_PANE_AREA_WIDTH
        + if has_tree { tree_width } else { 0 }
        + if has_preview { preview_width } else { 0 };
    if area.width < needed && has_preview {
        has_preview = false;
    }
    let needed = MIN_PANE_AREA_WIDTH + if has_tree { tree_width } else { 0 };
    if area.width < needed && has_tree {
        has_tree = false;
    }

    let swapped = app.layout_swapped;

    let mut constraints = Vec::new();
    if has_tree {
        constraints.push(Constraint::Length(tree_width));
    }
    if swapped && has_preview {
        constraints.push(Constraint::Length(preview_width));
    }
    constraints.push(Constraint::Min(20));
    if !swapped && has_preview {
        constraints.push(Constraint::Length(preview_width));
    }

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(area);

    let mut idx = 0;

    if has_tree {
        app.ws_mut().last_file_tree_rect = Some(chunks[idx]);
        render_file_tree(app, frame, chunks[idx]);
        idx += 1;
    } else {
        app.ws_mut().last_file_tree_rect = None;
    }

    if swapped && has_preview {
        app.ws_mut().last_preview_rect = Some(chunks[idx]);
        render_preview(app, frame, chunks[idx]);
        idx += 1;
    }

    let pane_caret = render_panes(app, frame, chunks[idx]);
    idx += 1;

    if !swapped && has_preview {
        app.ws_mut().last_preview_rect = Some(chunks[idx]);
        render_preview(app, frame, chunks[idx]);
    }

    if !has_preview {
        app.ws_mut().last_preview_rect = None;
    }

    pane_caret
}

// ─── File tree ────────────────────────────────────────────

fn render_file_tree(app: &mut App, frame: &mut Frame, area: Rect) {
    let is_focused = app.ws().focus_target == FocusTarget::FileTree;
    let is_border_active = matches!(
        app.dragging.as_ref().or(app.hover_border.as_ref()),
        Some(DragTarget::FileTreeBorder)
    );
    let border_color = if is_border_active {
        ACCENT_GREEN
    } else if is_focused {
        FOCUS_BORDER
    } else {
        BORDER
    };

    let title_style = if is_focused {
        Style::default()
            .fg(ACCENT_BLUE)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(TEXT_DIM)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(" FILES ", title_style))
        .style(Style::default().bg(PANEL_BG));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let visible_height = inner.height as usize;
    app.ws_mut().file_tree.ensure_visible(visible_height);

    let entries = app.ws().file_tree.visible_entries();
    let scroll = app.ws().file_tree.scroll_offset;
    let selected = app.ws().file_tree.selected_index;
    let max_width = inner.width as usize;

    for (i, entry) in entries.iter().skip(scroll).take(visible_height).enumerate() {
        let y = inner.y + i as u16;
        let entry_index = scroll + i;
        let is_selected = entry_index == selected;

        // Selection indicator bar on the left
        let indicator = if is_selected { "\u{258e}" } else { " " }; // ▎ or space
        let indicator_style = if is_selected {
            Style::default()
                .fg(ACCENT_BLUE)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default()
        };

        // Tree indent with connector lines
        let indent = if entry.depth > 0 {
            let mut s = String::new();
            for _ in 0..entry.depth.saturating_sub(1) {
                s.push_str("\u{2502} "); // │
            }
            s.push_str("\u{251c}\u{2500}"); // ├─
            s
        } else {
            String::new()
        };

        // Icon + name
        let (icon, name_display, name_color) = if entry.is_dir {
            let icon = if entry.is_expanded {
                "\u{1f4c2} "
            } else {
                "\u{1f4c1} "
            }; // 📂 / 📁
            (icon, &entry.name, ACCENT_BLUE)
        } else {
            let (icon, color) = file_icon(&entry.name);
            (icon, &entry.name, color)
        };

        let content = format!("{}{}{}", indent, icon, name_display);
        let truncated = truncate_to_width(&content, max_width.saturating_sub(1));

        // Build styled spans
        let mut spans = vec![Span::styled(indicator, indicator_style)];

        let content_style = if is_selected {
            Style::default()
                .fg(TEXT)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default().fg(name_color).bg(PANEL_BG)
        };

        spans.push(Span::styled(truncated, content_style));

        // Fill remaining width
        let line_widget = Paragraph::new(Line::from(spans));
        frame.render_widget(line_widget, Rect::new(inner.x, y, inner.width, 1));
    }
}

// ─── Panes ────────────────────────────────────────────────

/// Returns the focused pane's caret anchor (the only pane that resolves one),
/// captured outside the per-pane borrow so it can be threaded up to `render`.
fn render_panes(app: &mut App, frame: &mut Frame, area: Rect) -> Option<(u16, u16)> {
    let rects = app.ws().layout.calculate_rects(area);
    app.ws_mut().last_pane_rects = rects.clone();

    for &(pane_id, rect) in &rects {
        if let Some(pane) = app.ws_mut().panes.get_mut(&pane_id) {
            let inner_rows = rect.height.saturating_sub(2);
            let inner_cols = rect.width.saturating_sub(2);
            let _ = pane.resize(inner_rows, inner_cols); // now returns Result<bool>
        }
    }

    // Update Claude monitor for each pane using the pane's own cwd
    // (may differ from workspace cwd if user cd'd inside the pane)
    let pane_cwds: Vec<(usize, std::path::PathBuf)> = rects
        .iter()
        .filter_map(|&(pane_id, _)| {
            app.ws()
                .panes
                .get(&pane_id)
                .map(|p| (pane_id, p.cwd.clone()))
        })
        .collect();
    for (pane_id, cwd) in pane_cwds {
        app.claude_monitor.update(pane_id, &cwd);
    }

    let focused_id = app.ws().focused_pane_id;
    let focus_target = app.ws().focus_target;
    let selection = app.selection.clone();
    let mut caret: Option<(u16, u16)> = None;
    for (pane_id, rect) in rects {
        if let Some(pane) = app.ws().panes.get(&pane_id) {
            let is_focused = pane_id == focused_id && focus_target == FocusTarget::Pane;
            let pane_sel = selection.as_ref().filter(
                |s| matches!(s.target, crate::app::SelectionTarget::Pane(id) if id == pane_id),
            );
            let claude_state = app.claude_monitor.state(pane_id);
            let pane_caret =
                render_single_pane(pane, is_focused, pane_sel, &claude_state, frame, rect);
            if is_focused {
                caret = pane_caret;
            }
        }
    }

    render_boundary_tint(app, frame, area);

    caret
}

/// Tint the shared internal divider that's being hovered or dragged so
/// it reads as draggable / double-clickable, matching the file-tree and
/// preview border affordance. The divider's two border columns (or
/// rows) are already painted by the adjacent panes; we just recolor
/// them over the divider's span. (Issue #247)
fn render_boundary_tint(app: &mut App, frame: &mut Frame, area: Rect) {
    let Some(DragTarget::PaneSplit(path, _, _)) =
        app.dragging.as_ref().or(app.hover_border.as_ref())
    else {
        return;
    };
    let path = path.clone();
    let Some(boundary) = app
        .ws()
        .layout
        .split_boundaries(area)
        .into_iter()
        .find(|b| b.path == path)
    else {
        return;
    };
    let buf = frame.buffer_mut();
    match boundary.direction {
        SplitDirection::Vertical => {
            for cx in [boundary.position.saturating_sub(1), boundary.position] {
                for y in boundary.span.0..boundary.span.1 {
                    if let Some(cell) = buf.cell_mut((cx, y)) {
                        cell.set_fg(ACCENT_GREEN);
                    }
                }
            }
        }
        SplitDirection::Horizontal => {
            for cy in [boundary.position.saturating_sub(1), boundary.position] {
                for x in boundary.span.0..boundary.span.1 {
                    if let Some(cell) = buf.cell_mut((x, cy)) {
                        cell.set_fg(ACCENT_GREEN);
                    }
                }
            }
        }
    }
}

/// Returns the caret anchor resolved by `render_terminal_content`, or `None`
/// when the pane isn't focused / has exited / has no visible caret this frame.
fn render_single_pane(
    pane: &crate::pane::Pane,
    is_focused: bool,
    selection: Option<&crate::app::TextSelection>,
    claude_state: &crate::claude_monitor::ClaudeState,
    frame: &mut Frame,
    area: Rect,
) -> Option<(u16, u16)> {
    // Cosmetic indicators (border accent, pane label) consume the
    // sticky `*_ever_seen()` latches, not the live title check —
    // Claude and Codex both rewrite their OSC titles to in-flight
    // task summaries that frequently drop the literal client name,
    // which would otherwise flip the indicators off mid-session
    // even though the client is still interactive. See issue #209.
    let is_claude = pane.claude_ever_seen();
    let is_codex = pane.codex_ever_seen();
    let client_accent = if is_claude {
        Some(ACCENT_CLAUDE)
    } else if is_codex {
        Some(ACCENT_CODEX)
    } else {
        None
    };
    let border_color = if is_focused {
        client_accent.unwrap_or(FOCUS_BORDER)
    } else {
        BORDER
    };

    let is_scrolled = pane.is_scrolled_back();
    let label = if is_claude {
        "claude"
    } else if is_codex {
        "codex"
    } else {
        "shell"
    };

    // Build claude status suffix
    let claude_suffix = if is_claude {
        let mut parts = Vec::new();
        if claude_state.subagent_count > 0 {
            // Show agent type names if available, else just count
            if !claude_state.subagent_types.is_empty() {
                parts.push(format!(
                    "\u{1f916} {}",
                    claude_state.subagent_types.join(", ")
                ));
            } else {
                parts.push(format!("\u{1f916}\u{00d7}{}", claude_state.subagent_count));
            }
        }
        if let Some(ref tool) = claude_state.current_tool {
            parts.push(format!("\u{1f527} {}", tool));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!(" {} ", parts.join(" "))
        }
    } else {
        String::new()
    };

    let pane_title = if is_focused {
        format!(" \u{25cf} {} [{}]{} ", label, pane.id, claude_suffix)
    } else {
        format!("   {} [{}]{} ", label, pane.id, claude_suffix)
    };

    let title_style = if is_focused {
        if let Some(accent) = client_accent {
            Style::default().fg(accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(FOCUS_BORDER)
                .add_modifier(Modifier::BOLD)
        }
    } else {
        Style::default().fg(TEXT_DIM)
    };

    // Bottom title: scroll indicator OR claude stats
    let bottom_title = if is_scrolled {
        Line::from(Span::styled(
            " \u{2191} SCROLL ",
            Style::default()
                .fg(ACCENT_CLAUDE)
                .bg(SCROLL_BG)
                .add_modifier(Modifier::BOLD),
        ))
    } else if is_claude {
        let mut spans = Vec::new();

        // Todo progress bar
        let (completed, total) = claude_state.todo_progress();
        if total > 0 {
            let bar = make_progress_bar(completed, total, 10);
            spans.push(Span::styled(
                format!(" \u{2713} {} {}/{} ", bar, completed, total),
                Style::default().fg(ACCENT_GREEN),
            ));
            // Show current in-progress task
            if let Some(current) = claude_state
                .todos
                .iter()
                .find(|t| t.status == "in_progress")
            {
                let short = truncate_to_width(&current.content, 30);
                spans.push(Span::styled(
                    format!("\u{25b6} {} ", short),
                    Style::default().fg(ACCENT_BLUE),
                ));
            }
        }

        // Total tokens used this session
        let total_tokens = claude_state.total_tokens();
        if total_tokens > 0 {
            spans.push(Span::styled(
                format!(" {} tokens ", format_tokens(total_tokens)),
                Style::default().fg(TEXT_DIM),
            ));
        }

        Line::from(spans)
    } else {
        Line::from("")
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(pane_title, title_style))
        .title_bottom(bottom_title)
        .style(Style::default().bg(BG));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if pane.exited {
        let msg = Paragraph::new("\u{2718} Process exited")
            .style(Style::default().fg(TEXT_DIM).bg(BG))
            .alignment(Alignment::Center);
        frame.render_widget(msg, inner);
        None
    } else {
        render_terminal_content(pane, is_focused, selection, frame, inner)
    }
}

/// Paints the pane's vt100 grid and returns the focused caret anchor (in
/// terminal coordinates) when one is resolved this frame, or `None`. The
/// caller (`render`) decides how to deliver it; this function no longer calls
/// `frame.set_cursor_position` itself, so on conpty the caret can be applied
/// after `terminal.draw` without ratatui re-showing it at a stale position
/// (#260).
fn render_terminal_content(
    pane: &crate::pane::Pane,
    is_focused: bool,
    selection: Option<&crate::app::TextSelection>,
    frame: &mut Frame,
    area: Rect,
) -> Option<(u16, u16)> {
    let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
    let screen = parser.screen();

    let rows = area.height as usize;
    let cols = area.width as usize;
    let buf = frame.buffer_mut();

    for row in 0..rows {
        for col in 0..cols {
            let cell = screen.cell(row as u16, col as u16);
            if let Some(cell) = cell {
                let x = area.x + col as u16;
                let y = area.y + row as u16;

                let contents = cell.contents();
                let display_char = if contents.is_empty() { " " } else { contents };

                let fg = vt100_color_to_ratatui(cell.fgcolor());
                let bg = vt100_color_to_ratatui(cell.bgcolor());

                let mut modifiers = Modifier::empty();
                if cell.bold() {
                    modifiers |= Modifier::BOLD;
                }
                if cell.italic() {
                    modifiers |= Modifier::ITALIC;
                }
                if cell.underline() {
                    modifiers |= Modifier::UNDERLINED;
                }

                let style = if cell.inverse() {
                    Style::default().fg(bg).bg(fg).add_modifier(modifiers)
                } else {
                    Style::default().fg(fg).bg(bg).add_modifier(modifiers)
                };

                // Apply selection highlight (only if dragged, not single click)
                let has_selection = selection.is_some_and(|s| {
                    let (sr, sc, er, ec) = s.normalized();
                    (sr != er || sc != ec) && s.contains(row as u32, col as u32)
                });
                let final_style = if has_selection {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    style
                };

                if let Some(buf_cell) = buf.cell_mut((x, y)) {
                    buf_cell.set_symbol(display_char);
                    buf_cell.set_style(final_style);
                }
            }
        }
    }

    // Show cursor when focused.
    // For non-Claude panes, respect the PTY's hide_cursor request.
    // For Claude Code, always show because Claude relies on the terminal cursor.
    //
    // The old bottom-row IME overlay used to gate this with
    // `!overlay_active` so the pane cursor wouldn't fight the overlay
    // caret. The centered overlay now renders AFTER `render_main_area`
    // and calls `frame.set_cursor_position` itself, so last-write-wins
    // already puts the caret inside the composition box when the
    // overlay is open. No extra gate needed.
    // Use the sticky `claude_ever_seen()` latch only while the PTY is
    // actively hiding its cursor. That preserves Claude's task-title
    // rewrite behavior (title drops "claude" mid-session) without
    // misclassifying a pane that once ran Claude but has since
    // returned to a normal shell prompt.
    let claude_pane = should_track_claude_caret(
        pane.is_claude_running(),
        pane.claude_ever_seen(),
        screen.hide_cursor(),
    );
    let mut caret: Option<(u16, u16)> = None;
    let show_cursor = is_focused && (!screen.hide_cursor() || claude_pane);
    if show_cursor {
        let cursor = screen.cursor_position();
        // Legacy Claude paints its visible caret as an inverse-video
        // cell somewhere at-or-left-of the vt100 cursor. Scan a small
        // window left of vt100 each frame for the closest inverse
        // cell — that is Claude's actual visible caret. The exact
        // offset varies by edit context (end-of-input ~1, mid-line
        // 0, post-backspace 2), so detect rather than hard-code.
        // Modern Claude (v2.1.x+) paints no inverse cell at all and
        // instead parks the visible hardware cursor on the edit cell;
        // `resolve_claude_caret` trusts that cursor when it finds no
        // inverse cell inside the input block.
        //
        // When the scan finds nothing — either Claude is in the
        // OFF phase of caret blink, or vt100 has briefly jumped to
        // a remote row to paint streaming output — fall back to the
        // most recently detected position cached on the Pane. This
        // keeps the host caret pinned to Claude's input cell across
        // both blink and streaming bursts instead of chasing vt100
        // around the screen.
        let detected: Option<(u16, u16)> = if claude_pane {
            let found = resolve_claude_caret(screen);
            if let Some(path) = std::env::var_os("RENGA_DEBUG_CURSOR_LOG") {
                if is_focused {
                    debug_log_caret_scan(screen, found, &path);
                }
            }
            if let Some(pos) = found {
                if let Ok(mut g) = pane.claude_caret_cache.lock() {
                    *g = Some(pos);
                }
            }
            found
        } else {
            None
        };
        let target: (u16, u16) = if claude_pane {
            detected
                .or_else(|| pane.claude_caret_cache.lock().ok().and_then(|g| *g))
                .unwrap_or(cursor)
        } else {
            cursor
        };
        // vt100 reports col == cols while an autowrap (DECAWM) is pending —
        // i.e. after a glyph lands in the last column but before the next
        // glyph wraps the line (vt100's `col_inc` advances past the last
        // column without clamping, so `cursor_position()` returns the
        // out-of-range column). Real terminals keep the caret visible on the
        // last cell in that state, so clamp the column instead of letting the
        // `< area.x + area.width` guard drop the caret entirely. Dropping it
        // left the host caret unset for the frame; on WSL/conpty the hardware
        // caret then stayed stranded at the last painted cell — the visible
        // desync on plain PTY (bash) panes. Claude panes never hit this: their
        // caret comes from `resolve_claude_caret`, which only ever returns an
        // in-range column.
        let cursor_y = area.y + target.0;
        if let Some(clamped_col) = clamp_caret_col(target.1, area.width) {
            if cursor_y < area.y + area.height {
                // Surfaced to `render` instead of set on the frame here, so the
                // out-of-range pending-wrap case still yields no caret (`None`)
                // exactly as before — preserving #253's plain-PTY behavior.
                caret = Some((area.x + clamped_col, cursor_y));
            }
        }
    }

    drop(parser); // release lock before scrollbar_info

    // Scrollbar on the right edge
    let (scroll_offset, total_lines) = pane.scrollbar_info();
    if total_lines > rows {
        let scrollbar_x = area.x + area.width - 1;
        let max_scroll = total_lines.saturating_sub(rows);
        let visible_ratio = rows as f32 / total_lines as f32;
        let thumb_height = (area.height as f32 * visible_ratio).max(1.0) as u16;

        // Position: 0 = bottom, max_scroll = top
        let scroll_ratio = if max_scroll > 0 {
            1.0 - (scroll_offset as f32 / max_scroll as f32)
        } else {
            1.0
        };
        let thumb_top = ((area.height - thumb_height) as f32 * scroll_ratio) as u16;

        let buf = frame.buffer_mut();
        for row in 0..area.height {
            let y = area.y + row;
            let is_thumb = row >= thumb_top && row < thumb_top + thumb_height;
            let (sym, style) = if is_thumb {
                ("\u{2588}", Style::default().fg(SCROLL_THUMB)) // █ thumb
            } else {
                ("\u{2502}", Style::default().fg(TEXT_DIM)) // │ track
            };
            if let Some(cell) = buf.cell_mut((scrollbar_x, y)) {
                cell.set_symbol(sym);
                cell.set_style(style);
            }
        }
    }

    caret
}

fn should_track_claude_caret(
    is_claude_running: bool,
    claude_ever_seen: bool,
    hide_cursor: bool,
) -> bool {
    is_claude_running || (claude_ever_seen && hide_cursor)
}

/// Map a vt100 cursor column onto a host-caret column inside a pane's inner
/// area of the given `width`.
///
/// vt100 reports `col == cols` while an autowrap (DECAWM) is pending — the
/// caret has advanced past the last column but no glyph has wrapped the line
/// yet (vt100's `col_inc` does not clamp). Clamp such an out-of-range column
/// onto the last visible cell (`width - 1`) so the caret stays visible on the
/// final cell, matching real terminals, instead of being dropped. Returns
/// `None` only when the area has zero width (nothing to draw into).
fn clamp_caret_col(col: u16, width: u16) -> Option<u16> {
    (width > 0).then(|| col.min(width - 1))
}

/// Prompt glyphs Claude Code renders at the left edge of its input box.
const CLAUDE_PROMPT_GLYPHS: &[&str] = &[
    ">", "\u{276F}", // ❯
    "\u{203A}", // ›
    "\u{27E9}", // ⟩
    "\u{3009}", // 〉
    "\u{276D}", // ❭
    "\u{2771}", // ❱
];

/// Columns scanned at the left of a row when looking for a prompt glyph.
const CLAUDE_PROMPT_SCAN_COLS: u16 = 8;

/// Maximum rows to walk downward from `prompt_row` while probing for
/// a wrapped continuation of Claude's input box. Each continuation row
/// must pass the `is_continuation_candidate` check below, so large
/// values are safe; the cap just bounds worst-case work per frame.
const CLAUDE_INPUT_WALK_MAX: u16 = 20;

fn cell_is_prompt_glyph(screen: &vt100::Screen, row: u16, col: u16) -> bool {
    screen
        .cell(row, col)
        .is_some_and(|c| CLAUDE_PROMPT_GLYPHS.iter().any(|g| *g == c.contents()))
}

fn row_starts_with_prompt(screen: &vt100::Screen, row: u16) -> bool {
    let cols = screen.size().1.min(CLAUDE_PROMPT_SCAN_COLS);
    (0..cols).any(|col| cell_is_prompt_glyph(screen, row, col))
}

fn row_col0_is_blank(screen: &vt100::Screen, row: u16) -> bool {
    screen
        .cell(row, 0)
        .map(|c| {
            let s = c.contents();
            s.is_empty() || s == " "
        })
        .unwrap_or(true)
}

/// A row qualifies as a wrapped-input continuation iff (a) col 0 is
/// blank (Claude indents continuation to match the prompt glyph width)
/// AND (b) the row has some non-blank content somewhere. Status /
/// hint / footer rows (`? for shortcuts`, `Tip: …`, etc.) have
/// non-blank content starting at col 0 and are rejected by (a); fully
/// blank padding rows are rejected by (b) and handled by the blank
/// streak counter in `resolve_input_row_last`.
fn is_continuation_candidate(screen: &vt100::Screen, row: u16) -> bool {
    row_col0_is_blank(screen, row) && row_has_non_blank(screen, row)
}

fn row_has_non_blank(screen: &vt100::Screen, row: u16) -> bool {
    let cols = screen.size().1;
    for col in 0..cols {
        if let Some(c) = screen.cell(row, col) {
            let s = c.contents();
            if !s.is_empty() && s != " " {
                return true;
            }
        }
    }
    false
}

/// Find the bottom-most row that contains a Claude prompt glyph
/// (`>`/`❯`/…) in its first few columns. Returns `None` if no prompt
/// row is visible (e.g. Claude is fully occluded by streaming).
fn find_prompt_row(screen: &vt100::Screen) -> Option<u16> {
    let screen_rows = screen.size().0;
    (0..screen_rows)
        .rev()
        .find(|&row| row_starts_with_prompt(screen, row))
}

/// Walk downward from `prompt_row` to find the bottom-most row that
/// still belongs to Claude's input box. Stops when it encounters a
/// nested prompt, a status/footer row (any row with non-blank content
/// at col 0), or two consecutive blank rows (input-box padding /
/// bottom of screen).
///
/// Returns the last row that qualified as a wrapped continuation.
/// Defaults to `prompt_row` when nothing below it looks like wrapped
/// content.
fn resolve_input_row_last(screen: &vt100::Screen, prompt_row: u16) -> u16 {
    let screen_rows = screen.size().0;
    let mut last = prompt_row;
    let mut blank_streak = 0u16;
    let max_row = prompt_row
        .saturating_add(CLAUDE_INPUT_WALK_MAX)
        .min(screen_rows.saturating_sub(1));
    let mut r = prompt_row.saturating_add(1);
    while r <= max_row {
        if row_starts_with_prompt(screen, r) {
            break;
        }
        if row_has_non_blank(screen, r) {
            // Has content — only accept as continuation if col 0 is
            // blank. Any row whose col 0 carries a non-space glyph is
            // a status / hint / footer line (e.g. `? for shortcuts`,
            // `Tip: …`) and terminates the walk without being
            // promoted.
            if is_continuation_candidate(screen, r) {
                last = r;
                blank_streak = 0;
            } else {
                break;
            }
        } else {
            blank_streak += 1;
            if blank_streak >= 2 {
                break;
            }
        }
        r = r.saturating_add(1);
    }
    last
}

/// Pick the column for Claude's visible caret on `row` using the
/// 3-tier priority established by PR #133: rightmost inverse cell,
/// else rightmost non-blank cell + 1, else column 2 (just after the
/// prompt glyph).
fn pick_caret_col_on_row(screen: &vt100::Screen, row: u16) -> u16 {
    let cols = screen.size().1;
    for col in (0..cols).rev() {
        if screen.cell(row, col).is_some_and(|c| c.inverse()) {
            return col;
        }
    }
    let mut last_nonblank: Option<u16> = None;
    for col in (0..cols).rev() {
        if let Some(c) = screen.cell(row, col) {
            let s = c.contents();
            if !s.is_empty() && s != " " {
                last_nonblank = Some(col);
                break;
            }
        }
    }
    last_nonblank
        .map(|c| c.saturating_add(1).min(cols.saturating_sub(1)))
        .unwrap_or(2)
}

/// Resolve Claude's visible caret position on the current screen,
/// accounting for wrapped input rows.
///
/// Algorithm (Proposal A of Issue #147, extended for ← navigation):
///   1. Find `prompt_row` bottom-up (row whose first few cols contain
///      `>`/`❯`/… — Claude's input box prompt glyph).
///   2. Walk downward up to `CLAUDE_INPUT_WALK_MAX` rows to compute
///      `input_row_last` — the bottom of the wrapped input block.
///   3. Scan `[prompt_row ..= input_row_last]` bottom-to-top for the
///      row that actually paints the inverse caret cell. The user can
///      move the caret back up with ← onto any wrapped row, including
///      the prompt row itself, and Claude redraws the inverse marker
///      on whichever row currently owns the caret. Picking the
///      bottom-most row with an inverse cell keeps the end-of-input
///      case (caret on last wrapped line) working while also handling
///      ← navigation back to earlier wrapped lines.
///   4. When no row in the block has an inverse cell, trust the
///      VISIBLE vt100 hardware cursor if it sits inside the input
///      block. Modern Claude Code (observed on v2.1.x, Windows
///      native) stopped painting an inverse caret cell entirely: it
///      positions the terminal cursor at the edit point (DECTCEM
///      visible) and lets the host terminal draw the caret. Without
///      this tier the resolver pinned the caret to "rightmost
///      non-blank + 1", so ←/→ appeared dead and mid-line edits
///      landed away from the drawn caret. Gating on the input block
///      preserves the #131/#133 protection: a cursor parked on a
///      remote row (spinner / streaming repaint) is still ignored.
///   5. Otherwise fall back to the 3-tier column search on
///      `input_row_last`.
///
/// Returns `None` when no prompt row is visible; callers fall back to
/// `claude_caret_cache` and finally to the raw vt100 cursor.
fn resolve_claude_caret(screen: &vt100::Screen) -> Option<(u16, u16)> {
    let prompt_row = find_prompt_row(screen)?;
    let last = resolve_input_row_last(screen, prompt_row);
    let cols = screen.size().1;
    for row in (prompt_row..=last).rev() {
        for col in (0..cols).rev() {
            if screen.cell(row, col).is_some_and(|c| c.inverse()) {
                return Some((row, col));
            }
        }
    }
    // Tier 4: no inverse caret anywhere in the input block — modern
    // Claude Code drives the hardware cursor instead. Trust it while
    // it's visible and inside the block. Clamp a pending-autowrap
    // column (vt100 reports col == cols) onto the last cell so this
    // function keeps its "only returns in-range columns" contract.
    if !screen.hide_cursor() {
        let (cur_row, cur_col) = screen.cursor_position();
        if (prompt_row..=last).contains(&cur_row) {
            return Some((cur_row, cur_col.min(cols.saturating_sub(1))));
        }
    }
    Some((last, pick_caret_col_on_row(screen, last)))
}

fn debug_log_caret_scan(
    screen: &vt100::Screen,
    resolved: Option<(u16, u16)>,
    path: &std::ffi::OsStr,
) {
    let (screen_rows, cols) = screen.size();
    let start_row = screen_rows.saturating_sub(8);
    let mut bottom_chars = String::new();
    for row in start_row..screen_rows {
        bottom_chars.push_str(&format!("r{row}:"));
        for col in 0..cols.min(12) {
            let s = screen
                .cell(row, col)
                .map(|c| c.contents().to_string())
                .unwrap_or_default();
            bottom_chars.push_str(&format!("[{s}]"));
        }
        bottom_chars.push(' ');
    }

    // Re-derive the same intermediate facts `resolve_claude_caret`
    // used so a captured log can tell the caret-jitter triggers apart
    // (Issue #260): A = vt100 cursor jumping between frames, C = the
    // in-block inverse cell appearing/disappearing across blink, E =
    // the input-block boundary shifting under streaming repaint. None
    // of this changes behavior — it only mirrors the resolver's reads.
    let raw_cursor = screen.cursor_position();
    let hide_cursor = screen.hide_cursor();
    let prompt_row = find_prompt_row(screen);
    let (block, inverse, tier) = match prompt_row {
        // No prompt row visible → `resolve_claude_caret` returns None
        // and the caller falls back to cache / raw vt100 cursor.
        None => (None, None, "none"),
        Some(pr) => {
            let last = resolve_input_row_last(screen, pr);
            // Rightmost inverse cell scanned bottom-to-top across the
            // whole block — identical order to the resolver's tier 1-3.
            let mut inv: Option<(u16, u16)> = None;
            'scan: for row in (pr..=last).rev() {
                for col in (0..cols).rev() {
                    if screen.cell(row, col).is_some_and(|c| c.inverse()) {
                        inv = Some((row, col));
                        break 'scan;
                    }
                }
            }
            // Mirror the resolver's branch selection: inverse cell wins,
            // else the trusted in-block hardware cursor (tier 4), else
            // the 3-tier column fallback on the last row (tier 5).
            let tier = if inv.is_some() {
                "inverse"
            } else if !hide_cursor && (pr..=last).contains(&raw_cursor.0) {
                "cursor"
            } else {
                "fallback"
            };
            (Some((pr, last)), inv, tier)
        }
    };
    let line = format!(
        "[renga dbg] resolved={resolved:?} raw_cursor={raw_cursor:?} \
         hide_cursor={hide_cursor} block={block:?} inverse={inverse:?} \
         tier={tier} bottom: {bottom_chars}\n"
    );
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::path::PathBuf::from(path))
    {
        use std::io::Write;
        let _ = f.write_all(line.as_bytes());
    }
}

// ─── Preview ──────────────────────────────────────────────

fn render_preview(app: &mut App, frame: &mut Frame, area: Rect) {
    // Extract values we need before any mutable borrow.
    let is_focused = app.ws().focus_target == FocusTarget::Preview;
    let filename = app.ws().preview.filename();
    let title = format!(" {} ", filename);
    let is_image = app.ws().preview.is_image();
    let is_binary = app.ws().preview.is_binary;
    let line_count = app.ws().preview.lines.len();
    let scroll_pos = app.ws().preview.scroll_offset;

    let is_border_active = matches!(
        app.dragging.as_ref().or(app.hover_border.as_ref()),
        Some(DragTarget::PreviewBorder)
    );
    let border_color = if is_border_active {
        ACCENT_GREEN
    } else if is_focused {
        ACCENT_CLAUDE
    } else {
        BORDER
    };

    // Line count in bottom-right
    let line_info = if is_image {
        Span::styled(" image ", Style::default().fg(TEXT_DIM))
    } else if !is_binary {
        Span::styled(
            format!(" {}/{} ", scroll_pos + 1, line_count),
            Style::default().fg(TEXT_DIM),
        )
    } else {
        Span::default()
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            title,
            Style::default()
                .fg(ACCENT_CLAUDE)
                .add_modifier(Modifier::BOLD),
        ))
        .title_bottom(line_info)
        .style(Style::default().bg(PANEL_BG));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Image preview
    if is_image {
        let is_dragging = app.dragging.is_some();
        if is_dragging {
            // Skip expensive Sixel re-encode during drag; show placeholder.
            let placeholder = Paragraph::new("Resizing...")
                .alignment(ratatui::layout::Alignment::Center)
                .style(Style::default().fg(TEXT_DIM).bg(PANEL_BG));
            frame.render_widget(placeholder, inner);
        } else if let Some(ref mut protocol) = app.ws_mut().preview.image_protocol {
            let image_widget = ratatui_image::StatefulImage::default().resize(
                ratatui_image::Resize::Fit(Some(ratatui_image::FilterType::CatmullRom)),
            );
            frame.render_stateful_widget(image_widget, inner, protocol);
        }
        return;
    }

    if is_binary {
        let msg = Paragraph::new(app.messages().preview_binary)
            .style(Style::default().fg(TEXT_DIM).bg(PANEL_BG));
        frame.render_widget(msg, inner);
        return;
    }

    let ws = app.ws();
    let visible_height = inner.height as usize;
    let scroll = ws.preview.scroll_offset;
    let h_scroll = ws.preview.h_scroll_offset;
    let has_highlights = !ws.preview.highlighted_lines.is_empty();

    for i in 0..visible_height {
        let line_idx = scroll + i;
        if line_idx >= ws.preview.lines.len() {
            break;
        }

        let y = inner.y + i as u16;
        let line_num = line_idx + 1;
        let num_str = format!("{:>4}\u{2502}", line_num);
        let max_content = (inner.width as usize).saturating_sub(5);

        let mut spans = vec![Span::styled(num_str, Style::default().fg(LINE_NUM_COLOR))];

        if has_highlights && line_idx < ws.preview.highlighted_lines.len() {
            // Drop `h_scroll` chars from the start of the line, walking
            // spans so syntax highlighting is preserved.
            let mut chars_skipped = 0usize;
            let mut used_width = 0usize;
            for styled_span in &ws.preview.highlighted_lines[line_idx] {
                if used_width >= max_content {
                    break;
                }

                let span_chars = styled_span.text.chars().count();
                let visible_text: std::borrow::Cow<'_, str> = if chars_skipped + span_chars
                    <= h_scroll
                {
                    // Entire span is off-screen to the left.
                    chars_skipped += span_chars;
                    continue;
                } else if chars_skipped >= h_scroll {
                    std::borrow::Cow::Borrowed(styled_span.text.as_str())
                } else {
                    // Partially skip into this span.
                    let skip_in_span = h_scroll - chars_skipped;
                    chars_skipped = h_scroll;
                    let remainder: String = styled_span.text.chars().skip(skip_in_span).collect();
                    std::borrow::Cow::Owned(remainder)
                };

                if visible_text.is_empty() {
                    continue;
                }
                let remaining = max_content - used_width;
                let text = truncate_to_width(&visible_text, remaining);
                used_width += unicode_width::UnicodeWidthStr::width(text.as_str());
                let (r, g, b) = styled_span.fg;
                spans.push(Span::styled(
                    text,
                    Style::default().fg(syntax_rgb_to_ansi_color(r, g, b)),
                ));
            }
        } else {
            let plain = &ws.preview.lines[line_idx];
            let dropped: String = plain.chars().skip(h_scroll).collect();
            let content = truncate_to_width(&dropped, max_content);
            spans.push(Span::styled(content, Style::default().fg(TEXT)));
        }

        let paragraph = Paragraph::new(Line::from(spans)).style(Style::default().bg(PANEL_BG));
        frame.render_widget(paragraph, Rect::new(inner.x, y, inner.width, 1));
    }

    // Selection highlight overlay. The selection is stored in SOURCE
    // coordinates (absolute line index + char offset into the line),
    // so we subtract the current scroll + h_scroll to produce screen
    // positions. Cells outside the visible window are skipped. The
    // highlighted band is also clamped to the actual line length so
    // it never paints past the text that would actually be copied.
    if let Some(sel) = app.selection.as_ref() {
        if matches!(sel.target, crate::app::SelectionTarget::Preview) {
            let (sr, sc, er, ec) = sel.normalized();
            if sr != er || sc != ec {
                let content = sel.content_rect;
                let scroll_v = ws.preview.scroll_offset as i64;
                let h_scroll = ws.preview.h_scroll_offset as i64;
                let buf = frame.buffer_mut();

                for abs_row in sr..=er {
                    let screen_row_i = abs_row as i64 - scroll_v;
                    if screen_row_i < 0 {
                        continue;
                    }
                    if screen_row_i >= content.height as i64 {
                        break;
                    }
                    let y = content.y + screen_row_i as u16;

                    // Line's actual character count (sets the right
                    // clamp for the highlight band).
                    let line_chars = ws
                        .preview
                        .lines
                        .get(abs_row as usize)
                        .map(|s| s.chars().count())
                        .unwrap_or(0);
                    if line_chars == 0 {
                        continue;
                    }

                    let src_col_start = if abs_row == sr { sc as usize } else { 0 };
                    let src_col_end_inclusive = if abs_row == er {
                        ec as usize
                    } else {
                        line_chars.saturating_sub(1)
                    };
                    let src_col_end_clamped =
                        src_col_end_inclusive.min(line_chars.saturating_sub(1));
                    if src_col_start > src_col_end_clamped {
                        continue;
                    }

                    for src_col in src_col_start..=src_col_end_clamped {
                        let screen_col_i = src_col as i64 - h_scroll;
                        if screen_col_i < 0 {
                            continue;
                        }
                        if screen_col_i >= content.width as i64 {
                            break;
                        }
                        let x = content.x + screen_col_i as u16;
                        if let Some(cell) = buf.cell_mut((x, y)) {
                            cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
                        }
                    }
                }
            }
        }
    }
}

// ─── macOS first-launch tip banner ────────────────────────

/// Warm yellow for the warning glyph + headline — visually distinct
/// from the status-bar hints directly below (dim gray + accent blue).
const TIP_WARN: Color = Color::Yellow;

/// Renders the 2-row Option-as-Meta banner. See `crate::macos_tip`
/// for the gating logic; this function is only called when the
/// banner has been surfaced, so there's no OS check here.
fn render_macos_tip(app: &App, frame: &mut Frame, area: Rect) {
    let m = app.messages();
    let para = Paragraph::new(vec![
        Line::from(Span::styled(
            m.macos_tip_line1,
            Style::default().fg(TIP_WARN).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            m.macos_tip_line2,
            Style::default().fg(ACCENT_BLUE),
        )),
    ])
    .style(Style::default().bg(HEADER_BG));
    frame.render_widget(para, area);
}

// ─── Status bar (context-aware) ───────────────────────────

fn render_status_bar(app: &App, frame: &mut Frame, area: Rect) {
    let focus = app.ws().focus_target;
    let m = app.messages();

    // Rename mode overrides focus-specific hints — key input is being
    // captured by the buffer regardless of which pane/panel is focused.
    let hints = if app.rename_input.is_some() {
        Line::from(vec![
            Span::styled(" Enter", Style::default().fg(ACCENT_BLUE)),
            Span::styled(m.rename_confirm, Style::default().fg(TEXT_DIM)),
            Span::styled("Esc", Style::default().fg(ACCENT_BLUE)),
            Span::styled(m.rename_cancel, Style::default().fg(TEXT_DIM)),
            Span::styled(m.rename_empty_enter_label, Style::default().fg(ACCENT_BLUE)),
            Span::styled(m.rename_reset, Style::default().fg(TEXT_DIM)),
        ])
    } else {
        match focus {
            FocusTarget::Preview => Line::from(vec![
                Span::styled(" Scroll", Style::default().fg(ACCENT_BLUE)),
                Span::styled(m.preview_scroll, Style::default().fg(TEXT_DIM)),
                Span::styled("^W", Style::default().fg(ACCENT_BLUE)),
                Span::styled(m.preview_close, Style::default().fg(TEXT_DIM)),
                Span::styled("^P", Style::default().fg(ACCENT_BLUE)),
                Span::styled(m.preview_swap, Style::default().fg(TEXT_DIM)),
                Span::styled("^Q", Style::default().fg(ACCENT_BLUE)),
                Span::styled(m.preview_quit, Style::default().fg(TEXT_DIM)),
            ]),
            FocusTarget::FileTree => Line::from(vec![
                Span::styled(" j/k", Style::default().fg(ACCENT_BLUE)),
                Span::styled(m.tree_move, Style::default().fg(TEXT_DIM)),
                Span::styled("h/l", Style::default().fg(ACCENT_BLUE)),
                Span::styled(m.tree_parent_child, Style::default().fg(TEXT_DIM)),
                Span::styled("Enter", Style::default().fg(ACCENT_BLUE)),
                Span::styled(m.tree_open, Style::default().fg(TEXT_DIM)),
                Span::styled("c/v", Style::default().fg(ACCENT_BLUE)),
                Span::styled(m.tree_claude_launch, Style::default().fg(TEXT_DIM)),
                Span::styled(".", Style::default().fg(ACCENT_BLUE)),
                Span::styled(m.tree_hidden, Style::default().fg(TEXT_DIM)),
                Span::styled("Esc", Style::default().fg(ACCENT_BLUE)),
                Span::styled(m.tree_back, Style::default().fg(TEXT_DIM)),
                Span::styled("^F", Style::default().fg(ACCENT_BLUE)),
                Span::styled(m.tree_close, Style::default().fg(TEXT_DIM)),
                Span::styled("^Q", Style::default().fg(ACCENT_BLUE)),
                Span::styled(m.tree_quit, Style::default().fg(TEXT_DIM)),
            ]),
            FocusTarget::Pane => Line::from(vec![
                Span::styled(" ^D", Style::default().fg(ACCENT_BLUE)),
                Span::styled(m.pane_split_vertical, Style::default().fg(TEXT_DIM)),
                Span::styled("^E", Style::default().fg(ACCENT_BLUE)),
                Span::styled(m.pane_split_horizontal, Style::default().fg(TEXT_DIM)),
                Span::styled("^W", Style::default().fg(ACCENT_BLUE)),
                Span::styled(m.pane_close, Style::default().fg(TEXT_DIM)),
                Span::styled("A-T", Style::default().fg(ACCENT_BLUE)),
                Span::styled(m.pane_new_tab, Style::default().fg(TEXT_DIM)),
                Span::styled("A-R", Style::default().fg(ACCENT_BLUE)),
                Span::styled(m.pane_rename_tab, Style::default().fg(TEXT_DIM)),
                Span::styled("^F", Style::default().fg(ACCENT_BLUE)),
                Span::styled(m.pane_tree, Style::default().fg(TEXT_DIM)),
                Span::styled("^P", Style::default().fg(ACCENT_BLUE)),
                Span::styled(m.pane_swap, Style::default().fg(TEXT_DIM)),
                Span::styled("^;/A-;", Style::default().fg(ACCENT_BLUE)),
                Span::styled(m.pane_ime, Style::default().fg(TEXT_DIM)),
                Span::styled("A-P", Style::default().fg(ACCENT_BLUE)),
                Span::styled(m.pane_peer_launch, Style::default().fg(TEXT_DIM)),
                Span::styled("^Q", Style::default().fg(ACCENT_BLUE)),
                Span::styled(m.pane_quit, Style::default().fg(TEXT_DIM)),
            ]),
        }
    };

    let status = Paragraph::new(hints).style(Style::default().bg(HEADER_BG));
    frame.render_widget(status, area);

    // Right-side info: Claude state of focused pane
    let focused_id = app.ws().focused_pane_id;
    let claude_state = app.claude_monitor.state(focused_id);
    let has_claude = app
        .ws()
        .panes
        .get(&focused_id)
        .is_some_and(|p| p.claude_ever_seen());

    let mut right_spans = Vec::new();

    if has_claude {
        // Model
        if let Some(model) = claude_state.short_model() {
            right_spans.push(Span::styled(
                format!(" \u{1f9e0} {} ", model),
                Style::default().fg(ACCENT_CLAUDE),
            ));
        }

        // Context usage
        if claude_state.context_tokens > 0 {
            let ratio = claude_state.context_usage();
            let bar = make_progress_bar((ratio * 10.0) as usize, 10, 6);
            let color = if ratio > 0.9 {
                Color::Red
            } else if ratio > 0.7 {
                Color::Yellow
            } else {
                ACCENT_GREEN
            };
            right_spans.push(Span::styled(
                format!(
                    " {} {}/{} ",
                    bar,
                    format_tokens(claude_state.context_tokens),
                    format_tokens(claude_state.context_limit())
                ),
                Style::default().fg(color),
            ));
        }
    }

    // Git branch (even without claude)
    if let Some(ref branch) = claude_state.git_branch {
        let short = truncate_to_width(branch, 20);
        right_spans.push(Span::styled(
            format!(" \u{2387} {} ", short),
            Style::default().fg(ACCENT_BLUE),
        ));
    }

    // Update notice (highest priority — overrides above if present)
    if let Some(new_version) = app.version_info.update_available() {
        right_spans.push(Span::styled(
            format!(" \u{2191} v{} ", new_version),
            Style::default()
                .fg(ACCENT_CLAUDE)
                .add_modifier(Modifier::BOLD),
        ));
    }

    if !right_spans.is_empty() {
        let total_width: u16 = right_spans
            .iter()
            .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()) as u16)
            .sum();
        if area.width > total_width {
            let right_rect = Rect::new(area.x + area.width - total_width, area.y, total_width, 1);
            let widget =
                Paragraph::new(Line::from(right_spans)).style(Style::default().bg(HEADER_BG));
            frame.render_widget(widget, right_rect);
        }
    }
}

// ─── Helpers ──────────────────────────────────────────────

/// Build a progress bar string like `▓▓▓▓░░░░░░`.
fn make_progress_bar(current: usize, total: usize, width: usize) -> String {
    if total == 0 {
        return String::new();
    }
    let filled = ((current as f32 / total as f32) * width as f32).round() as usize;
    let filled = filled.min(width);
    let mut s = String::with_capacity(width * 3);
    for _ in 0..filled {
        s.push('\u{2593}'); // ▓
    }
    for _ in filled..width {
        s.push('\u{2591}'); // ░
    }
    s
}

/// Format token count: 1234 → "1.2k", 1234567 → "1.2M"
fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn truncate_to_width(s: &str, max_width: usize) -> String {
    let mut result = String::new();
    let mut width = 0;
    for ch in s.chars() {
        let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > max_width {
            break;
        }
        result.push(ch);
        width += ch_width;
    }
    result
}

fn vt100_color_to_ratatui(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(idx) => Color::Indexed(idx),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

#[cfg(test)]
mod syntax_color_tests {
    use super::syntax_rgb_to_ansi_color;
    use ratatui::style::Color;

    #[test]
    fn maps_magenta_family_to_ansi_magenta() {
        assert!(matches!(
            syntax_rgb_to_ansi_color(0xcc, 0x99, 0xcc),
            Color::Magenta | Color::LightMagenta
        ));
        assert!(matches!(
            syntax_rgb_to_ansi_color(0xff, 0x00, 0xff),
            Color::Magenta | Color::LightMagenta
        ));
    }

    #[test]
    fn maps_cyan_family_to_ansi_cyan() {
        assert!(matches!(
            syntax_rgb_to_ansi_color(0x66, 0xcc, 0xcc),
            Color::Cyan | Color::LightCyan
        ));
        assert!(matches!(
            syntax_rgb_to_ansi_color(0x00, 0xff, 0xff),
            Color::Cyan | Color::LightCyan
        ));
    }

    #[test]
    fn maps_blue_family_to_ansi_blue() {
        assert!(matches!(
            syntax_rgb_to_ansi_color(0x66, 0x99, 0xcc),
            Color::Blue | Color::LightBlue
        ));
    }

    #[test]
    fn keeps_red_and_yellow_families_distinct() {
        assert!(matches!(
            syntax_rgb_to_ansi_color(0xf2, 0x77, 0x7a),
            Color::Red | Color::LightRed
        ));
        assert!(matches!(
            syntax_rgb_to_ansi_color(0xff, 0xff, 0x00),
            Color::Yellow | Color::LightYellow
        ));
    }

    #[test]
    fn maps_real_theme_gold_orange_and_brown_without_red_collapse() {
        let red = syntax_rgb_to_ansi_color(0xf2, 0x77, 0x7a);
        let orange = syntax_rgb_to_ansi_color(0xf9, 0x91, 0x57);
        let brown = syntax_rgb_to_ansi_color(0xd2, 0x7b, 0x53);

        assert!(matches!(
            syntax_rgb_to_ansi_color(0xff, 0xcc, 0x66),
            Color::Yellow | Color::LightYellow
        ));
        assert!(matches!(orange, Color::Yellow | Color::LightYellow));
        assert!(matches!(brown, Color::Yellow | Color::LightYellow));
        assert_ne!(orange, red);
        assert_ne!(brown, red);
    }

    #[test]
    fn maps_other_theme_yellows_to_ansi_yellow() {
        assert!(matches!(
            syntax_rgb_to_ansi_color(0xe5, 0xc0, 0x7b),
            Color::Yellow | Color::LightYellow
        ));
        assert!(matches!(
            syntax_rgb_to_ansi_color(0xb5, 0x89, 0x00),
            Color::Yellow | Color::LightYellow
        ));
    }
}

#[cfg(test)]
mod overlay_wrap_tests {
    use super::{clamp_caret_col, should_track_claude_caret, wrap_overlay_buffer};

    #[test]
    fn pending_autowrap_col_clamps_onto_last_cell() {
        // vt100 reports col == cols (here 80) while a DECAWM autowrap is
        // pending. The caret must land on the last visible cell (79), not be
        // dropped — dropping it leaves the host caret desynced on plain PTY
        // (bash) panes, especially on WSL/conpty.
        assert_eq!(clamp_caret_col(80, 80), Some(79));
    }

    #[test]
    fn in_range_caret_col_is_unchanged() {
        assert_eq!(clamp_caret_col(0, 80), Some(0));
        assert_eq!(clamp_caret_col(5, 80), Some(5));
        assert_eq!(clamp_caret_col(79, 80), Some(79));
    }

    #[test]
    fn zero_width_area_has_no_caret() {
        assert_eq!(clamp_caret_col(0, 0), None);
        assert_eq!(clamp_caret_col(3, 0), None);
    }

    #[test]
    fn empty_buffer_reports_origin() {
        let (rows, r, c) = wrap_overlay_buffer("", 0, 10);
        assert_eq!(rows, vec![String::new()]);
        assert_eq!((r, c), (0, 0));
    }

    #[test]
    fn cursor_on_soft_wrap_boundary_lands_on_next_row() {
        // "abcd" with inner_w=3 soft-wraps between 'c' and 'd'.
        // Cursor at 3 means "between c and d" → should appear at
        // col 0 of the row that holds 'd', not col 3 of the row
        // that ended at 'c'.
        let (rows, r, c) = wrap_overlay_buffer("abcd", 3, 3);
        assert_eq!(rows, vec!["abc".to_string(), "d".to_string()]);
        assert_eq!((r, c), (1, 0));
    }

    #[test]
    fn cursor_after_trailing_newline_lands_on_fresh_row() {
        let (rows, r, c) = wrap_overlay_buffer("abc\n", 4, 10);
        assert_eq!(rows, vec!["abc".to_string(), String::new()]);
        assert_eq!((r, c), (1, 0));
    }

    #[test]
    fn cursor_past_end_clamps_to_eof() {
        // Deliberately pass a cursor beyond buffer length — the
        // function must clamp without panicking so transient bad
        // state (e.g. race between overlay edit and render) never
        // crashes the TUI.
        let (rows, r, c) = wrap_overlay_buffer("abc", 99, 10);
        assert_eq!(rows, vec!["abc".to_string()]);
        assert_eq!((r, c), (0, 3));
    }

    #[test]
    fn cjk_width_two_wraps_at_display_width() {
        // Three CJK chars each width 2, inner_w=4 → row 0 holds
        // two chars, row 1 holds the third. Cursor at 2 (boundary)
        // should land on row 1 col 0.
        let (rows, r, c) = wrap_overlay_buffer("あいう", 2, 4);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], "あい");
        assert_eq!(rows[1], "う");
        assert_eq!((r, c), (1, 0));
    }

    #[test]
    fn hard_newline_resets_column() {
        let (rows, r, c) = wrap_overlay_buffer("ab\ncd", 5, 10);
        assert_eq!(rows, vec!["ab".to_string(), "cd".to_string()]);
        assert_eq!((r, c), (1, 2));
    }

    #[test]
    fn sticky_claude_latch_only_applies_while_pty_cursor_is_hidden() {
        assert!(should_track_claude_caret(false, true, true));
        assert!(!should_track_claude_caret(false, true, false));
        assert!(should_track_claude_caret(true, false, false));
    }
}

#[cfg(test)]
mod claude_caret_tests {
    use super::{
        find_prompt_row, pick_caret_col_on_row, resolve_claude_caret, resolve_input_row_last,
    };

    /// Drive a vt100 parser with raw bytes to construct a screen for
    /// assertions. Using the parser directly (rather than hand-building
    /// cells) keeps tests faithful to real Claude output.
    fn make_screen(rows: u16, cols: u16, bytes: &[u8]) -> vt100::Parser {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(bytes);
        parser
    }

    // SGR 7 = inverse on, SGR 27 = inverse off.
    const INV_ON: &[u8] = b"\x1b[7m";
    const INV_OFF: &[u8] = b"\x1b[27m";

    fn at(r: u16, c: u16) -> Vec<u8> {
        format!("\x1b[{};{}H", r + 1, c + 1).into_bytes()
    }

    #[test]
    fn single_line_prompt_places_caret_on_prompt_row() {
        // `> hi█` on row 2 of a 4-row screen.
        let mut bytes = at(2, 0);
        bytes.extend_from_slice(b"> hi");
        bytes.extend_from_slice(INV_ON);
        bytes.extend_from_slice(b" ");
        bytes.extend_from_slice(INV_OFF);
        let p = make_screen(4, 20, &bytes);
        let screen = p.screen();

        assert_eq!(find_prompt_row(screen), Some(2));
        assert_eq!(resolve_input_row_last(screen, 2), 2);
        let (r, c) = resolve_claude_caret(screen).unwrap();
        assert_eq!(r, 2);
        // Caret is the inverse blank at col 4 (after `> hi`).
        assert_eq!(c, 4);
    }

    #[test]
    fn wrapped_input_tracks_caret_onto_continuation_row() {
        // Row 2: `> aaaaaaaa` (prompt + 8 chars, fills width 10 after `> `)
        // Row 3: `  bb█`     (continuation with inverse caret)
        let mut bytes = at(2, 0);
        bytes.extend_from_slice(b"> aaaaaaaa");
        bytes.extend_from_slice(&at(3, 0));
        bytes.extend_from_slice(b"  bb");
        bytes.extend_from_slice(INV_ON);
        bytes.extend_from_slice(b" ");
        bytes.extend_from_slice(INV_OFF);
        let p = make_screen(5, 10, &bytes);
        let screen = p.screen();

        assert_eq!(find_prompt_row(screen), Some(2));
        // The walk must descend to the continuation row.
        assert_eq!(resolve_input_row_last(screen, 2), 3);
        let (r, c) = resolve_claude_caret(screen).unwrap();
        assert_eq!(r, 3, "caret must land on wrapped continuation row");
        assert_eq!(c, 4, "caret should sit on the inverse cell after `  bb`");
    }

    #[test]
    fn wrapped_input_caret_can_return_to_prompt_row() {
        // Regression for Issue #147 follow-up: after wrapping onto a
        // continuation row, the user presses ← to move the caret back
        // onto the prompt row. Claude repaints the inverse caret on
        // row 2 (prompt row) while row 3 still shows the wrapped
        // tail with no inverse cell. The resolver must track the
        // caret back to row 2 instead of sticking to the bottom row.
        let mut bytes = at(2, 0);
        bytes.extend_from_slice(b"> aa");
        bytes.extend_from_slice(INV_ON);
        bytes.extend_from_slice(b"a");
        bytes.extend_from_slice(INV_OFF);
        bytes.extend_from_slice(b"aaaaa");
        bytes.extend_from_slice(&at(3, 0));
        bytes.extend_from_slice(b"  bb");
        let p = make_screen(5, 10, &bytes);
        let screen = p.screen();

        assert_eq!(
            resolve_input_row_last(screen, 2),
            3,
            "walk still reaches the wrapped continuation row"
        );
        let (r, c) = resolve_claude_caret(screen).unwrap();
        assert_eq!(
            r, 2,
            "caret must follow the inverse cell back onto the prompt row"
        );
        assert_eq!(c, 4, "inverse caret sits on the `a` at col 4 of row 2");
    }

    #[test]
    fn wrapped_input_without_inverse_falls_back_to_last_nonblank_plus_one() {
        // Continuation row has text but no inverse cell (caret blink
        // OFF phase). The fallback should still place the caret on the
        // continuation row, just after its last non-blank column.
        let mut bytes = at(2, 0);
        bytes.extend_from_slice(b"> aaaaaaaa");
        bytes.extend_from_slice(&at(3, 0));
        bytes.extend_from_slice(b"  bbb");
        let p = make_screen(5, 10, &bytes);
        let screen = p.screen();

        let (r, c) = resolve_claude_caret(screen).unwrap();
        assert_eq!(r, 3);
        // Last non-blank on row 3 is col 4 (`b`); fallback lands at col 5.
        assert_eq!(c, 5);
    }

    #[test]
    fn visible_cursor_inside_input_box_is_trusted_without_inverse_cell() {
        // Regression (Windows native, Claude Code v2.1.x): modern Claude
        // paints NO inverse caret cell — it parks the visible hardware
        // cursor on the edit cell instead. Mid-line ←-navigation: text
        // `> hello world`, cursor moved back to col 8. The resolver must
        // return the cursor cell, not "rightmost non-blank + 1" (which
        // pinned the caret to end-of-input and made ←/→ look dead).
        let mut bytes = at(2, 0);
        bytes.extend_from_slice(b"> hello world");
        bytes.extend_from_slice(&at(2, 8));
        let p = make_screen(5, 20, &bytes);
        let screen = p.screen();

        assert_eq!(resolve_claude_caret(screen), Some((2, 8)));
    }

    #[test]
    fn visible_cursor_on_continuation_row_is_trusted_without_inverse_cell() {
        // Same as above, but the cursor sits mid-text on a wrapped
        // continuation row.
        let mut bytes = at(2, 0);
        bytes.extend_from_slice(b"> aaaaaaaa");
        bytes.extend_from_slice(&at(3, 0));
        bytes.extend_from_slice(b"  bbb");
        bytes.extend_from_slice(&at(3, 3));
        let p = make_screen(5, 10, &bytes);
        let screen = p.screen();

        assert_eq!(resolve_claude_caret(screen), Some((3, 3)));
    }

    #[test]
    fn hidden_cursor_inside_input_box_keeps_text_fallback() {
        // DECTCEM-hidden cursor (legacy Claude hides it while painting
        // its own inverse caret; also streaming repaints) must NOT be
        // trusted even when parked inside the input box.
        let mut bytes = at(2, 0);
        bytes.extend_from_slice(b"> hello world");
        bytes.extend_from_slice(&at(2, 8));
        bytes.extend_from_slice(b"\x1b[?25l");
        let p = make_screen(5, 20, &bytes);
        let screen = p.screen();

        // Falls back to last non-blank (`d` at col 12) + 1.
        assert_eq!(resolve_claude_caret(screen), Some((2, 13)));
    }

    #[test]
    fn visible_cursor_outside_input_box_keeps_text_fallback() {
        // #131/#133 protection: a visible cursor parked on a remote row
        // (spinner / streaming repaint) is not the input caret. Keep the
        // text-derived fallback on the input row.
        let mut bytes = at(2, 0);
        bytes.extend_from_slice(b"> hello");
        bytes.extend_from_slice(&at(0, 5));
        let p = make_screen(5, 20, &bytes);
        let screen = p.screen();

        // Last non-blank on row 2 is `o` at col 6; fallback lands at 7.
        assert_eq!(resolve_claude_caret(screen), Some((2, 7)));
    }

    #[test]
    fn inverse_cell_wins_over_visible_cursor() {
        // Legacy Claude paints the inverse caret cell and the vt100
        // cursor can sit 1-2 cells away (#129). The painted cell stays
        // authoritative when both signals are present.
        let mut bytes = at(2, 0);
        bytes.extend_from_slice(b"> hi");
        bytes.extend_from_slice(INV_ON);
        bytes.extend_from_slice(b" ");
        bytes.extend_from_slice(INV_OFF);
        bytes.extend_from_slice(&at(2, 1));
        let p = make_screen(5, 20, &bytes);
        let screen = p.screen();

        assert_eq!(resolve_claude_caret(screen), Some((2, 4)));
    }

    #[test]
    fn pending_wrap_cursor_inside_input_box_clamps_to_last_col() {
        // Typing into the last cell leaves vt100 in pending-autowrap
        // state reporting col == cols. The resolver must clamp onto the
        // last visible cell to keep its in-range contract.
        let mut bytes = at(2, 0);
        bytes.extend_from_slice(b"> aaaaaaaa"); // exactly fills width 10
        let p = make_screen(5, 10, &bytes);
        let screen = p.screen();

        assert_eq!(screen.cursor_position(), (2, 10), "pending-wrap premise");
        assert_eq!(resolve_claude_caret(screen), Some((2, 9)));
    }

    #[test]
    fn hint_row_stops_the_downward_walk() {
        // Hint `? for shortcuts` below the input row must NOT be
        // included — the caret stays on the prompt row.
        let mut bytes = at(2, 0);
        bytes.extend_from_slice(b"> hi");
        bytes.extend_from_slice(&at(3, 0));
        bytes.extend_from_slice(b"? for shortcuts");
        let p = make_screen(5, 20, &bytes);
        let screen = p.screen();

        assert_eq!(resolve_input_row_last(screen, 2), 2);
    }

    #[test]
    fn tip_footer_row_stops_the_walk() {
        // Guard against the `? for shortcuts`-only bias: any row whose
        // col 0 carries a non-blank glyph (e.g. `Tip: …`) must
        // terminate the walk, even when the footer text differs.
        let mut bytes = at(2, 0);
        bytes.extend_from_slice(b"> hi");
        bytes.extend_from_slice(&at(3, 0));
        bytes.extend_from_slice(b"Tip: press Alt+Enter to send");
        let p = make_screen(5, 30, &bytes);
        let screen = p.screen();

        assert_eq!(resolve_input_row_last(screen, 2), 2);
    }

    #[test]
    fn continuation_containing_question_mark_is_not_treated_as_hint() {
        // A wrapped continuation whose text happens to start with `?`
        // after the indent (e.g. `  ?foo`) must still be tracked. The
        // indent at col 0 is what distinguishes continuation from
        // hint rows.
        let mut bytes = at(2, 0);
        bytes.extend_from_slice(b"> aaaaaaaa");
        bytes.extend_from_slice(&at(3, 0));
        bytes.extend_from_slice(b"  ?foo");
        let p = make_screen(5, 10, &bytes);
        let screen = p.screen();

        assert_eq!(
            resolve_input_row_last(screen, 2),
            3,
            "col 0 indent marks this as continuation, not a hint"
        );
    }

    #[test]
    fn walk_tracks_seven_wrapped_continuation_rows() {
        // Narrow pane where input wraps many times. Each continuation
        // row must be reachable by the downward walk (cap is high
        // enough to cover realistic wrap depths).
        let mut bytes = at(0, 0);
        bytes.extend_from_slice(b"> aaaaaaaa"); // row 0
        for r in 1..=7u16 {
            bytes.extend_from_slice(&at(r, 0));
            bytes.extend_from_slice(b"  bb");
        }
        let p = make_screen(10, 10, &bytes);
        let screen = p.screen();

        assert_eq!(resolve_input_row_last(screen, 0), 7);
    }

    #[test]
    fn nested_prompt_below_stops_the_walk_before_it() {
        // A second `>` prompt below the first marks a new input box
        // and must not be swallowed as a continuation row.
        let mut bytes = at(1, 0);
        bytes.extend_from_slice(b"> older");
        bytes.extend_from_slice(&at(3, 0));
        bytes.extend_from_slice(b"> newer");
        let p = make_screen(5, 20, &bytes);
        let screen = p.screen();

        // `find_prompt_row` picks the bottom-most prompt.
        assert_eq!(find_prompt_row(screen), Some(3));
        // Walk from the lower prompt downward — nothing beneath.
        assert_eq!(resolve_input_row_last(screen, 3), 3);
    }

    #[test]
    fn two_consecutive_blank_rows_stop_the_walk() {
        // Row 1 prompt, rows 2 and 3 blank → walk stops without
        // promoting last beyond row 1.
        let mut bytes = at(1, 0);
        bytes.extend_from_slice(b"> hi");
        let p = make_screen(5, 10, &bytes);
        let screen = p.screen();

        assert_eq!(resolve_input_row_last(screen, 1), 1);
    }

    #[test]
    fn no_prompt_row_returns_none() {
        // Screen has text but no `>`/`❯` — e.g. plain shell or
        // streaming occlusion. Caller must fall back to cache/cursor.
        let bytes = b"hello world".to_vec();
        let p = make_screen(3, 20, &bytes);
        assert!(resolve_claude_caret(p.screen()).is_none());
    }

    #[test]
    fn empty_input_row_places_caret_right_after_prompt() {
        // `> ` with no text, no inverse cell. With the cursor visible
        // inside the input box (modern Claude), the resolver trusts it:
        // after printing `> ` the cursor sits at col 2 — exactly where
        // real Claude parks the caret of an empty input box.
        let mut bytes = at(2, 0);
        bytes.extend_from_slice(b"> ");
        let p = make_screen(4, 10, &bytes);
        let screen = p.screen();

        let (r, c) = resolve_claude_caret(screen).unwrap();
        assert_eq!((r, c), (2, 2));

        // With the cursor hidden (legacy Claude), the text-derived
        // fallback takes over: the trailing space is a real cell and
        // the non-blank search skips spaces, so it anchors on the `>`
        // at col 0 and places the caret at col 1. The hard-coded `2`
        // fallback only fires when the row has zero non-blank cells —
        // see `fully_blank_prompt_row_falls_back_to_col_two`.
        let mut bytes = at(2, 0);
        bytes.extend_from_slice(b"> \x1b[?25l");
        let p = make_screen(4, 10, &bytes);
        let screen = p.screen();

        assert_eq!(pick_caret_col_on_row(screen, 2), 1);
        let (r, c) = resolve_claude_caret(screen).unwrap();
        assert_eq!((r, c), (2, 1));
    }

    #[test]
    fn fully_blank_prompt_row_falls_back_to_col_two() {
        // Edge case: a row with zero non-blank cells. The col search
        // falls through to the hard-coded `2` fallback inherited from
        // PR #133.
        let p = make_screen(4, 10, b"");
        assert_eq!(pick_caret_col_on_row(p.screen(), 2), 2);
    }
}
