//! Keyboard handling and slash-command parsing. `handle_key` is a pure reducer
//! returning an `InputOutcome` the event loop acts on (so it is unit-testable
//! without a terminal or running tasks).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::AppState;

/// What the event loop should do after a key press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputOutcome {
    /// Nothing beyond mutating AppState (e.g. editing the input line).
    None,
    /// Submit the given text to the focused session.
    Submit(String),
    /// Spawn a new session in the given directory (None = cwd). The argument is
    /// raw user text; the caller must validate/canonicalize the path.
    NewSession(Option<String>),
    /// Close the focused session.
    CloseFocused,
    /// Cancel the in-flight turn of the focused session.
    CancelFocused,
    /// Switch focus to the session at this 1-based rail index.
    FocusIndex(usize),
    /// Set the model of the focused session. Raw user text; the caller validates.
    SetModel(String),
    /// Open the session browser overlay.
    OpenBrowser,
    /// A character was typed into the browser query (re-run the search).
    BrowserType,
    /// A character was deleted from the browser query (re-run the search).
    BrowserBackspace,
    /// Open the selected session from the browser.
    BrowserSubmit,
    /// Quit the app.
    Quit,
}

/// Parse a `/command ...` line into an outcome. Empty or whitespace-only input
/// returns `None`. A bare `/` or an unrecognized `/command` also returns `None`
/// (the input is discarded). `/model` with no argument returns `None`. Non-slash
/// text submits to the focused session.
pub fn parse_line(line: &str) -> InputOutcome {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return InputOutcome::None;
    }
    if let Some(rest) = trimmed.strip_prefix('/') {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let cmd = parts.next().unwrap_or("");
        let arg = parts.next().map(|s| s.trim().to_string());
        return match cmd {
            "new" => InputOutcome::NewSession(arg),
            "close" => InputOutcome::CloseFocused,
            "cancel" => InputOutcome::CancelFocused,
            "model" => arg
                .map(InputOutcome::SetModel)
                .unwrap_or(InputOutcome::None),
            "sessions" => InputOutcome::OpenBrowser,
            "quit" | "q" => InputOutcome::Quit,
            _ => InputOutcome::None,
        };
    }
    InputOutcome::Submit(trimmed.to_string())
}

/// Handle one key event, mutating `app.input` for editing keys and returning an
/// outcome for action keys.
pub fn handle_key(app: &mut AppState, key: KeyEvent) -> InputOutcome {
    // Ctrl-C always quits.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return InputOutcome::Quit;
    }
    // When the browser overlay is open, keys drive the overlay, not the input line.
    if app.browser_open {
        return handle_browser_key(app, key);
    }
    // Ctrl-1..9 switches focus.
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && let KeyCode::Char(c) = key.code
        && c.is_ascii_digit()
        && c != '0'
    {
        return InputOutcome::FocusIndex((c as u8 - b'0') as usize);
    }
    match key.code {
        KeyCode::Enter => {
            let line = std::mem::take(&mut app.input);
            parse_line(&line)
        }
        KeyCode::Char(c) => {
            // Ignore any remaining control-modified chord (Ctrl-C and Ctrl-digit
            // are handled above); never inject the literal char into the input.
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                return InputOutcome::None;
            }
            app.input.push(c);
            InputOutcome::None
        }
        KeyCode::Backspace => {
            app.input.pop();
            InputOutcome::None
        }
        KeyCode::Esc => {
            // Browser Esc is handled in handle_browser_key; here Esc clears input.
            app.input.clear();
            InputOutcome::None
        }
        _ => InputOutcome::None,
    }
}

/// Handle a key while the session browser overlay is open.
fn handle_browser_key(app: &mut AppState, key: KeyEvent) -> InputOutcome {
    match key.code {
        KeyCode::Esc => {
            app.browser_open = false;
            InputOutcome::None
        }
        KeyCode::Enter => InputOutcome::BrowserSubmit,
        KeyCode::Up => {
            app.browser.selected = app.browser.selected.saturating_sub(1);
            InputOutcome::None
        }
        KeyCode::Down => {
            app.browser.selected += 1;
            app.browser.clamp();
            InputOutcome::None
        }
        KeyCode::Backspace => {
            app.browser.query.pop();
            InputOutcome::BrowserBackspace
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.browser.query.push(c);
            InputOutcome::BrowserType
        }
        _ => InputOutcome::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppState;
    use std::sync::Arc;
    use synapse_core::SessionManager;

    #[test]
    fn plain_text_submits() {
        assert_eq!(
            parse_line("hello world"),
            InputOutcome::Submit("hello world".into())
        );
    }

    #[test]
    fn slash_new_with_and_without_arg() {
        assert_eq!(parse_line("/new"), InputOutcome::NewSession(None));
        assert_eq!(
            parse_line("/new /tmp/x"),
            InputOutcome::NewSession(Some("/tmp/x".into()))
        );
    }

    #[test]
    fn slash_model_and_quit() {
        assert_eq!(
            parse_line("/model claude-opus-4-8"),
            InputOutcome::SetModel("claude-opus-4-8".into())
        );
        assert_eq!(parse_line("/quit"), InputOutcome::Quit);
    }

    #[test]
    fn unknown_command_is_noop() {
        assert_eq!(parse_line("/wat"), InputOutcome::None);
    }

    fn manager() -> Arc<SessionManager> {
        use std::path::PathBuf;
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

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn ctrl_c_quits() {
        let mut app = AppState::new(manager());
        assert_eq!(
            handle_key(&mut app, key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            InputOutcome::Quit
        );
    }

    #[test]
    fn ctrl_digit_focuses_index() {
        let mut app = AppState::new(manager());
        assert_eq!(
            handle_key(&mut app, key(KeyCode::Char('3'), KeyModifiers::CONTROL)),
            InputOutcome::FocusIndex(3)
        );
    }

    #[test]
    fn typing_pushes_into_input() {
        let mut app = AppState::new(manager());
        handle_key(&mut app, key(KeyCode::Char('h'), KeyModifiers::NONE));
        handle_key(&mut app, key(KeyCode::Char('i'), KeyModifiers::NONE));
        assert_eq!(app.input, "hi");
    }

    #[test]
    fn ctrl_modified_char_does_not_enter_input() {
        let mut app = AppState::new(manager());
        let out = handle_key(&mut app, key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(out, InputOutcome::None);
        assert!(app.input.is_empty(), "Ctrl-A must not inject 'a'");
    }

    #[test]
    fn backspace_on_empty_is_safe() {
        let mut app = AppState::new(manager());
        assert_eq!(
            handle_key(&mut app, key(KeyCode::Backspace, KeyModifiers::NONE)),
            InputOutcome::None
        );
        assert!(app.input.is_empty());
    }

    #[test]
    fn enter_clears_buffer_and_parses() {
        let mut app = AppState::new(manager());
        app.input = "hello".into();
        let out = handle_key(&mut app, key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(out, InputOutcome::Submit("hello".into()));
        assert!(app.input.is_empty(), "Enter must clear the input buffer");
    }

    #[test]
    fn esc_closes_browser() {
        let mut app = AppState::new(manager());
        app.browser_open = true;
        let out = handle_key(&mut app, key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(out, InputOutcome::None);
        assert!(!app.browser_open);
    }

    #[test]
    fn browser_open_routes_enter_to_submit() {
        use crate::app::AppState;
        let mut app = AppState::new(manager());
        app.browser_open = true;
        assert_eq!(
            handle_key(&mut app, key(KeyCode::Enter, KeyModifiers::NONE)),
            InputOutcome::BrowserSubmit
        );
    }

    #[test]
    fn browser_open_typing_updates_query() {
        use crate::app::AppState;
        let mut app = AppState::new(manager());
        app.browser_open = true;
        handle_key(&mut app, key(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(app.browser.query, "x");
    }
}
