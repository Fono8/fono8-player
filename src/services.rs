//! Streaming services share the same screens: an account card in Settings →
//! Accounts and the mixed Discover page. Each service fills these views from its
//! own state and handles the [`ServiceAction`]s they send back (see `app.rs`).

use gpui::Entity;

use crate::ui::text_input::TextInput;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Service {
    YouTube,
    Spotify,
    /// Import only: TIDAL lists are matched to YouTube Music / Spotify tracks.
    Tidal,
}

impl Service {
    /// Display order in Settings and Discover.
    pub const ALL: [Service; 3] = [Service::YouTube, Service::Spotify, Service::Tidal];

    pub fn name(self) -> &'static str {
        match self {
            Service::YouTube => "YouTube Music",
            Service::Spotify => "Spotify",
            Service::Tidal => "TIDAL",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Service::YouTube => "youtube",
            Service::Spotify => "spotify",
            Service::Tidal => "tidal",
        }
    }

    /// A short id for element ids.
    pub fn key(self) -> &'static str {
        match self {
            Service::YouTube => "yt",
            Service::Spotify => "sp",
            Service::Tidal => "td",
        }
    }

    /// Part of the "All" search on Discover (TIDAL is only an import source).
    pub fn searchable(self) -> bool {
        !matches!(self, Service::Tidal)
    }

    /// The service has a "Liked songs" collection Fono8 can import.
    pub fn has_liked(self) -> bool {
        matches!(self, Service::Spotify)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ServiceAction {
    /// Sign in, open the account page or change the account.
    Account,
    /// Sign out or disconnect and clear the session.
    Disconnect,
    /// Sign in with the value of the setup field (Spotify Client ID).
    Setup,
}

#[derive(Clone, Debug)]
pub struct Button {
    pub action: ServiceAction,
    pub icon: &'static str,
    pub caption: String,
    pub enabled: bool,
    pub accent: bool,
}

impl Button {
    pub fn new(action: ServiceAction, icon: &'static str, caption: String) -> Button {
        Button { action, icon, caption, enabled: true, accent: false }
    }

    pub fn enabled(mut self, enabled: bool) -> Button {
        self.enabled = enabled;
        self
    }

    pub fn accent(mut self, accent: bool) -> Button {
        self.accent = accent;
        self
    }
}

/// One card in Settings → Accounts.
pub struct AccountView {
    pub service: Service,
    /// Account and session lines.
    pub lines: Vec<String>,
    pub buttons: Vec<Button>,
    /// A field and button shown before the service can be used (Spotify Client ID), with a hint.
    pub setup: Option<(Entity<TextInput>, Button, String)>,
    /// The last account-related message.
    pub status: String,
}

/// One result row on the Discover page.
#[derive(Clone, Debug)]
pub struct Row {
    pub service: Service,
    pub title: String,
    pub subtitle: String,
    /// The raw artist text of track rows (for artist links).
    pub artist: Option<String>,
    /// A library path whose artwork is shown (tracks only).
    pub cover: Option<String>,
    pub selected: bool,
    /// Rows that cannot be imported are shown muted.
    pub enabled: bool,
}
