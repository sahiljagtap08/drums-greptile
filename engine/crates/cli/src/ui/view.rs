//! ratatui drawing for `drums watch`'s live TUI. Purely a consumer of
//! [`super::model::ViewModel`] — no `EngineEvent` handling, no state beyond
//! what it's handed each frame. Not unit-tested (frame snapshots are brittle
//! and low-value); the state it draws from is what `ui::model`'s tests pin.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;
use std::time::Instant;

use engine_core::Provenance;

use super::model::{FailureCard, StageRow, StageStatus, ViewModel};
use super::HeaderInfo;

const RUNNING_FRAMES: [&str; 4] = ["◐", "◓", "◑", "◒"];

fn spinner_frame(elapsed_ms: u64) -> &'static str {
    RUNNING_FRAMES[((elapsed_ms / 250) % RUNNING_FRAMES.len() as u64) as usize]
}

fn styled(style: Style, use_color: bool) -> Style {
    if use_color {
        style
    } else {
        Style::default()
    }
}

pub fn draw(f: &mut Frame, vm: &ViewModel, header: &HeaderInfo, now: Instant, use_color: bool) {
    let area = f.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(f, rows[0], header, use_color);
    draw_content(f, rows[1], vm, now, use_color);
    draw_ticker(f, rows[2], vm, use_color);
    draw_footer(f, rows[3], vm);

    if vm.show_help {
        draw_help(f, area);
    }
}

fn draw_header(f: &mut Frame, area: Rect, header: &HeaderInfo, use_color: bool) {
    let block = Block::bordered().title(" drums ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let narrow = area.width < 80;
    let (line1, line2) = if narrow {
        (
            format!(
                "watching {}  repairs {}",
                header.repo_name, header.repair_label
            ),
            format!(
                "agent {}  ingest :{}",
                header.agent_label, header.ingest_port
            ),
        )
    } else {
        (
            format!(
                "{:<10}{:<30}{:<10}{}",
                "watching", header.repo_name, "repairs", header.repair_label
            ),
            format!(
                "{:<10}{:<30}{:<10}:{}",
                "agent", header.agent_label, "ingest", header.ingest_port
            ),
        )
    };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(inner);
    let dim = styled(Style::default().add_modifier(Modifier::DIM), use_color);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(line1, dim))),
        rows[0],
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(line2, dim))),
        rows[1],
    );
}

fn chip_text(chip: Option<Provenance>, running: bool) -> String {
    match chip {
        Some(p) => format!("[{}]", p.chip()),
        None if running => "…".to_string(),
        None => String::new(),
    }
}

fn chip_span(text: String, chip: Option<Provenance>, use_color: bool) -> Span<'static> {
    match chip {
        Some(p) => Span::styled(
            text,
            styled(Style::default().fg(super::model::chip_color(p)), use_color),
        ),
        None => Span::styled(
            text,
            styled(Style::default().add_modifier(Modifier::DIM), use_color),
        ),
    }
}

fn stage_line(stage: &StageRow, now: Instant, use_color: bool) -> Line<'static> {
    let (icon, icon_style) = match stage.status {
        StageStatus::Done => ("✓".to_string(), Style::default().fg(Color::Green)),
        StageStatus::Failed => ("✗".to_string(), Style::default().fg(Color::Red)),
        StageStatus::Running => (
            spinner_frame(stage.elapsed_ms(now)).to_string(),
            Style::default().fg(Color::DarkGray),
        ),
    };
    let label = format!("{:<11}", stage.label);
    let chip_text = chip_text(stage.chip, stage.is_running());
    let chip_pad = 12usize.saturating_sub(chip_text.chars().count());
    let chip = chip_span(chip_text, stage.chip, use_color);
    // Fixed-width timer column: format!("{:>7}", ...) keeps this exact width
    // whether the value is "0.2s" or "128.4s", so the duration text never
    // shifts other columns as it ticks up frame to frame.
    let dur = format!(
        "{:>7}",
        format!("{:.1}s", stage.elapsed_ms(now) as f64 / 1000.0)
    );

    Line::from(vec![
        Span::raw("    "),
        Span::styled(icon, styled(icon_style, use_color)),
        Span::raw(" "),
        Span::raw(label),
        Span::raw(" "),
        chip,
        Span::raw(" ".repeat(chip_pad.max(1))),
        Span::raw(stage.detail.clone()),
        Span::raw("  "),
        Span::styled(
            dur,
            styled(Style::default().add_modifier(Modifier::DIM), use_color),
        ),
    ])
}

fn card_lines(
    card: &FailureCard,
    selected: bool,
    now: Instant,
    use_color: bool,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let marker = if selected { "▶ " } else { "  " };
    let header_style = styled(
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        use_color,
    );
    let title = format!("{marker}▲ {} failing", card.service);
    lines.push(Line::from(vec![
        Span::styled(title, header_style),
        Span::raw("   "),
        Span::styled(
            card.detected_at_label.clone(),
            styled(Style::default().add_modifier(Modifier::DIM), use_color),
        ),
    ]));
    lines.push(Line::from(Span::raw(format!(
        "    {} {} · {}",
        card.method, card.path, card.claim_text
    ))));

    for stage in &card.stages {
        lines.push(stage_line(stage, now, use_color));
    }

    if let Some(ready) = &card.ready_line {
        let style = styled(
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            use_color,
        );
        lines.push(Line::from(vec![
            Span::styled("  ✓ ", style),
            Span::raw(ready.clone()),
        ]));
    }
    lines.push(Line::raw(""));
    lines
}

/// Builds the whole scrollable content buffer plus each card's starting line
/// index (used to scroll the currently-selected card into view).
fn build_lines(vm: &ViewModel, now: Instant, use_color: bool) -> (Vec<Line<'static>>, Vec<usize>) {
    let mut lines = Vec::new();
    let mut starts = Vec::new();
    for (i, card) in vm.cards.iter().enumerate() {
        starts.push(lines.len());
        lines.extend(card_lines(card, i == vm.selected, now, use_color));
    }
    if vm.cards.is_empty() {
        lines.push(Line::raw("  watching for failures…"));
    }
    (lines, starts)
}

fn draw_content(f: &mut Frame, area: Rect, vm: &ViewModel, now: Instant, use_color: bool) {
    let (lines, starts) = build_lines(vm, now, use_color);
    let viewport = area.height as usize;
    let selected_start = starts.get(vm.selected).copied().unwrap_or(0);
    let max_scroll = lines.len().saturating_sub(viewport);
    let scroll = selected_start.min(max_scroll) as u16;
    // Deliberately no `.wrap(..)`: these are single logical rows (a chip is
    // never allowed to wrap mid-glyph onto the next terminal line) — an
    // overlong row clips instead, which is the honest degrade at 80 columns.
    let content = Paragraph::new(lines).scroll((scroll, 0));
    f.render_widget(content, area);
}

fn draw_ticker(f: &mut Frame, area: Rect, vm: &ViewModel, use_color: bool) {
    let text = vm.ticker.back().cloned().unwrap_or_default();
    let style = styled(Style::default().add_modifier(Modifier::DIM), use_color);
    f.render_widget(Paragraph::new(Line::from(Span::styled(text, style))), area);
}

fn draw_footer(f: &mut Frame, area: Rect, vm: &ViewModel) {
    let footer = format!(
        "record {}  ·  {} events        [q] quit  [?] keys",
        vm.record_path.display(),
        vm.event_count
    );
    f.render_widget(Paragraph::new(footer), area);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let w = 44.min(area.width.saturating_sub(4)).max(1);
    let h = 6.min(area.height.saturating_sub(4)).max(1);
    let popup = Rect {
        x: (area.width.saturating_sub(w)) / 2,
        y: (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, popup);
    let block = Block::bordered().title(" keys ");
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    let text = vec![
        Line::raw("q / ctrl-c   quit"),
        Line::raw("?            toggle this help"),
        Line::raw("↑ / ↓        select a failure card"),
    ];
    f.render_widget(Paragraph::new(text), inner);
}
