//! TIDAL's OAuth endpoints for the shared PKCE flow in [`crate::oauth`].

use crate::oauth::Provider;

use super::{REDIRECT_URI, SCOPES};

/// TIDAL Client IDs are short alphanumeric strings (case-sensitive).
pub fn is_client_id(value: &str) -> bool {
    (8..=64).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_alphanumeric())
}

pub static PROVIDER: Provider = Provider {
    authorize_url: "https://login.tidal.com/authorize",
    token_url: "https://auth.tidal.com/v1/oauth2/token",
    redirect_uri: REDIRECT_URI,
    callback_path: "/tidal/callback",
    scopes: SCOPES,
    extra: &[],
    is_client_id,
    lowercase_client_id: false,
    client_invalid: "tidal_client_invalid",
    auth_failed: "tidal_auth_failed",
    rate_limit: "tidal_rate_limit",
    login_denied: "tidal_login_denied",
    login_timeout: "tidal_login_timeout",
    login_required: "tidal_login_required",
    session_expired: "tidal_session_expired",
    keyring_unavailable: "tidal_keyring_unavailable",
    port_busy: "tidal_port_busy",
};
