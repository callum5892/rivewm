//! Named-pipe IPC: a client writes one request line and reads one response
//! line back.
//!
//! The pipe is `\\.\pipe\rivewm-<user>`. Only the current user's account
//! may open it, and remote clients are rejected, since whoever can talk to
//! it can rearrange every window on the desktop. Creating it also doubles as
//! a single-instance check: a second rivewm finds the pipe taken.

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL,
    LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FlushFileBuffers, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{HSTRING, PWSTR};

/// Longest request line we'll read.
const MAX_REQUEST: u64 = 64 * 1024;
const BUFFER_SIZE: u32 = 64 * 1024;

/// `\\.\pipe\rivewm-<user>`.
pub fn pipe_name() -> String {
    let user = std::env::var("USERNAME").unwrap_or_default();
    format!(r"\\.\pipe\rivewm-{user}")
}

#[derive(Debug)]
pub enum ServeError {
    /// Another rivewm already owns the pipe.
    AlreadyRunning,
    Os(windows::core::Error),
}

impl std::fmt::Display for ServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServeError::AlreadyRunning => f.write_str("rivewm is already running"),
            ServeError::Os(err) => write!(f, "couldn't create IPC pipe: {err}"),
        }
    }
}

impl std::error::Error for ServeError {}

/// Starts answering requests on a background thread. `handler` gets each
/// request line (without its newline) and returns the response line.
/// Requests are handled one at a time.
pub fn serve(handler: impl Fn(&str) -> String + Send + 'static) -> Result<(), ServeError> {
    let name = pipe_name();
    let sddl = current_user_only_sddl().map_err(ServeError::Os)?;
    let first = create_pipe(&name, &sddl, true).map_err(|err| {
        if err.code() == ERROR_ACCESS_DENIED.to_hresult() {
            ServeError::AlreadyRunning
        } else {
            ServeError::Os(err)
        }
    })?;

    std::thread::Builder::new()
        .name("rivewm-ipc".into())
        .spawn(move || {
            let mut listening = first;
            loop {
                if !wait_for_client(&listening) {
                    continue;
                }
                // Open the next instance before serving this client, so a
                // new client never finds the pipe missing.
                let next = loop {
                    match create_pipe(&name, &sddl, false) {
                        Ok(pipe) => break pipe,
                        Err(_) => std::thread::sleep(Duration::from_millis(100)),
                    }
                };
                serve_client(&listening, &handler);
                unsafe {
                    let _ = DisconnectNamedPipe(raw(&listening));
                }
                listening = next;
            }
        })
        .expect("failed to spawn IPC thread");
    Ok(())
}

/// Sends one request to the running rivewm and returns its response line.
pub fn request(line: &str) -> io::Result<String> {
    let name = pipe_name();
    let deadline = Instant::now() + Duration::from_secs(2);
    let pipe = loop {
        match OpenOptions::new().read(true).write(true).open(&name) {
            Ok(pipe) => break pipe,
            Err(err)
                if err.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32)
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "rivewm isn't running",
                ));
            }
            Err(err) => return Err(err),
        }
    };
    (&pipe).write_all(format!("{}\n", line.trim_end()).as_bytes())?;
    let mut response = String::new();
    BufReader::new(&pipe).read_line(&mut response)?;
    Ok(response.trim_end().to_owned())
}

fn wait_for_client(pipe: &File) -> bool {
    match unsafe { ConnectNamedPipe(raw(pipe), None) } {
        Ok(()) => true,
        // The client connected between create and connect: still fine.
        Err(err) => err.code() == ERROR_PIPE_CONNECTED.to_hresult(),
    }
}

fn serve_client(pipe: &File, handler: &impl Fn(&str) -> String) {
    let mut line = String::new();
    let mut reader = BufReader::new(pipe.take(MAX_REQUEST));
    if reader.read_line(&mut line).is_err() || line.is_empty() {
        return;
    }
    let mut response = handler(line.trim_end());
    response.push('\n');
    let mut writer = pipe;
    if writer.write_all(response.as_bytes()).is_ok() {
        // Make sure the client has it all before we disconnect.
        unsafe {
            let _ = FlushFileBuffers(raw(pipe));
        }
    }
}

fn create_pipe(name: &str, sddl: &HSTRING, first: bool) -> windows::core::Result<File> {
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl,
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )?;
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    let mut open_mode = PIPE_ACCESS_DUPLEX;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    let handle = unsafe {
        CreateNamedPipeW(
            &HSTRING::from(name),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            BUFFER_SIZE,
            BUFFER_SIZE,
            0,
            Some(&attributes),
        )
    };
    let error = handle.is_invalid().then(windows::core::Error::from_thread);
    unsafe {
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }
    if let Some(error) = error {
        return Err(error);
    }
    // SAFETY: a fresh, valid handle that nothing else owns.
    Ok(unsafe { File::from_raw_handle(handle.0) })
}

/// A security descriptor granting full access to the current user and
/// nobody else (not even other users' read access, which the default
/// named-pipe DACL allows).
fn current_user_only_sddl() -> windows::core::Result<HSTRING> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
        let mut len = 0;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
        // u64s keep the buffer aligned for TOKEN_USER.
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        let result = GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr() as *mut c_void),
            len,
            &mut len,
        );
        let _ = CloseHandle(token);
        result?;
        let user = &*(buf.as_ptr() as *const TOKEN_USER);

        let mut sid = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut sid)?;
        let sid_string = sid.to_string();
        let _ = LocalFree(Some(HLOCAL(sid.0 as *mut c_void)));
        // D:P = protected DACL; one ACE: Allow, Generic All, to this SID.
        Ok(HSTRING::from(format!("D:P(A;;GA;;;{})", sid_string?)))
    }
}

fn raw(pipe: &File) -> HANDLE {
    HANDLE(pipe.as_raw_handle())
}
