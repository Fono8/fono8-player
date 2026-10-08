//! The command/event protocol between Fono8 and a YouTube Music engine.
//!
//! The `fono8-web` helper speaks it as JSON lines over stdin/stdout; the
//! in-process Chromium backend passes the same values over channels.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Translated strings for the engine's own UI, sent with `init` and after a language change.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Texts {
    pub home: String,
    pub account_tab: String,
    pub player_tab: String,
    pub session_short: String,
    pub session_temporary: String,
    pub session_notice: String,
    /// `yt_auth_*` texts keyed by state: checking, signed_in, signed_out, unknown.
    #[serde(default)]
    pub auth: HashMap<String, String>,
    /// `yt_navigation_blocked` with `{host}` and `{reason}` placeholders.
    #[serde(default)]
    pub navigation_blocked: String,
    /// `yt_nav_*` texts keyed by reason.
    #[serde(default)]
    pub navigation_reasons: HashMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    Init {
        texts: Texts,
        language: String,
    },
    Show {
        #[serde(default)]
        tab: u8,
    },
    Hide,
    Request {
        id: String,
        endpoint: String,
        body: Value,
    },
    Cancel,
    RefreshAuth,
    Load {
        video: String,
    },
    Play,
    Pause,
    Stop,
    Seek {
        ms: u64,
    },
    Volume {
        value: f32,
    },
    Muted {
        muted: bool,
    },
    Quit,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Ready {
        persistent: bool,
    },
    Auth {
        state: String,
    },
    Navigation,
    Response {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    Player {
        state: String,
        position: u64,
        duration: u64,
    },
    Ended,
    /// Loudness of five frequency bands (dB, bass first) while playing, ~25 times a second.
    Levels {
        bands: Vec<f32>,
    },
    Error {
        key: String,
    },
    /// A blocked navigation or similar notice to show in Fono8 (engines without their own toolbar).
    Warning {
        text: String,
    },
    Closed,
    /// Synthesized by Fono8 when the engine process is gone.
    Exited,
}
