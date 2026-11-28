//! Bounded, streaming subprocess execution with strict wall-clock timeouts.
//!
//! Uses sum types for stream configuration (`StdoutConfig`, `StderrConfig`) and
//! process outcomes (`CommandOutcome`, `ProcessExit`, `StdoutCapture`) so mutually
//! exclusive states and signal/error collisions cannot be represented.

use std::collections::VecDeque;
use std::env;
use std::ffi::OsString;
use std::io;
use std::num::{NonZeroI32, NonZeroU64, NonZeroUsize};
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
use crate::types::{EntryName, ErrorMessage};

pub use crate::sys::PrivilegeDrop;

#[derive(Debug, Clone)]
pub struct CommandOptions {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub timeout: Duration,
    pub env_override: Option<Vec<(OsString, OsString)>>,
    pub privilege_drop: Option<PrivilegeDrop>,
    pub stream_lines_max: NonZeroU64,
}

pub enum StdoutConfig<FOut> {
    StreamOnly {
        on_line: Option<FOut>,
    },
    Capture {
        bytes_max: NonZeroUsize,
        on_line: Option<FOut>,
    },
}

pub enum StderrConfig<FErr> {
    Inherit,
    Piped {
        tail_bytes_max: usize,
        on_line: Option<FErr>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessExit {
    Success,
    NonZero(NonZeroI32),
    Signaled(NonZeroI32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StdoutCapture {
    NotCaptured { lines_count: u64 },
    Complete { text: String, lines_count: u64 },
    Truncated { text: String, lines_count: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    Completed {
        exit: ProcessExit,
        stdout: StdoutCapture,
        stderr_tail: String,
    },
    TimedOut {
        timeout: Duration,
        stderr_tail: String,
    },
    Failed {
        message: ErrorMessage,
    },
}

impl CommandOutcome {
    /// Format a spawn/IO error, timeout, or non-zero/signaled exit status, or `None` on success.
    pub fn failure_detail(&self) -> Option<String> {
        match self {
            Self::Failed { message } => Some(message.as_str().to_string()),
            Self::TimedOut { timeout, .. } => {
                Some(format!("timed out after {}s", timeout.as_secs()))
            }
            Self::Completed {
                exit: ProcessExit::Success,
                ..
            } => None,
            Self::Completed {
                exit: ProcessExit::NonZero(code),
                stderr_tail,
                ..
            } => {
                let trimmed = stderr_tail.trim();
                if trimmed.is_empty() {
                    Some(format!("exit {code}"))
                } else {
                    Some(trimmed.to_string())
                }
            }
            Self::Completed {
                exit: ProcessExit::Signaled(signal),
                stderr_tail,
                ..
            } => {
                let trimmed = stderr_tail.trim();
                if trimmed.is_empty() {
                    Some(format!("killed by signal {signal}"))
                } else {
                    Some(trimmed.to_string())
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum StreamTag {
    Stdout,
    Stderr,
}

/// Locate an executable by bare name using `PATH` (equivalent to `shutil.which`).
pub fn find_executable_in_path(name: &str) -> Option<PathBuf> {
    let bare_name = EntryName::try_from_bytes(name.as_bytes())
        .expect("find_executable_in_path expects a single non-empty bare executable name");

    let path_var = env::var_os("PATH")?;
    for dir in env::split_paths(&path_var).take(PATH_ENTRIES_MAX) {
        let candidate = dir.join(bare_name.as_os_str());
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    metadata.is_file() && (metadata.permissions().mode() & 0o111) != 0
}

fn failed_outcome(raw: impl Into<String>) -> CommandOutcome {
    let message =
        ErrorMessage::from_truncated(raw.into()).expect("command failure message is non-empty");
    CommandOutcome::Failed { message }
}

/// Run a subprocess with strict wall-clock timeout and bounded memory usage.
pub fn run_command_streaming<FOut, FErr>(
    options: &CommandOptions,
    stdout_config: StdoutConfig<FOut>,
    stderr_config: StderrConfig<FErr>,
) -> CommandOutcome
where
    FOut: FnMut(&str),
    FErr: FnMut(&str),
{
    assert!(!options.program.is_empty(), "program must be non-empty");
    assert!(!options.timeout.is_zero(), "timeout must be positive");

    let (capture_stdout_bytes_max, mut on_stdout_line) = match stdout_config {
        StdoutConfig::StreamOnly { on_line } => (None, on_line),
        StdoutConfig::Capture { bytes_max, on_line } => (Some(bytes_max), on_line),
    };
    let (inherit_stderr, capture_stderr_bytes_max, mut on_stderr_line) = match stderr_config {
        StderrConfig::Inherit => (true, 0usize, None),
        StderrConfig::Piped {
            tail_bytes_max,
            on_line,
        } => (false, tail_bytes_max, on_line),
    };

    let mut command = Command::new(&options.program);
    command
        .args(&options.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(if inherit_stderr {
            Stdio::inherit()
        } else {
            Stdio::piped()
        });

    if let Some(env_pairs) = &options.env_override {
        command.env_clear();
        command.envs(env_pairs.iter().map(|(k, v)| (k, v)));
    }
    sys::configure_child_pre_exec(&mut command, options.privilege_drop.as_ref());

    let mut child = match command.spawn() {
        Ok(proc_handle) => proc_handle,
        Err(err) => return failed_outcome(err.to_string()),
    };

    let deadline = Instant::now() + options.timeout;
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    if let Some(pipe) = &stdout_pipe
        && let Err(err) = sys::set_nonblocking(pipe.as_raw_fd())
    {
        let _ = kill_and_reap(&mut child);
        return failed_outcome(format!("failed to set stdout nonblocking: {err}"));
    }
    if let Some(pipe) = &stderr_pipe
        && let Err(err) = sys::set_nonblocking(pipe.as_raw_fd())
    {
        let _ = kill_and_reap(&mut child);
        return failed_outcome(format!("failed to set stderr nonblocking: {err}"));
    }

    let initial_cap = capture_stdout_bytes_max.map_or(0, |cap| cap.get().min(4096));
    let mut stdout_capture = Vec::<u8>::with_capacity(initial_cap);
    let mut stderr_tail = VecDeque::<u8>::with_capacity(capture_stderr_bytes_max);
    let mut stdout_line_buf = Vec::<u8>::new();
    let mut stderr_line_buf = Vec::<u8>::new();
    let mut stdout_lines_count: u64 = 0;
    let mut stderr_lines_count: u64 = 0;
    let mut stdout_truncated = false;
    let mut read_buf = [0u8; PIPE_READ_BYTES];

    enum LoopTermination {
        Eof,
        TimedOut,
        IoError(String),
    }

    let mut loop_outcome = LoopTermination::Eof;
    let mut iterations_count: usize = 0;
    'poll_loop: while stdout_pipe.is_some() || stderr_pipe.is_some() {
        iterations_count = iterations_count.saturating_add(1);
        if iterations_count > SUBPROCESS_READ_ITERATIONS_MAX {
            loop_outcome = LoopTermination::IoError(format!(
                "subprocess exceeded {SUBPROCESS_READ_ITERATIONS_MAX} read iterations"
            ));
            break;
        }

        let now = Instant::now();
        if now >= deadline {
            loop_outcome = LoopTermination::TimedOut;
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
        assert!((1..=2).contains(&pollfds_count));

        let poll_slice = pollfds
            .get_mut(..pollfds_count)
            .expect("pollfds_count in 1..=2");
        let ready_count = match sys::poll_fds(poll_slice, timeout_ms) {
            Ok(count) => count,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => {
                loop_outcome = LoopTermination::IoError(format!("poll failed: {err}"));
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
                loop_outcome =
                    LoopTermination::IoError("invalid pipe descriptor in poll".to_string());
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
                            if let Some(cap) = capture_stdout_bytes_max {
                                let space_left = cap.get().saturating_sub(stdout_capture.len());
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
                            if capture_stderr_bytes_max > 0 {
                                append_bounded_tail(
                                    &mut stderr_tail,
                                    chunk,
                                    capture_stderr_bytes_max,
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
                    loop_outcome = LoopTermination::IoError(format!("pipe read failed: {err}"));
                    break 'poll_loop;
                }
            }
        }
    }

    // Flush any trailing non-newline-terminated line.
    if !stdout_line_buf.is_empty() && stdout_lines_count < options.stream_lines_max.get() {
        stdout_lines_count += 1;
        if let Some(callback) = on_stdout_line.as_mut() {
            let text = String::from_utf8_lossy(&stdout_line_buf);
            callback(&text);
        }
    }
    if !stderr_line_buf.is_empty()
        && stderr_lines_count < options.stream_lines_max.get()
        && let Some(callback) = on_stderr_line.as_mut()
    {
        let text = String::from_utf8_lossy(&stderr_line_buf);
        callback(&text);
    }

    drop(stdout_pipe);
    drop(stderr_pipe);

    assert!(stderr_tail.len() <= capture_stderr_bytes_max);
    let stderr_text = String::from_utf8_lossy(stderr_tail.make_contiguous()).into_owned();

    match loop_outcome {
        LoopTermination::TimedOut => {
            let _ = kill_and_reap(&mut child);
            return CommandOutcome::TimedOut {
                timeout: options.timeout,
                stderr_tail: stderr_text,
            };
        }
        LoopTermination::IoError(msg) => {
            let _ = kill_and_reap(&mut child);
            return failed_outcome(msg);
        }
        LoopTermination::Eof => {}
    }

    let exit_status = match wait_until(&mut child, deadline) {
        Ok(Some(status)) => status,
        Ok(None) => {
            let _ = kill_and_reap(&mut child);
            return CommandOutcome::TimedOut {
                timeout: options.timeout,
                stderr_tail: stderr_text,
            };
        }
        Err(err) => {
            let _ = kill_and_reap(&mut child);
            return failed_outcome(format!("wait failed: {err}"));
        }
    };

    let Some(exit) = status_to_process_exit(exit_status) else {
        return failed_outcome("child exited without exit code or signal");
    };

    let stdout = match capture_stdout_bytes_max {
        None => StdoutCapture::NotCaptured {
            lines_count: stdout_lines_count,
        },
        Some(cap) => {
            assert!(stdout_capture.len() <= cap.get());
            let text = String::from_utf8_lossy(&stdout_capture).into_owned();
            if stdout_truncated {
                StdoutCapture::Truncated {
                    text,
                    lines_count: stdout_lines_count,
                }
            } else {
                StdoutCapture::Complete {
                    text,
                    lines_count: stdout_lines_count,
                }
            }
        }
    };

    CommandOutcome::Completed {
        exit,
        stdout,
        stderr_tail: stderr_text,
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
    lines_max: NonZeroU64,
    mut callback: Option<&mut F>,
) -> u64
where
    F: FnMut(&str),
{
    assert!(
        chunk.len() <= PIPE_READ_BYTES,
        "chunk exceeded PIPE_READ_BYTES"
    );
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
            if lines_count < lines_max.get() {
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
    assert!(lines_count <= lines_max.get());
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
    if let Err(err) = sys::kill_process_group(child.id()) {
        eprintln!("disk-cleanup: warning: failed to kill child process group: {err}");
    }
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

fn status_to_process_exit(status: ExitStatus) -> Option<ProcessExit> {
    if let Some(code) = status.code() {
        return match NonZeroI32::new(code) {
            None => Some(ProcessExit::Success),
            Some(nonzero) => Some(ProcessExit::NonZero(nonzero)),
        };
    }
    if let Some(signal) = status.signal() {
        return NonZeroI32::new(signal).map(ProcessExit::Signaled);
    }
    None
}
