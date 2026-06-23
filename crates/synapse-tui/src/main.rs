//! synapse-tui: concurrent multi-session terminal UI for Synapse.

mod app;
mod build;
mod input;
mod render;

use std::io::{Stdout, stdout};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::broadcast::error::RecvError;

use synapse_core::SessionManager;

use crate::app::AppState;
use crate::input::InputOutcome;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let rt = build::build()?;
    let manager = SessionManager::new(rt.provider, rt.tools, rt.pricing, rt.store, rt.base_config);

    // Restore the terminal on panic so a crash never wrecks the shell.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen);
        default_hook(info);
    }));

    let mut terminal = setup_terminal()?;
    let res = run(&mut terminal, manager).await;
    // Always restore the terminal, but never let a cleanup failure mask the
    // primary error from run().
    if let Err(e) = restore_terminal(&mut terminal) {
        match &res {
            Ok(()) => return Err(e),
            Err(primary) => log::error!("failed to restore terminal ({e}) after error: {primary}"),
        }
    }
    res
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

/// Restore the terminal to its normal state. Must be called after
/// `setup_terminal`. Returns early on the first crossterm error, so a failure
/// may leave later restore steps unrun -- callers should treat this as
/// best-effort and not mask a primary error with it.
fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

/// The TUI event loop. Subscribes to the manager BEFORE spawning the first
/// session (so no events are missed), then multiplexes terminal input, session
/// events, and a render tick until the user quits. Assumes the terminal is in
/// raw mode + alternate screen (set up by the caller).
async fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    manager: Arc<SessionManager>,
) -> Result<()> {
    let mut app = AppState::new(Arc::clone(&manager));

    // Subscribe before spawning so no events are missed.
    let mut events = EventStream::new();
    let mut rx = manager.subscribe();
    let mut tick = tokio::time::interval(Duration::from_millis(33)); // ~30fps

    // Monotonic counter: never reused, so closed sessions never collide on
    // worktree paths with new ones.
    let mut next_seq: u64 = 0;
    next_seq += 1;
    let cwd = std::env::current_dir()?;
    let first_cwd = prepare_cwd(&cwd, next_seq); // does its own block_in_place
    let first = tokio::task::block_in_place(|| manager.spawn("main", first_cwd));
    app.focused = Some(first);

    loop {
        tokio::select! {
            maybe_ev = events.next() => {
                if let Some(Ok(Event::Key(key))) = maybe_ev
                    && key.kind == KeyEventKind::Press
                {
                    let outcome = input::handle_key(&mut app, key);
                    act(&mut app, &manager, &mut next_seq, outcome);
                    if app.should_quit {
                        break;
                    }
                }
            }
            recv = rx.recv() => {
                match recv {
                    Ok(ev) => app.apply_event(ev),
                    Err(RecvError::Lagged(_)) => {
                        // Lagged: live counters/status stay correct (read from snapshots),
                        // but dropped events leave gaps in the transcript buffer. Re-open
                        // the session from the store if full history is needed.
                    }
                    Err(RecvError::Closed) => break,
                }
            }
            _ = tick.tick() => {
                terminal.draw(|f| render::render(f, &app))?;
            }
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}

/// Prepare a session cwd: a git worktree when possible, else the dir itself.
/// The git subprocess work is blocking, so it runs via `block_in_place` to keep
/// the async event loop responsive. (Requires the multi-threaded runtime, which
/// `#[tokio::main]` provides by default.)
fn prepare_cwd(base: &Path, session_seq: u64) -> PathBuf {
    tokio::task::block_in_place(|| prepare_cwd_blocking(base, session_seq))
}

/// The actual (blocking) worktree preparation. Logs a warning on any fallback so
/// a silent loss of isolation is visible.
fn prepare_cwd_blocking(base: &Path, session_seq: u64) -> PathBuf {
    match synapse_core::session_runtime::worktree::prepare(base, session_seq) {
        Ok(wt) => {
            if !wt.isolated {
                log::warn!(
                    "session {session_seq}: no worktree isolation for {} (not a git repo)",
                    base.display()
                );
            }
            wt.path
        }
        Err(e) => {
            log::warn!(
                "session {session_seq}: worktree setup failed for {} ({e}); using it directly",
                base.display()
            );
            base.to_path_buf()
        }
    }
}

/// Resolve a user-supplied directory string to an existing directory, falling
/// back to the current directory when missing or invalid.
fn resolve_dir(arg: Option<String>) -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match arg {
        Some(s) => {
            let p = PathBuf::from(&s);
            if p.is_dir() {
                p
            } else {
                log::warn!("requested dir {s:?} is not a directory; using cwd");
                cwd
            }
        }
        None => cwd,
    }
}

/// Run an FTS5 search (or list recent sessions when the query is empty) and
/// populate the browser results. SQLite access is blocking, so it runs via
/// block_in_place.
fn run_search(app: &mut AppState, manager: &Arc<SessionManager>) {
    use crate::app::BrowserRow;
    let Some(store) = manager.store() else {
        app.browser.results.clear();
        return;
    };
    let query = app.browser.query.trim().to_owned();
    let rows = tokio::task::block_in_place(|| {
        if query.is_empty() {
            store
                .list_sessions(20, 0)
                .map(|sessions| {
                    sessions
                        .into_iter()
                        .map(|s| BrowserRow {
                            session_id: s.id,
                            snippet: s.summary.unwrap_or(s.project),
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        } else {
            store
                .search(&query, 20)
                .map(|hits| {
                    hits.into_iter()
                        .map(|h| BrowserRow {
                            session_id: h.session_id,
                            snippet: h.snippet,
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        }
    });
    app.browser.results = rows;
    app.browser.clamp();
}

/// Apply an input outcome to the app + manager.
fn act(
    app: &mut AppState,
    manager: &Arc<SessionManager>,
    next_seq: &mut u64,
    outcome: InputOutcome,
) {
    match outcome {
        InputOutcome::None => {}
        InputOutcome::Quit => app.should_quit = true,
        InputOutcome::Submit(text) => {
            if let Some(id) = app.focused {
                app.record_user_input(id, &text);
                manager.send(id, text);
            }
        }
        InputOutcome::NewSession(dir) => {
            let base = resolve_dir(dir);
            *next_seq += 1;
            let seq = *next_seq;
            let cwd = prepare_cwd(&base, seq); // does its own block_in_place
            let id = tokio::task::block_in_place(|| manager.spawn(format!("s{seq}"), cwd));
            app.focused = Some(id);
        }
        InputOutcome::CloseFocused => {
            if let Some(id) = app.focused {
                let pos = manager.snapshots().iter().position(|s| s.id == id);
                manager.close(id);
                app.transcripts.remove(&id);
                let remaining = manager.snapshots();
                // Focus the session that took the closed one's slot (the "next"),
                // or the last remaining session if we closed the final one.
                let idx = pos.unwrap_or(0).min(remaining.len().saturating_sub(1));
                app.focused = remaining.get(idx).map(|s| s.id);
            }
        }
        InputOutcome::CancelFocused => {
            if let Some(id) = app.focused {
                manager.cancel(id);
            }
        }
        InputOutcome::FocusIndex(i) => {
            let snaps = manager.snapshots();
            if let Some(s) = snaps.get(i.saturating_sub(1)) {
                app.focused = Some(s.id);
            }
        }
        InputOutcome::SetModel(model) => {
            // v1: model is fixed at spawn via base_config. Live per-session model
            // switching needs a manager setter -- deferred. Surface a notice in the
            // focused transcript instead of silently swallowing the command.
            if let Some(id) = app.focused {
                app.record_system(
                    id,
                    &format!(
                        "/model: live switching to '{model}' is not supported yet \
                         (model is fixed when the session is spawned)"
                    ),
                );
            }
        }
        InputOutcome::OpenBrowser => {
            app.browser_open = true;
            app.browser.query.clear();
            app.browser.selected = 0;
            run_search(app, manager);
        }
        InputOutcome::BrowserType | InputOutcome::BrowserBackspace => {
            run_search(app, manager);
        }
        InputOutcome::BrowserSubmit => {
            if let Some(sid) = app.browser.selected_id() {
                if let Some(existing) = manager.find_by_stored_session_id(sid) {
                    // Already open; focus it rather than resuming a duplicate that
                    // would corrupt the store row by interleaving two threads.
                    app.focused = Some(existing);
                } else {
                    // NOTE (v1 limitation): the resumed session runs in the TUI's
                    // current dir, not the stored session's original project path
                    // (the store schema carries no cwd). Tool calls run here.
                    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                    match tokio::task::block_in_place(|| {
                        manager.resume(sid, format!("resumed-{sid}"), cwd)
                    }) {
                        Ok(id) => {
                            app.focused = Some(id);
                            let n = manager.context_message_count(id).unwrap_or(0);
                            app.record_system(
                                id,
                                &format!("[resumed stored session {sid}: {n} messages in context, history not shown]"),
                            );
                        }
                        Err(e) => log::warn!("resume of stored session {sid} failed: {e}"),
                    }
                }
            }
            app.browser_open = false;
        }
    }
}
