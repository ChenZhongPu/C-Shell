//! Unix loader and resident runner transport.

use super::{Msg, Runner, pump, serve};
use crate::toolchain::Toolchain;
use anyhow::{Context, Result};
use std::ffi::{CStr, CString, c_void};
use std::fs::File;
use std::io::{self, BufReader};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

pub(super) type Commands = File;

pub(super) fn write_output(fd: i32, bytes: &[u8]) {
    let mut rest = bytes;
    while !rest.is_empty() {
        let written = unsafe { libc::write(fd, rest.as_ptr().cast(), rest.len()) };
        if written <= 0 {
            return;
        }
        rest = &rest[written as usize..];
    }
}

fn dl_error() -> String {
    let message = unsafe { libc::dlerror() };
    if message.is_null() {
        "unknown dynamic loader error".to_string()
    } else {
        unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .replace('\n', " ")
    }
}

/// # Safety
/// `slots` must point at `SLOT_CAP` writable pointers and the library must
/// export `symbol` with the signature `void (void **)`.
pub(super) unsafe fn call_step(
    path: &str,
    symbol: &str,
    slots: *mut *mut c_void,
) -> std::result::Result<(), String> {
    let path = CString::new(path).map_err(|e| e.to_string())?;
    let symbol = CString::new(symbol).map_err(|e| e.to_string())?;
    let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if handle.is_null() {
        return Err(dl_error());
    }
    let entry = unsafe { libc::dlsym(handle, symbol.as_ptr()) };
    if entry.is_null() {
        return Err(dl_error());
    }
    let step: extern "C" fn(*mut *mut c_void) = unsafe { std::mem::transmute(entry) };
    step(slots);
    unsafe {
        libc::fflush(std::ptr::null_mut());
    }
    Ok(())
}

pub(super) fn runner_main() -> std::process::ExitCode {
    let commands = BufReader::new(unsafe { File::from_raw_fd(3) });
    let reply = unsafe { File::from_raw_fd(4) };
    serve(commands, reply)
}

/// A pipe whose descriptors are close-on-exec and well above the stdio range,
/// so duplicating them onto fixed numbers in the child cannot collide.
fn pipe() -> io::Result<(RawFd, RawFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut moved = [0; 2];
    for (index, fd) in fds.iter().enumerate() {
        moved[index] = unsafe { libc::fcntl(*fd, libc::F_DUPFD_CLOEXEC, 10) };
        if moved[index] < 0 {
            return Err(io::Error::last_os_error());
        }
        unsafe {
            libc::close(*fd);
        }
    }
    Ok((moved[0], moved[1]))
}

pub(super) fn spawn(_tc: &Toolchain, _timeout: Duration) -> Result<Runner> {
    let exe = std::env::current_exe().context("cannot locate the c-shell executable")?;
    let (commands_read, commands_write) = pipe()?;
    let (reply_read, reply_write) = pipe()?;
    let mut command = Command::new(exe);
    command
        .env("C_SHELL_RUNNER", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(commands_read, 3) < 0 || libc::dup2(reply_write, 4) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let spawned = command.spawn();
    unsafe {
        libc::close(commands_read);
        libc::close(reply_write);
    }
    let mut child = spawned.context("cannot start the resident runner")?;
    let (tx, rx) = mpsc::channel();
    pump(child.stdout.take().expect("piped"), tx.clone(), Msg::Out);
    pump(child.stderr.take().expect("piped"), tx.clone(), Msg::Err);
    pump(unsafe { File::from_raw_fd(reply_read) }, tx, Msg::Reply);
    Ok(Runner {
        child,
        commands: unsafe { File::from_raw_fd(commands_write) },
        rx,
    })
}

pub(super) fn kill(runner: &mut Runner) {
    unsafe {
        libc::killpg(runner.child.id() as i32, libc::SIGKILL);
    }
    let _ = runner.child.wait();
}
