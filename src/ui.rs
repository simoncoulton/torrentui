//! Pure rendering: `draw(frame, &app)` has no side effects on app state.
//! Every colour comes from `app.theme`.

use ratatui::{
    Frame,
    layout::{Constraint, Flex, Layout, Position, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{
        Block, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Sparkline, Table,
        TableState, Wrap,
    },
};

use crate::{
    app::{App, Filter, Input, Mode},
    config::Rate,
    engine::{Status, TorrentView},
    fmt,
    theme::Theme,
};

pub fn draw(frame: &mut Frame, app: &App) {
    let t = &app.theme;
    // Only the foreground is themed; the background is left to the terminal.
    frame.render_widget(Block::new().style(t.base()), frame.area());

    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(15),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    // The file panel needs room: on a narrow terminal it stays closed.
    let (main, panel) = if app.file_panel && body.width >= PANEL_MIN_TERMINAL {
        let width = (body.width * 2 / 5).clamp(32, 70);
        let [main, panel] =
            Layout::horizontal([Constraint::Min(30), Constraint::Length(width)]).areas(body);
        (main, Some(panel))
    } else {
        (body, None)
    };
    let [list, details] =
        Layout::vertical([Constraint::Min(5), Constraint::Length(10)]).areas(main);

    draw_header(frame, app, header);
    draw_list(frame, app, list);
    draw_details(frame, app, details);
    if let Some(panel) = panel {
        draw_file_panel(frame, app, panel);
    }
    draw_footer(frame, app, footer);

    match &app.mode {
        Mode::Normal => {}
        Mode::Add => draw_add(frame, app),
        Mode::ConfirmRemove {
            name,
            folder,
            delete_files,
            ..
        } => draw_confirm(frame, t, name, folder, *delete_files),
        Mode::Files => draw_files(frame, app),
        Mode::Theme => draw_themes(frame, app),
        Mode::Limits => draw_limits(frame, app),
        Mode::ConfirmStop => draw_confirm_stop(frame, app),
        Mode::Tags => draw_tags(frame, app),
        Mode::Filter => draw_filter(frame, app),
        Mode::Help => draw_help(frame, t),
    }
}

/// Narrowest terminal that still gets a file panel.
const PANEL_MIN_TERMINAL: u16 = 90;

fn status_color(t: &Theme, s: Status) -> Color {
    match s {
        Status::Downloading => t.success,
        Status::Seeding => t.info,
        Status::Paused => t.warning,
        Status::Initializing => t.special,
        Status::Error => t.error,
    }
}

fn block<'a>(t: &Theme) -> Block<'a> {
    Block::bordered().border_style(Style::new().fg(t.border))
}

fn title<'a>(t: &Theme, text: impl Into<String>) -> Span<'a> {
    Span::styled(text.into(), Style::new().fg(t.accent).bold())
}

fn key<'a>(t: &Theme, k: &'static str) -> Span<'a> {
    Span::styled(k, Style::new().fg(t.accent).bold())
}

/// Clear a popup area and restore the theme's base style.
fn popup(frame: &mut Frame, t: &Theme, area: Rect) {
    frame.render_widget(Clear, area);
    frame.render_widget(Block::new().style(t.base()), area);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [row] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    let [rect] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(row);
    rect
}

fn rate_with_limit<'a>(t: &Theme, current: u64, limit: Rate) -> Vec<Span<'a>> {
    let mut spans = vec![Span::raw(fmt::rate_or_zero(current))];
    if limit.bps().is_some() {
        spans.push(Span::styled(
            format!(" (max {})", limit.to_text()),
            t.muted(),
        ));
    }
    spans
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let t = &app.theme;
    let block = block(t)
        .title(title(t, " torrentui "))
        .title_bottom(Line::styled(format!(" {} ", app.download_dir), t.muted()).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [text, spark] =
        Layout::horizontal([Constraint::Length(62), Constraint::Min(0)]).areas(inner);
    let mut spans = vec![Span::styled("↓ ", Style::new().fg(t.success))];
    spans.extend(rate_with_limit(t, app.total_down(), app.limits.0));
    spans.push(Span::raw("   "));
    spans.push(Span::styled("↑ ", Style::new().fg(t.info)));
    spans.extend(rate_with_limit(t, app.total_up(), app.limits.1));
    spans.push(Span::raw(format!("   {} torrents", app.torrents.len())));
    frame.render_widget(Paragraph::new(Line::from(spans)), text);

    // Only show the most recent samples that fit the width.
    let width = spark.width as usize;
    let skip = app.down_history.len().saturating_sub(width);
    let data: Vec<u64> = app.down_history.iter().skip(skip).copied().collect();
    frame.render_widget(
        Sparkline::default()
            .data(data)
            .style(Style::new().fg(t.success)),
        spark,
    );
}

fn draw_list(frame: &mut Frame, app: &App, area: Rect) {
    let t = &app.theme;
    let heading = match &app.filter {
        Filter::All => " Torrents ".to_string(),
        f => format!(
            " Torrents — {} ({} of {}) ",
            filter_label(f),
            app.visible().len(),
            app.torrents.len()
        ),
    };
    let block = block(t).title(title(t, heading));

    if app.torrents.is_empty() {
        let msg = Paragraph::new(vec![
            Line::from(""),
            Line::from("No torrents yet.").centered(),
            Line::from(vec![
                Span::raw("Press "),
                key(t, "a"),
                Span::raw(" to add a magnet link or .torrent file."),
            ])
            .centered(),
        ])
        .block(block);
        frame.render_widget(msg, area);
        return;
    }

    // The tags column only earns its width on wide terminals; narrower ones
    // still see tags in the details pane.
    const TAGS_W: u16 = 16;
    let show_tags = area.width >= 110 && app.visible().any(|tor| !app.tags_of(tor).is_empty());

    let mut titles = vec!["Name"];
    if show_tags {
        titles.push("Tags");
    }
    titles.extend(["Size", "Progress", "Down", "Up", "Peers", "ETA", "Status"]);
    let header = Row::new(titles).style(Style::new().fg(t.accent).bold());

    let rows = app.visible().map(|tor| {
        let pct = fmt::percent(tor.progress_bytes, tor.total_bytes);
        let mut cells = vec![Cell::from(tor.name.clone())];
        if show_tags {
            cells.push(
                Cell::from(fit(&app.tags_of(tor).join(", "), TAGS_W as usize))
                    .style(Style::new().fg(t.accent)),
            );
        }
        cells.extend([
            Cell::from(fmt::bytes(tor.total_bytes)),
            Cell::from(format!("{} {:>5.1}%", fmt::bar(pct, 10), pct))
                .style(Style::new().fg(status_color(t, tor.status))),
            Cell::from(fmt::rate(tor.down_bps)),
            Cell::from(fmt::rate(tor.up_bps)),
            Cell::from(tor.peers_live.to_string()),
            Cell::from(tor.eta.clone().unwrap_or_else(|| "-".into())),
            Cell::from(tor.status.label()).style(Style::new().fg(status_color(t, tor.status))),
        ]);
        Row::new(cells)
    });

    let mut widths = vec![Constraint::Min(16)];
    if show_tags {
        widths.push(Constraint::Length(TAGS_W));
    }
    widths.extend([
        Constraint::Length(10),
        Constraint::Length(18),
        Constraint::Length(12),
        Constraint::Length(12),
        Constraint::Length(5),
        Constraint::Length(9),
        Constraint::Length(11),
    ]);

    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .column_spacing(1)
        .row_highlight_style(t.selection())
        .highlight_symbol("▶ ");

    let mut state = TableState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_details(frame: &mut Frame, app: &App, area: Rect) {
    let t = &app.theme;
    let block = block(t).title(title(t, " Details "));
    let Some(tor) = app.current() else {
        frame.render_widget(block, area);
        return;
    };
    frame.render_widget(
        Paragraph::new(detail_lines(t, tor, app.tags_of(tor)))
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// The right-hand panel: every file of the selected torrent with its own
/// download progress.
fn draw_file_panel(frame: &mut Frame, app: &App, area: Rect) {
    let t = &app.theme;
    let mut block = block(t).title(title(t, " Files "));
    let message = |text: &str| Paragraph::new(Span::styled(text.to_string(), t.muted()));
    let inner = block.inner(area);
    let Some(id) = app.panel_wanted() else {
        frame.render_widget(block, area);
        return;
    };
    let Some(files) = app.panel_files.as_ref().filter(|p| p.id == id) else {
        frame.render_widget(block, area);
        frame.render_widget(message("Loading…"), inner);
        return;
    };

    let entries = &files.list.entries;
    let rows = inner.height as usize;
    // Page keys can overshoot; stop at the last full page.
    let start = app.panel_scroll.min(entries.len().saturating_sub(rows));
    if entries.len() > rows {
        let end = (start + rows).min(entries.len());
        block = block.title_bottom(Span::styled(
            format!(" {}-{} of {} (PgUp/PgDn) ", start + 1, end, entries.len()),
            t.muted(),
        ));
    }
    // " 100%" is the right column; a space separates it from the name.
    const PCT_W: usize = 4;
    let name_w = (inner.width as usize).saturating_sub(PCT_W + 1);
    let lines: Vec<Line> = entries
        .iter()
        .skip(start)
        .take(rows)
        .map(|f| {
            let name = format!("{:<name_w$} ", fit(&f.path, name_w));
            if !files.selected.contains(&f.index) {
                return Line::styled(format!("{name}{:>PCT_W$}", "skip"), t.muted());
            }
            // Whole percent, rounded down: 100% means the file is complete.
            let pct = (f.done * 100).checked_div(f.len).unwrap_or(100);
            let style = match pct {
                100 => Style::new().fg(t.success),
                0 => t.muted(),
                _ => Style::new().fg(t.warning),
            };
            Line::from(vec![
                Span::raw(name),
                Span::styled(format!("{:>PCT_W$}", format!("{pct}%")), style),
            ])
        })
        .collect();
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn detail_lines(t: &Theme, tor: &TorrentView, tags: &[String]) -> Vec<Line<'static>> {
    let label = |s: &'static str| Span::styled(format!("{s:<10}"), t.muted());
    let ratio = if tor.total_bytes > 0 {
        tor.uploaded_bytes as f64 / tor.total_bytes as f64
    } else {
        0.0
    };
    let mut lines = vec![
        Line::from(vec![label("Name"), Span::raw(tor.name.clone()).bold()]),
        Line::from(
            std::iter::once(label("Tags"))
                .chain(if tags.is_empty() {
                    vec![Span::styled("none (T to add)", t.muted())]
                } else {
                    tags.iter()
                        .map(|tag| Span::styled(format!("#{tag} "), Style::new().fg(t.accent)))
                        .collect()
                })
                .collect::<Vec<_>>(),
        ),
        Line::from(vec![label("Hash"), Span::raw(tor.info_hash.clone())]),
        Line::from(vec![
            label("Saved to"),
            Span::raw(tor.output_folder.display().to_string()),
        ]),
        Line::from(vec![
            label("Progress"),
            Span::raw(format!(
                "{} / {} ({:.1}%)",
                fmt::bytes(tor.progress_bytes),
                fmt::bytes(tor.total_bytes),
                fmt::percent(tor.progress_bytes, tor.total_bytes)
            )),
        ]),
        Line::from(vec![
            label("Uploaded"),
            Span::raw(format!(
                "{} (ratio {:.2})",
                fmt::bytes(tor.uploaded_bytes),
                ratio
            )),
        ]),
        Line::from(vec![
            label("Peers"),
            Span::raw(format!(
                "{} live, {} connecting, {} queued, {} seen",
                tor.peers_live, tor.peers_connecting, tor.peers_queued, tor.peers_seen
            )),
        ]),
    ];
    if let Some(err) = &tor.error {
        lines.push(Line::from(vec![
            label("Error"),
            Span::styled(err.clone(), Style::new().fg(t.error)),
        ]));
    }
    lines
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let t = &app.theme;
    let line = if let Some((msg, is_error)) = app.status() {
        let color = if is_error { t.error } else { t.success };
        let mut spans = vec![Span::styled(msg.to_string(), Style::new().fg(color))];
        if app.pending_adds > 0 {
            spans.push(Span::styled(
                format!("  [adding {}…]", app.pending_adds),
                Style::new().fg(t.warning),
            ));
        }
        push_badge(&mut spans, t, app);
        Line::from(spans)
    } else if app.pending_adds > 0 {
        let mut spans = vec![Span::styled(
            format!(
                "Adding {} torrent(s) — fetching metadata…",
                app.pending_adds
            ),
            Style::new().fg(t.warning),
        )];
        push_badge(&mut spans, t, app);
        Line::from(spans)
    } else {
        // (key, label, rank): when the terminal is too narrow, the highest
        // rank (rightmost first) is dropped until the rest fit.
        let mut hints: Vec<(&'static str, &'static str, u8)> = vec![
            ("a", "add", 0),
            (
                "p",
                // Say what pressing it will do to the selected torrent.
                if app.current().is_some_and(|t| t.status == Status::Paused) {
                    "unpause"
                } else {
                    "pause"
                },
                1,
            ),
            ("f", "files", 1),
            ("t", "tags", 1),
            ("/", "filter", 1),
            ("d", "remove", 1),
            ("x", "actions", 1),
            ("l", "limits", 2),
            ("q", "quit", 0),
            ("Q", "quit + daemon", 2),
            ("?", "help", 0),
        ];
        if app.filter != Filter::All {
            hints.insert(0, ("Esc", "show all", 0));
        }
        if !app.has_actions {
            hints.retain(|h| h.0 != "x");
        }
        // These act on the selected torrent, so they mean nothing without one.
        if app.current().is_none() {
            hints.retain(|h| !matches!(h.0, "p" | "f" | "t" | "d" | "x"));
        }
        let badge = actions_badge(t, app);
        let badge_w: usize = badge.iter().map(|s| s.content.chars().count()).sum();
        let width = |h: &[(&str, &str, u8)]| {
            h.iter()
                .map(|(k, text, _)| k.chars().count() + 1 + text.len() + 2)
                .sum::<usize>()
        };
        // The last hint's trailing spaces don't need room, unless a badge follows.
        let slack = if badge_w == 0 { 2 } else { 0 };
        while width(&hints) + badge_w > area.width as usize + slack {
            let worst = hints.iter().map(|h| h.2).max().unwrap_or(0);
            if worst == 0 {
                break;
            }
            if let Some(i) = hints.iter().rposition(|h| h.2 == worst) {
                hints.remove(i);
            }
        }
        let spans: Vec<Span> = hints
            .into_iter()
            .flat_map(|(k, text, _)| [key(t, k), Span::raw(format!(" {text}  "))])
            .chain(badge)
            .collect();
        Line::from(spans)
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// Draw a single-line text field and return the cursor position.
fn draw_input(
    frame: &mut Frame,
    t: &Theme,
    field: Rect,
    input: &Input,
    placeholder: &str,
) -> Option<Position> {
    if field.width == 0 || field.height == 0 {
        return None;
    }
    let width = field.width as usize;
    let text: Vec<char> = input.text().chars().collect();
    if text.is_empty() {
        frame.render_widget(
            Paragraph::new(placeholder.to_string()).style(t.muted()),
            field,
        );
        return Some(Position::new(field.x, field.y));
    }
    // Scroll horizontally so the cursor is always visible.
    let cursor = input.cursor();
    let start = (cursor + 1).saturating_sub(width);
    let visible: String = text.iter().skip(start).take(width).collect();
    frame.render_widget(Paragraph::new(visible), field);
    let col = (cursor - start).min(width - 1) as u16;
    Some(Position::new(field.x + col, field.y))
}

fn draw_add(frame: &mut Frame, app: &App) {
    let t = &app.theme;
    let full = frame.area();
    let area = centered(full, full.width.saturating_sub(4).clamp(1, 90), 5);
    popup(frame, t, area);
    let block = Block::bordered()
        .border_style(Style::new().fg(t.accent))
        .title(title(t, " Add torrent "))
        .title_bottom(Line::styled(" Enter: add  Esc: cancel ", t.muted()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [hint, field] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(inner);
    frame.render_widget(
        Paragraph::new("Magnet link, .torrent URL, or path to a .torrent file (paste works)")
            .style(t.muted()),
        hint,
    );
    if let Some(pos) = draw_input(frame, t, field, &app.input, "") {
        frame.set_cursor_position(pos);
    }
}

fn draw_confirm(frame: &mut Frame, t: &Theme, name: &str, folder: &str, delete_files: bool) {
    let full = frame.area();
    let area = centered(full, full.width.saturating_sub(4).clamp(1, 70), 9);
    popup(frame, t, area);
    let (heading, lines) = if delete_files {
        (
            " Remove torrent AND DELETE its data? ",
            vec![
                Line::from(name.to_string()).bold(),
                Line::from(vec![
                    Span::styled("Saved in ", t.muted()),
                    Span::raw(folder.to_string()),
                ]),
                Line::from(""),
                Line::styled(
                    "This permanently deletes the downloaded files.",
                    Style::new().fg(t.error).bold(),
                ),
                Line::from(vec![
                    Span::styled("y", Style::new().fg(t.error).bold()),
                    Span::raw(" yes, delete them    "),
                    key(t, "n"),
                    Span::raw(" / Esc  cancel"),
                ]),
            ],
        )
    } else {
        (
            " Remove torrent? ",
            vec![
                Line::from(name.to_string()).bold(),
                Line::styled("Downloaded files are kept on disk.", t.muted()),
                Line::from(""),
                Line::from(vec![
                    key(t, "y"),
                    Span::raw(" / Enter  remove    "),
                    key(t, "n"),
                    Span::raw(" / Esc  cancel"),
                ]),
                Line::from(vec![
                    Span::styled("D", Style::new().fg(t.error).bold()),
                    Span::raw("  remove and delete the data instead…"),
                ]),
            ],
        )
    };
    let block = Block::bordered()
        .border_style(Style::new().fg(if delete_files { t.error } else { t.accent }))
        .title(Span::styled(
            heading,
            Style::new()
                .fg(if delete_files { t.error } else { t.accent })
                .bold(),
        ));
    frame.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
        area,
    );
}

/// Truncate `s` to at most `width` chars, marking the cut with an ellipsis.
fn fit(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else if width == 0 {
        String::new()
    } else {
        let head: String = s.chars().take(width - 1).collect();
        format!("{head}…")
    }
}

fn draw_files(frame: &mut Frame, app: &App) {
    let t = &app.theme;
    let Some(picker) = &app.picker else { return };
    let full = frame.area();
    let width = full.width.saturating_sub(4).clamp(1, 110);
    let height = full.height.saturating_sub(2).clamp(1, 24);
    let area = centered(full, width, height);
    popup(frame, t, area);

    let block = Block::bordered()
        .border_style(Style::new().fg(t.accent))
        .title(title(t, format!(" Select files — {} ", picker.title)))
        .title_bottom(Line::styled(
            " Space toggle  a all  n none  i invert  Enter apply  Esc cancel ",
            t.muted(),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [summary, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    frame.render_widget(
        Paragraph::new(format!(
            "{} of {} files selected ({})",
            picker.selected_count(),
            picker.files.len(),
            fmt::bytes(picker.selected_bytes())
        ))
        .style(Style::new().fg(t.accent)),
        summary,
    );

    // Fixed-width, right-aligned size (and progress) column so rows line up.
    let tail_w: usize = if picker.show_progress() { 19 } else { 11 };
    // 2 for the highlight symbol, 4 for "[x] ", 2 for the gap.
    let name_w = (list_area.width as usize).saturating_sub(2 + 4 + 2 + tail_w);
    let items: Vec<ListItem> = picker
        .files
        .iter()
        .zip(&picker.checked)
        .map(|(f, checked)| {
            let mark = if *checked { "[x]" } else { "[ ]" };
            let tail = if picker.show_progress() {
                format!(
                    "{:>5.1}%  {:>9}",
                    fmt::percent(f.done, f.len),
                    fmt::bytes(f.len)
                )
            } else {
                format!("{:>9}", fmt::bytes(f.len))
            };
            let line = Line::from(vec![
                Span::raw(format!("{mark} {:<name_w$}", fit(&f.path, name_w))),
                Span::styled(format!("  {tail:>tail_w$}"), t.muted()),
            ]);
            let style = if *checked { Style::new() } else { t.muted() };
            ListItem::new(line).style(style)
        })
        .collect();
    let list = List::new(items)
        .highlight_style(t.selection())
        .highlight_symbol("▶ ");
    let mut state = ListState::default().with_selected(Some(picker.cursor));
    frame.render_stateful_widget(list, list_area, &mut state);
}

fn draw_themes(frame: &mut Frame, app: &App) {
    let t = &app.theme;
    let Some(picker) = &app.theme_picker else {
        return;
    };
    let full = frame.area();
    let height = (app.themes.len() as u16 + 2).clamp(3, full.height.saturating_sub(2).max(3));
    let area = centered(full, full.width.saturating_sub(4).clamp(1, 56), height);
    popup(frame, t, area);
    let block = Block::bordered()
        .border_style(Style::new().fg(t.accent))
        .title(title(t, " Theme "))
        .title_bottom(Line::styled(
            " ↑↓ preview  Enter save  Esc cancel ",
            t.muted(),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let name_w = app
        .themes
        .iter()
        .map(|th| th.name.chars().count())
        .max()
        .unwrap_or(0);
    let items: Vec<ListItem> = app
        .themes
        .iter()
        .map(|th| {
            let mut spans = vec![Span::raw(format!("{:<name_w$}  ", th.name))];
            // Each theme's own colours, so you can compare before choosing.
            spans.extend(
                th.swatch()
                    .into_iter()
                    .map(|c| Span::styled("██ ", Style::new().fg(c))),
            );
            ListItem::new(Line::from(spans))
        })
        .collect();
    let list = List::new(items)
        .highlight_style(t.selection())
        .highlight_symbol("▶ ");
    let mut state = ListState::default().with_selected(Some(picker.cursor));
    frame.render_stateful_widget(list, inner, &mut state);
}

fn draw_limits(frame: &mut Frame, app: &App) {
    let t = &app.theme;
    let Some(dialog) = &app.limits_dialog else {
        return;
    };
    let full = frame.area();
    let area = centered(full, full.width.saturating_sub(4).clamp(1, 56), 8);
    popup(frame, t, area);
    let block = Block::bordered()
        .border_style(Style::new().fg(t.accent))
        .title(title(t, " Speed limits "))
        .title_bottom(Line::styled(
            " Tab switch  Enter save  Esc cancel ",
            t.muted(),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [hint, down, up, err] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(2),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new("e.g. 500KiB/s, 2MiB/s — empty or 0 = unlimited").style(t.muted()),
        hint,
    );

    let mut cursor = None;
    for (i, (label, row, input)) in [
        ("Download", down, &dialog.download),
        ("Upload", up, &dialog.upload),
    ]
    .into_iter()
    .enumerate()
    {
        let [label_area, field] =
            Layout::horizontal([Constraint::Length(10), Constraint::Min(0)]).areas(row);
        let style = if dialog.focus == i {
            Style::new().fg(t.accent).bold()
        } else {
            Style::new()
        };
        frame.render_widget(Paragraph::new(label).style(style), label_area);
        let pos = draw_input(frame, t, field, input, "unlimited");
        if dialog.focus == i {
            cursor = pos;
        }
    }
    if let Some(e) = &dialog.error {
        frame.render_widget(
            Paragraph::new(e.clone())
                .style(Style::new().fg(t.error))
                .wrap(Wrap { trim: true }),
            err,
        );
    }
    if let Some(pos) = cursor {
        frame.set_cursor_position(pos);
    }
}

fn draw_confirm_stop(frame: &mut Frame, app: &App) {
    let t = &app.theme;
    let full = frame.area();
    let area = centered(full, full.width.saturating_sub(4).clamp(1, 64), 7);
    popup(frame, t, area);
    let block = Block::bordered()
        .border_style(Style::new().fg(t.warning))
        .title(Span::styled(
            " Stop the daemon? ",
            Style::new().fg(t.warning).bold(),
        ));
    let (headline, detail) = if app.is_busy() {
        (
            "Downloads are in progress.",
            "Stopping pauses them and ends post-download actions.",
        )
    } else {
        (
            "This closes the window and stops the daemon.",
            "Nothing will download or seed until torrentui runs again.",
        )
    };
    let lines = vec![
        Line::from(headline),
        Line::styled(detail, t.muted()),
        Line::from(""),
        Line::from(vec![
            key(t, "y"),
            Span::raw(" stop the daemon and quit    "),
            key(t, "n"),
            Span::raw(" / Esc  cancel"),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
        area,
    );
}

/// "[running N action(s)…]" while post-download commands are in flight.
fn actions_badge<'a>(t: &Theme, app: &App) -> Option<Span<'a>> {
    (app.actions_running > 0).then(|| {
        Span::styled(
            format!("[running {} action(s)…]", app.actions_running),
            Style::new().fg(t.warning),
        )
    })
}

fn push_badge(spans: &mut Vec<Span<'_>>, t: &Theme, app: &App) {
    if let Some(badge) = actions_badge(t, app) {
        spans.push(Span::raw("  "));
        spans.push(badge);
    }
}

fn filter_label(f: &Filter) -> String {
    match f {
        Filter::All => "all torrents".into(),
        Filter::Untagged => "untagged".into(),
        Filter::Tag(tag) => format!("#{tag}"),
    }
}

fn draw_tags(frame: &mut Frame, app: &App) {
    let t = &app.theme;
    let Some(editor) = &app.tag_editor else {
        return;
    };
    let full = frame.area();
    let width = full.width.saturating_sub(4).clamp(1, 70);
    let area = centered(full, width, 9);
    popup(frame, t, area);
    let block = Block::bordered()
        .border_style(Style::new().fg(t.accent))
        .title(title(
            t,
            format!(
                " Tags — {} ",
                fit(&editor.name, (width as usize).saturating_sub(12))
            ),
        ))
        .title_bottom(Line::styled(" Enter save  Esc cancel ", t.muted()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [hint, field, _, in_use, err] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new("Separate with spaces or commas; empty removes all").style(t.muted()),
        hint,
    );
    if let Some(pos) = draw_input(frame, t, field, &editor.input, "e.g. movies 4k sci-fi") {
        frame.set_cursor_position(pos);
    }
    if !editor.in_use.is_empty() {
        let list: Vec<String> = editor
            .in_use
            .iter()
            .map(|(tag, n)| format!("{tag} ({n})"))
            .collect();
        frame.render_widget(
            Paragraph::new(format!("In use: {}", list.join(", ")))
                .style(t.muted())
                .wrap(Wrap { trim: true }),
            in_use,
        );
    }
    if let Some(e) = &editor.error {
        frame.render_widget(
            Paragraph::new(fit(e, err.width as usize)).style(Style::new().fg(t.error)),
            err,
        );
    }
}

fn draw_filter(frame: &mut Frame, app: &App) {
    let t = &app.theme;
    let Some(picker) = &app.filter_picker else {
        return;
    };
    let full = frame.area();
    let height = (picker.entries.len() as u16 + 2).clamp(3, full.height.saturating_sub(2).max(3));
    let area = centered(full, full.width.saturating_sub(4).clamp(1, 44), height);
    popup(frame, t, area);
    let block = Block::bordered()
        .border_style(Style::new().fg(t.accent))
        .title(title(t, " Filter by tag "))
        .title_bottom(Line::styled(
            " ↑↓ choose  Enter apply  Esc cancel ",
            t.muted(),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let name_w = picker
        .entries
        .iter()
        .map(|(f, _)| filter_label(f).chars().count())
        .max()
        .unwrap_or(0);
    let items: Vec<ListItem> = picker
        .entries
        .iter()
        .map(|(f, n)| {
            ListItem::new(Line::from(vec![
                Span::raw(format!("{:<name_w$}  ", filter_label(f))),
                Span::styled(n.to_string(), t.muted()),
            ]))
        })
        .collect();
    let list = List::new(items)
        .highlight_style(t.selection())
        .highlight_symbol("▶ ");
    let mut state = ListState::default().with_selected(Some(picker.cursor));
    frame.render_stateful_widget(list, inner, &mut state);
}

fn draw_help(frame: &mut Frame, t: &Theme) {
    let rows = [
        ("a", "Add a magnet link, .torrent URL or file"),
        ("p / Space", "Pause or resume selected torrent"),
        ("f / Enter", "Choose which files to download"),
        ("F", "Show / hide the per-file progress panel"),
        ("PgUp / PgDn", "Scroll the file panel"),
        ("d / Del", "Remove torrent, keep files (asks first)"),
        ("D", "Remove torrent and DELETE its data (asks first)"),
        ("x", "Run on_complete actions on a finished torrent"),
        ("t", "Edit tags of the selected torrent"),
        ("/", "Filter the list by tag (Esc shows all)"),
        ("T", "Pick a colour theme (saved)"),
        ("l", "Set download / upload speed limits (saved)"),
        ("↑ ↓ / j k", "Move selection"),
        ("g / G", "Jump to first / last"),
        ("?", "Toggle this help"),
        ("q / Ctrl-C", "Quit; the daemon keeps downloading"),
        ("Q", "Quit and stop the daemon"),
    ];
    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, d)| {
            Line::from(vec![
                Span::styled(format!("{k:<12}"), Style::new().fg(t.accent).bold()),
                Span::raw(*d),
            ])
        })
        .collect();
    let full = frame.area();
    let area = centered(
        full,
        full.width.saturating_sub(4).clamp(1, 62),
        rows.len() as u16 + 2,
    );
    popup(frame, t, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            block(t)
                .border_style(Style::new().fg(t.accent))
                .title(title(t, " Help — any key to close ")),
        ),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn render(app: &App, w: u16, h: u16) -> String {
        render_buf(app, w, h).0
    }

    fn render_buf(app: &App, w: u16, h: u16) -> (String, ratatui::buffer::Buffer) {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buf = term.backend().buffer().clone();
        let text = (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        (text, buf)
    }

    fn sample() -> TorrentView {
        TorrentView {
            id: 1,
            name: "ubuntu.iso".into(),
            info_hash: "abc".into(),
            output_folder: "/tmp".into(),
            status: Status::Downloading,
            progress_bytes: 50,
            total_bytes: 100,
            uploaded_bytes: 10,
            down_bps: 2048,
            up_bps: 0,
            peers_live: 3,
            peers_connecting: 1,
            peers_queued: 0,
            peers_seen: 9,
            eta: Some("1m".into()),
            error: None,
        }
    }

    #[test]
    fn renders_empty_state() {
        let app = App::new("/dl".into());
        let out = render(&app, 100, 30);
        assert!(out.contains("No torrents yet"));
        assert!(out.contains("torrentui"));
    }

    #[test]
    fn renders_torrent_row() {
        let mut app = App::new("/dl".into());
        app.update_torrents(vec![sample()]);
        let out = render(&app, 120, 30);
        assert!(out.contains("ubuntu.iso"));
        assert!(out.contains("50.0%"));
        assert!(out.contains("Downloading"));
    }

    #[test]
    fn themes_never_set_a_background() {
        let mut app = App::new("/dl".into());
        app.set_themes(crate::theme::builtin_themes(), "tokyo-night");
        app.update_torrents(vec![sample()]);
        for mode in [Mode::Normal, Mode::Help, Mode::Add] {
            app.mode = mode;
            let (_, buf) = render_buf(&app, 100, 30);
            for y in 0..30 {
                for x in 0..100 {
                    // The selected row legitimately uses the selection colour.
                    let cell_bg = buf[(x, y)].bg;
                    assert!(
                        cell_bg == Color::Reset || cell_bg == app.theme.selection_bg,
                        "cell ({x},{y}) in {:?} has bg {cell_bg:?}",
                        app.mode
                    );
                }
            }
        }
    }

    #[test]
    fn default_theme_leaves_terminal_background() {
        let mut app = App::new("/dl".into());
        app.update_torrents(vec![sample()]);
        let (_, buf) = render_buf(&app, 100, 30);
        assert_eq!(buf[(0, 0)].bg, Color::Reset);
    }

    #[test]
    fn status_colours_come_from_theme() {
        let mut app = App::new("/dl".into());
        app.set_themes(crate::theme::builtin_themes(), "dracula");
        app.update_torrents(vec![sample()]);
        let (text, buf) = render_buf(&app, 120, 30);
        let y = text
            .lines()
            .position(|l| l.contains("Downloading"))
            .unwrap() as u16;
        // Locate the status cell by matching consecutive symbols on that row.
        let word: Vec<String> = "Downloading".chars().map(String::from).collect();
        let x = (0..120 - word.len() as u16)
            .find(|x| {
                word.iter()
                    .enumerate()
                    .all(|(i, c)| buf[(x + i as u16, y)].symbol() == c)
            })
            .unwrap();
        assert_eq!(buf[(x, y)].fg, app.theme.success);
    }

    #[test]
    fn header_shows_configured_limit() {
        let mut app = App::new("/dl".into());
        app.limits = ("2MiB/s".parse().unwrap(), Rate::default());
        let out = render(&app, 120, 30);
        assert!(out.contains("(max 2MiB/s)"), "{out}");
    }

    #[test]
    fn fit_truncates_on_char_boundaries() {
        assert_eq!(fit("abc", 5), "abc");
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(fit("héllo wörld", 6), "héllo…");
        assert_eq!(fit("abc", 0), "");
    }

    #[test]
    fn renders_file_picker() {
        use crate::{
            app::{FilePicker, PickerTarget},
            engine::{FileEntry, FileList},
        };
        let mut app = App::new("/dl".into());
        let files = FileList {
            entries: vec![
                FileEntry {
                    index: 0,
                    path: "movie.mkv".into(),
                    len: 2048,
                    done: 0,
                },
                FileEntry {
                    index: 1,
                    path: "readme.txt".into(),
                    len: 10,
                    done: 0,
                },
            ],
            padding: vec![],
        };
        app.queue_picker(FilePicker::new(
            PickerTarget::Existing { id: 1 },
            "pack".into(),
            files,
            |i| i == 0,
        ));
        let out = render(&app, 100, 30);
        assert!(out.contains("Select files"));
        assert!(out.contains("[x] movie.mkv"));
        assert!(out.contains("[ ] readme.txt"));
        assert!(out.contains("1 of 2 files selected"));
        for (w, h) in [(0, 0), (1, 1), (10, 3), (20, 8), (40, 12)] {
            render(&app, w, h);
        }
    }

    #[test]
    fn renders_theme_picker_and_limits_dialog() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let key = |c| KeyEvent {
            code: c,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        let mut app = App::new("/dl".into());
        app.handle_key(key(KeyCode::Char('T')));
        let out = render(&app, 100, 30);
        assert!(out.contains("Theme"));
        assert!(out.contains("tokyo-night"));
        assert!(out.contains("catppuccin-frappe"));
        app.handle_key(key(KeyCode::Esc));

        app.handle_key(key(KeyCode::Char('l')));
        let out = render(&app, 100, 30);
        assert!(out.contains("Speed limits"));
        assert!(out.contains("unlimited"));
    }

    #[test]
    fn tiny_terminal_does_not_panic() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let key = |c| KeyEvent {
            code: c,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        let mut app = App::new("/dl".into());
        app.update_torrents(vec![sample()]);
        let mut modes = vec![
            Mode::Normal,
            Mode::Add,
            Mode::Help,
            Mode::ConfirmRemove {
                id: 1,
                name: "x".into(),
                folder: "/dl".into(),
                delete_files: false,
            },
            Mode::ConfirmRemove {
                id: 1,
                name: "x".into(),
                folder: "/dl".into(),
                delete_files: true,
            },
        ];
        for m in modes.drain(..) {
            app.mode = m;
            for (w, h) in [(0, 0), (1, 1), (10, 3), (20, 8), (40, 12)] {
                render(&app, w, h);
            }
        }
        app.mode = Mode::Normal;
        for c in ['t', 'l'] {
            app.handle_key(key(KeyCode::Char(c)));
            for (w, h) in [(0, 0), (1, 1), (10, 3), (20, 8), (40, 12)] {
                render(&app, w, h);
            }
            app.handle_key(key(KeyCode::Esc));
        }
    }

    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn tagged(app: &mut App) {
        let mut a = sample();
        a.id = 1;
        a.name = "alpha.iso".into();
        a.info_hash = "h1".into();
        let mut b = sample();
        b.id = 2;
        b.name = "bravo.iso".into();
        b.info_hash = "h2".into();
        app.update_torrents(vec![a, b]);
        let mut tags = crate::tags::Tags::new();
        tags.insert("h1".into(), vec!["iso".into(), "linux".into()]);
        app.set_tags(tags);
    }

    #[test]
    fn details_show_tags_or_a_hint() {
        let mut app = App::new("/dl".into());
        tagged(&mut app);
        let out = render(&app, 100, 30);
        assert!(out.contains("#linux"), "{out}");
        assert!(out.contains("#iso"));
        app.handle_key(key(KeyCode::Char('j')));
        assert!(render(&app, 100, 30).contains("none (T to add)"));
    }

    #[test]
    fn tags_column_only_on_wide_terminals_with_tags() {
        let mut app = App::new("/dl".into());
        app.update_torrents(vec![sample()]);
        let header = |out: &str| {
            out.lines()
                .find(|l| l.contains("Progress"))
                .unwrap()
                .to_string()
        };
        assert!(
            !header(&render(&app, 130, 30)).contains("Tags"),
            "no tags, no column"
        );
        tagged(&mut app);
        assert!(header(&render(&app, 130, 30)).contains("Tags"));
        assert!(!header(&render(&app, 100, 30)).contains("Tags"));
        let row = |out: &str| {
            out.lines()
                .find(|l| l.contains("alpha.iso"))
                .unwrap()
                .to_string()
        };
        assert!(row(&render(&app, 130, 30)).contains("iso, linux"));
        assert!(!row(&render(&app, 100, 30)).contains("iso, linux"));
    }

    #[test]
    fn filtered_list_title_and_rows() {
        let mut app = App::new("/dl".into());
        tagged(&mut app);
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Char('j')));
        let popup = render(&app, 100, 30);
        assert!(
            popup.contains("Filter by tag") && popup.contains("#linux"),
            "{popup}"
        );
        app.handle_key(key(KeyCode::Enter));
        let out = render(&app, 100, 30);
        assert!(out.contains("#iso (1 of 2)"), "{out}");
        assert!(out.contains("alpha.iso") && !out.contains("bravo.iso"));
        assert!(out.contains("Esc show all"));
        // The header still counts every torrent.
        assert!(out.contains("2 torrents"));
    }

    #[test]
    fn tag_editor_popup_shows_input_suggestions_and_errors() {
        let mut app = App::new("/dl".into());
        tagged(&mut app);
        app.handle_key(key(KeyCode::Char('t')));
        let out = render(&app, 100, 30);
        assert!(out.contains("Tags — alpha.iso"), "{out}");
        assert!(out.contains("iso linux"));
        assert!(out.contains("In use: iso (1), linux (1)"));
        app.handle_key(key(KeyCode::Char('!')));
        app.handle_key(key(KeyCode::Enter));
        assert!(render(&app, 100, 30).contains("contains '!'"));
    }

    #[test]
    fn tag_popups_keep_terminal_background() {
        let mut app = App::new("/dl".into());
        app.set_themes(crate::theme::builtin_themes(), "dracula");
        tagged(&mut app);
        for open in ['t', '/'] {
            app.handle_key(key(KeyCode::Char(open)));
            let (_, buf) = render_buf(&app, 100, 30);
            for y in 0..30 {
                for x in 0..100 {
                    let c = buf[(x, y)].bg;
                    assert!(
                        c == Color::Reset || c == app.theme.selection_bg,
                        "({x},{y}) {c:?} after {open}"
                    );
                }
            }
            app.handle_key(key(KeyCode::Esc));
        }
    }

    #[test]
    fn footer_drops_low_priority_hints_when_narrow() {
        let mut app = App::new("/dl".into());
        app.update_torrents(vec![sample()]);
        let last = |w: u16, h: u16| {
            render(&app, w, h)
                .lines()
                .last()
                .unwrap()
                .trim_end()
                .to_string()
        };
        let wide = last(120, 30);
        for k in ["add", "tags", "filter", "limits", "help", "quit"] {
            assert!(wide.contains(k), "{wide}");
        }
        let narrow = last(60, 30);
        assert!(
            narrow.contains("add") && narrow.contains("help") && narrow.contains("quit"),
            "{narrow}"
        );
        assert!(!narrow.contains("limits"), "{narrow}");
        assert!(narrow.chars().count() <= 60);
    }

    #[test]
    fn tag_dialogs_survive_tiny_terminals() {
        let mut app = App::new("/dl".into());
        tagged(&mut app);
        for open in ['t', '/'] {
            app.handle_key(key(KeyCode::Char(open)));
            for (w, h) in [(0, 0), (1, 1), (10, 3), (20, 8), (40, 12)] {
                render(&app, w, h);
            }
            app.handle_key(key(KeyCode::Esc));
        }
    }

    #[test]
    fn footer_shows_actions_hint_and_running_badge() {
        let mut app = App::new("/dl".into());
        app.update_torrents(vec![sample()]);
        let footer = |app: &App, w: u16| {
            render(app, w, 30)
                .lines()
                .last()
                .unwrap()
                .trim_end()
                .to_string()
        };
        assert!(!footer(&app, 120).contains("actions"));
        app.has_actions = true;
        assert!(footer(&app, 120).contains("x actions"));
        app.actions_running = 2;
        let busy = footer(&app, 120);
        assert!(
            busy.contains("[running 2 action(s)…]") && busy.contains("quit"),
            "{busy}"
        );
        // The badge wins over low-priority hints on a narrow terminal.
        let narrow = footer(&app, 70);
        assert!(
            narrow.contains("[running 2 action(s)…]") && narrow.contains("quit"),
            "{narrow}"
        );
        // And it shows next to a status message.
        app.set_status("Added: x", false);
        assert!(footer(&app, 120).contains("Added: x") && footer(&app, 120).contains("[running 2"));
    }

    #[test]
    fn remove_dialogs_say_what_will_happen() {
        let mut app = App::new("/dl".into());
        app.update_torrents(vec![sample()]);
        app.handle_key(key(KeyCode::Char('d')));
        let keep = render(&app, 100, 30);
        assert!(
            keep.contains("Remove torrent?") && keep.contains("files are kept"),
            "{keep}"
        );
        assert!(!keep.contains("permanently"));
        app.handle_key(key(KeyCode::Char('D')));
        let del = render(&app, 100, 30);
        assert!(
            del.contains("AND DELETE its data?") && del.contains("permanently deletes"),
            "{del}"
        );
        assert!(
            del.contains("Saved in /tmp") && del.contains("yes, delete them"),
            "{del}"
        );
    }

    #[test]
    fn footer_offers_unpause_for_a_paused_torrent() {
        let mut app = App::new("/dl".into());
        let footer = |app: &App| render(app, 120, 30).lines().last().unwrap().to_string();
        let empty = footer(&app);
        assert!(empty.contains("a add") && empty.contains("l limits") && empty.contains("? help"));
        for gone in ["pause", "files", "tags", "remove"] {
            assert!(
                !empty.contains(gone),
                "{gone} shown with no torrent: {empty}"
            );
        }
        app.update_torrents(vec![sample()]);
        let full = footer(&app);
        for shown in ["p pause", "f files", "t tags", "d remove"] {
            assert!(full.contains(shown), "{shown} missing: {full}");
        }
        assert!(footer(&app).contains("p pause") && !footer(&app).contains("unpause"));
        let mut paused = sample();
        paused.status = Status::Paused;
        app.update_torrents(vec![paused]);
        assert!(footer(&app).contains("p unpause"), "{}", footer(&app));
        // Follows the selection, not the list.
        let mut other = sample();
        other.id = 2;
        app.update_torrents(vec![sample(), other]);
        app.handle_key(key(KeyCode::Char('k')));
        assert!(footer(&app).contains("p pause") && !footer(&app).contains("unpause"));
    }

    fn panel_files(done: [u64; 3]) -> crate::engine::FileList {
        use crate::engine::{FileEntry, FileList};
        FileList {
            entries: ["a/movie.mkv", "a/subs.srt", "a/extra.nfo"]
                .iter()
                .zip(done)
                .enumerate()
                .map(|(index, (path, done))| FileEntry {
                    index,
                    path: path.to_string(),
                    len: 200,
                    done,
                })
                .collect(),
            padding: vec![],
        }
    }

    #[test]
    fn file_panel_shows_per_file_progress() {
        let mut app = App::new("/dl".into());
        app.update_torrents(vec![sample()]);
        assert!(
            !render(&app, 120, 30).contains("Files"),
            "closed by default"
        );

        app.handle_key(key(KeyCode::Char('F')));
        assert!(render(&app, 120, 30).contains("Loading"));

        // File 2 is not selected, file 1 is complete, file 0 half done.
        app.set_panel_files(1, panel_files([100, 200, 0]), vec![0, 1]);
        let out = render(&app, 120, 30);
        assert!(out.contains("a/movie.mkv") && out.contains("50%"), "{out}");
        assert!(out.contains("a/subs.srt") && out.contains("100%"), "{out}");
        assert!(out.contains("a/extra.nfo") && out.contains("skip"), "{out}");

        // Too narrow for a panel: the list keeps the room.
        assert!(!render(&app, 80, 30).contains("a/movie.mkv"));

        app.handle_key(key(KeyCode::Char('F')));
        assert!(!render(&app, 120, 30).contains("a/movie.mkv"));
    }

    #[test]
    fn file_panel_rounds_down_and_scrolls() {
        use crate::engine::{FileEntry, FileList};
        let mut app = App::new("/dl".into());
        app.update_torrents(vec![sample()]);
        app.handle_key(key(KeyCode::Char('F')));
        let list = FileList {
            entries: (0..60)
                .map(|index| FileEntry {
                    index,
                    path: format!("file-{index:02}"),
                    len: 1000,
                    done: if index == 0 { 999 } else { 0 },
                })
                .collect(),
            padding: vec![],
        };
        app.set_panel_files(1, list, (0..60).collect());
        let top = render(&app, 120, 30);
        assert!(top.contains("file-00") && top.contains(" 99%"), "{top}");
        assert!(!top.contains("file-59"));
        app.handle_key(key(KeyCode::PageDown));
        app.handle_key(key(KeyCode::PageDown));
        app.handle_key(key(KeyCode::PageDown));
        app.handle_key(key(KeyCode::PageDown));
        app.handle_key(key(KeyCode::PageDown));
        app.handle_key(key(KeyCode::PageDown));
        let bottom = render(&app, 120, 30);
        assert!(
            bottom.contains("file-59") && !bottom.contains("file-00"),
            "{bottom}"
        );
    }

    #[test]
    fn file_panel_ignores_answers_for_other_torrents() {
        let mut app = App::new("/dl".into());
        app.update_torrents(vec![sample()]);
        app.set_panel_files(1, panel_files([0, 0, 0]), vec![0]);
        assert!(app.panel_files.is_none(), "closed panel stores nothing");
        app.handle_key(key(KeyCode::Char('F')));
        app.set_panel_files(7, panel_files([0, 0, 0]), vec![0]);
        assert!(app.panel_files.is_none(), "answer for another torrent");
        app.handle_key(key(KeyCode::Char('F')));
        assert_eq!(app.panel_wanted(), None);
    }
}
