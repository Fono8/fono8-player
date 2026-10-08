//! Launching a Chromium-based browser with the DevTools protocol on a pipe
//! (`--remote-debugging-pipe`: fd 3 carries commands, fd 4 replies, each
//! message terminated by NUL). Shared by YouTube Music and Spotify.

use std::io::{PipeReader, PipeWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// Start `process` (already given its arguments, without `--remote-debugging-pipe`)
/// with the DevTools pipe attached. Returns the child, the command writer and the reply reader.
pub fn launch(mut process: Command) -> std::io::Result<(Child, PipeWriter, PipeReader)> {
    let (command_reader, command_writer) = std::io::pipe()?;
    let (event_reader, event_writer) = std::io::pipe()?;
    process.arg("--remote-debugging-pipe");
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        let read_fd = command_reader.as_raw_fd();
        let write_fd = event_writer.as_raw_fd();
        // SAFETY: only async-signal-safe calls (dup, dup2, fcntl) between fork and exec.
        // The pipe ends may already sit on 3/4 with close-on-exec set, so go through
        // fresh duplicates and clear the flag explicitly.
        unsafe {
            process.pre_exec(move || {
                let reader = libc::dup(read_fd);
                let writer = libc::dup(write_fd);
                if reader < 0 || writer < 0 || libc::dup2(reader, 3) < 0 || libc::dup2(writer, 4) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::fcntl(3, libc::F_SETFD, 0);
                libc::fcntl(4, libc::F_SETFD, 0);
                Ok(())
            });
        }
    }
    #[cfg(not(unix))]
    {
        // Windows passes pipe handles differently; not supported yet.
        let _ = (&command_reader, &event_writer, &mut process);
        return Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "DevTools pipe is not supported on this platform"));
    }
    #[allow(unreachable_code)]
    {
        let child = process.spawn()?;
        drop(command_reader);
        drop(event_writer);
        Ok((child, command_writer, event_reader))
    }
}

/// Split the reply stream into JSON messages on a reader thread; `None` marks the end.
pub fn read_messages(reader: PipeReader, name: &str) -> std::io::Result<Receiver<Option<Value>>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new().name(name.into()).spawn(move || {
        let mut reader = reader;
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 65536];
        loop {
            let n = match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            buffer.extend_from_slice(&chunk[..n]);
            while let Some(end) = buffer.iter().position(|b| *b == 0) {
                let message: Vec<u8> = buffer.drain(..=end).collect();
                if let Ok(value) = serde_json::from_slice::<Value>(&message[..message.len() - 1]) {
                    if sender.send(Some(value)).is_err() {
                        return;
                    }
                }
            }
        }
        let _ = sender.send(None);
    })?;
    Ok(receiver)
}

/// A synchronous DevTools client: one call at a time, events are skipped.
pub struct Connection {
    writer: PipeWriter,
    replies: Receiver<Option<Value>>,
    next_id: u64,
    closed: bool,
}

impl Connection {
    pub fn new(writer: PipeWriter, replies: Receiver<Option<Value>>) -> Connection {
        Connection { writer, replies, next_id: 0, closed: false }
    }

    pub fn closed(&self) -> bool {
        self.closed
    }

    /// Send a command and wait for its reply (`result`), or `None` on error, timeout or exit.
    pub fn call(&mut self, session: Option<&str>, method: &str, params: Value, timeout: Duration) -> Option<Value> {
        if self.closed {
            return None;
        }
        self.next_id += 1;
        let id = self.next_id;
        let mut message = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        let mut bytes = message.to_string().into_bytes();
        bytes.push(0);
        if self.writer.write_all(&bytes).is_err() {
            self.closed = true;
            return None;
        }
        let deadline = Instant::now() + timeout;
        loop {
            let wait = deadline.saturating_duration_since(Instant::now());
            match self.replies.recv_timeout(wait) {
                Ok(Some(reply)) if reply.get("id").and_then(Value::as_u64) == Some(id) => {
                    return if reply.get("error").is_some() { None } else { reply.get("result").cloned() };
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(RecvTimeoutError::Disconnected) => {
                    self.closed = true;
                    return None;
                }
                Err(RecvTimeoutError::Timeout) => return None,
            }
        }
    }

    /// Evaluate `expression` in a page session and return its JSON value.
    pub fn evaluate(&mut self, session: &str, expression: &str) -> Option<Value> {
        let result = self.call(
            Some(session),
            "Runtime.evaluate",
            json!({"expression": expression, "returnByValue": true, "awaitPromise": true}),
            Duration::from_secs(10),
        )?;
        if result.get("exceptionDetails").is_some() {
            return None;
        }
        result.get("result").and_then(|r| r.get("value")).cloned()
    }

    /// Ask the browser to quit.
    pub fn close_browser(&mut self) {
        if !self.closed {
            let _ = self.call(None, "Browser.close", json!({}), Duration::from_secs(2));
            self.closed = true;
        }
    }
}

/// Snap-confined browsers cannot read hidden directories under `$HOME`.
pub fn profile_dir_for(browser: &Path, default: &Path, name: &str) -> PathBuf {
    let snap = browser.starts_with("/snap/") || std::fs::read_link(browser).map(|t| t.starts_with("/snap/")).unwrap_or(false);
    if snap {
        if let (Some(home), Some(browser_name)) = (dirs::home_dir(), browser.file_name()) {
            return home.join("snap").join(browser_name).join("common").join(name);
        }
    }
    default.to_path_buf()
}
