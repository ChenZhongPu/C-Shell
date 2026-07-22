//! Child processes with a deadline and timeout-path tree cleanup.
//!
//! Compilers, capability probes, generated user programs and the optional
//! `clang-format` presentation process go through this module. Two failure
//! modes motivate the ceremony:
//!
//! 1. A hung process (compiler bug, infinite loop) must not freeze the REPL:
//!    every wait has a deadline.
//! 2. Killing only the direct child is not enough. A program that `fork()`s
//!    leaves descendants that keep running *and* keep the output pipe open,
//!    which would block the reader threads forever. On Unix the child gets
//!    its own process group and the whole group is killed; on Windows
//!    `taskkill /T` walks the tree. A process that deliberately escapes
//!    (`setsid`, re-parenting) can still hold the pipe, so readers drain
//!    into shared buffers and are given a grace period rather than joined
//!    unconditionally — worst case costs two abandoned reader threads, never
//!    a hang.
//!
//! Taped stdin is request-driven rather than copied eagerly: generated scanf
//! wrappers signal over stderr, this module supplies either the next retained
//! line or one fresh parent-stdin line, and historical requests can never fall
//! through to the real terminal.

use std::io::{BufRead, Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use wait_timeout::ChildExt;

/// Per stream. Readers continue draining after this so a noisy child cannot
/// deadlock, but excess bytes are discarded rather than exhausting our heap.
pub const MAX_CAPTURE_BYTES: usize = 8 * 1024 * 1024;

pub type ChunkSink = Arc<dyn Fn(&[u8]) + Send + Sync>;

pub struct Observers {
    pub stdout: ChunkSink,
    pub stderr: ChunkSink,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StdinEvent {
    pub bytes: Vec<u8>,
    pub eof: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StdinRequest {
    Replay,
    Current,
}

pub struct Captured {
    /// `None` when the deadline expired and the process tree was killed.
    pub status: Option<ExitStatus>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    /// Fresh lines supplied to wrapped stdin calls in the newest input.
    pub stdin_recorded: Vec<StdinEvent>,
    /// Historical request count differed from the retained tape.
    pub stdin_diverged: bool,
}

#[derive(Default)]
struct CaptureBuffer {
    bytes: Vec<u8>,
    truncated: bool,
}

enum ChildInput {
    Null,
    Inherit,
    Bytes(Vec<u8>),
    Tape {
        requests: mpsc::Receiver<StdinRequest>,
        replay: Vec<StdinEvent>,
        allow_current: bool,
    },
}

/// Run `cmd` to completion or `timeout`, capturing both output streams.
///
/// `interactive_stdin` inherits our stdin (the user's program may `scanf`);
/// otherwise the child gets a closed stdin, so a compiler that decides to
/// prompt cannot stall on it.
pub fn run_captured(
    cmd: &mut Command,
    timeout: Duration,
    interactive_stdin: bool,
) -> std::io::Result<Captured> {
    let input = if interactive_stdin {
        ChildInput::Inherit
    } else {
        ChildInput::Null
    };
    run_inner(cmd, timeout, input, None)
}

/// As `run_captured`, while also forwarding raw chunks to observers. Capture
/// remains enabled for protocol validation and post-run diagnostics.
pub fn run_observed(
    cmd: &mut Command,
    timeout: Duration,
    interactive_stdin: bool,
    observers: Observers,
) -> std::io::Result<Captured> {
    let input = if interactive_stdin {
        ChildInput::Inherit
    } else {
        ChildInput::Null
    };
    run_inner(cmd, timeout, input, Some(observers))
}

/// Run with request-driven stdin. Generated wrappers emit `StdinRequest`s
/// through the output observer; replay requests consume retained lines while
/// current requests synchronously read and record one fresh line. The child
/// never inherits the real terminal, so historical code cannot prompt again.
pub fn run_observed_with_stdin_tape(
    cmd: &mut Command,
    timeout: Duration,
    replay: &[StdinEvent],
    allow_current: bool,
    requests: mpsc::Receiver<StdinRequest>,
    observers: Observers,
) -> std::io::Result<Captured> {
    run_inner(
        cmd,
        timeout,
        ChildInput::Tape {
            requests,
            replay: replay.to_vec(),
            allow_current,
        },
        Some(observers),
    )
}

/// Run a child with a byte buffer on stdin. Used by presentation filters such
/// as clang-format; writing happens concurrently with output draining so a
/// large input/output pair cannot deadlock on full pipes.
pub fn run_with_input(
    cmd: &mut Command,
    timeout: Duration,
    input: &[u8],
) -> std::io::Result<Captured> {
    run_inner(cmd, timeout, ChildInput::Bytes(input.to_vec()), None)
}

fn run_inner(
    cmd: &mut Command,
    timeout: Duration,
    input: ChildInput,
    observers: Option<Observers>,
) -> std::io::Result<Captured> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    match &input {
        ChildInput::Null => cmd.stdin(Stdio::null()),
        ChildInput::Inherit => cmd.stdin(Stdio::inherit()),
        ChildInput::Bytes(_) | ChildInput::Tape { .. } => cmd.stdin(Stdio::piped()),
    };

    #[cfg(unix)]
    let hand_over_tty = unix::prepare(cmd, matches!(&input, ChildInput::Inherit));
    let mut child = cmd.spawn()?;
    #[cfg(unix)]
    if hand_over_tty {
        unix::give_terminal_to(child.id());
    }

    let out_observer = observers.as_ref().map(|o| Arc::clone(&o.stdout));
    let err_observer = observers.as_ref().map(|o| Arc::clone(&o.stderr));
    let (out_buf, out_thread) = drain(child.stdout.take().expect("stdout piped"), out_observer);
    let (err_buf, err_thread) = drain(child.stderr.take().expect("stderr piped"), err_observer);
    let child_stdin = child.stdin.take();
    let (status, stdin_recorded, stdin_diverged) = match input {
        ChildInput::Tape {
            requests,
            replay,
            allow_current,
        } => wait_with_stdin_tape(
            &mut child,
            child_stdin.expect("taped stdin piped"),
            timeout,
            requests,
            &replay,
            allow_current,
        )?,
        ChildInput::Bytes(bytes) => {
            let _input_thread = child_stdin.map(|mut stdin| {
                std::thread::spawn(move || {
                    let _ = stdin.write_all(&bytes);
                })
            });
            (wait_or_kill(&mut child, timeout)?, Vec::new(), false)
        }
        ChildInput::Null | ChildInput::Inherit => {
            (wait_or_kill(&mut child, timeout)?, Vec::new(), false)
        }
    };

    #[cfg(unix)]
    if hand_over_tty {
        unix::take_terminal_back();
    }

    // Readers normally end the instant the last write end closes. If a
    // descendant survived (deliberate escape) the pipe stays open; after the
    // grace period the thread is abandoned with whatever was captured.
    let deadline = Instant::now() + Duration::from_millis(500);
    for t in [&out_thread, &err_thread] {
        while !t.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    let stdout = out_buf.lock().expect("reader buffer");
    let stderr = err_buf.lock().expect("reader buffer");
    Ok(Captured {
        status,
        stdout: stdout.bytes.clone(),
        stderr: stderr.bytes.clone(),
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
        stdin_recorded,
        stdin_diverged,
    })
}

fn wait_or_kill(child: &mut Child, timeout: Duration) -> std::io::Result<Option<ExitStatus>> {
    match child.wait_timeout(timeout)? {
        Some(status) => Ok(Some(status)),
        None => {
            // The group leader is still represented by this unreaped Child,
            // so its pid/group id cannot have been recycled.
            kill_tree(child);
            let _ = child.wait();
            Ok(None)
        }
    }
}

fn wait_with_stdin_tape(
    child: &mut Child,
    stdin: std::process::ChildStdin,
    timeout: Duration,
    requests: mpsc::Receiver<StdinRequest>,
    replay: &[StdinEvent],
    allow_current: bool,
) -> std::io::Result<(Option<ExitStatus>, Vec<StdinEvent>, bool)> {
    let mut replay_index = 0usize;
    let mut recorded = Vec::new();
    let mut diverged = false;
    let mut child_stdin = Some(stdin);
    let mut deadline = Instant::now() + timeout;

    loop {
        while let Ok(request) = requests.try_recv() {
            let event = match request {
                StdinRequest::Replay => match replay.get(replay_index) {
                    Some(event) => {
                        replay_index += 1;
                        event.clone()
                    }
                    None => {
                        diverged = true;
                        StdinEvent {
                            bytes: Vec::new(),
                            eof: true,
                        }
                    }
                },
                StdinRequest::Current if allow_current && child_stdin.is_some() => {
                    let mut bytes = Vec::new();
                    let read = std::io::stdin().lock().read_until(b'\n', &mut bytes)?;
                    // Waiting for a human is not execution time. Give the C
                    // code a fresh deadline after every supplied line.
                    deadline = Instant::now() + timeout;
                    let event = StdinEvent {
                        bytes,
                        eof: read == 0,
                    };
                    recorded.push(event.clone());
                    event
                }
                StdinRequest::Current => {
                    let event = StdinEvent {
                        bytes: Vec::new(),
                        eof: true,
                    };
                    recorded.push(event.clone());
                    event
                }
            };

            let write_failed = !event.eof
                && child_stdin.as_mut().is_none_or(|stdin| {
                    stdin.write_all(&event.bytes).is_err() || stdin.flush().is_err()
                });
            if write_failed {
                diverged = true;
            }
            if event.eof || write_failed {
                // Dropping the last write handle is how scanf observes EOF.
                child_stdin.take();
            }
        }
        if let Some(status) = child.try_wait()? {
            diverged |= replay_index != replay.len();
            return Ok((Some(status), recorded, diverged));
        }
        if Instant::now() >= deadline {
            kill_tree(child);
            let _ = child.wait();
            return Ok((None, recorded, diverged));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Read `r` to exhaustion on a thread, into a bounded buffer the caller can
/// snapshot even if the thread never finishes.
#[allow(clippy::type_complexity)]
fn drain(
    mut r: impl Read + Send + 'static,
    observer: Option<ChunkSink>,
) -> (Arc<Mutex<CaptureBuffer>>, JoinHandle<()>) {
    let buf = Arc::new(Mutex::new(CaptureBuffer::default()));
    let sink = Arc::clone(&buf);
    let t = std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match r.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Some(observer) = &observer {
                        observer(&chunk[..n]);
                    }
                    let mut captured = sink.lock().expect("reader buffer");
                    let left = MAX_CAPTURE_BYTES.saturating_sub(captured.bytes.len());
                    let keep = left.min(n);
                    captured.bytes.extend_from_slice(&chunk[..keep]);
                    captured.truncated |= keep < n;
                }
            }
        }
    });
    (buf, t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_is_bounded_but_reader_drains_to_eof() {
        let data = vec![b'x'; MAX_CAPTURE_BYTES + 1024];
        let (buf, thread) = drain(std::io::Cursor::new(data), None);
        thread.join().expect("reader thread");
        let got = buf.lock().expect("capture buffer");
        assert_eq!(got.bytes.len(), MAX_CAPTURE_BYTES);
        assert!(got.truncated);
    }
}

#[cfg(unix)]
fn kill_tree(child: &Child) {
    // The child leads its own process group; the negative pid reaches every
    // descendant that did not deliberately escape with setsid.
    unsafe {
        libc::killpg(child.id() as i32, libc::SIGKILL);
    }
}

#[cfg(windows)]
fn kill_tree(child: &Child) {
    // taskkill /T walks the parent-child tree — not as airtight as a Job
    // Object, but it needs no unsafe and ships with every Windows.
    let _ = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &child.id().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(unix)]
mod unix {
    use std::io::IsTerminal;
    use std::process::Command;
    use std::sync::Once;

    /// Put the child in its own process group, and when stdin is the
    /// terminal, arrange for that group to become the foreground group.
    ///
    /// The foreground handoff is not optional: a background process group
    /// reading the terminal is stopped by SIGTTIN, so without it every
    /// `scanf` at the prompt would freeze the user's program. This is the
    /// same dance every job-control shell performs.
    pub fn prepare(cmd: &mut Command, interactive: bool) -> bool {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);

        let tty = interactive && std::io::stdin().is_terminal();
        if tty {
            ignore_sigttou();
            unsafe {
                // Child side, between fork and exec: claim the terminal
                // *before* the program runs. Doing it only from the parent
                // leaves a window where the child reads the tty while still
                // in the background and gets stopped.
                cmd.pre_exec(|| {
                    libc::setpgid(0, 0);
                    libc::signal(libc::SIGTTOU, libc::SIG_IGN);
                    libc::tcsetpgrp(0, libc::getpid());
                    Ok(())
                });
            }
        }
        tty
    }

    /// Parent side of the same handoff; whichever of the two runs second is
    /// a harmless no-op.
    pub fn give_terminal_to(pid: u32) {
        unsafe {
            libc::tcsetpgrp(0, pid as i32);
        }
    }

    pub fn take_terminal_back() {
        unsafe {
            libc::tcsetpgrp(0, libc::getpgrp());
        }
    }

    /// Reclaiming the terminal from the background raises SIGTTOU, whose
    /// default disposition stops the process dead. Ignored once, up front.
    fn ignore_sigttou() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| unsafe {
            libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        });
    }
}
