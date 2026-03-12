use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::{Duration, Instant};

use crate::moon::{self, OutputMsg, Project, Task};

const MAX_OUTPUT_LINES: usize = 10_000;

/// Convert vt100 color to an ANSI SGR parameter string for foreground.
fn vt_fg(color: &vt100::Color) -> Option<String> {
    match color {
        vt100::Color::Default => Some("39".into()),
        vt100::Color::Idx(n) => {
            if *n < 8 {
                Some(format!("{}", 30 + n))
            } else if *n < 16 {
                Some(format!("{}", 90 + n - 8))
            } else {
                Some(format!("38;5;{n}"))
            }
        }
        vt100::Color::Rgb(r, g, b) => Some(format!("38;2;{r};{g};{b}")),
    }
}

/// Convert vt100 color to an ANSI SGR parameter string for background.
fn vt_bg(color: &vt100::Color) -> Option<String> {
    match color {
        vt100::Color::Default => Some("49".into()),
        vt100::Color::Idx(n) => {
            if *n < 8 {
                Some(format!("{}", 40 + n))
            } else if *n < 16 {
                Some(format!("{}", 100 + n - 8))
            } else {
                Some(format!("48;5;{n}"))
            }
        }
        vt100::Color::Rgb(r, g, b) => Some(format!("48;2;{r};{g};{b}")),
    }
}

/// Extract screen lines from vt100 by iterating cells directly.
///
/// This produces strings with ANSI SGR codes for coloring, but WITHOUT
/// cursor-positioning sequences. Each cell is emitted in order, preserving
/// exact column alignment. Trailing whitespace is trimmed per row.
fn extract_screen_lines(screen: &vt100::Screen) -> Vec<String> {
    let (rows, cols) = screen.size();
    let mut lines = Vec::with_capacity(rows as usize);

    for row in 0..rows {
        let mut line = String::new();
        let mut prev_fg = vt100::Color::Default;
        let mut prev_bg = vt100::Color::Default;
        let mut prev_bold = false;
        let mut prev_italic = false;
        let mut prev_underline = false;
        let mut prev_inverse = false;

        let mut col = 0u16;
        while col < cols {
            let Some(cell) = screen.cell(row, col) else {
                col += 1;
                continue;
            };

            if cell.is_wide_continuation() {
                col += 1;
                continue;
            }

            // Emit SGR codes for style changes.
            let fg = cell.fgcolor();
            let bg = cell.bgcolor();
            let bold = cell.bold();
            let italic = cell.italic();
            let underline = cell.underline();
            let inverse = cell.inverse();

            let style_changed = fg != prev_fg
                || bg != prev_bg
                || bold != prev_bold
                || italic != prev_italic
                || underline != prev_underline
                || inverse != prev_inverse;

            if style_changed {
                let mut params = Vec::new();
                // Reset first, then set attributes.
                params.push("0".into());
                if bold {
                    params.push("1".into());
                }
                if italic {
                    params.push("3".into());
                }
                if underline {
                    params.push("4".into());
                }
                if inverse {
                    params.push("7".into());
                }
                if let Some(s) = vt_fg(&fg) {
                    if fg != vt100::Color::Default {
                        params.push(s);
                    }
                }
                if let Some(s) = vt_bg(&bg) {
                    if bg != vt100::Color::Default {
                        params.push(s);
                    }
                }
                let _ = write!(line, "\x1b[{}m", params.join(";"));
                prev_fg = fg;
                prev_bg = bg;
                prev_bold = bold;
                prev_italic = italic;
                prev_underline = underline;
                prev_inverse = inverse;
            }

            let contents = cell.contents();
            if contents.is_empty() {
                line.push(' ');
            } else {
                line.push_str(&contents);
            }

            col += if cell.is_wide() { 2 } else { 1 };
        }

        // Reset at end of line.
        if prev_fg != vt100::Color::Default
            || prev_bg != vt100::Color::Default
            || prev_bold
            || prev_italic
            || prev_underline
            || prev_inverse
        {
            line.push_str("\x1b[0m");
        }

        // Trim trailing whitespace (plain spaces only, preserve ANSI codes).
        let trimmed = line.trim_end();
        lines.push(trimmed.to_string());
    }

    // Trim trailing empty rows.
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }

    lines
}

/// Strip ANSI escape codes, returning plain text for comparison.
fn strip_ansi_plain(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut rest = s;
    while !rest.is_empty() {
        if let Some(pos) = rest.find('\x1b') {
            result.push_str(&rest[..pos]);
            rest = &rest[pos..];
            if rest.starts_with("\x1b[") {
                if let Some(end) = rest[2..].find(|c: char| matches!(c as u8, 0x40..=0x7E)) {
                    rest = &rest[2 + end + 1..];
                } else {
                    rest = &rest[1..];
                }
            } else if rest.len() >= 2 {
                rest = &rest[2..];
            } else {
                break;
            }
        } else {
            result.push_str(rest);
            break;
        }
    }
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Projects,
    Tasks,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Output,
}

#[derive(Debug, Clone)]
pub enum Action {
    Quit,
    Refresh,
    NavLeft,
    NavRight,
    MoveDown,
    MoveUp,
    JumpTop,
    JumpBottom,
    HalfPageDown,
    HalfPageUp,
    Enter,
    Back,
    ToggleHelp,
    RunAll,
    ForceRun,
    InteractiveRun,
    TaskInfo,
    Reattach,
    None,
}

/// Saved state for a task running in the background (detached but not killed).
struct TaskState {
    output_lines: Vec<String>,
    output_scroll: usize,
    output_follow: bool,
    output_finished: bool,
    output_exit_code: Option<i32>,
    line_rx: Option<tokio::sync::mpsc::UnboundedReceiver<OutputMsg>>,
    exit_code_rx: Option<tokio::sync::oneshot::Receiver<Option<i32>>>,
    stdin_tx: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    vt: Option<vt100::Parser>,
    pty_scrollback_len: usize,
    was_alternate_screen: bool,
    child_pid: Arc<AtomicU32>,
}

pub struct App {
    pub running: bool,
    pub panel: Panel,
    pub mode: Mode,
    pub projects: Vec<Project>,
    pub tasks: Vec<Task>,
    pub project_index: usize,
    pub task_index: usize,
    pub status_message: Option<(String, Instant)>,
    pub g_pressed: bool,
    pub show_help: bool,
    pub show_task_info: bool,
    pub loading_tasks: bool,

    // Output view.
    pub output_lines: Vec<String>,
    pub output_scroll: usize,
    pub output_follow: bool,
    pub output_target: String,
    pub output_finished: bool,
    pub output_exit_code: Option<i32>,
    /// Height (in rows) of the visible output pane, updated on every render.
    pub output_visible_height: usize,

    // Channels for streaming task output.
    pub line_rx: Option<tokio::sync::mpsc::UnboundedReceiver<OutputMsg>>,

    // Exit code from the running task.
    pub exit_code_rx: Option<tokio::sync::oneshot::Receiver<Option<i32>>>,

    // Sender for forwarding input to an interactive task's stdin.
    pub stdin_tx: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,

    // vt100 parser for interactive (PTY) task output.
    pub vt: Option<vt100::Parser>,

    // Scrollback tracking: lines at the start of output_lines that have scrolled
    // off the vt100 visible screen. We keep them so earlier output (e.g. tailwind
    // build lines) isn't lost when a TUI program takes over the screen.
    pty_scrollback_len: usize,
    was_alternate_screen: bool,

    // Async task loading.
    pub task_rx: Option<tokio::sync::oneshot::Receiver<Vec<Task>>>,
    pub project_rx: Option<tokio::sync::oneshot::Receiver<Vec<Project>>>,

    // PID of the currently running child process (0 = none).
    pub child_pid: Arc<AtomicU32>,

    // State for tasks that were detached while still running, keyed by "project:task".
    background_tasks: HashMap<String, TaskState>,
}

impl App {
    pub fn new() -> Self {
        Self {
            running: true,
            panel: Panel::Projects,
            mode: Mode::Normal,
            projects: Vec::new(),
            tasks: Vec::new(),
            project_index: 0,
            task_index: 0,
            status_message: None,
            g_pressed: false,
            show_help: false,
            show_task_info: false,
            loading_tasks: false,
            output_lines: Vec::new(),
            output_scroll: 0,
            output_follow: true,
            output_target: String::new(),
            output_finished: false,
            output_exit_code: None,
            output_visible_height: 0,
            line_rx: None,
            exit_code_rx: None,
            stdin_tx: None,
            vt: None,
            pty_scrollback_len: 0,
            was_alternate_screen: false,
            task_rx: None,
            project_rx: None,
            child_pid: Arc::new(AtomicU32::new(0)),
            background_tasks: HashMap::new(),
        }
    }

    /// Spawn an async task query in the background.
    pub fn spawn_task_query(&mut self, project_id: &str) {
        let pid = project_id.to_string();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.task_rx = Some(rx);
        self.loading_tasks = true;
        tokio::spawn(async move {
            let tasks = moon::query_tasks(&pid).await;
            let _ = tx.send(tasks);
        });
    }

    /// Spawn an async project query in the background.
    pub fn spawn_project_query(&mut self) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.project_rx = Some(rx);
        tokio::spawn(async move {
            let projects = moon::query_projects().await;
            let _ = tx.send(projects);
        });
    }

    /// Poll for completed async queries. Returns true if projects were updated.
    pub fn poll_async(&mut self) -> bool {
        let mut projects_updated = false;

        // Check exit code from running task.
        let exit_code = self.exit_code_rx.as_mut().and_then(|rx| rx.try_recv().ok());
        if let Some(code) = exit_code {
            // Drain any remaining output before dropping the receiver.
            self.poll_output();
            self.output_exit_code = code;
            self.output_finished = true;
            self.line_rx = None;
            self.exit_code_rx = None;
            self.stdin_tx = None;
            let exit = code.unwrap_or(-1);
            let status = if exit == 0 { "done" } else { "FAILED" };
            self.set_status(format!("{} ({status}, exit {exit})", self.output_target));
        }

        // Check task query.
        if let Some(rx) = &mut self.task_rx {
            if let Ok(tasks) = rx.try_recv() {
                self.tasks = tasks;
                self.task_index = 0;
                self.loading_tasks = false;
                self.task_rx = None;
            }
        }

        // Check project query.
        if let Some(rx) = &mut self.project_rx {
            if let Ok(projects) = rx.try_recv() {
                self.projects = projects;
                self.project_rx = None;
                projects_updated = true;
            }
        }

        projects_updated
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_message = Some((msg.into(), Instant::now()));
    }

    pub fn status_text(&self) -> Option<&str> {
        self.status_message.as_ref().and_then(|(msg, when)| {
            if when.elapsed() < Duration::from_secs(5) {
                Some(msg.as_str())
            } else {
                Option::None
            }
        })
    }

    pub fn selected_project(&self) -> Option<&Project> {
        self.projects.get(self.project_index)
    }

    pub fn selected_task(&self) -> Option<&Task> {
        self.tasks.get(self.task_index)
    }

    pub fn list_len(&self) -> usize {
        match self.panel {
            Panel::Projects => self.projects.len(),
            Panel::Tasks => self.tasks.len(),
        }
    }

    /// Drain any pending output lines from the channel.
    ///
    /// IMPORTANT: This must NOT be called in Normal mode on every iteration.
    /// Call it only in Output mode or when transitioning (reattach, task exit).
    pub fn poll_output(&mut self) {
        let Some(rx) = &mut self.line_rx else {
            return;
        };
        let mut got_pty = false;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                OutputMsg::Append(line) => {
                    self.output_lines.push(line);
                }
                OutputMsg::PtyOutput(bytes) => {
                    if let Some(vt) = &mut self.vt {
                        vt.process(&bytes);
                        got_pty = true;
                    }
                }
            }
        }
        // After processing all PTY bytes, extract the current screen content
        // by iterating cells directly (preserves exact spacing and colors).
        // Lines that scroll off the visible screen are preserved as scrollback
        // at the start of output_lines (tracked by pty_scrollback_len).
        if got_pty {
            if let Some(vt) = &self.vt {
                let screen = vt.screen();
                let new_screen = extract_screen_lines(screen);
                let is_alt = screen.alternate_screen();
                let (_, cols) = screen.size();

                if is_alt && !self.was_alternate_screen {
                    // Entering alternate screen (TUI program took over):
                    // freeze current output as scrollback.
                    let non_empty_end = self
                        .output_lines
                        .iter()
                        .rposition(|l| !strip_ansi_plain(l).trim().is_empty())
                        .map_or(0, |i| i + 1);
                    self.pty_scrollback_len = non_empty_end;
                } else if !is_alt && self.was_alternate_screen {
                    // Left alternate screen: discard scrollback.
                    self.pty_scrollback_len = 0;
                } else if !is_alt {
                    // Normal mode: detect lines scrolling off the top.
                    let old_screen = &self.output_lines[self.pty_scrollback_len..];
                    if !old_screen.is_empty() && !new_screen.is_empty() {
                        let old_top = strip_ansi_plain(&old_screen[0]);
                        let plain_rows: Vec<String> = screen.rows(0, cols).collect();
                        let new_top = plain_rows.first().map(|s| s.as_str()).unwrap_or("");
                        if old_top.trim() != new_top.trim() && !old_top.trim().is_empty() {
                            // First line changed — figure out how many lines scrolled off.
                            let scroll_amount = plain_rows
                                .iter()
                                .position(|r| r.trim() == old_top.trim())
                                .unwrap_or_else(|| {
                                    // Old top not found at all — check if any old line remains.
                                    let any_overlap = old_screen.iter().any(|l| {
                                        let p = strip_ansi_plain(l);
                                        !p.trim().is_empty()
                                            && plain_rows.iter().any(|r| r.trim() == p.trim())
                                    });
                                    if any_overlap {
                                        1 // conservative fallback
                                    } else {
                                        old_screen.len() // full clear: save everything
                                    }
                                });
                            self.pty_scrollback_len += scroll_amount;
                        }
                    }
                }

                self.was_alternate_screen = is_alt;

                // Replace screen portion, keep scrollback intact.
                self.output_lines.truncate(self.pty_scrollback_len);
                self.output_lines.extend(new_screen);
            }
        }
        // Cap output to prevent unbounded memory growth.
        if self.output_lines.len() > MAX_OUTPUT_LINES {
            let keep_from = self.output_lines.len() - MAX_OUTPUT_LINES;
            self.output_lines = self.output_lines.split_off(keep_from);
            self.output_scroll = self.output_scroll.saturating_sub(keep_from);
        }
        if self.output_follow {
            self.output_scroll = self.output_lines.len().saturating_sub(1);
        }
    }

    pub fn is_running_task(&self) -> bool {
        self.line_rx.is_some() && !self.output_finished
    }

    pub fn handle_action(&mut self, action: Action) {
        match action {
            Action::Quit => {
                if self.mode == Mode::Output {
                    self.mode = Mode::Normal;
                } else {
                    self.running = false;
                }
            }
            Action::Back => {
                if self.mode == Mode::Output {
                    self.mode = Mode::Normal;
                } else if self.panel == Panel::Tasks {
                    self.panel = Panel::Projects;
                }
            }
            Action::Refresh => {
                self.set_status("Refreshing...");
            }
            Action::NavLeft => {
                if self.panel == Panel::Tasks {
                    self.panel = Panel::Projects;
                }
            }
            Action::NavRight => {
                if self.panel == Panel::Projects && !self.tasks.is_empty() {
                    self.panel = Panel::Tasks;
                }
            }
            Action::MoveDown => {
                let len = self.list_len();
                match self.panel {
                    Panel::Projects => {
                        if len > 0 && self.project_index < len - 1 {
                            self.project_index += 1;
                        }
                    }
                    Panel::Tasks => {
                        if len > 0 && self.task_index < len - 1 {
                            self.task_index += 1;
                        }
                    }
                }
            }
            Action::MoveUp => match self.panel {
                Panel::Projects => {
                    self.project_index = self.project_index.saturating_sub(1);
                }
                Panel::Tasks => {
                    self.task_index = self.task_index.saturating_sub(1);
                }
            },
            Action::JumpTop => match self.panel {
                Panel::Projects => self.project_index = 0,
                Panel::Tasks => self.task_index = 0,
            },
            Action::JumpBottom => {
                let len = self.list_len();
                if len > 0 {
                    match self.panel {
                        Panel::Projects => self.project_index = len - 1,
                        Panel::Tasks => self.task_index = len - 1,
                    }
                }
            }
            Action::HalfPageDown => {
                let len = self.list_len();
                if len > 0 {
                    match self.panel {
                        Panel::Projects => {
                            self.project_index = (self.project_index + 10).min(len - 1)
                        }
                        Panel::Tasks => self.task_index = (self.task_index + 10).min(len - 1),
                    }
                }
            }
            Action::HalfPageUp => match self.panel {
                Panel::Projects => {
                    self.project_index = self.project_index.saturating_sub(10);
                }
                Panel::Tasks => {
                    self.task_index = self.task_index.saturating_sub(10);
                }
            },
            Action::Enter => match self.panel {
                Panel::Projects => {
                    if !self.tasks.is_empty() {
                        self.panel = Panel::Tasks;
                        self.task_index = 0;
                    }
                }
                Panel::Tasks => {
                    self.run_selected_task();
                }
            },
            Action::RunAll => {
                if let Some(project) = self.selected_project() {
                    let project_id = project.id.clone();
                    self.start_task_run(&project_id, "ci", false);
                }
            }
            Action::ForceRun => match self.panel {
                Panel::Tasks => {
                    self.force_run_selected_task();
                }
                Panel::Projects => {
                    if let Some(project) = self.selected_project() {
                        let project_id = project.id.clone();
                        self.start_task_run(&project_id, "ci", true);
                    }
                }
            },
            Action::InteractiveRun => {
                if self.panel == Panel::Tasks {
                    let project = self.selected_project().map(|p| p.id.clone());
                    let task = self.tasks.get(self.task_index).map(|t| t.id.clone());
                    if let (Some(p), Some(t)) = (project, task) {
                        self.start_interactive_run(&p, &t);
                    }
                }
            }
            Action::TaskInfo => {
                if self.panel == Panel::Tasks && !self.tasks.is_empty() {
                    self.show_task_info = !self.show_task_info;
                }
            }
            Action::Reattach => {
                if self.is_running_task() || !self.output_lines.is_empty() {
                    // Drain any buffered output before showing.
                    self.poll_output();
                    self.mode = Mode::Output;
                    self.output_follow = true;
                    self.output_scroll = self.output_lines.len().saturating_sub(1);
                }
            }
            Action::ToggleHelp => {
                self.show_help = !self.show_help;
            }
            Action::None => {}
        }
    }

    fn run_selected_task(&mut self) {
        let project = match self.selected_project() {
            Some(p) => p.id.clone(),
            None => return,
        };
        let task = match self.tasks.get(self.task_index) {
            Some(t) => t,
            None => return,
        };
        let task_id = task.id.clone();
        // Interactive tasks need a PTY so moon gets a real terminal and
        // properly runs interactive sub-tasks (e.g. dx serve).
        if task.interactive {
            self.start_interactive_run(&project, &task_id);
        } else {
            self.start_task_run(&project, &task_id, false);
        }
    }

    fn force_run_selected_task(&mut self) {
        let project = match self.selected_project() {
            Some(p) => p.id.clone(),
            None => return,
        };
        let task = match self.tasks.get(self.task_index) {
            Some(t) => t,
            None => return,
        };
        let task_id = task.id.clone();
        if task.interactive {
            self.start_interactive_run(&project, &task_id);
        } else {
            self.start_task_run(&project, &task_id, true);
        }
    }

    /// Save the current foreground task's state into `background_tasks` so it
    /// keeps running while we switch to another task.  Does nothing if there
    /// is no current task or it has already finished.
    fn save_current_task_state(&mut self) {
        if self.output_target.is_empty() || self.output_finished {
            return;
        }
        let state = TaskState {
            output_lines: std::mem::take(&mut self.output_lines),
            output_scroll: self.output_scroll,
            output_follow: self.output_follow,
            output_finished: self.output_finished,
            output_exit_code: self.output_exit_code,
            line_rx: self.line_rx.take(),
            exit_code_rx: self.exit_code_rx.take(),
            stdin_tx: self.stdin_tx.take(),
            vt: self.vt.take(),
            pty_scrollback_len: self.pty_scrollback_len,
            was_alternate_screen: self.was_alternate_screen,
            child_pid: Arc::clone(&self.child_pid),
        };
        self.background_tasks.insert(self.output_target.clone(), state);
    }

    /// Returns the PIDs of all live child processes (foreground + background).
    pub fn all_child_pids(&self) -> Vec<u32> {
        let mut pids = Vec::new();
        let pid = self.child_pid.load(std::sync::atomic::Ordering::SeqCst);
        if pid != 0 {
            pids.push(pid);
        }
        for state in self.background_tasks.values() {
            let pid = state.child_pid.load(std::sync::atomic::Ordering::SeqCst);
            if pid != 0 {
                pids.push(pid);
            }
        }
        pids
    }

    /// Check if the given target is already running (foreground or background)
    /// and re-attach if so.
    fn try_reattach(&mut self, project_id: &str, task_id: &str) -> bool {
        let target = format!("{project_id}:{task_id}");

        // Already the foreground task and still running.
        if self.output_target == target && !self.output_finished {
            self.poll_output();
            self.mode = Mode::Output;
            self.output_follow = true;
            self.output_scroll = self.output_lines.len().saturating_sub(1);
            self.set_status(format!("Re-attached to {target}"));
            return true;
        }

        // Check whether the task was sent to the background while still running.
        if self.background_tasks.get(&target).map_or(false, |s| !s.output_finished) {
            // Push the current foreground task (if any) into the background.
            self.save_current_task_state();

            // Restore the background task as the new foreground.
            let state = self.background_tasks.remove(&target).unwrap();
            self.output_target = target.clone();
            self.output_lines = state.output_lines;
            self.output_scroll = state.output_scroll;
            self.output_follow = state.output_follow;
            self.output_finished = state.output_finished;
            self.output_exit_code = state.output_exit_code;
            self.line_rx = state.line_rx;
            self.exit_code_rx = state.exit_code_rx;
            self.stdin_tx = state.stdin_tx;
            self.vt = state.vt;
            self.pty_scrollback_len = state.pty_scrollback_len;
            self.was_alternate_screen = state.was_alternate_screen;
            self.child_pid = state.child_pid;

            self.poll_output();
            self.mode = Mode::Output;
            self.output_follow = true;
            self.output_scroll = self.output_lines.len().saturating_sub(1);
            self.set_status(format!("Re-attached to {target}"));
            return true;
        }

        false
    }

    fn start_task_run(&mut self, project_id: &str, task_id: &str, force: bool) {
        if self.try_reattach(project_id, task_id) {
            return;
        }

        // Save the current running task (if any) so it keeps running in the background.
        self.save_current_task_state();

        let target = format!("{project_id}:{task_id}");
        self.output_lines.clear();
        self.output_scroll = 0;
        self.output_follow = true;
        self.output_target = target;
        self.output_finished = false;
        self.output_exit_code = None;
        self.stdin_tx = None;
        self.vt = None;
        self.pty_scrollback_len = 0;
        self.was_alternate_screen = false;
        self.mode = Mode::Output;
        // Give this task its own PID slot so it doesn't race with background tasks.
        self.child_pid = Arc::new(AtomicU32::new(0));

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.line_rx = Some(rx);

        let (code_tx, code_rx) = tokio::sync::oneshot::channel();
        self.exit_code_rx = Some(code_rx);

        let pid = project_id.to_string();
        let tid = task_id.to_string();
        let cpid = Arc::clone(&self.child_pid);

        tokio::spawn(async move {
            let code = moon::run_task(&pid, &tid, force, tx, cpid).await;
            let _ = code_tx.send(code);
        });

        let label = if force { "Force running" } else { "Running" };
        self.set_status(format!("{label} {}...", self.output_target));
    }

    fn start_interactive_run(&mut self, project_id: &str, task_id: &str) {
        if self.try_reattach(project_id, task_id) {
            return;
        }

        // Save the current running task (if any) so it keeps running in the background.
        self.save_current_task_state();

        let target = format!("{project_id}:{task_id}");
        self.output_lines.clear();
        self.output_scroll = 0;
        self.output_follow = true;
        self.output_target = target;
        self.output_finished = false;
        self.output_exit_code = None;
        self.pty_scrollback_len = 0;
        self.was_alternate_screen = false;
        self.mode = Mode::Output;
        // Give this task its own PID slot so it doesn't race with background tasks.
        self.child_pid = Arc::new(AtomicU32::new(0));

        // Size PTY and vt100 parser to match the output pane (minus borders
        // and title/status bars) so the child program renders for the correct width.
        let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let pane_cols = term_cols.saturating_sub(2); // left + right border
        let pane_rows = term_rows.saturating_sub(4); // title bar + status bar + top/bottom border
        self.vt = Some(vt100::Parser::new(pane_rows, pane_cols, 1000));

        let (line_tx, line_rx) = tokio::sync::mpsc::unbounded_channel::<OutputMsg>();
        self.line_rx = Some(line_rx);

        let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel();
        self.stdin_tx = Some(input_tx);

        let (code_tx, code_rx) = tokio::sync::oneshot::channel();
        self.exit_code_rx = Some(code_rx);

        let pid = project_id.to_string();
        let tid = task_id.to_string();
        let cpid = Arc::clone(&self.child_pid);

        tokio::spawn(async move {
            let code = moon::run_task_interactive(
                &pid, &tid, pane_rows, pane_cols, line_tx, input_rx, cpid,
            )
            .await;
            let _ = code_tx.send(code);
        });

        self.set_status(format!("Interactive: {}...", self.output_target));
    }
}
