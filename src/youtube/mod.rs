//! Experimental YouTube Music integration: a helper process hosts the web
//! session; Fono8 keeps the mixed queue, the imported playlists and this panel state.

#[cfg(target_os = "linux")]
pub mod chromium;
pub mod models;
pub mod provider;
pub mod session;

use std::collections::HashSet;

use serde_json::Value;

use crate::i18n::Message;
use crate::library::Track;
use models::RemotePlaylist;
use provider::{Operation, Outcome, Step};
pub use session::{HelperEvent, HelperLink, Session};

#[derive(Clone, Debug, PartialEq)]
pub enum ResultItem {
    Track(Track),
    Playlist(RemotePlaylist),
}

/// Panel and transport state plus session bookkeeping.
pub struct YouTube {
    pub session: Option<Session>,
    pub operation: Option<Operation>,
    pub busy: bool,
    pub message: Message,
    pub results: Vec<ResultItem>,
    pub selected: HashSet<usize>,
    /// Target playlist for "Add selected"; `None` means the whole library.
    pub target: Option<i64>,
    pub forget_pending: bool,
    /// A plain browser window used for signing in (Chromium engine); the session resumes when it closes.
    pub login: Option<std::process::Child>,
    /// Sign-in detected in the profile; the window is closed a moment later.
    pub login_detected: Option<std::time::Instant>,
    pub last_cookie_check: Option<std::time::Instant>,
    /// An operation to run again once the account page is ready.
    pub retry: Option<Request>,
    /// The running request is an import search: its outcome goes to the import, not to Discover.
    pub matching: bool,
}

/// What the user asked for, kept so it can be retried after the page finishes loading.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    Search(String),
    Playlists,
    Import(String, bool),
}

impl Default for YouTube {
    fn default() -> Self {
        YouTube {
            session: None,
            operation: None,
            busy: false,
            message: Message::new("yt_intro"),
            results: Vec::new(),
            selected: HashSet::new(),
            target: None,
            forget_pending: false,
            login: None,
            login_detected: None,
            last_cookie_check: None,
            retry: None,
            matching: false,
        }
    }
}

impl YouTube {
    /// `yt_auth_*` key suffix for the sidebar and panel status.
    pub fn auth_state(&self) -> &str {
        self.session.as_ref().map(|s| s.auth_state.as_str()).unwrap_or("unchecked")
    }

    pub fn persistent(&self) -> bool {
        self.session.as_ref().map(|s| s.persistent).unwrap_or(false)
    }

    pub fn start(&mut self, operation: Operation, step: Step) -> Option<Outcome> {
        self.busy = true;
        self.message = Message::new("yt_loading");
        self.operation = Some(operation);
        self.advance(step)
    }

    /// Send the next request, or finish. Returns the outcome when the operation is complete.
    fn advance(&mut self, step: Step) -> Option<Outcome> {
        match step {
            Step::Request(endpoint, body) => match self.session.as_mut() {
                Some(session) => {
                    session.request(endpoint, body, 0);
                    None
                }
                None => {
                    self.operation = None;
                    self.busy = false;
                    Some(Outcome::Failed(Message::new("yt_open_account")))
                }
            },
            Step::Done(outcome) => {
                self.operation = None;
                self.busy = false;
                Some(outcome)
            }
        }
    }

    /// Feed a transport response to the in-flight operation.
    pub fn respond(&mut self, response: Result<Value, Message>) -> Option<Outcome> {
        let mut operation = self.operation.take()?;
        let step = operation.respond(response);
        self.operation = Some(operation);
        self.advance(step)
    }

    pub fn fail(&mut self, message: Message) {
        self.operation = None;
        self.busy = false;
        self.message = message;
    }

    pub fn set_results(&mut self, items: Vec<ResultItem>) {
        self.selected.clear();
        self.message = Message::new("yt_results").with("n", items.len());
        self.results = items;
    }

    pub fn selected_items(&self) -> Vec<ResultItem> {
        let mut indices: Vec<usize> = self.selected.iter().copied().collect();
        indices.sort_unstable();
        indices.into_iter().filter_map(|i| self.results.get(i).cloned()).collect()
    }
}
