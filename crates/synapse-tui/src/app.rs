//! TUI application state: focused session, per-session transcript buffers, and
//! the input line. The reducer (`apply_event`) is pure over `AppState` so it
//! can be unit-tested without a terminal.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use synapse_core::{AgentEvent, SessionEvent, SessionId, SessionManager};

/// Maximum [`Line`] entries retained per session transcript. Coalesced
/// assistant deltas count as ONE entry (not one terminal row).
const MAX_LINES: usize = 2000;

/// Maximum characters in a single coalesced assistant line. Past this, a new
/// assistant line is started so one long streamed response cannot grow a single
/// String without bound.
const MAX_LINE_CHARS: usize = 16_384;

/// One search hit shown in the session browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserRow {
    pub session_id: i64,
    pub snippet: String,
}

/// State for the session browser overlay.
#[derive(Debug, Default)]
pub struct BrowserState {
    pub query: String,
    pub results: Vec<BrowserRow>,
    pub selected: usize,
}

impl BrowserState {
    /// Clamp the selection into range after the results change.
    pub fn clamp(&mut self) {
        if self.results.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.results.len() {
            self.selected = self.results.len() - 1;
        }
    }

    /// The currently selected stored session id, if any.
    pub fn selected_id(&self) -> Option<i64> {
        self.results.get(self.selected).map(|r| r.session_id)
    }
}

/// One rendered transcript line with a coarse kind for styling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// Raw user input text WITHOUT any prompt prefix; the renderer prepends "> ".
    User(String),
    Assistant(String),
    Tool(String),
    ToolResult(String),
    Error(String),
    System(String),
}

/// All UI state. Holds an `Arc<SessionManager>` for actions. Live counters and
/// status are read on each render from `manager` (the source of truth);
/// transcript lines are reduced locally into `transcripts` by `apply_event`.
pub struct AppState {
    pub manager: Arc<SessionManager>,
    pub focused: Option<SessionId>,
    /// Per-session transcript ring buffers.
    pub transcripts: HashMap<SessionId, VecDeque<Line>>,
    /// Current input line for the focused session.
    pub input: String,
    /// True when the session browser overlay is open.
    pub browser_open: bool,
    /// Browser overlay state: query, results, selection.
    pub browser: BrowserState,
    /// Set to true to break the event loop.
    pub should_quit: bool,
}

impl AppState {
    pub fn new(manager: Arc<SessionManager>) -> Self {
        Self {
            manager,
            focused: None,
            transcripts: HashMap::new(),
            input: String::new(),
            browser_open: false,
            browser: BrowserState::default(),
            should_quit: false,
        }
    }

    /// Append `line` to the transcript for `id`, coalescing consecutive
    /// `Assistant` deltas into the previous entry (until it reaches
    /// `MAX_LINE_CHARS`, after which a new assistant line starts). Evicts from
    /// the front once the buffer exceeds `MAX_LINES`.
    fn push_line(&mut self, id: SessionId, line: Line) {
        let buf = self.transcripts.entry(id).or_default();
        // Coalesce consecutive assistant text deltas into the last line, but cap
        // the per-line length so a long streaming response cannot grow unbounded.
        if let (Line::Assistant(delta), Some(Line::Assistant(last))) = (&line, buf.back_mut())
            && last.len() + delta.len() <= MAX_LINE_CHARS
        {
            last.push_str(delta);
            return;
        }
        buf.push_back(line);
        while buf.len() > MAX_LINES {
            buf.pop_front();
        }
    }

    /// Apply one multiplexed session event to the transcript buffers.
    pub fn apply_event(&mut self, ev: SessionEvent) {
        let id = ev.id;
        match ev.event {
            AgentEvent::Text(t) => self.push_line(id, Line::Assistant(t)),
            AgentEvent::ToolStart { name, .. } => {
                self.push_line(id, Line::Tool(format!("[tool] {name}")))
            }
            AgentEvent::ToolResult {
                content, is_error, ..
            } => {
                let head: String = content.lines().take(8).collect::<Vec<_>>().join("\n");
                if is_error {
                    self.push_line(id, Line::Error(head));
                } else {
                    self.push_line(id, Line::ToolResult(head));
                }
            }
            AgentEvent::Error(msg) => self.push_line(id, Line::Error(msg)),
            AgentEvent::ModelSwitch { from, to } => {
                self.push_line(id, Line::System(format!("[model: {from} -> {to}]")))
            }
            // Counters live in the snapshot; these need no transcript line.
            AgentEvent::TurnStart
            | AgentEvent::TurnEnd
            | AgentEvent::Usage { .. }
            | AgentEvent::Cost { .. } => {}
        }
    }

    /// Record a locally-typed user line in the focused transcript. Stores the
    /// raw text; the renderer is responsible for any prompt prefix.
    pub fn record_user_input(&mut self, id: SessionId, text: &str) {
        self.push_line(id, Line::User(text.to_string()));
    }

    /// Record a system note line in a session's transcript.
    pub fn record_system(&mut self, id: SessionId, text: &str) {
        self.push_line(id, Line::System(text.to_string()));
    }

    /// Borrow the focused session's transcript without cloning. Use this on the
    /// render hot path (called every frame). Returns `None` if no session is
    /// focused or it has no transcript yet.
    pub fn focused_transcript(&self) -> Option<&VecDeque<Line>> {
        self.focused.and_then(|id| self.transcripts.get(&id))
    }

    /// Transcript lines for the focused session as an owned Vec. Convenience for
    /// tests and non-hot-path callers; prefer `focused_transcript` when rendering.
    /// Returns an empty `Vec` if no session is focused or it has no transcript.
    #[allow(dead_code)] // used in unit tests; not on the render hot path
    pub fn focused_lines(&self) -> Vec<Line> {
        self.focused
            .and_then(|id| self.transcripts.get(&id))
            .map(|b| b.iter().cloned().collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // Build a stub manager mirroring synapse-core's test harness.
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
            model: "stub".into(),
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

    #[test]
    fn text_deltas_coalesce_into_one_assistant_line() {
        let mut app = AppState::new(manager());
        let id = 7;
        app.focused = Some(id);
        app.apply_event(SessionEvent {
            id,
            event: AgentEvent::Text("Hel".into()),
        });
        app.apply_event(SessionEvent {
            id,
            event: AgentEvent::Text("lo".into()),
        });
        let lines = app.focused_lines();
        assert_eq!(lines, vec![Line::Assistant("Hello".into())]);
    }

    #[test]
    fn events_route_to_their_session_only() {
        let mut app = AppState::new(manager());
        app.focused = Some(1);
        app.apply_event(SessionEvent {
            id: 2,
            event: AgentEvent::Text("other".into()),
        });
        // Focused (1) has nothing; session 2 has the line.
        assert!(app.focused_lines().is_empty());
        assert_eq!(app.transcripts.get(&2).unwrap().len(), 1);
    }

    #[test]
    fn tool_events_become_tool_lines() {
        let mut app = AppState::new(manager());
        let id = 3;
        app.focused = Some(id);
        app.apply_event(SessionEvent {
            id,
            event: AgentEvent::ToolStart {
                id: "a".into(),
                name: "bash".into(),
            },
        });
        assert_eq!(app.focused_lines(), vec![Line::Tool("[tool] bash".into())]);
    }

    #[test]
    fn tool_line_breaks_assistant_coalesce() {
        let mut app = AppState::new(manager());
        let id = 5;
        app.focused = Some(id);
        app.apply_event(SessionEvent {
            id,
            event: AgentEvent::Text("A".into()),
        });
        app.apply_event(SessionEvent {
            id,
            event: AgentEvent::ToolStart {
                id: "t".into(),
                name: "bash".into(),
            },
        });
        app.apply_event(SessionEvent {
            id,
            event: AgentEvent::Text("B".into()),
        });
        // The tool line must break coalescing: three distinct lines, not "AB".
        assert_eq!(
            app.focused_lines(),
            vec![
                Line::Assistant("A".into()),
                Line::Tool("[tool] bash".into()),
                Line::Assistant("B".into()),
            ]
        );
    }

    #[test]
    fn browser_clamp_and_selected_id() {
        let mut b = BrowserState {
            query: "auth".into(),
            results: vec![
                BrowserRow {
                    session_id: 10,
                    snippet: "a".into(),
                },
                BrowserRow {
                    session_id: 11,
                    snippet: "b".into(),
                },
            ],
            selected: 5,
        };
        b.clamp();
        assert_eq!(b.selected, 1);
        assert_eq!(b.selected_id(), Some(11));
    }

    #[test]
    fn browser_empty_results_selected_id_none() {
        let mut b = BrowserState::default();
        b.clamp();
        assert_eq!(b.selected, 0);
        assert_eq!(b.selected_id(), None);
    }
}
