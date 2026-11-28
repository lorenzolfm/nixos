//! Bounded, streaming subprocess execution with strict wall-clock timeouts.

use std::collections::VecDeque;
use std::env;
use std::ffi::OsString;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::constants::{
    PATH_ENTRIES_MAX, PIPE_READ_BYTES, SUBPROCESS_LINE_BYTES_MAX, SUBPROCESS_READ_ITERATIONS_MAX,
    WAIT_ITERATIONS_MAX,
};
use crate::sys;

pub use crate::sys::PrivilegeDrop;

#[derive(Debug, Clone)]
pub struct CommandOptions {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub timeout: Duration,
    pub env_override: Option<Vec<(OsString, OsString)>>,
    pub privilege_drop: Option<PrivilegeDrop>,
    pub stream_lines_max: u64,
    pub capture_stdout_bytes_max: usize,
    pub capture_stderr_bytes_max: usize,
    pub inherit_stderr: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    pub exit_code: i32,
    pub timed_out: bool,
    pub stdout_text: String,
    pub stderr_tail: String,
    pub stdout_lines_count: u64,
    pub stdout_truncated: bool,
    pub error_message: Option<String>,
}

impl CommandResult {
    /// Format a spawn/IO error, timeout, or non-zero exit status, or `None` on success.
    pub fn failure_detail(&self, timeout: Duration) -> Option<String> {
        assert!(!timeout.is_zero(), "timeout must be positive");
        if let Some(err_msg) = &self.error_message {
            assert!(!err_msg.is_empty(), "error_message must be non-empty");
            return Some(err_msg.clone());
        }
        if self.timed_out {
            return Some(format!("timed out after {}s", timeout.as_secs()));
        }
        if self.exit_code != 0 {
            let trimmed = self.stderr_tail.trim();
            let detail = if trimmed.is_empty() {
                format!("exit {}", self.exit_code)
            } else {
                trimmed.to_string()
            };
            return Some(detail);
        }
        assert_eq!(self.exit_code, 0);
        None
    }
}

#[derive(Debug, Clone, Copy)]
enum StreamTag {
    Stdout,
    Stderr,
}

/// Locate an executable by name using `PATH` (equivalent to `shutil.which`).
pub fn find_executable_in_path(name: &str) -> Option<PathBuf> {
    assert!(!name.is_empty(), "executable name must be non-empty");
    assert!(
        !name.contains('/'),
        "find_executable_in_path expects a bare name"
    );

    let path_var = env::var_os("PATH")?;
    for dir in env::split_paths(&path_var).take(PATH_ENTRIES_MAX) {
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    assert!(!path.as_os_str().is_empty(), "path must be non-empty");
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    metadata.is_file() && (metadata.permissions().mode() & 0o111) != 0
}

/// Run a subprocess with strict wall-clock timeout and bounded memory usage.
pub fn run_command_streaming<FOut, FErr>(
    options: &CommandOptions,
    mut on_stdout_line: Option<FOut>,
    mut on_stderr_line: Option<FErr>,
) -> CommandResult
where
    FOut: FnMut(&str),
    FErr: FnMut(&str),
{
    assert!(!options.program.is_empty(), "program must be non-empty");
    assert!(!options.timeout.is_zero(), "timeout must be positive");
    assert!(
        options.stream_lines_max > 0,
        "stream_lines_max must be positive"
    );

    let mut command = Command::new(&options.program);
    command
        .args(&options.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(if options.inherit_stderr {
            Stdio::inherit()
        } else {
            Stdio::piped()
        });

    if let Some(env_pairs) = &options.env_override {
        command.env_clear();
        command.envs(env_pairs.iter().map(|(k, v)| (k, v)));
    }
    if let Some(drop_cfg) = &options.privilege_drop {
        sys::configure_privilege_drop(&mut command, drop_cfg);
    }

    let mut child = match command.spawn() {
        Ok(proc_handle) => proc_handle,
        Err(err) => {
            return CommandResult {
                exit_code: -1,
                timed_out: false,
                stdout_text: String::new(),
                stderr_tail: String::new(),
                stdout_lines_count: 0,
                stdout_truncated: false,
                error_message: Some(err.to_string()),
            };
        }
    };

    let deadline = Instant::now() + options.timeout;
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    if let Some(pipe) = &stdout_pipe
        && let Err(err) = sys::set_nonblocking(pipe.as_raw_fd())
    {
        kill_and_reap(&mut child);
        return CommandResult {
            exit_code: -1,
            timed_out: false,
            stdout_text: String::new(),
            stderr_tail: String::new(),
            stdout_lines_count: 0,
            stdout_truncated: false,
            error_message: Some(format!("failed to set stdout nonblocking: {err}")),
        };
    }
    if let Some(pipe) = &stderr_pipe
        && let Err(err) = sys::set_nonblocking(pipe.as_raw_fd())
    {
        kill_and_reap(&mut child);
        return CommandResult {
            exit_code: -1,
            timed_out: false,
            stdout_text: String::new(),
            stderr_tail: String::new(),
            stdout_lines_count: 0,
            stdout_truncated: false,
            error_message: Some(format!("failed to set stderr nonblocking: {err}")),
        };
    }

    let mut stdout_capture = Vec::<u8>::with_capacity(options.capture_stdout_bytes_max.min(4096));
    let mut stderr_tail = VecDeque::<u8>::with_capacity(options.capture_stderr_bytes_max);
    let mut stdout_line_buf = Vec::<u8>::new();
    let mut stderr_line_buf = Vec::<u8>::new();
    let mut stdout_lines_count: u64 = 0;
    let mut stderr_lines_count: u64 = 0;
    let mut stdout_truncated = false;
    let mut timed_out = false;
    let mut io_error_message: Option<String> = None;
    let mut read_buf = [0u8; PIPE_READ_BYTES];

    let mut iterations_count: usize = 0;
    'poll_loop: while stdout_pipe.is_some() || stderr_pipe.is_some() {
        iterations_count = iterations_count.saturating_add(1);
        if iterations_count > SUBPROCESS_READ_ITERATIONS_MAX {
            io_error_message = Some(format!(
                "subprocess exceeded {SUBPROCESS_READ_ITERATIONS_MAX} read iterations"
            ));
            break;
        }

        let now = Instant::now();
        if now >= deadline {
            timed_out = true;
            break;
        }
        let remaining = deadline.saturating_duration_since(now);
        let timeout_ms = duration_to_poll_ms(remaining);

        let mut pollfds = [
            libc::pollfd {
                fd: -1,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: -1,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let mut stream_tags = [StreamTag::Stdout; 2];
        let mut pollfds_count: usize = 0;

        if let Some(pipe) = &stdout_pipe {
            if let Some(slot) = pollfds.get_mut(pollfds_count) {
                slot.fd = pipe.as_raw_fd();
            }
            if let Some(tag_slot) = stream_tags.get_mut(pollfds_count) {
                *tag_slot = StreamTag::Stdout;
            }
            pollfds_count += 1;
        }
        if let Some(pipe) = &stderr_pipe {
            if let Some(slot) = pollfds.get_mut(pollfds_count) {
                slot.fd = pipe.as_raw_fd();
            }
            if let Some(tag_slot) = stream_tags.get_mut(pollfds_count) {
                *tag_slot = StreamTag::Stderr;
            }
            pollfds_count += 1;
        }
        assert!(pollfds_count >= 1);
        assert!(pollfds_count <= 2);

        let poll_slice = pollfds
            .get_mut(..pollfds_count)
            .expect("pollfds_count in 1..=2");
        let ready_count = match sys::poll_fds(poll_slice, timeout_ms) {
            Ok(count) => count,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => {
                io_error_message = Some(format!("poll failed: {err}"));
                break;
            }
        };
        if ready_count == 0 {
            continue;
        }

        for index in 0..pollfds_count {
            let pfd = match pollfds.get(index) {
                Some(entry) => *entry,
                None => continue,
            };
            if pfd.revents == 0 {
                continue;
            }
            let tag = match stream_tags.get(index) {
                Some(entry) => *entry,
                None => continue,
            };

            if (pfd.revents & libc::POLLNVAL) != 0 {
                io_error_message = Some("invalid pipe descriptor in poll".to_string());
                break 'poll_loop;
            }

            let read_result = sys::read_fd_nonblocking(pfd.fd, &mut read_buf);
            match read_result {
                Ok(0) => match tag {
                    StreamTag::Stdout => stdout_pipe = None,
                    StreamTag::Stderr => stderr_pipe = None,
                },
                Ok(bytes_read) => {
                    let chunk = read_buf
                        .get(..bytes_read)
                        .expect("bytes_read <= buffer len");
                    match tag {
                        StreamTag::Stdout => {
                            if options.capture_stdout_bytes_max > 0 {
                                let space_left = options
                                    .capture_stdout_bytes_max
                                    .saturating_sub(stdout_capture.len());
                                let take_len = chunk.len().min(space_left);
                                if take_len > 0
                                    && let Some(prefix) = chunk.get(..take_len)
                                {
                                    stdout_capture.extend_from_slice(prefix);
                                }
                                if chunk.len() > space_left {
                                    stdout_truncated = true;
                                }
                            }
                            stdout_lines_count = drain_stream_lines(
                                &mut stdout_line_buf,
                                chunk,
                                stdout_lines_count,
                                options.stream_lines_max,
                                on_stdout_line.as_mut(),
                            );
                        }
                        StreamTag::Stderr => {
                            if options.capture_stderr_bytes_max > 0 {
                                append_bounded_tail(
                                    &mut stderr_tail,
                                    chunk,
                                    options.capture_stderr_bytes_max,
                                );
                            }
                            stderr_lines_count = drain_stream_lines(
                                &mut stderr_line_buf,
                                chunk,
                                stderr_lines_count,
                                options.stream_lines_max,
                                on_stderr_line.as_mut(),
                            );
                        }
                    }
                }
                Err(err)
                    if err.kind() == io::ErrorKind::WouldBlock
                        || err.kind() == io::ErrorKind::Interrupted =>
                {
                    continue;
                }
                Err(err) => {
                    io_error_message = Some(format!("pipe read failed: {err}"));
                    break 'poll_loop;
                }
            }
        }
    }

    // Flush any trailing non-newline-terminated line.
    if !stdout_line_buf.is_empty() && stdout_lines_count < options.stream_lines_max {
        stdout_lines_count += 1;
        if let Some(callback) = on_stdout_line.as_mut() {
            let text = String::from_utf8_lossy(&stdout_line_buf);
            callback(&text);
        }
    }
    if !stderr_line_buf.is_empty()
        && stderr_lines_count < options.stream_lines_max
        && let Some(callback) = on_stderr_line.as_mut()
    {
        let text = String::from_utf8_lossy(&stderr_line_buf);
        callback(&text);
    }

    drop(stdout_pipe);
    drop(stderr_pipe);

    let exit_status = if timed_out || io_error_message.is_some() {
        kill_and_reap(&mut child)
    } else {
        match wait_until(&mut child, deadline) {
            Ok(Some(status)) => Some(status),
            Ok(None) => {
                timed_out = true;
                kill_and_reap(&mut child)
            }
            Err(err) => {
                io_error_message = Some(format!("wait failed: {err}"));
                kill_and_reap(&mut child)
            }
        }
    };

    let exit_code = match exit_status {
        Some(status) => status_to_exit_code(status),
        None => -1,
    };

    assert!(stdout_capture.len() <= options.capture_stdout_bytes_max);
    assert!(stderr_tail.len() <= options.capture_stderr_bytes_max);
    if timed_out {
        assert!(io_error_message.is_none());
    }

    let stderr_slice = stderr_tail.make_contiguous();
    CommandResult {
        exit_code,
        timed_out,
        stdout_text: String::from_utf8_lossy(&stdout_capture).into_owned(),
        stderr_tail: String::from_utf8_lossy(stderr_slice).into_owned(),
        stdout_lines_count,
        stdout_truncated,
        error_message: io_error_message,
    }
}

fn append_bounded_tail(tail: &mut VecDeque<u8>, chunk: &[u8], cap_bytes_max: usize) {
    assert!(cap_bytes_max > 0, "cap_bytes_max must be positive");
    assert!(tail.len() <= cap_bytes_max, "tail exceeded cap_bytes_max");

    if chunk.len() >= cap_bytes_max {
        tail.clear();
        let tail_start = chunk.len() - cap_bytes_max;
        let suffix = chunk.get(tail_start..).expect("tail_start within chunk");
        tail.extend(suffix);
    } else {
        let next_len = tail.len().saturating_add(chunk.len());
        if next_len > cap_bytes_max {
            let excess = next_len - cap_bytes_max;
            tail.drain(..excess);
        }
        tail.extend(chunk);
    }

    assert!(tail.len() <= cap_bytes_max, "tail exceeded cap_bytes_max");
}

fn drain_stream_lines<F>(
    line_buffer: &mut Vec<u8>,
    chunk: &[u8],
    mut lines_count: u64,
    lines_max: u64,
    mut callback: Option<&mut F>,
) -> u64
where
    F: FnMut(&str),
{
    assert!(
        chunk.len() <= PIPE_READ_BYTES,
        "chunk exceeded PIPE_READ_BYTES"
    );
    assert!(lines_max > 0, "lines_max must be positive");
    assert!(
        line_buffer.len() <= SUBPROCESS_LINE_BYTES_MAX,
        "line_buffer exceeded SUBPROCESS_LINE_BYTES_MAX"
    );

    let mut start: usize = 0;
    // Bounded by `chunk.len() + 1` iterations.
    for _ in 0..=chunk.len() {
        if start >= chunk.len() {
            break;
        }
        let rest = match chunk.get(start..) {
            Some(slice) => slice,
            None => break,
        };
        if let Some(rel_newline) = rest.iter().position(|&b| b == b'\n') {
            let space_left = SUBPROCESS_LINE_BYTES_MAX.saturating_sub(line_buffer.len());
            let copy_len = rel_newline.min(space_left);
            if copy_len > 0
                && let Some(segment) = rest.get(..copy_len)
            {
                line_buffer.extend_from_slice(segment);
            }
            if lines_count < lines_max {
                lines_count += 1;
                if let Some(cb) = callback.as_deref_mut() {
                    let text = String::from_utf8_lossy(line_buffer);
                    cb(&text);
                }
            }
            line_buffer.clear();
            start = start.saturating_add(rel_newline).saturating_add(1);
        } else {
            let space_left = SUBPROCESS_LINE_BYTES_MAX.saturating_sub(line_buffer.len());
            let copy_len = rest.len().min(space_left);
            if copy_len > 0
                && let Some(segment) = rest.get(..copy_len)
            {
                line_buffer.extend_from_slice(segment);
            }
            break;
        }
    }

    assert!(line_buffer.len() <= SUBPROCESS_LINE_BYTES_MAX);
    assert!(lines_count <= lines_max);
    lines_count
}

fn duration_to_poll_ms(remaining: Duration) -> i32 {
    let millis = remaining.as_millis().min(1000);
    let clamped = millis.max(1);
    let result = i32::try_from(clamped).expect("clamped millis in 1..=1000 fits i32");
    assert!((1..=1000).contains(&result));
    result
}

fn wait_until(child: &mut Child, deadline: Instant) -> io::Result<Option<ExitStatus>> {
    assert!(child.id() > 0, "child PID must be positive");
    // Bounded polling loop so we never block indefinitely if a child ignores EOF.
    const POLL_SLEEP: Duration = Duration::from_millis(10);
    for _ in 0..WAIT_ITERATIONS_MAX {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(Some(status)),
            Ok(None) => {}
            Err(err) => return Err(err),
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        let sleep_for = deadline.saturating_duration_since(now).min(POLL_SLEEP);
        thread::sleep(sleep_for);
    }
    Ok(None)
}

fn kill_and_reap(child: &mut Child) -> Option<ExitStatus> {
    assert!(child.id() > 0, "child PID must be positive");
    if let Err(err) = child.kill()
        && err.kind() != io::ErrorKind::InvalidInput
    {
        eprintln!("disk-cleanup: warning: failed to kill child process: {err}");
    }
    let reap_deadline = Instant::now() + Duration::from_secs(5);
    match wait_until(child, reap_deadline) {
        Ok(status) => status,
        Err(err) => {
            eprintln!("disk-cleanup: warning: failed to reap child process: {err}");
            None
        }
    }
}

fn status_to_exit_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    if let Some(signal) = status.signal() {
        assert!(signal > 0, "termination signal must be positive");
        return -signal;
    }
    -1
}
