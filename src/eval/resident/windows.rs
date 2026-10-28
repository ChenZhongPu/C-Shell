//! Windows loader and resident runner transport.

use super::{Msg, Runner, pump, serve};
use crate::proc;
use crate::toolchain::Toolchain;
use anyhow::{Context, Result, bail};
use std::ffi::{CString, OsStr, c_void};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::os::windows::process::CommandExt;
use std::process::{Child, ChildStdin, Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_OUTBOUND, WriteFile,
};
use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
use windows_sys::Win32::System::Diagnostics::Debug::{
    SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SEM_NOOPENFILEERRORBOX, SetErrorMode,
};
use windows_sys::Win32::System::LibraryLoader::{
    AddDllDirectory, GetProcAddress, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
    LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LoadLibraryExW,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_REJECT_REMOTE_CLIENTS,
};
use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;

pub(super) type Commands = ChildStdin;

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

pub(super) fn write_output(fd: i32, mut bytes: &[u8]) {
    let handle = unsafe {
        GetStdHandle(if fd == 1 {
            STD_OUTPUT_HANDLE
        } else {
            STD_ERROR_HANDLE
        })
    };
    while !bytes.is_empty() {
        let mut written = 0;
        let length = bytes.len().min(u32::MAX as usize) as u32;
        if unsafe {
            WriteFile(
                handle,
                bytes.as_ptr().cast(),
                length,
                &mut written,
                std::ptr::null_mut(),
            )
        } == 0
            || written == 0
        {
            return;
        }
        bytes = &bytes[written as usize..];
    }
}

/// Load each step for the lifetime of the runner, preserving function pointers
/// and the statics of previously defined functions.
pub(super) unsafe fn call_step(
    path: &str,
    symbol: &str,
    slots: *mut *mut c_void,
) -> std::result::Result<(), String> {
    let path = wide(OsStr::new(path));
    let handle = unsafe {
        LoadLibraryExW(
            path.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
        )
    };
    if handle.is_null() {
        return Err(io::Error::last_os_error().to_string());
    }
    let symbol = CString::new(symbol).map_err(|error| error.to_string())?;
    let entry = unsafe { GetProcAddress(handle, symbol.as_ptr().cast()) }
        .ok_or_else(|| io::Error::last_os_error().to_string())?;
    let number = symbol
        .to_str()
        .ok()
        .and_then(|name| name.strip_prefix("cs_step_"))
        .ok_or_else(|| "invalid step entry point".to_string())?;
    let flush_symbol =
        CString::new(format!("cs_flush_{number}")).map_err(|error| error.to_string())?;
    let flush = unsafe { GetProcAddress(handle, flush_symbol.as_ptr().cast()) }
        .ok_or_else(|| io::Error::last_os_error().to_string())?;
    let step: unsafe extern "C" fn(*mut *mut c_void) = unsafe { std::mem::transmute(entry) };
    let flush: unsafe extern "C" fn() = unsafe { std::mem::transmute(flush) };
    unsafe {
        step(slots);
        // The helper belongs to the DLL's CRT, which may differ from Rust's.
        flush();
    }
    Ok(())
}

fn runner_reply_pipe() -> Result<File> {
    let name = std::env::var_os("C_SHELL_RUNNER_PIPE").context("missing runner reply pipe")?;
    let name = wide(&name);
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_OUTBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_REJECT_REMOTE_CLIENTS,
            1,
            8192,
            0,
            0,
            std::ptr::null(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error()).context("cannot create runner reply pipe");
    }
    let reply = unsafe { File::from_raw_handle(handle.cast()) };
    if unsafe { ConnectNamedPipe(reply.as_raw_handle().cast(), std::ptr::null_mut()) } == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_PIPE_CONNECTED as i32) {
            return Err(error).context("cannot connect runner reply pipe");
        }
    }
    Ok(reply)
}

fn run_server() -> Result<ExitCode> {
    unsafe {
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX);
    }
    let compiler_dir =
        std::env::var_os("C_SHELL_RUNNER_COMPILER_DIR").context("missing compiler directory")?;
    let compiler_dir = wide(&compiler_dir);
    if unsafe { AddDllDirectory(compiler_dir.as_ptr()) }.is_null() {
        return Err(io::Error::last_os_error()).context("cannot add compiler DLL directory");
    }
    let reply = runner_reply_pipe()?;
    let stdin = io::stdin();
    Ok(serve(stdin.lock(), reply))
}

pub(super) fn runner_main() -> ExitCode {
    match run_server() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("resident runner: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn connect_reply(child: &mut Child, name: &str, timeout: Duration) -> Result<File> {
    let start = Instant::now();
    loop {
        match OpenOptions::new().read(true).open(name) {
            Ok(reply) => return Ok(reply),
            Err(error) if matches!(error.raw_os_error(), Some(2 | 231)) => {}
            Err(error) => return Err(error).context("cannot open runner reply pipe"),
        }
        if let Some(status) = child.try_wait()? {
            bail!("resident runner exited while starting: {status}");
        }
        if proc::interrupted() || start.elapsed() >= timeout {
            bail!("resident runner did not start before the deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub(super) fn spawn(tc: &Toolchain, timeout: Duration) -> Result<Runner> {
    let exe = std::env::current_exe().context("cannot locate the c-shell executable")?;
    let compiler = std::fs::canonicalize(&tc.path).context("cannot locate the C compiler")?;
    let compiler_dir = compiler
        .parent()
        .context("compiler has no parent directory")?;
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|error| anyhow::anyhow!("{error}"))?;
    let pipe = format!(
        r"\\.\pipe\c-shell-{}-{:032x}",
        std::process::id(),
        u128::from_le_bytes(random)
    );
    let mut child = Command::new(exe)
        .env("C_SHELL_RUNNER", "1")
        .env("C_SHELL_RUNNER_PIPE", &pipe)
        .env("C_SHELL_RUNNER_COMPILER_DIR", compiler_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .context("cannot start the resident runner")?;
    let job = match proc::windows::Job::attach(&child) {
        Ok(job) => job,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error).context("cannot contain the resident runner");
        }
    };
    let reply = match connect_reply(&mut child, &pipe, timeout) {
        Ok(reply) => reply,
        Err(error) => {
            job.terminate();
            let _ = child.wait();
            return Err(error);
        }
    };
    let commands = child.stdin.take().expect("piped");
    let (tx, rx) = mpsc::channel();
    pump(child.stdout.take().expect("piped"), tx.clone(), Msg::Out);
    pump(child.stderr.take().expect("piped"), tx.clone(), Msg::Err);
    pump(reply, tx, Msg::Reply);
    Ok(Runner {
        child,
        commands,
        job,
        rx,
    })
}

pub(super) fn kill(runner: &mut Runner) {
    runner.job.terminate();
    let _ = runner.child.wait();
}
