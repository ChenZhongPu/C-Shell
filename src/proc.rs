//! Child processes with a deadline and lifecycle tree cleanup.
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
//!    its own process group and that group is terminated when the run ends.
//!    On Windows every run is assigned to a Job Object with
//!    `KILL_ON_JOB_CLOSE`. A process that deliberately escapes on Unix with
//!    `setsid` can still hold the pipe, so readers drain into shared buffers
//!    and are given a grace period rather than joined unconditionally — worst
//!    case costs two abandoned reader threads, never a hang.
//!
//! Taped stdin is request-driven rather than copied eagerly: generated scanf
//! wrappers signal over stderr, this module supplies either the next retained
//! line or one fresh parent-stdin line, and historical requests can never fall
//! through to the real terminal.

#[cfg(unix)]
use std::io::BufRead;
use std::io::{Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
#[cfg(not(unix))]
use wait_timeout::ChildExt;

/// Per stream. Readers continue draining after this so a noisy child cannot
/// deadlock, but excess bytes are discarded rather than exhausting our heap.
pub const MAX_CAPTURE_BYTES: usize = 8 * 1024 * 1024;

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

pub fn begin_input() {
    INTERRUPTED.store(false, Ordering::Relaxed);
}
pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::Relaxed)
}

pub(crate) fn install_interrupt_handler() {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return;
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        #[cfg(unix)]
        unsafe {
            extern "C" fn interrupt(_signal: libc::c_int) {
                INTERRUPTED.store(true, Ordering::Relaxed);
            }
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = interrupt as *const () as usize;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
        }
        #[cfg(windows)]
        unsafe {
            unsafe extern "system" fn interrupt(event: u32) -> i32 {
                if event == 0 || event == 1 {
                    INTERRUPTED.store(true, Ordering::Relaxed);
                    1
                } else {
                    0
                }
            }
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(interrupt), 1);
        }
    });
}

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
    Finished,
}

pub struct Captured {
    pub interrupted: bool,
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
    install_interrupt_handler();
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    match &input {
        ChildInput::Null => cmd.stdin(Stdio::null()),
        ChildInput::Inherit => cmd.stdin(Stdio::inherit()),
        ChildInput::Bytes(_) | ChildInput::Tape { .. } => cmd.stdin(Stdio::piped()),
    };

    #[cfg(unix)]
    let hand_over_tty = unix::prepare(cmd, matches!(&input, ChildInput::Inherit));
    let mut child = cmd.spawn()?;
    #[cfg(windows)]
    let _job = windows::Job::attach(&child).ok();
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
            &err_thread,
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
    // descendant escaped containment (for example with setsid on Unix), the
    // pipe stays open; after the grace period the thread is abandoned with
    // whatever was captured.
    let deadline = Instant::now() + Duration::from_millis(500);
    for t in [&out_thread, &err_thread] {
        while !t.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    let stdout = out_buf.lock().expect("reader buffer");
    let stderr = err_buf.lock().expect("reader buffer");
    Ok(Captured {
        interrupted: interrupted()
            || status.is_some_and(|status| {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    status.signal() == Some(libc::SIGINT)
                }
                #[cfg(windows)]
                {
                    status.code().map(|code| code as u32) == Some(0xC000013A)
                }
            }),
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
    #[cfg(unix)]
    {
        unix::wait_or_kill(child, timeout)
    }

    #[cfg(not(unix))]
    {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = child.wait_timeout(Duration::from_millis(5))? {
                return Ok(Some(status));
            }
            if interrupted() || Instant::now() >= deadline {
                // The group leader is still represented by this unreaped Child,
                // so its pid/group id cannot have been recycled.
                kill_tree(child);
                let _ = child.wait();
                return Ok(None);
            }
        }
    }
}

/// Temporarily make parent stdin cancellable while a scanf call is active.
/// The evaluated child receives its own pipe, so this does not change its IO.
#[cfg(unix)]
struct StdinMode(Option<i32>);

#[cfg(unix)]
impl StdinMode {
    fn new(enabled: bool) -> std::io::Result<Self> {
        if !enabled {
            return Ok(Self(None));
        }
        let flags = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_GETFL) };
        if flags == -1
            || unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_SETFL, flags | libc::O_NONBLOCK) }
                == -1
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self(Some(flags)))
    }
}

#[cfg(unix)]
impl Drop for StdinMode {
    fn drop(&mut self) {
        if let Some(flags) = self.0 {
            unsafe {
                libc::fcntl(libc::STDIN_FILENO, libc::F_SETFL, flags);
            }
        }
    }
}

#[cfg(not(unix))]
struct StdinMode;

#[cfg(not(unix))]
impl StdinMode {
    fn new(_enabled: bool) -> std::io::Result<Self> {
        Ok(Self)
    }
}

struct PendingInput {
    request: StdinRequest,
    supplied: bool,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct ParentInput {
    deferred: Vec<u8>,
    #[cfg(windows)]
    eof: bool,
}

// Like Rust's shared stdin buffer, read-ahead belongs to the parent stream,
// not one generated process. In particular, a Windows pipe read may contain
// several lines needed by later evaluations (including temporary checks).
static PARENT_INPUT: std::sync::LazyLock<Mutex<ParentInput>> =
    std::sync::LazyLock::new(Mutex::default);

impl ParentInput {
    fn defer(&mut self, mut bytes: Vec<u8>) {
        bytes.append(&mut self.deferred);
        self.deferred = bytes;
    }

    fn poll(&mut self, bytes: &mut Vec<u8>) -> std::io::Result<Option<StdinEvent>> {
        bytes.append(&mut self.deferred);
        if bytes.contains(&b'\n') {
            return Ok(Some(self.line(bytes, false)));
        }
        #[cfg(unix)]
        match std::io::stdin().lock().read_until(b'\n', bytes) {
            Ok(read) => Ok(Some(
                self.line(bytes, read == 0 || bytes.last() != Some(&b'\n')),
            )),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
        #[cfg(windows)]
        {
            if self.eof {
                return Ok(Some(self.line(bytes, true)));
            }
            match windows::read_ready_stdin()? {
                None => Ok(None),
                Some((incoming, eof)) => {
                    bytes.extend(incoming);
                    self.eof = eof;
                    if eof || bytes.contains(&b'\n') {
                        Ok(Some(self.line(bytes, eof)))
                    } else {
                        Ok(None)
                    }
                }
            }
        }
    }

    fn line(&mut self, bytes: &mut Vec<u8>, eof: bool) -> StdinEvent {
        if let Some(end) = bytes.iter().position(|byte| *byte == b'\n') {
            self.defer(bytes.split_off(end + 1));
            StdinEvent {
                bytes: std::mem::take(bytes),
                eof: false,
            }
        } else {
            StdinEvent {
                bytes: std::mem::take(bytes),
                eof,
            }
        }
    }
}

struct Tape<'a> {
    replay: &'a [StdinEvent],
    index: usize,
    recorded: Vec<StdinEvent>,
    diverged: bool,
    stdin: Option<std::process::ChildStdin>,
}

impl Tape<'_> {
    fn supply(&mut self, request: StdinRequest, current: StdinEvent, write: bool) {
        let event = match request {
            StdinRequest::Replay => match self.replay.get(self.index) {
                Some(event) => {
                    self.index += 1;
                    event.clone()
                }
                None => {
                    self.diverged = true;
                    StdinEvent {
                        bytes: Vec::new(),
                        eof: true,
                    }
                }
            },
            StdinRequest::Current => {
                self.recorded.push(current.clone());
                current
            }
            StdinRequest::Finished => unreachable!("completion is not a request"),
        };
        if !write {
            return;
        }
        let write_failed = !event.bytes.is_empty()
            && self.stdin.as_mut().is_none_or(|stdin| {
                stdin.write_all(&event.bytes).is_err() || stdin.flush().is_err()
            });
        // A buffered/no-input conversion may already have returned. A closed
        // pipe is not a tape-order mismatch; only the requests establish that.
        if event.eof || write_failed {
            self.stdin.take();
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
    stderr_reader: &JoinHandle<()>,
) -> std::io::Result<(Option<ExitStatus>, Vec<StdinEvent>, bool)> {
    let _stdin_mode = match StdinMode::new(allow_current) {
        Ok(mode) => mode,
        Err(error) => {
            kill_tree(child);
            let _ = child.wait();
            return Err(error);
        }
    };
    let mut tape = Tape {
        replay,
        index: 0,
        recorded: Vec::new(),
        diverged: false,
        stdin: Some(stdin),
    };
    let mut pending: Option<PendingInput> = None;
    let mut parent_input = PARENT_INPUT.lock().expect("parent stdin buffer");
    let mut deadline = Instant::now() + timeout;
    let mut exited = None;
    loop {
        if interrupted() {
            kill_tree(child);
            let _ = child.wait();
            return Ok((None, tape.recorded, tape.diverged));
        }
        // Snapshot before draining so an exiting child cannot outrun its
        // stderr observer and make the final request disappear from the tape.
        let requests_complete = stderr_reader.is_finished();
        let mut completed_call = false;
        while let Ok(request) = requests.try_recv() {
            if request == StdinRequest::Finished {
                completed_call = true;
                if let Some(input) = pending.take()
                    && !input.supplied
                {
                    // libc completed using its own buffer. Retain the call
                    // boundary without consuming another parent-input line.
                    parent_input.defer(input.bytes);
                    tape.supply(
                        input.request,
                        StdinEvent {
                            bytes: Vec::new(),
                            eof: false,
                        },
                        false,
                    );
                }
            } else {
                pending = Some(PendingInput {
                    request,
                    supplied: false,
                    bytes: Vec::new(),
                });
            }
        }
        if exited.is_none() {
            #[cfg(unix)]
            let status = if unix::exited_unreaped(child.id())? {
                unix::kill_group(child.id());
                Some(child.wait()?)
            } else {
                None
            };
            #[cfg(not(unix))]
            let status = child.try_wait()?;
            if let Some(status) = status {
                exited = Some((status, Instant::now() + Duration::from_millis(500)));
            }
        }
        if let Some((status, drain_deadline)) = exited.as_ref() {
            if requests_complete || Instant::now() >= *drain_deadline {
                if let Some(input) = pending.take()
                    && !input.supplied
                {
                    tape.supply(
                        input.request,
                        StdinEvent {
                            bytes: input.bytes,
                            eof: false,
                        },
                        false,
                    );
                }
                tape.diverged |= tape.index != replay.len();
                return Ok((Some(*status), tape.recorded, tape.diverged));
            }
        } else if let Some(input) = pending.as_mut()
            && !input.supplied
        {
            let event = if input.request == StdinRequest::Replay {
                Some(StdinEvent {
                    bytes: Vec::new(),
                    eof: false,
                })
            } else if allow_current && tape.stdin.is_some() {
                match parent_input.poll(&mut input.bytes) {
                    Ok(Some(event)) => {
                        // Only supplying fresh input restarts the execution
                        // deadline. Buffered calls and replay must not let an
                        // infinite scanf loop renew its timeout forever.
                        deadline = Instant::now() + timeout;
                        Some(event)
                    }
                    Ok(None) => None,
                    Err(error) => {
                        kill_tree(child);
                        let _ = child.wait();
                        return Err(error);
                    }
                }
            } else {
                Some(StdinEvent {
                    bytes: Vec::new(),
                    eof: true,
                })
            };
            if let Some(event) = event {
                tape.supply(input.request, event, true);
                input.supplied = true;
            }
        }
        let waiting_for_input = pending.as_ref().is_some_and(|input| {
            input.request == StdinRequest::Current && !input.supplied && allow_current
        });
        if exited.is_none() && (completed_call || !waiting_for_input) && Instant::now() >= deadline
        {
            kill_tree(child);
            let _ = child.wait();
            return Ok((None, tape.recorded, tape.diverged));
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

    #[cfg(unix)]
    #[test]
    fn normal_exit_kills_forked_descendants() {
        let mut command = background_child_command();
        let captured = run_captured(&mut command, Duration::from_secs(2), false)
            .expect("run shell with background child");
        assert_background_child_stopped(captured);
    }

    #[cfg(unix)]
    #[test]
    fn normal_exit_kills_forked_descendants_with_stdin_tape() {
        let mut command = background_child_command();
        let (_request_tx, request_rx) = std::sync::mpsc::channel();
        let observers = Observers {
            stdout: Arc::new(|_: &[u8]| {}),
            stderr: Arc::new(|_: &[u8]| {}),
        };
        let captured = run_observed_with_stdin_tape(
            &mut command,
            Duration::from_secs(2),
            &[],
            false,
            request_rx,
            observers,
        )
        .expect("run taped shell with background child");
        assert_background_child_stopped(captured);
    }

    #[cfg(unix)]
    fn background_child_command() -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 10 & printf '%s\\n' \"$!\""]);
        command
    }

    #[cfg(unix)]
    fn assert_background_child_stopped(captured: Captured) {
        assert!(
            captured.status.is_some_and(|status| status.success()),
            "shell did not exit cleanly"
        );
        let pid = String::from_utf8(captured.stdout)
            .expect("background pid is UTF-8")
            .trim()
            .parse::<i32>()
            .expect("background pid is numeric");
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let gone = unsafe { libc::kill(pid, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            if gone {
                return;
            }
            if Instant::now() >= deadline {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
                panic!("background child {pid} survived its parent's normal exit");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(unix)]
fn kill_tree(child: &Child) {
    unix::kill_group(child.id());
}

#[cfg(windows)]
fn kill_tree(child: &Child) {
    // The Job Object handles normal completion; taskkill stops the direct
    // tree promptly on timeout before its handle is closed.
    let _ = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &child.id().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(windows)]
mod windows {
    use std::io;
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType, ReadFile};
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetNumberOfConsoleInputEvents, GetStdHandle, INPUT_RECORD, KEY_EVENT,
        PeekConsoleInputW, ReadConsoleW, STD_INPUT_HANDLE,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;

    pub struct Job(HANDLE);

    /// Never enter a blocking line read just because a wrapped call emitted
    /// its begin marker: libc may finish from its own buffer a moment later.
    /// Console reads start only once Enter is queued; pipes read only bytes
    /// reported ready by PeekNamedPipe. The caller accumulates partial lines.
    pub fn read_ready_stdin() -> io::Result<Option<(Vec<u8>, bool)>> {
        unsafe {
            let handle = GetStdHandle(STD_INPUT_HANDLE);
            let mut mode = 0;
            if GetConsoleMode(handle, &mut mode) != 0 {
                let mut count = 0;
                if GetNumberOfConsoleInputEvents(handle, &mut count) == 0 {
                    return Err(io::Error::last_os_error());
                }
                if count == 0 {
                    return Ok(None);
                }
                let mut records: Vec<INPUT_RECORD> = vec![std::mem::zeroed(); count as usize];
                let mut read = 0;
                if PeekConsoleInputW(handle, records.as_mut_ptr(), count, &mut read) == 0 {
                    return Err(io::Error::last_os_error());
                }
                let complete = records[..read as usize].iter().any(|record| {
                    record.EventType == KEY_EVENT as u16
                        && record.Event.KeyEvent.bKeyDown != 0
                        && record.Event.KeyEvent.uChar.UnicodeChar == 13
                });
                if !complete {
                    return Ok(None);
                }
                let mut line = Vec::new();
                loop {
                    let mut buffer = [0u16; 2048];
                    let mut read = 0;
                    if ReadConsoleW(
                        handle,
                        buffer.as_mut_ptr().cast(),
                        buffer.len() as u32,
                        &mut read,
                        std::ptr::null_mut(),
                    ) == 0
                    {
                        if super::interrupted() {
                            return Ok(None);
                        }
                        return Err(io::Error::last_os_error());
                    }
                    line.extend_from_slice(&buffer[..read as usize]);
                    if read == 0 || line.last().is_some_and(|ch| *ch == 10 || *ch == 13) {
                        break;
                    }
                }
                if line.last() == Some(&13) {
                    line.push(10);
                }
                let eof = line.is_empty();
                return Ok(Some((String::from_utf16_lossy(&line).into_bytes(), eof)));
            }
            let mut available = 8192;
            if GetFileType(handle) == FILE_TYPE_PIPE {
                if PeekNamedPipe(
                    handle,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut available,
                    std::ptr::null_mut(),
                ) == 0
                {
                    let error = io::Error::last_os_error();
                    if matches!(error.raw_os_error(), Some(109 | 233)) {
                        return Ok(Some((Vec::new(), true)));
                    }
                    return Err(error);
                }
                if available == 0 {
                    return Ok(None);
                }
            }
            let mut buffer = vec![0u8; available.min(8192) as usize];
            let mut read = 0;
            if ReadFile(
                handle,
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            ) == 0
            {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(109) {
                    return Err(error);
                }
            }
            buffer.truncate(read as usize);
            Ok(Some((buffer, read == 0)))
        }
    }

    impl Job {
        pub fn attach(child: &Child) -> io::Result<Self> {
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    return Err(io::Error::last_os_error());
                }

                let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as *const std::ffi::c_void,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) == 0
                {
                    CloseHandle(handle);
                    return Err(io::Error::last_os_error());
                }

                if AssignProcessToJobObject(handle, child.as_raw_handle() as HANDLE) == 0 {
                    CloseHandle(handle);
                    return Err(io::Error::last_os_error());
                }
                Ok(Self(handle))
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(unix)]
mod unix {
    use std::io;
    use std::io::IsTerminal;
    use std::process::{Child, Command, ExitStatus};
    use std::sync::Once;
    use std::time::{Duration, Instant};

    pub fn wait_or_kill(child: &mut Child, timeout: Duration) -> io::Result<Option<ExitStatus>> {
        let deadline = Instant::now() + timeout;
        loop {
            if super::interrupted() {
                kill_group(child.id());
                let _ = child.wait();
                return Ok(None);
            }
            if exited_unreaped(child.id())? {
                kill_group(child.id());
                return child.wait().map(Some);
            }
            if Instant::now() >= deadline {
                kill_group(child.id());
                let _ = child.wait();
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Observe a completed child without reaping it, preserving its process
    /// group ID until `kill_group` has safely reached its descendants.
    pub fn exited_unreaped(pid: u32) -> io::Result<bool> {
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as _,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(info.si_signo != 0)
    }

    pub fn kill_group(leader_pid: u32) {
        // The leader remains unreaped while this is called, so the process
        // group ID cannot have been recycled to an unrelated group.
        unsafe {
            libc::killpg(leader_pid as i32, libc::SIGKILL);
        }
    }

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
