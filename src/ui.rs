//! Pure rendering: `draw(frame, &app)` has no side effects on app state.

use ratatui::{
    Frame,
    layout::{Constraint, Flex, Layout, Position, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Cell, Clear, Paragraph, Row, Sparkline, Table, TableState, Wrap},
};

use crate::{
    app::{App, Mode},
    engine::{Status, TorrentView},
    fmt,
};

const ACCENT: Color = Color::Cyan;

pub fn draw(frame: &mut Frame, app: &App) {
    let [header, list, details, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(5),
        Constraint::Length(9),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, app, header);
    draw_list(frame, app, list);
    draw_details(frame, app, details);
    draw_footer(frame, app, footer);

    match &app.mode {
        Mode::Normal => {}
        Mode::Add => draw_add(frame, app),
        Mode::ConfirmRemove { name, .. } => draw_confirm(frame, name),
        Mode::Help => draw_help(frame),
    }
}

fn status_color(s: Status) -> Color {
    match s {
        Status::Downloading => Color::Green,
        Status::Seeding => Color::Blue,
        Status::Paused => Color::Yellow,
        Status::Initializing => Color::Magenta,
        Status::Error => Color::Red,
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::bordered()
        .title(" torrent-tui ".bold().fg(ACCENT))
        .title_bottom(
            Line::from(format!(" {} ", app.download_dir))
                .dim()
                .right_aligned(),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [text, spark] =
        Layout::horizontal([Constraint::Length(44), Constraint::Min(0)]).areas(inner);
    let line = Line::from(vec![
        Span::styled("↓ ", Style::new().fg(Color::Green)),
        Span::raw(fmt::rate_or_zero(app.total_down())),
        Span::raw("   "),
        Span::styled("↑ ", Style::new().fg(Color::Blue)),
        Span::raw(fmt::rate_or_zero(app.total_up())),
        Span::raw(format!("   {} torrents", app.torrents.len())),
    ]);
    frame.render_widget(Paragraph::new(line), text);

    // Only show the most recent samples that fit the width.
    let width = spark.width as usize;
    let skip = app.down_history.len().saturating_sub(width);
    let data: Vec<u64> = app.down_history.iter().skip(skip).copied().collect();
    frame.render_widget(
        Sparkline::default()
            .data(data)
            .style(Style::new().fg(Color::Green)),
        spark,
    );
}

fn draw_list(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::bordered().title(" Torrents ");

    if app.torrents.is_empty() {
        let msg = Paragraph::new(vec![
            Line::from(""),
            Line::from("No torrents yet.").centered(),
            Line::from(vec![
                Span::raw("Press "),
                Span::styled("a", Style::new().fg(ACCENT).bold()),
                Span::raw(" to add a magnet link or .torrent file."),
            ])
            .centered(),
        ])
        .block(block);
        frame.render_widget(msg, area);
        return;
    }

    let header = Row::new([
        "Name", "Size", "Progress", "Down", "Up", "Peers", "ETA", "Status",
    ])
    .style(Style::new().bold().fg(ACCENT));

    let rows = app.torrents.iter().map(|t| {
        let pct = fmt::percent(t.progress_bytes, t.total_bytes);
        Row::new([
            Cell::from(t.name.clone()),
            Cell::from(fmt::bytes(t.total_bytes)),
            Cell::from(format!("{} {:>5.1}%", fmt::bar(pct, 10), pct)),
            Cell::from(fmt::rate(t.down_bps)),
            Cell::from(fmt::rate(t.up_bps)),
            Cell::from(t.peers_live.to_string()),
            Cell::from(t.eta.clone().unwrap_or_else(|| "-".into())),
            Cell::from(t.status.label()).style(Style::new().fg(status_color(t.status))),
        ])
    });

    let table = Table::new(
        rows,
        [
            Constraint::Min(16),
            Constraint::Length(10),
            Constraint::Length(18),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(5),
            Constraint::Length(9),
            Constraint::Length(11),
        ],
    )
    .header(header)
    .block(block)
    .column_spacing(1)
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
    .highlight_symbol("▶ ");

    let mut state = TableState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_details(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::bordered().title(" Details ");
    let Some(t) = app.current() else {
        frame.render_widget(block, area);
        return;
    };
    frame.render_widget(
        Paragraph::new(detail_lines(t))
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn detail_lines(t: &TorrentView) -> Vec<Line<'static>> {
    let label = |s: &'static str| Span::styled(format!("{s:<10}"), Style::new().dim());
    let ratio = if t.total_bytes > 0 {
        t.uploaded_bytes as f64 / t.total_bytes as f64
    } else {
        0.0
    };
    let mut lines = vec![
        Line::from(vec![label("Name"), Span::raw(t.name.clone()).bold()]),
        Line::from(vec![label("Hash"), Span::raw(t.info_hash.clone())]),
        Line::from(vec![
            label("Saved to"),
            Span::raw(t.output_folder.display().to_string()),
        ]),
        Line::from(vec![
            label("Progress"),
            Span::raw(format!(
                "{} / {} ({:.1}%)",
                fmt::bytes(t.progress_bytes),
                fmt::bytes(t.total_bytes),
                fmt::percent(t.progress_bytes, t.total_bytes)
            )),
        ]),
        Line::from(vec![
            label("Uploaded"),
            Span::raw(format!(
                "{} (ratio {:.2})",
                fmt::bytes(t.uploaded_bytes),
                ratio
            )),
        ]),
        Line::from(vec![
            label("Peers"),
            Span::raw(format!(
                "{} live, {} connecting, {} queued, {} seen",
                t.peers_live, t.peers_connecting, t.peers_queued, t.peers_seen
            )),
        ]),
    ];
    if let Some(err) = &t.error {
        lines.push(Line::from(vec![
            label("Error"),
            Span::styled(err.clone(), Style::new().fg(Color::Red)),
        ]));
    }
    lines
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let line = if let Some((msg, is_error)) = app.status() {
        let style = if is_error {
            Style::new().fg(Color::Red)
        } else {
            Style::new().fg(Color::Green)
        };
        let mut spans = vec![Span::styled(msg.to_string(), style)];
        if app.pending_adds > 0 {
            spans.push(Span::styled(
                format!("  [adding {}…]", app.pending_adds),
                Style::new().fg(Color::Yellow),
            ));
        }
        Line::from(spans)
    } else if app.pending_adds > 0 {
        Line::styled(
            format!(
                "Adding {} torrent(s) — fetching metadata…",
                app.pending_adds
            ),
            Style::new().fg(Color::Yellow),
        )
    } else {
        let key = |k: &'static str| Span::styled(k, Style::new().fg(ACCENT).bold());
        Line::from(vec![
            key("a"),
            Span::raw(" add  "),
            key("p"),
            Span::raw(" pause/resume  "),
            key("d"),
            Span::raw(" remove  "),
            key("↑↓/jk"),
            Span::raw(" move  "),
            key("?"),
            Span::raw(" help  "),
            key("q"),
            Span::raw(" quit"),
        ])
    };
    frame.render_widget(Paragraph::new(line), area);
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

fn draw_add(frame: &mut Frame, app: &App) {
    let full = frame.area();
    let area = centered(full, full.width.saturating_sub(4).clamp(1, 90), 5);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .border_style(Style::new().fg(ACCENT))
        .title(" Add torrent ")
        .title_bottom(Line::from(" Enter: add  Esc: cancel ").dim());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [hint, field] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)])
        .margin(0)
        .areas(inner);
    frame.render_widget(
        Paragraph::new("Magnet link, .torrent URL, or path to a .torrent file (paste works)").dim(),
        hint,
    );

    if field.width == 0 || field.height == 0 {
        return;
    }
    // Scroll horizontally so the cursor is always visible.
    let width = field.width as usize;
    let text: Vec<char> = app.input.text().chars().collect();
    let cursor = app.input.cursor();
    let start = (cursor + 1).saturating_sub(width);
    let visible: String = text.iter().skip(start).take(width).collect();
    frame.render_widget(Paragraph::new(visible), field);
    let col = (cursor - start).min(width - 1) as u16;
    frame.set_cursor_position(Position::new(field.x + col, field.y));
}

fn draw_confirm(frame: &mut Frame, name: &str) {
    let full = frame.area();
    let area = centered(full, full.width.saturating_sub(4).clamp(1, 70), 7);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .border_style(Style::new().fg(Color::Red))
        .title(" Remove torrent? ");
    let text = vec![
        Line::from(name.to_string()).bold(),
        Line::from(""),
        Line::from(vec![
            Span::styled("y", Style::new().fg(ACCENT).bold()),
            Span::raw(" remove, keep files"),
        ]),
        Line::from(vec![
            Span::styled("D", Style::new().fg(Color::Red).bold()),
            Span::raw(" remove and DELETE downloaded files"),
        ]),
        Line::from(vec![
            Span::styled("n", Style::new().fg(ACCENT).bold()),
            Span::raw(" / Esc  cancel"),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(text).block(block).wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_help(frame: &mut Frame) {
    let rows = [
        ("a", "Add a magnet link, .torrent URL or file"),
        ("p / Space", "Pause or resume selected torrent"),
        ("d / Del", "Remove selected torrent (asks first)"),
        ("↑ ↓ / j k", "Move selection"),
        ("g / G", "Jump to first / last"),
        ("?", "Toggle this help"),
        ("q / Ctrl-C", "Quit"),
    ];
    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, d)| {
            Line::from(vec![
                Span::styled(format!("{k:<12}"), Style::new().fg(ACCENT).bold()),
                Span::raw(*d),
            ])
        })
        .collect();
    let full = frame.area();
    let area = centered(
        full,
        full.width.saturating_sub(4).clamp(1, 60),
        rows.len() as u16 + 2,
    );
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Help — any key to close ")),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn render(app: &App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
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
    fn tiny_terminal_does_not_panic() {
        let mut app = App::new("/dl".into());
        app.update_torrents(vec![sample()]);
        for mode in [
            Mode::Normal,
            Mode::Add,
            Mode::Help,
            Mode::ConfirmRemove {
                id: 1,
                name: "x".into(),
            },
        ] {
            app.mode = mode;
            for (w, h) in [(1, 1), (10, 3), (20, 8), (40, 12)] {
                render(&app, w, h);
            }
        }
    }
}
