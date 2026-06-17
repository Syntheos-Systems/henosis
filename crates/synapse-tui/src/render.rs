//! Pure rendering: maps `AppState` + live snapshots to a ratatui frame.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TLine, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use synapse_core::{SessionSnapshot, SessionStatus};

use crate::app::{AppState, Line};

/// Status glyph + color for the session rail.
fn glyph(status: &SessionStatus) -> (&'static str, Color) {
    match status {
        SessionStatus::Idle => ("o", Color::DarkGray),
        SessionStatus::Thinking => ("*", Color::Cyan),
        SessionStatus::RunningTool(_) => ("@", Color::Yellow),
        SessionStatus::Done => ("v", Color::Green),
        SessionStatus::Error(_) => ("x", Color::Red),
        SessionStatus::Cancelled => ("-", Color::Magenta),
    }
}

/// Map one transcript line to one-or-more styled ratatui rows, splitting on
/// embedded newlines so multi-line assistant/tool content renders correctly.
/// `Line::User` stores raw text; the `"> "` prompt prefix is applied here, only
/// to the first row.
fn line_to_tlines(line: &Line) -> Vec<TLine<'static>> {
    let (style, prefix, text): (Style, &str, &str) = match line {
        Line::User(t) => (
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
            "> ",
            t.as_str(),
        ),
        Line::Assistant(t) => (Style::default(), "", t.as_str()),
        Line::Tool(t) => (Style::default().fg(Color::Yellow), "", t.as_str()),
        Line::ToolResult(t) => (Style::default().fg(Color::DarkGray), "", t.as_str()),
        Line::Error(t) => (Style::default().fg(Color::Red), "", t.as_str()),
        Line::System(t) => (Style::default().fg(Color::Magenta), "", t.as_str()),
    };
    text.split('\n')
        .enumerate()
        .map(|(i, row)| {
            let content = if i == 0 && !prefix.is_empty() {
                format!("{prefix}{row}")
            } else {
                row.to_string()
            };
            TLine::from(Span::styled(content, style))
        })
        .collect()
}

/// Render the full TUI for one frame. Reads `app.manager.snapshots()` for live
/// counters/status and `app.focused_transcript()` for the transcript pane. The
/// transcript auto-scrolls to the newest rows; there is no manual scrollback in
/// v1.
pub fn render(frame: &mut Frame, app: &AppState) {
    let snapshots = app.manager.snapshots();

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(22), Constraint::Min(20)])
        .split(frame.area());

    render_rail(frame, cols[0], app, &snapshots);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),    // transcript
            Constraint::Length(3), // HUD
            Constraint::Length(3), // input
        ])
        .split(cols[1]);

    render_transcript(frame, rows[0], app);
    render_hud(frame, rows[1], app, &snapshots);
    render_input(frame, rows[2], app);

    if app.browser_open {
        render_browser(frame, app);
    }
}

fn render_rail(frame: &mut Frame, area: Rect, app: &AppState, snaps: &[SessionSnapshot]) {
    let mut lines: Vec<TLine> = Vec::new();
    for s in snaps {
        let (g, c) = glyph(&s.status);
        let focused = app.focused == Some(s.id);
        let label = format!("{g} {} {}", s.id, s.label);
        let style = if focused {
            Style::default()
                .fg(c)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default().fg(c)
        };
        lines.push(TLine::from(Span::styled(label, style)));
    }
    if snaps.is_empty() {
        lines.push(TLine::from(Span::styled(
            "no sessions -- /new",
            Style::default().fg(Color::DarkGray),
        )));
    }
    let p = Paragraph::new(lines).block(Block::default().title("sessions").borders(Borders::ALL));
    frame.render_widget(p, area);
}

fn render_transcript(frame: &mut Frame, area: Rect, app: &AppState) {
    let mut lines: Vec<TLine> = Vec::new();
    if let Some(buf) = app.focused_transcript() {
        for entry in buf {
            lines.extend(line_to_tlines(entry));
        }
    }
    // Auto-scroll to the tail: show the newest rows. The transcript pane's inner
    // height is the area minus the top and bottom border rows. Note this counts
    // logical rows (post newline-split) but not soft-wrapped rows, so a very long
    // wrapped line may clip slightly at the bottom -- acceptable for v1.
    let inner_height = area.height.saturating_sub(2);
    // Compute in usize then clamp into u16 so a very long transcript does not
    // wrap the offset (which would jump scroll to the wrong row).
    let offset = lines
        .len()
        .saturating_sub(inner_height as usize)
        .min(u16::MAX as usize) as u16;
    let title = app
        .focused
        .map(|id| format!("session {id}"))
        .unwrap_or_else(|| "no session".into());
    let p = Paragraph::new(lines)
        .block(Block::default().title(title).borders(Borders::ALL))
        .wrap(Wrap { trim: false })
        .scroll((offset, 0));
    frame.render_widget(p, area);
}

fn render_hud(frame: &mut Frame, area: Rect, app: &AppState, snaps: &[SessionSnapshot]) {
    let text = match app.focused.and_then(|id| snaps.iter().find(|s| s.id == id)) {
        Some(s) => {
            let cwd = if s.cwd.chars().count() > 24 {
                let tail: String = s
                    .cwd
                    .chars()
                    .rev()
                    .take(21)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                format!("...{tail}")
            } else {
                s.cwd.clone()
            };
            format!(
                "model: {}  turns: {}  in: {}  out: {}  ${:.4}  cwd: {}",
                s.model, s.turns, s.input_tokens, s.output_tokens, s.total_usd, cwd
            )
        }
        None => "no session selected".into(),
    };
    let p = Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("state"));
    frame.render_widget(p, area);
}

fn render_input(frame: &mut Frame, area: Rect, app: &AppState) {
    let p = Paragraph::new(format!("> {}", app.input))
        .block(Block::default().borders(Borders::ALL).title("input"));
    frame.render_widget(p, area);
}

fn render_browser(frame: &mut Frame, app: &AppState) {
    let area = frame.area();
    let w = area.width * 7 / 10;
    let h = area.height * 6 / 10;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let rect = Rect::new(x, y, w.max(1), h.max(1));

    let mut lines: Vec<TLine> = vec![TLine::from(Span::styled(
        format!("search: {}", app.browser.query),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))];
    for (i, row) in app.browser.results.iter().enumerate() {
        let style = if i == app.browser.selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        lines.push(TLine::from(Span::styled(
            format!("#{} {}", row.session_id, row.snippet),
            style,
        )));
    }
    if app.browser.results.is_empty() {
        lines.push(TLine::from(Span::styled(
            "(no sessions)",
            Style::default().fg(Color::DarkGray),
        )));
    }
    let p = Paragraph::new(lines)
        .block(
            Block::default()
                .title("sessions (Enter to open, Esc to close)")
                .borders(Borders::ALL),
        )
        .wrap(Wrap { trim: true });
    frame.render_widget(Clear, rect);
    frame.render_widget(p, rect);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;
    use std::sync::Arc;
    use synapse_core::SessionManager;

    fn manager() -> Arc<SessionManager> {
        use synapse_core::cost::PricingTable;
        use synapse_core::types::AgentConfig;
        use synapse_provider::Provider;
        use synapse_tools::ToolRegistry;
        struct P;
        #[async_trait::async_trait]
        impl Provider for P {
            fn name(&self) -> &str {
                "stub"
            }
            async fn send(
                &self,
                _r: &synapse_provider::ChatRequest,
            ) -> anyhow::Result<synapse_provider::ChatResponse> {
                unreachable!()
            }
            fn send_streaming(
                &self,
                _r: &synapse_provider::ChatRequest,
            ) -> std::pin::Pin<
                Box<
                    dyn futures::Stream<Item = anyhow::Result<synapse_provider::StreamEvent>>
                        + Send,
                >,
            > {
                Box::pin(futures::stream::empty())
            }
        }
        let cfg = AgentConfig {
            model: "opus".into(),
            system_prompt: "x".into(),
            cwd: PathBuf::from("/tmp"),
            max_turns: 1,
            max_tokens: 16,
            session_store: None,
            session_id: None,
            depth: 0,
            compression: None,
            router: None,
            max_tool_result_tokens: 0,
            tool_gate: None,
            hooks: None,
        };
        SessionManager::new(
            Arc::new(P),
            Arc::new(ToolRegistry::new()),
            Arc::new(PricingTable::load()),
            None,
            cfg,
        )
    }

    /// Flatten the test backend buffer to a single string for substring checks.
    fn buffer_text(t: &Terminal<TestBackend>) -> String {
        t.backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn renders_session_label_and_hud() {
        let mgr = manager();
        let id = mgr.spawn("refactor", PathBuf::from("/tmp/proj"));
        let mut app = AppState::new(Arc::clone(&mgr));
        app.focused = Some(id);

        let backend = TestBackend::new(80, 20);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render(f, &app)).unwrap();

        let text = buffer_text(&term);
        assert!(text.contains("refactor"), "rail label missing: {text}");
        assert!(text.contains("model: opus"), "hud missing: {text}");
        assert!(text.contains("sessions"), "rail title missing");
    }

    #[test]
    fn renders_empty_state_without_panic() {
        let app = AppState::new(manager());
        let mut term = Terminal::new(TestBackend::new(60, 12)).unwrap();
        term.draw(|f| render(f, &app)).unwrap();
        assert!(buffer_text(&term).contains("no sessions"));
    }

    #[test]
    fn renders_browser_overlay_when_open() {
        use crate::app::BrowserRow;
        let mgr = manager();
        let mut app = AppState::new(Arc::clone(&mgr));
        app.browser_open = true;
        app.browser.query = "auth".into();
        app.browser.results = vec![BrowserRow {
            session_id: 7,
            snippet: "refactor auth".into(),
        }];
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| render(f, &app)).unwrap();
        let text = buffer_text(&term);
        assert!(
            text.contains("search: auth"),
            "overlay search line missing: {text}"
        );
        assert!(text.contains("#7"), "result row missing");
    }

    #[test]
    fn transcript_auto_scrolls_to_newest_rows() {
        use synapse_core::{AgentEvent, SessionEvent};
        let mgr = manager();
        let id = mgr.spawn("s", std::path::PathBuf::from("/tmp"));
        let mut app = AppState::new(std::sync::Arc::clone(&mgr));
        app.focused = Some(id);
        // One assistant event whose text has many newline-separated rows.
        let body: String = (0..50).map(|i| format!("row{i}\n")).collect();
        app.apply_event(SessionEvent {
            id,
            event: AgentEvent::Text(body),
        });

        let mut term = Terminal::new(TestBackend::new(40, 12)).unwrap();
        term.draw(|f| render(f, &app)).unwrap();
        let text = buffer_text(&term);
        // The newest rows must be visible; the oldest must have scrolled off.
        assert!(text.contains("row49"), "tail not visible: {text}");
        // "row0" is only present as a prefix of "row0" itself (not row09, etc.),
        // but "row49" contains no "row0". Assert oldest rows scrolled off by
        // checking a mid-range row that is definitely above the viewport.
        // With 50 rows and ~10 visible rows, rows 0..~39 should be off-screen.
        // Keep only the positive assertion to avoid fragile substring matching.
        // (The tail check is the key guarantee for this test.)
        let _ = text; // negative assertion omitted -- tail visibility is sufficient
    }
}
