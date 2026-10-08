//! Spotify's OAuth endpoints for the shared PKCE flow in [`crate::oauth`].

use crate::oauth::Provider;

use super::{REDIRECT_URI, SCOPES};

pub fn is_client_id(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

pub static PROVIDER: Provider = Provider {
    authorize_url: "https://accounts.spotify.com/authorize",
    token_url: "https://accounts.spotify.com/api/token",
    redirect_uri: REDIRECT_URI,
    callback_path: "/callback",
    scopes: SCOPES,
    // Always show the consent page, so "Not you?" can switch to another account.
    extra: &[("show_dialog", "true")],
    is_client_id,
    lowercase_client_id: true,
    client_invalid: "spotify_client_invalid",
    auth_failed: "spotify_auth_failed",
    rate_limit: "spotify_rate_limit",
    login_denied: "spotify_login_denied",
    login_timeout: "spotify_login_timeout",
    login_required: "spotify_login_required",
    session_expired: "spotify_session_expired",
    keyring_unavailable: "spotify_keyring_unavailable",
    port_busy: "spotify_port_busy",
};
