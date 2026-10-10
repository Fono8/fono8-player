//! A YouTube Music engine session: either the `fono8-web` helper process
//! (JSON lines over stdin/stdout) or, on Linux, an installed Chromium browser
//! driven over DevTools. Both speak [`fono8_ytm_core::protocol`].

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use fono8_ytm_core::protocol::{Command as HelperCommand, Event, Texts};
use serde_json::Value;

use crate::i18n::Translator;
use crate::meter::{Levels, Meter};

pub type HelperEvent = Event;

/// Where the helper binary lives: next to the executable, or in the build tree during development.
pub fn helper_path() -> Option<PathBuf> {
    let name = if cfg!(windows) { "fono8-web.exe" } else { "fono8-web" };
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    // Next to the executable, in the app bundle's Resources, or one level up (cargo test binaries live in deps/).
    let candidates = [dir.join(name), dir.join("..").join("Resources").join(name), dir.join("..").join(name)];
    candidates.into_iter().find(|p| p.is_file())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineKind {
    /// The `fono8-web` helper (WebKitGTK, WKWebView or WebView2).
    Helper,
    /// An installed Chromium-based browser over DevTools (Linux).
    Chromium,
}

#[derive(Clone)]
enum Transport {
    Pipe(Arc<Mutex<Option<std::process::ChildStdin>>>),
    Channel(Sender<HelperCommand>),
}

/// A thread-safe handle for sending commands; shared with the playback engine.
#[derive(Clone)]
pub struct HelperLink {
    transport: Transport,
}

impl HelperLink {
    fn send(&self, command: HelperCommand) -> bool {
        match &self.transport {
            Transport::Pipe(stdin) => {
                let Ok(mut guard) = stdin.lock() else { return false };
                let Some(stdin) = guard.as_mut() else { return false };
                let Ok(line) = serde_json::to_string(&command) else { return false };
                writeln!(stdin, "{line}").and_then(|_| stdin.flush()).is_ok()
            }
            Transport::Channel(sender) => sender.send(command).is_ok(),
        }
    }

    pub fn load(&self, video: &str) -> bool {
        self.send(HelperCommand::Load { video: video.to_string() })
    }

    pub fn play(&self) {
        self.send(HelperCommand::Play);
    }

    pub fn pause(&self) {
        self.send(HelperCommand::Pause);
    }

    pub fn stop(&self) {
        self.send(HelperCommand::Stop);
    }

    pub fn seek(&self, ms: u64) {
        self.send(HelperCommand::Seek { ms });
    }

    pub fn set_volume(&self, value: f32) {
        self.send(HelperCommand::Volume { value });
    }

    pub fn set_muted(&self, muted: bool) {
        self.send(HelperCommand::Muted { muted });
    }
}

pub struct Session {
    child: Child,
    link: HelperLink,
    events: Receiver<Event>,
    pub kind: EngineKind,
    pub auth_state: String,
    pub persistent: bool,
    next_id: u64,
    /// Request ids still waiting for a response, with the caller's tag.
    pub pending: HashMap<String, u64>,
    pub profile_dir: PathBuf,
}

/// Which engine to use: `FONO8_YTM_ENGINE=webkit|helper|chromium` overrides the default
/// (Chromium when a browser is installed on Linux or macOS, the helper elsewhere).
pub fn preferred_engine() -> EngineKind {
    match std::env::var("FONO8_YTM_ENGINE").unwrap_or_default().to_ascii_lowercase().as_str() {
        "chromium" | "chrome" => EngineKind::Chromium,
        "webkit" | "helper" | "wry" => EngineKind::Helper,
        _ => {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                if super::chromium::find_browser().is_some() {
                    return EngineKind::Chromium;
                }
            }
            EngineKind::Helper
        }
    }
}

impl Session {
    /// `levels` receives the page's band levels while YouTube Music plays (the animated logo).
    pub fn spawn(profile_dir: &Path, i18n: &Translator, levels: Levels) -> Result<Session, &'static str> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let session = match preferred_engine() {
            EngineKind::Chromium => Self::spawn_chromium(profile_dir, i18n, levels.clone())
                .or_else(|_| Self::spawn_helper(profile_dir, i18n, levels))?,
            EngineKind::Helper => Self::spawn_helper(profile_dir, i18n, levels)?,
        };
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let session = {
            let _ = preferred_engine();
            Self::spawn_helper(profile_dir, i18n, levels)?
        };
        session.link.send(HelperCommand::Init { texts: texts(i18n), language: i18n.language.to_string() });
        Ok(session)
    }

    fn spawn_helper(profile_dir: &Path, i18n: &Translator, levels: Levels) -> Result<Session, &'static str> {
        let helper = helper_path().ok_or("yt_browser_error")?;
        let mut child = Command::new(helper)
            .arg("--profile")
            .arg(profile_dir)
            .arg("--language")
            .arg(i18n.language)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|_| "yt_browser_error")?;
        let stdin = child.stdin.take().ok_or("yt_browser_error")?;
        let stdout = child.stdout.take().ok_or("yt_browser_error")?;
        let (sender, events) = mpsc::channel();
        std::thread::Builder::new()
            .name("fono8-web-reader".into())
            .spawn(move || reader(stdout, sender, Meter::new(levels)))
            .map_err(|_| "yt_browser_error")?;
        Ok(Session {
            child,
            link: HelperLink { transport: Transport::Pipe(Arc::new(Mutex::new(Some(stdin)))) },
            events,
            kind: EngineKind::Helper,
            auth_state: "checking".into(),
            persistent: false,
            next_id: 0,
            pending: HashMap::new(),
            profile_dir: profile_dir.to_path_buf(),
        })
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn spawn_chromium(profile_dir: &Path, i18n: &Translator, levels: Levels) -> Result<Session, &'static str> {
        let browser = super::chromium::find_browser().ok_or("yt_browser_error")?;
        let profile = super::chromium::profile_dir_for(&browser, profile_dir);
        let (commands, command_receiver) = mpsc::channel();
        let (sender, events) = mpsc::channel();
        let child = super::chromium::spawn(&browser, &profile, i18n.language, command_receiver, sender, Meter::new(levels))
            .map_err(|_| "yt_browser_error")?;
        crate::app::debug(|| format!("youtube: using {} with profile {}", browser.display(), profile.display()));
        Ok(Session {
            child,
            link: HelperLink { transport: Transport::Channel(commands) },
            events,
            kind: EngineKind::Chromium,
            auth_state: "checking".into(),
            persistent: true,
            next_id: 0,
            pending: HashMap::new(),
            profile_dir: profile,
        })
    }

    pub fn link(&self) -> HelperLink {
        self.link.clone()
    }

    pub fn retranslate(&self, i18n: &Translator) {
        self.link.send(HelperCommand::Init { texts: texts(i18n), language: i18n.language.to_string() });
    }

    pub fn show(&self, tab: u8) {
        self.link.send(HelperCommand::Show { tab });
    }

    #[allow(dead_code)]
    pub fn hide(&self) {
        self.link.send(HelperCommand::Hide);
    }

    pub fn refresh_auth(&self) {
        self.link.send(HelperCommand::RefreshAuth);
    }

    /// Queue an API request; the response arrives as `Event::Response` with this id.
    pub fn request(&mut self, endpoint: &str, body: Value, tag: u64) -> String {
        self.next_id += 1;
        let id = format!("q{}", self.next_id);
        self.pending.insert(id.clone(), tag);
        if !self.link.send(HelperCommand::Request { id: id.clone(), endpoint: endpoint.to_string(), body }) {
            self.pending.remove(&id);
        }
        id
    }

    pub fn cancel(&mut self) {
        self.pending.clear();
        self.link.send(HelperCommand::Cancel);
    }

    /// Drain queued events; synthesizes `Exited` once the engine process is gone.
    pub fn poll(&mut self) -> Vec<Event> {
        let mut events: Vec<Event> = self.events.try_iter().collect();
        for event in &events {
            if let Event::Auth { state } = event {
                self.auth_state = state.clone();
            }
            if let Event::Ready { persistent } = event {
                self.persistent = *persistent;
            }
        }
        if let Ok(Some(_)) = self.child.try_wait() {
            if let Transport::Pipe(stdin) = &self.link.transport {
                if let Ok(mut guard) = stdin.lock() {
                    guard.take();
                }
            }
            events.push(Event::Exited);
        }
        events
    }

    pub fn close(&mut self) {
        self.link.send(HelperCommand::Quit);
        if let Transport::Pipe(stdin) = &self.link.transport {
            if let Ok(mut guard) = stdin.lock() {
                guard.take();
            }
        }
        for _ in 0..50 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}

/// Helper events go to the model, except band levels, which go straight to the meter.
fn reader(stdout: std::process::ChildStdout, sender: Sender<Event>, mut meter: Meter) {
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Event>(&line) {
            Ok(Event::Levels { bands }) => meter.push(&bands),
            Ok(event) => {
                if sender.send(event).is_err() {
                    break;
                }
            }
            Err(error) => crate::app::debug(|| format!("youtube: unreadable helper event: {error}")),
        }
    }
}

fn texts(i18n: &Translator) -> Texts {
    let t = |key: &str| i18n.t(key);
    Texts {
        home: t("yt_home"),
        account_tab: t("yt_account_tab"),
        player_tab: t("yt_player_tab"),
        session_short: t("yt_session_short"),
        session_temporary: t("yt_session_temporary"),
        session_notice: t("yt_session_notice"),
        auth: ["checking", "signed_in", "signed_out", "unknown"]
            .iter()
            .map(|s| (s.to_string(), t(&format!("yt_auth_{s}"))))
            .collect(),
        navigation_blocked: t("yt_navigation_blocked"),
        navigation_reasons: ["yt_nav_invalid", "yt_nav_protocol", "yt_nav_host", "yt_nav_port", "yt_nav_credentials"]
            .iter()
            .map(|s| (s.to_string(), t(s)))
            .collect(),
    }
}

/// Remove the saved browser profile ("Disconnect and clear session").
pub fn forget_profile(path: &Path) -> std::io::Result<()> {
    if path.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(false) {
        std::fs::remove_file(path)
    } else if path.exists() {
        std::fs::remove_dir_all(path)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::{Duration, Instant};

    fn exercise(mut session: Session) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
            for event in session.poll() {
                println!("{event:?}");
                seen.push(event);
            }
            if seen.iter().any(|e| matches!(e, Event::Auth { state } if state != "checking")) {
                break;
            }
        }
        assert!(seen.iter().any(|e| matches!(e, Event::Ready { .. })));
        assert!(seen.iter().any(|e| matches!(e, Event::Navigation)));
        assert!(!seen.iter().any(|e| matches!(e, Event::Exited)));
        session.request("search", json!({"query": "x"}), 0);
        let deadline = Instant::now() + Duration::from_secs(25);
        let mut response = None;
        while Instant::now() < deadline && response.is_none() {
            std::thread::sleep(Duration::from_millis(200));
            response = session.poll().into_iter().find(|e| matches!(e, Event::Response { .. }));
        }
        println!("search: {response:?}");
        assert!(response.is_some(), "no response from the engine");
        session.close();
    }

    /// Starts the real helper (needs a desktop and network); `cargo test helper -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn helper_starts_reports_ready_and_navigates() {
        let dir = std::env::temp_dir().join(format!("fono8-yt-test-{}", std::process::id()));
        let session = Session::spawn_helper(&dir, &Translator::new("en"), Levels::default())
            .expect("helper binary next to the test binary");
        exercise(session);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Drives an installed Chromium browser; `cargo test chromium -- --ignored --nocapture`.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    #[ignore]
    fn chromium_starts_reports_ready_and_navigates() {
        // FONO8_YTM_TEST_PROFILE points at a copy of a signed-in profile to test searches.
        let dir = std::env::var_os("FONO8_YTM_TEST_PROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join(format!("fono8-chromium-test-{}", std::process::id())));
        let session =
            Session::spawn_chromium(&dir, &Translator::new("en"), Levels::default()).expect("a Chromium browser on PATH");
        assert_eq!(session.kind, EngineKind::Chromium);
        exercise(session);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
