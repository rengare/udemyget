// src/ui.rs – Ratatui TUI rendering.

use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, BorderType, Borders, Gauge, List, ListItem, Padding, Paragraph, Wrap,
    },
    Frame,
};

use crate::app::{App, CurriculumRow};
use crate::types::{ActiveTab, AppScreen, DownloadStatus};

// ── Palette ───────────────────────────────────────────────────────────────

const CLR_FG:     Color = Color::White;
const CLR_DIM:    Color = Color::Gray;
const CLR_ACCENT: Color = Color::Cyan;
const CLR_OK:     Color = Color::Green;
const CLR_ERR:    Color = Color::Red;
const CLR_WARN:   Color = Color::Yellow;
const CLR_SEL_BG: Color = Color::DarkGray;

fn style_normal()   -> Style { Style::default().fg(CLR_FG) }
fn style_dim()      -> Style { Style::default().fg(CLR_DIM) }
fn style_accent()   -> Style { Style::default().fg(CLR_ACCENT) }
fn style_ok()       -> Style { Style::default().fg(CLR_OK) }
fn style_err()      -> Style { Style::default().fg(CLR_ERR) }
fn style_warn()     -> Style { Style::default().fg(CLR_WARN) }
fn style_selected() -> Style {
    Style::default()
        .fg(CLR_FG)
        .bg(CLR_SEL_BG)
        .add_modifier(Modifier::BOLD)
}
fn style_header()   -> Style {
    Style::default()
        .fg(CLR_ACCENT)
        .add_modifier(Modifier::BOLD)
}

// ── Root render ───────────────────────────────────────────────────────────

pub fn render(f: &mut Frame, app: &App) {
    let area = f.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title bar
            Constraint::Length(1), // tab bar
            Constraint::Min(0),    // main body
            Constraint::Length(1), // status bar
        ])
        .split(area);

    render_titlebar(f, chunks[0]);
    render_tabbar(f, chunks[1], app);
    render_body(f, chunks[2], app);
    render_statusbar(f, chunks[3], app);
}

// ── Title bar ─────────────────────────────────────────────────────────────

fn render_titlebar(f: &mut Frame, area: Rect) {
    let title = Span::styled(
        " udemyget — Udemy course downloader ",
        style_header(),
    );
    let p = Paragraph::new(Line::from(title)).alignment(Alignment::Center);
    f.render_widget(p, area);
}

// ── Tab bar ───────────────────────────────────────────────────────────────

fn render_tabbar(f: &mut Frame, area: Rect, app: &App) {
    if app.screen == AppScreen::Login {
        return;
    }

    let courses_style = if app.active_tab == ActiveTab::Courses {
        style_selected()
    } else {
        style_dim()
    };
    let downloads_style = if app.active_tab == ActiveTab::Downloads {
        style_selected()
    } else {
        style_dim()
    };

    let active_count = app.downloads.iter().filter(|d| d.is_active()).count();
    let dl_label = if active_count > 0 {
        format!(" [2] Downloads ({active_count}) ")
    } else {
        " [2] Downloads ".to_string()
    };

    let line = Line::from(vec![
        Span::styled(" [1] Courses ", courses_style),
        Span::styled("│", style_dim()),
        Span::styled(dl_label, downloads_style),
        Span::styled("  Tab to switch", style_dim()),
    ]);

    f.render_widget(Paragraph::new(line), area);
}

// ── Body router ───────────────────────────────────────────────────────────

fn render_body(f: &mut Frame, area: Rect, app: &App) {
    match app.screen {
        AppScreen::Login => render_login(f, area, app),
        AppScreen::Courses | AppScreen::Curriculum => {
            if app.active_tab == ActiveTab::Downloads {
                render_downloads(f, area, app);
            } else {
                match app.screen {
                    AppScreen::Courses => render_courses(f, area, app),
                    AppScreen::Curriculum => render_curriculum(f, area, app),
                    _ => {}
                }
            }
        }
    }
}

// ── Setup screen ──────────────────────────────────────────────────────────

fn render_login(f: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(16),
            Constraint::Min(0),
        ])
        .split(area);

    let box_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(76),
            Constraint::Min(0),
        ])
        .split(chunks[1]);

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  Edit this file:  ", style_dim()),
            Span::styled(&app.cookie_file, style_accent().add_modifier(Modifier::BOLD)),
        ]),
        Line::from(""),
    ];

    if let Some(ref err) = app.login_error {
        lines.push(Line::from(Span::styled(format!("  x {err}"), style_err())));
    } else if app.loading {
        lines.push(Line::from(Span::styled("  Verifying...", style_warn())));
    } else {
        lines.push(Line::from(Span::styled(
            "  Press r after saving the file.",
            style_ok(),
        )));
    }

    lines.extend([
        Line::from(""),
        Line::from(Span::styled("  Steps:", style_dim())),
        Line::from(Span::styled("  1. Log in to udemy.com in Firefox / Chrome", style_dim())),
        Line::from(Span::styled("  2. DevTools (F12) -> Network tab -> reload the page", style_dim())),
        Line::from(Span::styled("  3. Click any request to udemy.com", style_dim())),
        Line::from(Span::styled("  4. Request Headers -> right-click Cookie -> Copy value", style_dim())),
        Line::from(Span::styled("  5. Replace ALL contents of the file above with the value and save.", style_dim())),
        Line::from(""),
        Line::from(Span::styled("  r reload  q quit", style_dim())),
    ]);

    let block = Block::default()
        .title(Span::styled(" udemyget - Cookie Setup ", style_header()))
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(style_accent());

    let p = Paragraph::new(Text::from(lines))
        .block(block)
        .wrap(Wrap { trim: false });

    f.render_widget(p, box_chunks[1]);
}


fn render_courses(f: &mut Frame, area: Rect, app: &App) {
    if app.loading {
        let p = Paragraph::new(Line::from(Span::styled(
            "  Loading courses…",
            style_warn(),
        )));
        f.render_widget(p, area);
        return;
    }

    if let Some(ref err) = app.error_msg {
        let p = Paragraph::new(Text::from(vec![
            Line::from(Span::styled(format!("  Error: {err}"), style_err())),
            Line::from(Span::styled("  Press r to retry", style_dim())),
        ]));
        f.render_widget(p, area);
        return;
    }

    if app.courses.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "  No courses found. Press r to refresh.",
            style_dim(),
        )));
        f.render_widget(p, area);
        return;
    }

    let selected = app.course_list_state.selected().unwrap_or(0);

    let items: Vec<ListItem> = app
        .courses
        .iter()
        .enumerate()
        .map(|(i, course)| {
            let is_sel = i == selected;
            let lectures = course
                .num_published_lectures
                .map(|n| format!("  {n} lectures"))
                .unwrap_or_default();

            let style = if is_sel { style_selected() } else { style_normal() };
            let prefix = if is_sel { "▶ " } else { "  " };

            ListItem::new(Line::from(vec![
                Span::styled(prefix, style_accent()),
                Span::styled(&course.title, style),
                Span::styled(lectures, style_dim()),
            ]))
        })
        .collect();

    let block = Block::default()
        .title(Span::styled(
            format!(" Courses ({}) ", app.courses.len()),
            style_header(),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(style_accent())
        .padding(Padding::horizontal(1));

    let list = List::new(items).block(block).highlight_style(style_selected());

    let mut state = app.course_list_state.clone();
    f.render_stateful_widget(list, area, &mut state);
}

// ── Curriculum view ───────────────────────────────────────────────────────

fn render_curriculum(f: &mut Frame, area: Rect, app: &App) {
    if app.loading {
        let p = Paragraph::new(Line::from(Span::styled(
            "  Loading curriculum…",
            style_warn(),
        )));
        f.render_widget(p, area);
        return;
    }

    let course_title = app
        .current_course
        .as_ref()
        .map(|c| c.title.as_str())
        .unwrap_or("Course");

    let selected = app.curriculum_list_state.selected().unwrap_or(0);

    let items: Vec<ListItem> = app
        .visible_rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let is_sel = i == selected;
            match row {
                CurriculumRow::Chapter(ci) => {
                    let ch = &app.curriculum[*ci];
                    let arrow = if ch.expanded { "▼" } else { "▶" };
                    let count = ch.lectures.len();
                    let style = if is_sel { style_selected() } else { style_accent() };
                    ListItem::new(Line::from(vec![
                        Span::styled(format!("{arrow} Section {}: ", ch.object_index), style),
                        Span::styled(&ch.title, style.add_modifier(Modifier::BOLD)),
                        Span::styled(
                            format!("  ({count} lectures)"),
                            if is_sel { style_selected() } else { style_dim() },
                        ),
                    ]))
                }
                CurriculumRow::Lecture(ci, li) => {
                    let lec = &app.curriculum[*ci].lectures[*li];
                    let dur = lec.duration_secs.map(|s| {
                        let m = s / 60;
                        let s = s % 60;
                        format!("[{m}:{s:02}]")
                    }).unwrap_or_default();
                    let icon = match lec.asset_type.as_str() {
                        "Video" => "○",
                        "Article" => "📄",
                        _ => "•",
                    };
                    let style = if is_sel { style_selected() } else { style_normal() };
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            format!("    {icon} {:3}. ", lec.object_index),
                            if is_sel { style_selected() } else { style_dim() },
                        ),
                        Span::styled(&lec.title, style),
                        Span::styled(format!("  {dur}"), style_dim()),
                    ]))
                }
            }
        })
        .collect();

    let total_lectures: usize = app.curriculum.iter().map(|ch| ch.lectures.len()).sum();

    let block = Block::default()
        .title(Span::styled(
            format!(" {course_title} — {total_lectures} lectures "),
            style_header(),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(style_accent())
        .padding(Padding::horizontal(1));

    let list = List::new(items).block(block).highlight_style(style_selected());

    let mut state = app.curriculum_list_state.clone();
    f.render_stateful_widget(list, area, &mut state);
}

// ── Downloads panel ───────────────────────────────────────────────────────

fn render_downloads(f: &mut Frame, area: Rect, app: &App) {
    if app.downloads.is_empty() {
        let p = Paragraph::new(Text::from(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  No downloads yet.",
                style_dim(),
            )),
            Line::from(Span::styled(
                "  Navigate to a course → press d (lecture) or a (all lectures).",
                style_dim(),
            )),
        ]));
        let block = Block::default()
            .title(Span::styled(" Downloads ", style_header()))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(style_accent());
        f.render_widget(p.block(block), area);
        return;
    }

    // Layout: each download gets 3 rows (title, gauge, spacer)
    let block = Block::default()
        .title(Span::styled(
            format!(" Downloads ({}) ", app.downloads.len()),
            style_header(),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(style_accent())
        .padding(Padding::horizontal(1));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let selected = app.downloads_list_state.selected().unwrap_or(0);

    // Build rows: each download → 3 lines (label + gauge + blank)
    let row_height: u16 = 3;
    let visible_count = (inner.height / row_height) as usize;

    // Simple scroll offset
    let offset = selected.saturating_sub(visible_count.saturating_sub(1));
    let visible_downloads = app.downloads.iter().enumerate().skip(offset);

    let mut y = inner.y;
    for (i, dl) in visible_downloads {
        if y + row_height > inner.y + inner.height {
            break;
        }

        let is_sel = i == selected;

        // ── Title line ──────────────────────────────────────────────────
        let (status_span, gauge_color) = match &dl.status {
            DownloadStatus::Queued      => (Span::styled("Queued",      style_dim()),  Color::Gray),
            DownloadStatus::Fetching    => (Span::styled("Fetching…",   style_warn()), Color::Yellow),
            DownloadStatus::Downloading => (Span::styled("Downloading", style_ok()),   Color::Green),
            DownloadStatus::Done        => (Span::styled("✓ Done",      style_ok()),   Color::Green),
            DownloadStatus::Failed(e)   => (Span::styled(format!("✗ {e}"), style_err()), Color::Red),
            DownloadStatus::Cancelled   => (Span::styled("Cancelled",   style_dim()),  Color::Gray),
        };

        let prefix = if is_sel { "▶ " } else { "  " };
        let label = format!(
            "{prefix}[{}] {} / {}",
            &dl.course_title,
            &dl.chapter_title,
            &dl.lecture_title,
        );
        let title_line = Line::from(vec![
            Span::styled(label, if is_sel { style_selected() } else { style_normal() }),
            Span::raw("  "),
            status_span,
        ]);

        let title_area = Rect {
            x: inner.x,
            y,
            width: inner.width,
            height: 1,
        };
        f.render_widget(Paragraph::new(title_line), title_area);
        y += 1;

        // ── Progress gauge ──────────────────────────────────────────────
        let gauge_area = Rect {
            x: inner.x,
            y,
            width: inner.width,
            height: 1,
        };
        let pct = dl.percent.clamp(0.0, 100.0) as u16;
        let gauge = Gauge::default()
            .gauge_style(Style::default().fg(gauge_color).bg(Color::DarkGray))
            .percent(pct)
            .label(format!("{pct}%"));
        f.render_widget(gauge, gauge_area);
        y += 1;

        // blank separator
        y += 1;
    }
}

// ── Status bar ────────────────────────────────────────────────────────────

fn render_statusbar(f: &mut Frame, area: Rect, app: &App) {
    let content = if let Some(ref err) = app.error_msg {
        Line::from(Span::styled(format!(" ✗ {err}"), style_err()))
    } else if let Some(ref msg) = app.status_msg {
        Line::from(Span::styled(format!(" {msg}"), style_dim()))
    } else {
        let hints = match app.screen {
            AppScreen::Login => " Enter confirm · Tab show/hide token · Ctrl-C quit",
            AppScreen::Courses => " ↑↓/jk navigate · Enter open · d download · r refresh · L logout · Tab tab · Ctrl-C quit",
            AppScreen::Curriculum => " ↑↓/jk navigate · Enter expand · d download · a all · b back · Tab tab",
        };
        Line::from(Span::styled(hints, style_dim()))
    };

    f.render_widget(Paragraph::new(content), area);
}
