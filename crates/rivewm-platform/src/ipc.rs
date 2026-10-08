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
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
    ERROR_PIPE_NOT_CONNECTED, HANDLE, HLOCAL, LocalFree,
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
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
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

/// One connected client, as seen by a request handler.
pub struct Connection<'a> {
    pipe: &'a File,
}

impl Connection<'_> {
    /// Sends one response line. A plain request sends exactly one; a
    /// subscription keeps sending until this fails (the client went away).
    pub fn send_line(&mut self, line: &str) -> io::Result<()> {
        let mut writer = self.pipe;
        writer.write_all(format!("{line}\n").as_bytes())?;
        // Push it to the client now rather than when the buffer fills.
        unsafe { FlushFileBuffers(raw(self.pipe)) }.map_err(io::Error::other)
    }
}

/// Starts answering requests in the background. `handler` gets each request
/// line (without its newline) and replies through the [`Connection`]. Every
/// client gets its own thread, so a long-lived one (a subscription) doesn't
/// hold up the others.
pub fn serve(
    handler: impl Fn(&str, &mut Connection) + Send + Sync + 'static,
) -> Result<(), ServeError> {
    let name = pipe_name();
    let sddl = current_user_only_sddl().map_err(ServeError::Os)?;
    let first = create_pipe(&name, &sddl, true).map_err(|err| {
        if err.code() == ERROR_ACCESS_DENIED.to_hresult() {
            ServeError::AlreadyRunning
        } else {
            ServeError::Os(err)
        }
    })?;
    let handler = Arc::new(handler);

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
                let client = std::mem::replace(&mut listening, next);
                let handler = handler.clone();
                let _ = std::thread::Builder::new()
                    .name("rivewm-ipc-client".into())
                    // Dropping `client` afterwards closes our end, which the
                    // client reads as end-of-stream.
                    .spawn(move || serve_client(&client, &*handler));
            }
        })
        .expect("failed to spawn IPC thread");
    Ok(())
}

/// Sends one request to the running rivewm and returns its response line.
pub fn request(line: &str) -> io::Result<String> {
    stream(line)?.next().unwrap_or_else(|| {
        Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "rivewm closed the connection without replying",
        ))
    })
}

/// Sends a request and returns every response line as it arrives, until
/// rivewm closes the connection. Used for `subscribe`.
pub fn stream(line: &str) -> io::Result<impl Iterator<Item = io::Result<String>>> {
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
    // A server that has hung up can also surface as "pipe not connected"
    // rather than a clean EOF; either way the stream is over.
    Ok(BufReader::new(pipe).lines().map_while(|line| match line {
        Err(err) if err.raw_os_error() == Some(ERROR_PIPE_NOT_CONNECTED.0 as i32) => None,
        other => Some(other),
    }))
}

fn wait_for_client(pipe: &File) -> bool {
    match unsafe { ConnectNamedPipe(raw(pipe), None) } {
        Ok(()) => true,
        // The client connected between create and connect: still fine.
        Err(err) => err.code() == ERROR_PIPE_CONNECTED.to_hresult(),
    }
}

fn serve_client(pipe: &File, handler: &impl Fn(&str, &mut Connection)) {
    let mut line = String::new();
    let mut reader = BufReader::new(pipe.take(MAX_REQUEST));
    if reader.read_line(&mut line).is_err() || line.is_empty() {
        return;
    }
    handler(line.trim_end(), &mut Connection { pipe });
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
