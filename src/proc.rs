//! Child processes with a deadline and whole-tree cleanup.
//!
//! Everything c-shell launches — compiler, probes, the user's program — goes
//! through here. Two failure modes motivate the ceremony:
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
//!    unconditionally — worst case costs one abandoned thread, never a hang.

use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use wait_timeout::ChildExt;

pub struct Captured {
    /// `None` when the deadline expired and the process tree was killed.
    pub status: Option<ExitStatus>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
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
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    if interactive_stdin {
        cmd.stdin(Stdio::inherit());
    } else {
        cmd.stdin(Stdio::null());
    }

    #[cfg(unix)]
    let hand_over_tty = unix::prepare(cmd, interactive_stdin);
    let mut child = cmd.spawn()?;
    #[cfg(unix)]
    if hand_over_tty {
        unix::give_terminal_to(child.id());
    }

    let (out_buf, out_thread) = drain(child.stdout.take().expect("stdout piped"));
    let (err_buf, err_thread) = drain(child.stderr.take().expect("stderr piped"));

    let status = match child.wait_timeout(timeout)? {
        Some(st) => Some(st),
        None => {
            // Not yet reaped: the pid is still held by the zombie, so the
            // group id cannot have been recycled — killing it is safe.
            kill_tree(&child);
            let _ = child.wait();
            None
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

    let stdout = out_buf.lock().expect("reader buffer").clone();
    let stderr = err_buf.lock().expect("reader buffer").clone();
    Ok(Captured {
        status,
        stdout,
        stderr,
    })
}

/// Read `r` to exhaustion on a thread, into a buffer the caller can snapshot
/// even if the thread never finishes.
#[allow(clippy::type_complexity)]
fn drain(mut r: impl Read + Send + 'static) -> (Arc<Mutex<Vec<u8>>>, JoinHandle<()>) {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&buf);
    let t = std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match r.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => sink
                    .lock()
                    .expect("reader buffer")
                    .extend_from_slice(&chunk[..n]),
            }
        }
    });
    (buf, t)
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
