//! YouTube Music through an installed Chromium browser (Chrome, Chromium, Brave,
//! Edge) driven over the DevTools protocol on a pipe (`--remote-debugging-pipe`,
//! no TCP port). Used on Linux, where WebKitGTK has no WebAuthn: a real Chromium
//! brings hardware security keys, passkeys and phone sign-in.
//!
//! The browser runs with its own profile directory (the same `ytmusic-profile/`
//! as the helper), two windows (account page in app mode, the web player) and
//! the shared [`Core`] state machine. Downloads are denied, popups closed and
//! main-frame navigations outside the Google/YouTube hosts are failed before
//! they load, exactly like the other engine.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use fono8_ytm_core::protocol::{Command, Event};
use fono8_ytm_core::{host_of, navigation_error, scripts, Core, Engine, PageLoad, Purpose, ToolbarState, View};
use serde_json::{json, Value};

const BROWSERS: &[&str] = &[
    "google-chrome",
    "google-chrome-stable",
    "chromium",
    "chromium-browser",
    "brave-browser",
    "microsoft-edge-stable",
    "microsoft-edge",
];

#[cfg(target_os = "macos")]
const MAC_BROWSERS: &[&str] = &[
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
];

/// `FONO8_BROWSER`, then the first Chromium-based browser in /Applications (macOS) or on `PATH`.
pub fn find_browser() -> Option<PathBuf> {
    if let Some(custom) = std::env::var_os("FONO8_BROWSER") {
        let path = PathBuf::from(custom);
        return if path.is_file() { Some(path) } else { None };
    }
    #[cfg(target_os = "macos")]
    for candidate in MAC_BROWSERS {
        if Path::new(candidate).is_file() {
            return Some(PathBuf::from(candidate));
        }
    }
    let path = std::env::var_os("PATH")?;
    for name in BROWSERS {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Snap-confined browsers cannot read hidden directories under `$HOME`.
pub fn profile_dir_for(browser: &Path, default: &Path) -> PathBuf {
    crate::cdp::profile_dir_for(browser, default, "fono8-ytmusic-profile")
}

enum Incoming {
    Cdp(Value),
    Command(Command),
    Eof,
}

#[derive(Clone, Debug)]
enum Reply {
    Ignore,
    Eval(Purpose),
    Targets,
    Created(View),
    Attached(View),
    WindowFor(&'static str),
}

struct Target {
    target_id: String,
    session_id: String,
}

struct CdpEngine {
    writer: std::io::PipeWriter,
    next_id: u64,
    replies: HashMap<u64, Reply>,
    targets: HashMap<View, Target>,
    attaching: HashMap<View, String>,
    urls: HashMap<View, String>,
    pending_url: HashMap<View, String>,
    events: Sender<Event>,
    debug: bool,
}

impl CdpEngine {
    fn log(&self, message: impl FnOnce() -> String) {
        if self.debug {
            eprintln!("[fono8 chromium] {}", message());
        }
    }

    fn send(&mut self, session: Option<&str>, method: &str, params: Value, reply: Reply) {
        self.next_id += 1;
        let id = self.next_id;
        let mut message = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        self.replies.insert(id, reply);
        let mut bytes = message.to_string().into_bytes();
        bytes.push(0);
        if self.writer.write_all(&bytes).is_err() {
            self.log(|| "pipe write failed".into());
        }
    }

    fn session(&self, view: View) -> Option<String> {
        self.targets.get(&view).map(|t| t.session_id.clone())
    }

    fn view_of_session(&self, session: &str) -> Option<View> {
        self.targets.iter().find(|(_, t)| t.session_id == session).map(|(v, _)| *v)
    }

    fn view_of_target(&self, target: &str) -> Option<View> {
        self.targets.iter().find(|(_, t)| t.target_id == target).map(|(v, _)| *v)
    }

    fn create_target(&mut self, view: View) {
        if self.attaching.contains_key(&view) || self.targets.contains_key(&view) {
            return;
        }
        self.attaching.insert(view, String::new());
        // The player is a hidden target: no window of its own, audio still plays.
        let params = match view {
            View::Account => json!({"url": "about:blank", "newWindow": true}),
            View::Player => json!({"url": "about:blank", "hidden": true}),
        };
        self.send(None, "Target.createTarget", params, Reply::Created(view));
    }

    fn attach(&mut self, view: View, target_id: String) {
        self.attaching.insert(view, target_id.clone());
        self.send(None, "Target.attachToTarget", json!({"targetId": target_id, "flatten": true}), Reply::Attached(view));
    }

    fn attached(&mut self, view: View, session_id: String, core: &mut Core) {
        let target_id = self.attaching.remove(&view).unwrap_or_default();
        self.targets.insert(view, Target { target_id, session_id: session_id.clone() });
        self.send(Some(&session_id), "Page.enable", json!({}), Reply::Ignore);
        self.send(
            Some(&session_id),
            "Fetch.enable",
            json!({"patterns": [{"urlPattern": "*", "resourceType": "Document", "requestStage": "Request"}]}),
            Reply::Ignore,
        );
        if view == View::Player {
            self.send(
                Some(&session_id),
                "Page.addScriptToEvaluateOnNewDocument",
                json!({"source": scripts::MEDIA_GUARD}),
                Reply::Ignore,
            );
        }
        if let Some(url) = self.pending_url.remove(&view) {
            self.send(Some(&session_id), "Page.navigate", json!({"url": url}), Reply::Ignore);
        }
        if view == View::Account && !self.urls.contains_key(&View::Account) {
            self.urls.insert(View::Account, String::new());
            // Fono8's own panel is the interface; the browser window only matters for sign-in.
            self.window_state(View::Account, "minimized");
            core.start(self);
        }
    }

    fn window_state(&mut self, view: View, state: &'static str) {
        if let Some(target) = self.targets.get(&view) {
            let target_id = target.target_id.clone();
            self.send(None, "Browser.getWindowForTarget", json!({"targetId": target_id}), Reply::WindowFor(state));
        }
    }

    fn handle_reply(&mut self, id: u64, message: &Value, core: &mut Core) {
        let Some(reply) = self.replies.remove(&id) else { return };
        let result = message.get("result").cloned().unwrap_or(Value::Null);
        let error = message.get("error").cloned();
        match reply {
            Reply::Ignore => {}
            Reply::Eval(purpose) => {
                let value = result.get("result").and_then(|r| r.get("value")).cloned();
                let raw = match (value, result.get("exceptionDetails").is_some() || error.is_some()) {
                    (Some(value), false) => serde_json::to_string(&value).unwrap_or_else(|_| "null".into()),
                    _ => "null".into(),
                };
                core.script_reply(purpose, &raw, self);
            }
            Reply::Targets => {
                let page = result
                    .get("targetInfos")
                    .and_then(Value::as_array)
                    .and_then(|infos| infos.iter().find(|i| i.get("type").and_then(Value::as_str) == Some("page")))
                    .and_then(|i| i.get("targetId").and_then(Value::as_str))
                    .map(str::to_string);
                match page {
                    Some(target_id) => self.attach(View::Account, target_id),
                    None => self.create_target(View::Account),
                }
                self.create_target(View::Player);
            }
            Reply::Created(view) => match result.get("targetId").and_then(Value::as_str) {
                Some(target_id) => self.attach(view, target_id.to_string()),
                None => {
                    self.attaching.remove(&view);
                    self.log(|| format!("createTarget failed: {error:?}"));
                }
            },
            Reply::Attached(view) => match result.get("sessionId").and_then(Value::as_str) {
                Some(session_id) => self.attached(view, session_id.to_string(), core),
                None => {
                    self.attaching.remove(&view);
                    self.log(|| format!("attach failed: {error:?}"));
                }
            },
            Reply::WindowFor(state) => {
                if let Some(window_id) = result.get("windowId").and_then(Value::as_i64) {
                    self.send(
                        None,
                        "Browser.setWindowBounds",
                        json!({"windowId": window_id, "bounds": {"windowState": state}}),
                        Reply::Ignore,
                    );
                }
            }
        }
    }

    fn handle_event(&mut self, message: &Value, core: &mut Core) {
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let session = message.get("sessionId").and_then(Value::as_str).map(str::to_string);
        let view = session.as_deref().and_then(|s| self.view_of_session(s));
        match method {
            "Page.frameNavigated" => {
                let Some(view) = view else { return };
                let frame = params.get("frame").cloned().unwrap_or(Value::Null);
                if frame.get("parentId").is_some() {
                    return;
                }
                let url = frame.get("url").and_then(Value::as_str).unwrap_or("").to_string();
                self.urls.insert(view, url.clone());
                core.page_load(view, PageLoad::Started, &url, self);
            }
            "Page.loadEventFired" => {
                let Some(view) = view else { return };
                let url = self.urls.get(&view).cloned().unwrap_or_default();
                core.page_load(view, PageLoad::Finished, &url, self);
            }
            "Page.navigatedWithinDocument" => {
                let Some(view) = view else { return };
                let url = params.get("url").and_then(Value::as_str).unwrap_or("").to_string();
                self.urls.insert(view, url.clone());
                core.url_changed(view, &url, self);
            }
            "Fetch.requestPaused" => {
                let Some(session) = session else { return };
                let request_id = params.get("requestId").and_then(Value::as_str).unwrap_or("").to_string();
                let url = params.get("request").and_then(|r| r.get("url")).and_then(Value::as_str).unwrap_or("");
                let document = params.get("resourceType").and_then(Value::as_str) == Some("Document");
                match (document, navigation_error(url)) {
                    (true, Some(reason)) => {
                        let host: String = host_of(url).chars().take(200).collect();
                        self.send(
                            Some(&session),
                            "Fetch.failRequest",
                            json!({"requestId": request_id, "errorReason": "BlockedByClient"}),
                            Reply::Ignore,
                        );
                        if let Some(view) = view {
                            core.navigation_blocked(view, &host, reason, self);
                        }
                    }
                    _ => self.send(Some(&session), "Fetch.continueRequest", json!({"requestId": request_id}), Reply::Ignore),
                }
            }
            "Target.targetCreated" => {
                let info = params.get("targetInfo").cloned().unwrap_or(Value::Null);
                // Popups opened by the page are never allowed.
                if info.get("openerId").is_some() {
                    if let Some(target_id) = info.get("targetId").and_then(Value::as_str) {
                        self.send(None, "Target.closeTarget", json!({"targetId": target_id}), Reply::Ignore);
                    }
                }
            }
            "Target.targetDestroyed" => {
                let target_id = params.get("targetId").and_then(Value::as_str).unwrap_or("");
                if let Some(view) = self.view_of_target(target_id) {
                    self.targets.remove(&view);
                    self.urls.remove(&view);
                    match view {
                        View::Account => core.closed(),
                        View::Player => core.page_load(View::Player, PageLoad::Started, "about:blank", self),
                    }
                }
            }
            _ => {}
        }
    }
}

impl Engine for CdpEngine {
    fn eval(&mut self, view: View, script: &str, purpose: Purpose) {
        match self.session(view) {
            Some(session) => self.send(
                Some(&session),
                "Runtime.evaluate",
                json!({"expression": script, "returnByValue": true, "awaitPromise": true}),
                Reply::Eval(purpose),
            ),
            None => {
                // No page: answer with "nothing" so the core can time out or retry.
                core_reply_later(self, purpose);
            }
        }
    }

    fn run(&mut self, view: View, script: &str) {
        if let Some(session) = self.session(view) {
            self.send(Some(&session), "Runtime.evaluate", json!({"expression": script, "awaitPromise": false}), Reply::Ignore);
        }
    }

    fn load_url(&mut self, view: View, url: &str) {
        match self.session(view) {
            Some(session) => self.send(Some(&session), "Page.navigate", json!({"url": url}), Reply::Ignore),
            None => {
                self.pending_url.insert(view, url.to_string());
                self.create_target(view);
            }
        }
    }

    fn show(&mut self, _view: View) {
        // Only the account page has a window; the player is hidden.
        let view = View::Account;
        if !self.targets.contains_key(&view) {
            self.create_target(view);
            return;
        }
        self.window_state(view, "normal");
        if let Some(target) = self.targets.get(&view) {
            let target_id = target.target_id.clone();
            self.send(None, "Target.activateTarget", json!({"targetId": target_id}), Reply::Ignore);
        }
    }

    fn hide(&mut self) {
        self.window_state(View::Account, "minimized");
    }

    fn toolbar(&mut self, _state: &ToolbarState) {
        // Chromium shows its own windows; warnings reach Fono8 as `Event::Warning`.
    }
}

/// Scripts evaluated without a page must not leave the core waiting forever.
fn core_reply_later(engine: &mut CdpEngine, purpose: Purpose) {
    let _ = engine.events.send(Event::Warning { text: String::new() });
    engine.replies.insert(0, Reply::Eval(purpose));
}

/// A plain, uncontrolled browser window on the same profile for signing in.
///
/// Google refuses to sign in while DevTools automation is attached ("This browser or
/// app may not be secure"), so the sign-in happens in the unmodified browser; the
/// controlled session is started again afterwards and finds the saved cookies.
pub fn spawn_login(browser: &Path, profile: &Path) -> std::io::Result<Child> {
    std::fs::create_dir_all(profile)?;
    Process::new(browser)
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-background-mode")
        .arg("--window-size=900,680")
        .arg("--class=fono8-ytmusic")
        .arg(format!("--app={}", fono8_ytm_core::ORIGIN))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

/// `true` once the profile holds a Google session cookie for YouTube, i.e. the user
/// finished signing in. Reads a copy of the cookie database (names only, never values).
pub fn signed_in_cookie(profile: &Path) -> bool {
    // Newer Chrome builds keep the database under Default/Network.
    let Some(dir) =
        [profile.join("Default"), profile.join("Default").join("Network")].into_iter().find(|dir| dir.join("Cookies").is_file())
    else {
        return false;
    };
    let cookies = dir.join("Cookies");
    let scratch = std::env::temp_dir().join(format!("fono8-cookies-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&scratch);
    let copy = scratch.join("Cookies");
    if std::fs::copy(&cookies, &copy).is_err() {
        return false;
    }
    for suffix in ["-wal", "-journal"] {
        let source = dir.join(format!("Cookies{suffix}"));
        let target = scratch.join(format!("Cookies{suffix}"));
        if source.is_file() {
            let _ = std::fs::copy(&source, &target);
        } else {
            let _ = std::fs::remove_file(&target);
        }
    }
    let found = rusqlite::Connection::open_with_flags(&copy, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
        .and_then(|db| {
            db.query_row(
                "SELECT COUNT(*) FROM cookies WHERE name IN ('SAPISID', '__Secure-3PAPISID') AND host_key LIKE '%youtube.com'",
                [],
                |row| row.get::<_, i64>(0),
            )
        })
        .map(|count| count > 0)
        .unwrap_or(false);
    let _ = std::fs::remove_dir_all(&scratch);
    found
}

/// Ask the sign-in browser to quit (SIGTERM lets it flush the profile), then wait briefly.
pub fn finish_login(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    for _ in 0..50 {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Start the browser and the protocol thread. Commands come from `commands`, events go to `events`.
pub fn spawn(
    browser: &Path,
    profile: &Path,
    language: &str,
    commands: Receiver<Command>,
    events: Sender<Event>,
    mut meter: crate::meter::Meter,
) -> std::io::Result<Child> {
    std::fs::create_dir_all(profile)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(profile, std::fs::Permissions::from_mode(0o700));
    }
    let debug = std::env::var_os("FONO8_DEBUG").is_some();
    let mut process = Process::new(browser);
    process
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-sync")
        .arg("--disable-extensions")
        .arg("--disable-background-networking")
        .arg("--disable-background-mode")
        .arg("--autoplay-policy=no-user-gesture-required")
        .arg("--window-size=900,680")
        .arg("--class=fono8-ytmusic")
        .arg("--app=about:blank")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(if debug { Stdio::inherit() } else { Stdio::null() });
    let (child, command_writer, event_reader) = crate::cdp::launch(process)?;
    let messages = crate::cdp::read_messages(event_reader, "fono8-cdp-reader")?;

    let (incoming, inbox) = mpsc::channel::<Incoming>();
    let reader_inbox = incoming.clone();
    std::thread::Builder::new().name("fono8-cdp-forward".into()).spawn(move || {
        for message in messages {
            let item = match message {
                Some(value) => Incoming::Cdp(value),
                None => Incoming::Eof,
            };
            let end = matches!(item, Incoming::Eof);
            if reader_inbox.send(item).is_err() || end {
                return;
            }
        }
        let _ = reader_inbox.send(Incoming::Eof);
    })?;
    let command_inbox = incoming;
    std::thread::Builder::new().name("fono8-cdp-commands".into()).spawn(move || {
        for command in commands {
            if command_inbox.send(Incoming::Command(command)).is_err() {
                break;
            }
        }
    })?;

    let persistent = true;
    let language = language.to_string();
    std::thread::Builder::new().name("fono8-cdp".into()).spawn(move || {
        let mut engine = CdpEngine {
            writer: command_writer,
            next_id: 0,
            replies: HashMap::new(),
            targets: HashMap::new(),
            attaching: HashMap::new(),
            urls: HashMap::new(),
            pending_url: HashMap::new(),
            events: events.clone(),
            debug,
        };
        let mut core = Core::new(persistent, &language);
        if debug {
            core.set_logger(|message| eprintln!("[fono8 chromium] {message}"));
        }
        engine.send(None, "Target.setDiscoverTargets", json!({"discover": true}), Reply::Ignore);
        engine.send(None, "Browser.setDownloadBehavior", json!({"behavior": "deny"}), Reply::Ignore);
        engine.send(None, "Target.getTargets", json!({}), Reply::Targets);
        let tick = Duration::from_millis(40);
        let mut next_tick = Instant::now() + tick;
        loop {
            let wait = next_tick.saturating_duration_since(Instant::now());
            match inbox.recv_timeout(wait) {
                Ok(Incoming::Cdp(message)) => {
                    if let Some(id) = message.get("id").and_then(Value::as_u64) {
                        engine.handle_reply(id, &message, &mut core);
                    } else {
                        engine.handle_event(&message, &mut core);
                    }
                }
                Ok(Incoming::Command(Command::Quit)) => {
                    core.command(Command::Quit, &mut engine);
                    engine.send(None, "Browser.close", json!({}), Reply::Ignore);
                    for event in core.take_events() {
                        let _ = events.send(event);
                    }
                    break;
                }
                Ok(Incoming::Command(command)) => core.command(command, &mut engine),
                Ok(Incoming::Eof) => break,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            // Scripts queued without a page answer "null" on the next loop.
            if let Some(Reply::Eval(purpose)) = engine.replies.remove(&0) {
                core.script_reply(purpose, "null", &mut engine);
            }
            if Instant::now() >= next_tick {
                next_tick = Instant::now() + tick;
                core.tick(&mut engine);
            }
            for event in core.take_events() {
                if matches!(event, Event::Warning { ref text } if text.is_empty()) {
                    continue;
                }
                if let Event::Levels { bands } = &event {
                    meter.push(bands);
                    continue;
                }
                if events.send(event).is_err() {
                    return;
                }
            }
        }
    })?;
    Ok(child)
}
