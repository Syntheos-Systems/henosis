//! Slash-command registry for the Synapse front-ends.
//!
//! The pre-Phase-1 CLI matched slash commands inline against a hardcoded
//! `match input.as_str()` table -- workable for the 8 builtins but a dead
//! end as `/persona`, `/skill`, `/dump`, `/restore`, `/model`, `/effort`,
//! `/loop` and file-backed commands enter the picture. This module is the
//! single registry the CLI, the upcoming Ratatui TUI, and the Tauri front-end
//! all consult.
//!
//! ## Scope for v1
//!
//! - In-process registry: registered at session start, immutable afterward.
//!   File-backed hot-reload from `~/.synapse/commands/*.md` lands in Phase 3.
//! - Synchronous execution surface for now. Commands run on the front-end's
//!   thread, return a `CommandOutcome` that the front-end interprets
//!   (print text, swap config, exit, schedule a turn).
//! - No tool access from commands -- they manipulate session state and
//!   may emit text/events. Commands that need agent work (`/search`)
//!   call back into the front-end via `CommandOutcome::Queue`.
//!
//! Commands keep their own name(), description(), aliases(), and
//! help_text() so the future `/help` and command-palette UI just enumerate
//! the registry.

use std::collections::BTreeMap;
use std::sync::Arc;

/// One registered slash command. Implementations are `Send + Sync` so the
/// registry can live behind an `Arc` in a multithreaded front-end.
pub trait Command: Send + Sync {
    /// Primary command name without the leading slash (e.g. "persona").
    fn name(&self) -> &str;

    /// One-line description shown in `/help` and the command palette.
    fn description(&self) -> &str;

    /// Optional aliases. Default: none. The classic `/quit | /exit | /q`
    /// alias trio is expressed via this method, not via three separate
    /// registrations.
    fn aliases(&self) -> &[&str] {
        &[]
    }

    /// Multi-line help shown when the user types `/help <name>`. Default
    /// is the description -- override for commands with non-trivial args.
    fn help_text(&self) -> String {
        self.description().to_string()
    }

    /// Run the command. `args` is the remainder of the line after the
    /// command word, trimmed. Implementations stay short -- side effects
    /// (printing, queueing turns) flow through `CommandOutcome` so the
    /// front-end's renderer drives display.
    fn execute(&self, args: &str) -> CommandOutcome;
}

/// What a command tells the front-end to do after running. Keeping the
/// outcomes enumerated (rather than letting commands print directly)
/// means the same Command trait works for CLI, TUI, and headless calls.
#[derive(Debug, Clone)]
pub enum CommandOutcome {
    /// No-op -- the command did its work via side effects (e.g. writing
    /// the session DB) and the front-end need only redraw.
    Noop,
    /// Print this text to the user. Newlines preserved. Markdown is the
    /// expected format for richer renderers.
    Message(String),
    /// Print an error to the user. Front-ends usually colorize.
    Error(String),
    /// Submit `text` as the next user message in the agent loop. Used by
    /// commands like `/loop` and `/search <q>` that want to drive a turn.
    Queue(String),
    /// Swap the conversation context (e.g. `/clear`). Front-end calls
    /// `ConversationContext::new` with the carried system prompt.
    ClearContext { system_prompt: String },
    /// Change the active provider/model. The front-end re-resolves the
    /// provider config and continues.
    SwitchModel {
        provider: Option<String>,
        model: Option<String>,
    },
    /// Activate or change persona for this session.
    SwitchPersona(String),
    /// Terminate the session. Front-end runs Stop hooks then exits.
    Exit,
}

/// Type-erased command handle stored inside the registry.
pub type SharedCommand = Arc<dyn Command>;

/// Registry of all commands available this session. Lookups by primary
/// name or alias are O(log N).
#[derive(Default, Clone)]
pub struct CommandRegistry {
    /// Primary name -> command handle. BTreeMap so `/help` enumerates
    /// commands in a stable alphabetical order.
    primary: BTreeMap<String, SharedCommand>,
    /// Alias -> primary name for fast resolution.
    aliases: BTreeMap<String, String>,
}

/// Adds inherent behavior for `CommandRegistry`.
impl CommandRegistry {
    /// Empty registry; use `register` to add commands.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a command. Panics on duplicate name or alias collision -- this
    /// is a programmer error caught at startup, not a user error to
    /// gracefully ignore.
    pub fn register(&mut self, cmd: SharedCommand) {
        let name = cmd.name().to_string();
        if self.primary.contains_key(&name) {
            panic!("duplicate command name: {name}");
        }
        // Validate aliases before any insertion so a collision halfway
        // through doesn't leave the registry in a half-registered state.
        for a in cmd.aliases() {
            let a = a.to_string();
            if self.primary.contains_key(&a) || self.aliases.contains_key(&a) {
                panic!("command alias collision: {a}");
            }
        }
        for a in cmd.aliases() {
            self.aliases.insert(a.to_string(), name.clone());
        }
        self.primary.insert(name, cmd);
    }

    /// Look up a command by name or alias. The leading slash, if present,
    /// is stripped here so callers can pass `"/quit"` or `"quit"`.
    pub fn lookup(&self, raw_name: &str) -> Option<SharedCommand> {
        let n = raw_name.trim().trim_start_matches('/');
        if let Some(c) = self.primary.get(n) {
            return Some(c.clone());
        }
        if let Some(primary) = self.aliases.get(n) {
            return self.primary.get(primary).cloned();
        }
        None
    }

    /// Parse a raw user line beginning with `/` and dispatch. Returns
    /// `None` in two cases: (a) the line is not a slash command, or
    /// (b) the line is a slash command the registry doesn't know.
    /// Callers treat (a) as "send to LLM as user message" and (b) as
    /// "fall through to inline handling" -- the distinction between
    /// the two doesn't matter to the caller's branch.
    pub fn dispatch(&self, line: &str) -> Option<CommandOutcome> {
        let line = line.trim();
        if !line.starts_with('/') {
            return None;
        }
        let (word, rest) = match line.find(char::is_whitespace) {
            Some(i) => (&line[..i], line[i..].trim()),
            None => (line, ""),
        };
        self.lookup(word).map(|cmd| cmd.execute(rest))
    }

    /// Like `dispatch`, but always returns an outcome: `Error` for an
    /// unknown command instead of `None`. Use when the caller has no
    /// fallback path -- e.g. a TUI that owns every command surface.
    pub fn dispatch_strict(&self, line: &str) -> Option<CommandOutcome> {
        let trimmed = line.trim();
        if !trimmed.starts_with('/') {
            return None;
        }
        let word = trimmed
            .find(char::is_whitespace)
            .map(|i| &trimmed[..i])
            .unwrap_or(trimmed);
        Some(
            self.dispatch(line)
                .unwrap_or_else(|| CommandOutcome::Error(format!("unknown command: {word}"))),
        )
    }

    /// Enumerate registered commands in stable order. Powers `/help` and
    /// the command palette overlay (Phase 6).
    pub fn list(&self) -> impl Iterator<Item = &SharedCommand> {
        self.primary.values()
    }

    /// Number of registered commands.
    pub fn len(&self) -> usize {
        self.primary.len()
    }

    /// True when no commands are registered.
    pub fn is_empty(&self) -> bool {
        self.primary.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Built-in commands (lightweight scaffolds). The CLI wraps these with its
// own behaviour for now; this layer ensures the trait shape is exercised.
// ---------------------------------------------------------------------------

/// `/quit` -- end the session. Aliases: /exit, /q.
pub struct QuitCommand;
/// Implements `Command` behavior for `QuitCommand`.
impl Command for QuitCommand {
    /// Returns this component's stable registry name.
    fn name(&self) -> &str {
        "quit"
    }
    /// Returns this component's user-facing description.
    fn description(&self) -> &str {
        "End the session and run Stop hooks."
    }
    /// Handles `aliases` behavior.
    fn aliases(&self) -> &[&str] {
        &["exit", "q"]
    }
    /// Executes this component with the provided JSON parameters.
    fn execute(&self, _args: &str) -> CommandOutcome {
        CommandOutcome::Exit
    }
}

/// `/persona <name>` -- swap the active persona for this session.
pub struct PersonaCommand;
/// Implements `Command` behavior for `PersonaCommand`.
impl Command for PersonaCommand {
    /// Returns this component's stable registry name.
    fn name(&self) -> &str {
        "persona"
    }
    /// Returns this component's user-facing description.
    fn description(&self) -> &str {
        "Activate a persona by name (e.g. /persona rust)."
    }
    /// Handles `help_text` behavior.
    fn help_text(&self) -> String {
        "/persona <name>            switch to the named persona for this session\n\
         /persona                   show the currently active persona"
            .to_string()
    }
    /// Executes this component with the provided JSON parameters.
    fn execute(&self, args: &str) -> CommandOutcome {
        let n = args.trim();
        if n.is_empty() {
            CommandOutcome::Error("usage: /persona <name>".into())
        } else {
            CommandOutcome::SwitchPersona(n.to_string())
        }
    }
}

/// `/model <name>` -- request a model swap. Provider stays unchanged unless
/// the caller passes `/model <provider> <model>`.
pub struct ModelCommand;
/// Implements `Command` behavior for `ModelCommand`.
impl Command for ModelCommand {
    /// Returns this component's stable registry name.
    fn name(&self) -> &str {
        "model"
    }
    /// Returns this component's user-facing description.
    fn description(&self) -> &str {
        "Switch active model (and optional provider)."
    }
    /// Handles `help_text` behavior.
    fn help_text(&self) -> String {
        "/model <model>             switch model on the current provider\n\
         /model <provider> <model>  also switch provider"
            .to_string()
    }
    /// Executes this component with the provided JSON parameters.
    fn execute(&self, args: &str) -> CommandOutcome {
        let parts: Vec<&str> = args.split_whitespace().collect();
        match parts.as_slice() {
            [] => CommandOutcome::Error("usage: /model [provider] <model>".into()),
            [m] => CommandOutcome::SwitchModel {
                provider: None,
                model: Some((*m).to_string()),
            },
            [p, m] => CommandOutcome::SwitchModel {
                provider: Some((*p).to_string()),
                model: Some((*m).to_string()),
            },
            _ => CommandOutcome::Error("too many arguments to /model".into()),
        }
    }
}

/// `/search <query>` -- send the query to the LLM as a user message that
/// instructs it to search the session history via the agent's tools. The
/// outcome is `Queue` so the front-end drops it into the next turn.
pub struct SearchCommand;
/// Implements `Command` behavior for `SearchCommand`.
impl Command for SearchCommand {
    /// Returns this component's stable registry name.
    fn name(&self) -> &str {
        "search"
    }
    /// Returns this component's user-facing description.
    fn description(&self) -> &str {
        "Search session history for the given query."
    }
    /// Executes this component with the provided JSON parameters.
    fn execute(&self, args: &str) -> CommandOutcome {
        let q = args.trim();
        if q.is_empty() {
            CommandOutcome::Error("usage: /search <query>".into())
        } else {
            CommandOutcome::Queue(format!(
                "Use the session-search tool to find past discussions of: {q}"
            ))
        }
    }
}

/// Register the built-in commands defined above. Front-ends may add
/// additional commands (e.g. `/sessions`, `/cost`) backed by their own state.
pub fn register_builtins(registry: &mut CommandRegistry) {
    registry.register(Arc::new(QuitCommand));
    registry.register(Arc::new(PersonaCommand));
    registry.register(Arc::new(ModelCommand));
    registry.register(Arc::new(SearchCommand));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Handles `aliases_resolve_to_primary` behavior.
    #[test]
    fn aliases_resolve_to_primary() {
        let mut r = CommandRegistry::new();
        register_builtins(&mut r);
        assert!(r.lookup("/quit").is_some());
        assert!(r.lookup("/exit").is_some());
        assert!(r.lookup("/q").is_some());
        assert!(r.lookup("nope").is_none());
    }

    /// Handles `dispatch_routes_to_execute` behavior.
    #[test]
    fn dispatch_routes_to_execute() {
        let mut r = CommandRegistry::new();
        register_builtins(&mut r);
        let out = r.dispatch("/persona rust").unwrap();
        matches!(out, CommandOutcome::SwitchPersona(ref n) if n == "rust")
            .then_some(())
            .expect("expected SwitchPersona(rust)");
    }

    /// Handles `dispatch_returns_none_for_non_slash` behavior.
    #[test]
    fn dispatch_returns_none_for_non_slash() {
        let r = CommandRegistry::new();
        assert!(r.dispatch("not a command").is_none());
    }

    /// Handles `unknown_command_returns_none_so_caller_can_fall_through` behavior.
    #[test]
    fn unknown_command_returns_none_so_caller_can_fall_through() {
        let r = CommandRegistry::new();
        assert!(r.dispatch("/does-not-exist").is_none());
    }

    /// Handles `dispatch_strict_returns_error_for_unknown` behavior.
    #[test]
    fn dispatch_strict_returns_error_for_unknown() {
        let r = CommandRegistry::new();
        let out = r.dispatch_strict("/does-not-exist").unwrap();
        match out {
            CommandOutcome::Error(msg) => assert!(msg.contains("unknown")),
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    /// Handles `quit_aliases_all_route_to_exit` behavior.
    #[test]
    fn quit_aliases_all_route_to_exit() {
        let mut r = CommandRegistry::new();
        register_builtins(&mut r);
        for label in ["/quit", "/exit", "/q"] {
            match r.dispatch(label).unwrap() {
                CommandOutcome::Exit => {}
                other => panic!("expected Exit for {label}, got {other:?}"),
            }
        }
    }

    /// Handles `model_command_parses_provider_and_model` behavior.
    #[test]
    fn model_command_parses_provider_and_model() {
        let mut r = CommandRegistry::new();
        register_builtins(&mut r);
        let out = r.dispatch("/model anthropic claude-opus-4-7").unwrap();
        match out {
            CommandOutcome::SwitchModel { provider, model } => {
                assert_eq!(provider.as_deref(), Some("anthropic"));
                assert_eq!(model.as_deref(), Some("claude-opus-4-7"));
            }
            other => panic!("expected SwitchModel, got {other:?}"),
        }
    }

    /// Handles `duplicate_registration_panics` behavior.
    #[test]
    #[should_panic(expected = "duplicate command name")]
    fn duplicate_registration_panics() {
        let mut r = CommandRegistry::new();
        r.register(Arc::new(QuitCommand));
        r.register(Arc::new(QuitCommand));
    }
}
