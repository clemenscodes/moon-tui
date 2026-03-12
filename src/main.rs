mod app;
mod moon;
mod ui;

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use app::{Action, App, Mode};

#[tokio::main]
async fn main() {
    let mut app = App::new();

    // Load projects before entering TUI.
    app.projects = moon::query_projects().await;
    if let Some(project) = app.projects.first() {
        app.tasks = moon::query_tasks(&project.id).await;
    }

    enable_raw_mode().expect("failed to enable raw mode");
    io::stdout()
        .execute(EnterAlternateScreen)
        .expect("failed to enter alternate screen");
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).expect("failed to create terminal");

    let result = run_event_loop(&mut terminal, &mut app).await;

    // Drop the terminal explicitly to flush CrosstermBackend's BufWriter before
    // we send control sequences directly to stdout. Without this, pending frame
    // data in the buffer would never reach the terminal (process::exit doesn't
    // flush stdio), leaving the terminal in a corrupted state.
    drop(terminal);

    // Clean up the terminal. Ignore errors — we're about to exit anyway.
    let _ = disable_raw_mode();
    let _ = io::stdout().execute(LeaveAlternateScreen);

    // Kill any running child process before exiting. For interactive tasks the
    // PTY master fd closing on exit also sends SIGHUP, but an explicit SIGTERM
    // covers non-interactive tasks and acts as a belt-and-suspenders for the
    // interactive case.
    let child_pid = app.child_pid.load(std::sync::atomic::Ordering::SeqCst);
    if child_pid != 0 {
        unsafe {
            libc::kill(child_pid as libc::pid_t, libc::SIGTERM);
        }
    }

    // Always exit immediately via process::exit. This is critical: background
    // spawn_blocking tasks (e.g. PTY master reads) block the tokio runtime from
    // shutting down normally, causing the terminal to hang. process::exit
    // bypasses the runtime shutdown entirely, so the terminal always returns
    // to the shell cleanly regardless of what background tasks are running.
    std::process::exit(if result.is_ok() { 0 } else { 1 });
}

/// Convert a crossterm key event to raw terminal bytes for forwarding to a
/// child process's stdin.
fn key_to_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    let bytes = match key.code {
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                if c.is_ascii_lowercase() || c.is_ascii_uppercase() {
                    vec![(c.to_ascii_lowercase() as u8) - b'a' + 1]
                } else {
                    return None;
                }
            } else {
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                s.as_bytes().to_vec()
            }
        }
        KeyCode::Enter => vec![b'\n'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        _ => return None,
    };
    Some(bytes)
}

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> io::Result<()> {
    let mut last_project_index = app.project_index;

    while app.running {
        // Only drain output when actively viewing it. This is THE critical
        // performance fix: in Normal mode, zero channel/Vec work happens,
        // so the TUI stays perfectly responsive regardless of background
        // task output volume.
        if app.mode == Mode::Output {
            app.poll_output();
        }

        // Poll async queries (non-blocking, always cheap).
        let projects_updated = app.poll_async();
        if projects_updated {
            let pid = app.projects.get(app.project_index).map(|p| p.id.clone());
            if let Some(pid) = pid {
                app.spawn_task_query(&pid);
            }
            if matches!(app.status_text(), Some("Refreshing...")) {
                app.set_status("Refreshed.");
            }
        }

        terminal.draw(|f| ui::render(f, app))?;

        // Fast poll only when actively viewing running output.
        // Normal mode uses 100ms — no wasted CPU, instant key response.
        let timeout = if app.mode == Mode::Output && app.is_running_task() {
            Duration::from_millis(16)
        } else {
            Duration::from_millis(100)
        };

        if event::poll(timeout)? {
            // Drain ALL pending key events before the next draw.
            loop {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        let action = map_key_event(key, app);
                        app.handle_action(action);
                    }
                }
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }

            // If project selection changed, reload tasks asynchronously.
            if app.project_index != last_project_index {
                last_project_index = app.project_index;
                let pid = app.projects.get(app.project_index).map(|p| p.id.clone());
                if let Some(pid) = pid {
                    app.spawn_task_query(&pid);
                }
            }

            // Handle refresh action — spawn async query.
            if matches!(app.status_text(), Some("Refreshing...")) {
                app.spawn_project_query();
            }
        }
    }

    Ok(())
}

fn map_key_event(key: KeyEvent, app: &mut App) -> Action {
    // Help overlay intercepts all keys when visible.
    if app.show_help {
        return match key.code {
            KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => Action::ToggleHelp,
            _ => Action::None,
        };
    }

    // Task info overlay intercepts all keys when visible.
    if app.show_task_info {
        return match key.code {
            KeyCode::Char('i') | KeyCode::Esc | KeyCode::Char('q') => Action::TaskInfo,
            _ => Action::None,
        };
    }

    match app.mode {
        Mode::Output => map_output_mode(key, app),
        Mode::Normal => map_normal_mode(key, app),
    }
}

fn map_normal_mode(key: KeyEvent, app: &mut App) -> Action {
    if app.g_pressed {
        app.g_pressed = false;
        if key.code == KeyCode::Char('g') {
            return Action::JumpTop;
        }
    }

    match key.code {
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Quit,

        KeyCode::Char('?') => Action::ToggleHelp,

        KeyCode::Char('h') | KeyCode::Left => Action::NavLeft,
        KeyCode::Char('l') | KeyCode::Right => Action::NavRight,

        KeyCode::Char('j') | KeyCode::Down => Action::MoveDown,
        KeyCode::Char('k') | KeyCode::Up => Action::MoveUp,
        KeyCode::Char('g') => {
            app.g_pressed = true;
            Action::None
        }
        KeyCode::Char('G') => Action::JumpBottom,
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::HalfPageDown,
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::HalfPageUp,

        KeyCode::Enter => Action::Enter,
        KeyCode::Esc => Action::Back,

        KeyCode::Char('r') => Action::Refresh,
        KeyCode::Char('R') => Action::ForceRun,
        KeyCode::Char('C') => Action::RunAll,
        KeyCode::Char('I') => Action::InteractiveRun,
        KeyCode::Char('i') => Action::TaskInfo,
        KeyCode::Char('o') => Action::Reattach,

        _ => Action::None,
    }
}

fn map_output_mode(key: KeyEvent, app: &mut App) -> Action {
    // Interactive mode: forward all keys to child stdin, except Esc to detach.
    if app.stdin_tx.is_some() {
        if key.code == KeyCode::Esc {
            return Action::Quit; // Back to Normal
        }
        if let Some(bytes) = key_to_bytes(key) {
            if let Some(tx) = &app.stdin_tx {
                let _ = tx.send(bytes);
            }
        }
        return Action::None;
    }

    // Non-interactive output: scroll controls.
    // max_scroll is the highest valid start line (last page).
    let total = app.output_lines.len();
    let visible = app.output_visible_height.max(1);
    let max_scroll = total.saturating_sub(visible);

    if app.g_pressed {
        app.g_pressed = false;
        if key.code == KeyCode::Char('g') {
            app.output_follow = false;
            app.output_scroll = 0;
            return Action::None;
        }
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Quit,
        KeyCode::Char('?') => Action::ToggleHelp,

        KeyCode::Char('j') | KeyCode::Down => {
            // Disable follow and scroll down one line, clamping at the last page.
            app.output_follow = false;
            app.output_scroll = app.output_scroll.min(max_scroll).saturating_add(1).min(max_scroll);
            Action::None
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if app.output_follow {
                // Exit follow mode, placing scroll one line above the last page.
                app.output_follow = false;
                app.output_scroll = max_scroll.saturating_sub(1);
            } else {
                app.output_scroll = app.output_scroll.saturating_sub(1);
            }
            Action::None
        }
        KeyCode::Char('g') => {
            app.g_pressed = true;
            Action::None
        }
        KeyCode::Char('G') => {
            // Jump to bottom and re-enable follow mode.
            app.output_follow = true;
            app.output_scroll = max_scroll;
            Action::None
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.output_follow = false;
            app.output_scroll = app.output_scroll.min(max_scroll).saturating_add(visible / 2).min(max_scroll);
            Action::None
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.output_follow = false;
            app.output_scroll = app.output_scroll.min(max_scroll).saturating_sub(visible / 2);
            Action::None
        }
        KeyCode::Char('f') => {
            app.output_follow = !app.output_follow;
            if app.output_follow {
                app.output_scroll = max_scroll;
            }
            Action::None
        }

        _ => Action::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use app::Panel;

    #[test]
    fn app_starts_in_normal_mode() {
        let app = App::new();
        assert!(app.running);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.panel, Panel::Projects);
    }

    #[test]
    fn quit_action_stops_app() {
        let mut app = App::new();
        app.handle_action(Action::Quit);
        assert!(!app.running);
    }

    #[test]
    fn nav_right_does_nothing_without_tasks() {
        let mut app = App::new();
        app.handle_action(Action::NavRight);
        assert_eq!(app.panel, Panel::Projects);
    }

    #[test]
    fn nav_right_switches_to_tasks_when_available() {
        let mut app = App::new();
        app.tasks = vec![moon::Task {
            id: "test".into(),
            command: "cargo".into(),
            args: vec!["test".into()],
            script: String::new(),
            target: "@test:test".into(),
            toolchains: vec!["rust".into()],
            interactive: false,
            task_type: "test".into(),
            deps: vec![],
        }];
        app.handle_action(Action::NavRight);
        assert_eq!(app.panel, Panel::Tasks);
    }

    #[test]
    fn back_from_tasks_goes_to_projects() {
        let mut app = App::new();
        app.tasks = vec![moon::Task {
            id: "test".into(),
            command: "cargo".into(),
            args: vec!["test".into()],
            script: String::new(),
            target: "@test:test".into(),
            toolchains: vec!["rust".into()],
            interactive: false,
            task_type: "test".into(),
            deps: vec![],
        }];
        app.panel = Panel::Tasks;
        app.handle_action(Action::Back);
        assert_eq!(app.panel, Panel::Projects);
    }

    #[test]
    fn move_down_clamps_to_list_length() {
        let mut app = App::new();
        app.projects = vec![
            moon::Project {
                id: "a".into(),
                tags: vec![],
            },
            moon::Project {
                id: "b".into(),
                tags: vec![],
            },
        ];
        app.handle_action(Action::MoveDown);
        assert_eq!(app.project_index, 1);
        app.handle_action(Action::MoveDown);
        assert_eq!(app.project_index, 1); // clamped
    }

    #[test]
    fn toggle_help() {
        let mut app = App::new();
        assert!(!app.show_help);
        app.handle_action(Action::ToggleHelp);
        assert!(app.show_help);
        app.handle_action(Action::ToggleHelp);
        assert!(!app.show_help);
    }
}
