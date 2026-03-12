use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// Message sent from task runners to the TUI output view.
#[derive(Debug)]
pub enum OutputMsg {
    /// Append a new line to output (piped non-interactive tasks).
    Append(String),
    /// Raw PTY bytes — fed to a vt100 parser for proper terminal emulation.
    PtyOutput(Vec<u8>),
}

#[derive(Debug, Clone)]
pub struct Project {
    pub id: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Task {
    pub id: String,
    pub command: String,
    pub args: Vec<String>,
    pub script: String,
    pub target: String,
    pub toolchains: Vec<String>,
    pub interactive: bool,
    pub task_type: String,
    pub deps: Vec<String>,
}

impl Task {
    /// Full command string: script if present, otherwise command + args.
    pub fn full_command(&self) -> String {
        if !self.script.is_empty() {
            return self.script.clone();
        }
        if self.args.is_empty() {
            self.command.clone()
        } else {
            format!("{} {}", self.command, self.args.join(" "))
        }
    }
}

// ── JSON shapes from `moon query` ──────────────────────────────────────────

#[derive(Deserialize)]
struct ProjectsResponse {
    projects: Vec<ProjectJson>,
}

#[derive(Deserialize)]
struct ProjectJson {
    id: String,
    config: ProjectConfig,
}

#[derive(Deserialize)]
struct ProjectConfig {
    tags: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct TasksResponse {
    tasks: BTreeMap<String, BTreeMap<String, TaskJson>>,
}

#[derive(Deserialize)]
struct TaskJson {
    id: String,
    command: Option<String>,
    args: Option<Vec<String>>,
    script: Option<String>,
    target: Option<String>,
    toolchains: Option<Vec<String>>,
    options: Option<TaskOptionsJson>,
    deps: Option<Vec<DepJson>>,
    #[serde(rename = "type")]
    task_type: Option<String>,
}

#[derive(Deserialize)]
struct TaskOptionsJson {
    interactive: Option<bool>,
}

#[derive(Deserialize)]
struct DepJson {
    target: Option<String>,
}

/// Extract JSON from moon's stdout, which may contain sync/summary text before
/// the actual JSON object.
fn extract_json(bytes: &[u8]) -> Option<&[u8]> {
    let s = std::str::from_utf8(bytes).ok()?;
    let start = s.find('{')?;
    Some(&bytes[start..])
}

/// Query moon for all workspace projects.
pub async fn query_projects() -> Vec<Project> {
    let output = Command::new("moon")
        .args(["query", "projects"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await;

    let Ok(output) = output else {
        return Vec::new();
    };

    let Some(json) = extract_json(&output.stdout) else {
        return Vec::new();
    };

    let Ok(resp) = serde_json::from_slice::<ProjectsResponse>(json) else {
        return Vec::new();
    };

    resp.projects
        .into_iter()
        .map(|p| Project {
            id: p.id,
            tags: p.config.tags.unwrap_or_default(),
        })
        .collect()
}

/// Query moon for tasks belonging to a specific project.
pub async fn query_tasks(project_id: &str) -> Vec<Task> {
    let output = Command::new("moon")
        .args(["query", "tasks", "--project", project_id])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await;

    let Ok(output) = output else {
        return Vec::new();
    };

    let Some(json) = extract_json(&output.stdout) else {
        return Vec::new();
    };

    let Ok(resp) = serde_json::from_slice::<TasksResponse>(json) else {
        return Vec::new();
    };

    resp.tasks
        .into_values()
        .flat_map(|tasks| {
            tasks.into_values().map(|t| {
                let deps = t
                    .deps
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|d| d.target)
                    .collect();
                Task {
                    id: t.id,
                    command: t.command.unwrap_or_default(),
                    args: t.args.unwrap_or_default(),
                    script: t.script.unwrap_or_default(),
                    target: t.target.unwrap_or_default(),
                    toolchains: t.toolchains.unwrap_or_default(),
                    interactive: t.options.and_then(|o| o.interactive).unwrap_or(false),
                    task_type: t.task_type.unwrap_or_default(),
                    deps,
                }
            })
        })
        .collect()
}

/// Open a PTY pair, returning (master_fd, slave_fd).
fn open_pty() -> Option<(i32, i32)> {
    let mut master: libc::c_int = 0;
    let mut slave: libc::c_int = 0;
    let ret = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ret == 0 {
        Some((master, slave))
    } else {
        None
    }
}

/// Set the PTY master's window size to the given dimensions.
fn set_winsize(master_fd: i32, rows: u16, cols: u16) {
    unsafe {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        libc::ioctl(master_fd, libc::TIOCSWINSZ, &ws);
    }
}

/// Scan a chunk of PTY output for terminal device queries and write
/// appropriate responses back to the master fd (child reads as input).
///
/// Without this, programs that detect a terminal (isatty=true in PTY) and
/// query cursor position via `\x1b[6n` (e.g. crossterm in dx serve) will
/// block forever waiting for a response, producing no further output.
fn respond_to_device_queries(master_fd: i32, data: &[u8]) {
    let s = match std::str::from_utf8(data) {
        Ok(s) => s,
        Err(_) => return,
    };

    let mut rest = s;
    while let Some(pos) = rest.find("\x1b[") {
        rest = &rest[pos + 2..];

        // \x1b[6n — Cursor Position Report → respond \x1b[1;1R
        if rest.starts_with("6n") {
            let resp = b"\x1b[1;1R";
            unsafe {
                libc::write(master_fd, resp.as_ptr() as *const libc::c_void, resp.len());
            }
        }
        // \x1b[5n — Device Status Report → respond \x1b[0n (terminal OK)
        else if rest.starts_with("5n") {
            let resp = b"\x1b[0n";
            unsafe {
                libc::write(master_fd, resp.as_ptr() as *const libc::c_void, resp.len());
            }
        }
        // \x1b[c or \x1b[0c — Device Attributes → respond as VT220
        else if rest.starts_with('c') || rest.starts_with("0c") {
            let resp = b"\x1b[?62;22c";
            unsafe {
                libc::write(master_fd, resp.as_ptr() as *const libc::c_void, resp.len());
            }
        }
    }
}

/// Spawn `moon run <project>:<task>` in a PTY for true interactive use.
/// The child sees a real terminal (isatty = true), so colors and prompts work.
/// Raw PTY bytes are forwarded via `PtyOutput` for processing by a vt100 parser.
pub async fn run_task_interactive(
    project_id: &str,
    task_id: &str,
    rows: u16,
    cols: u16,
    line_tx: tokio::sync::mpsc::UnboundedSender<OutputMsg>,
    mut input_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    child_pid: Arc<AtomicU32>,
) -> Option<i32> {
    let (master_fd, slave_fd) = open_pty()?;
    set_winsize(master_fd, rows, cols);

    let target = format!("{project_id}:{task_id}");

    let mut cmd = Command::new("moon");
    cmd.args(["run", &target, "--color"]);

    unsafe {
        cmd.pre_exec(move || {
            let mut empty_set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut empty_set);
            libc::sigprocmask(libc::SIG_SETMASK, &empty_set, std::ptr::null_mut());
            libc::close(master_fd);
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(slave_fd, libc::TIOCSCTTY, 0i32) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::dup2(slave_fd, libc::STDIN_FILENO);
            libc::dup2(slave_fd, libc::STDOUT_FILENO);
            libc::dup2(slave_fd, libc::STDERR_FILENO);
            if slave_fd > 2 {
                libc::close(slave_fd);
            }
            Ok(())
        });
    }

    let mut child = cmd.spawn().ok()?;
    child_pid.store(child.id().unwrap_or(0), Ordering::SeqCst);

    unsafe {
        libc::close(slave_fd);
    }

    // Read raw bytes from PTY master and forward them for vt100 processing.
    let master_read_fd = master_fd;
    let read_handle = tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 4096];
        loop {
            let n = unsafe {
                libc::read(
                    master_read_fd,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            if n == 0 {
                break;
            }

            let raw = &buf[..n as usize];
            respond_to_device_queries(master_read_fd, raw);

            if line_tx.send(OutputMsg::PtyOutput(raw.to_vec())).is_err() {
                break;
            }
        }
    });

    // Write user input to PTY master.
    let master_write_fd = master_fd;
    tokio::task::spawn_blocking(move || {
        while let Some(bytes) = input_rx.blocking_recv() {
            let written = unsafe {
                libc::write(
                    master_write_fd,
                    bytes.as_ptr() as *const libc::c_void,
                    bytes.len(),
                )
            };
            if written < 0 {
                break;
            }
        }
    });

    let status = child.wait().await.ok()?;
    child_pid.store(0, Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    read_handle.abort();

    unsafe {
        libc::close(master_fd);
    }

    Some(status.code().unwrap_or(-1))
}

/// Spawn `moon run <project>:<task>` and stream output line by line.
/// When `force` is true, passes `--force` to skip Moon's cache.
pub async fn run_task(
    project_id: &str,
    task_id: &str,
    force: bool,
    line_tx: tokio::sync::mpsc::UnboundedSender<OutputMsg>,
    child_pid: Arc<AtomicU32>,
) -> Option<i32> {
    let target = format!("{project_id}:{task_id}");
    let mut args = vec!["run", &target, "--color"];
    if force {
        args.push("--force");
    }
    let mut child = Command::new("moon")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;

    child_pid.store(child.id().unwrap_or(0), Ordering::SeqCst);
    let stdout = child.stdout.take()?;
    let stderr = child.stderr.take()?;

    let tx2 = line_tx.clone();

    // Stream stdout.
    let stdout_handle = tokio::spawn(async move {
        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line_tx.send(OutputMsg::Append(line)).is_err() {
                break;
            }
        }
    });

    // Stream stderr.
    let stderr_handle = tokio::spawn(async move {
        let reader = BufReader::new(stderr);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx2.send(OutputMsg::Append(line)).is_err() {
                break;
            }
        }
    });

    // Wait for child exit FIRST. Do NOT wait for pipe readers first —
    // if moon spawns subprocesses that inherit our pipes, the readers
    // would hang forever waiting for EOF that never comes.
    let status = child.wait().await.ok()?;
    child_pid.store(0, Ordering::SeqCst);

    // Give readers a brief window to drain any remaining output.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Abort readers — they may hang indefinitely if grandchild
    // processes still hold the pipe file descriptors open.
    stdout_handle.abort();
    stderr_handle.abort();

    Some(status.code().unwrap_or(-1))
}
