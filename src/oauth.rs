//! PKCE authorization-code flow without a client secret, as a pure state
//! machine shared by the streaming services: each one describes its endpoints
//! in a [`Provider`]; the caller performs the HTTP requests.

use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde_json::Value;
use sha2::{Digest, Sha256};

use std::sync::Arc;

use crate::i18n::Message;
use crate::keystore::TokenStore;
use crate::loopback::Loopback;
use crate::net::{Body, Method, Response, Transport};

/// One service's OAuth endpoints, scopes and messages.
pub struct Provider {
    pub authorize_url: &'static str,
    pub token_url: &'static str,
    pub redirect_uri: &'static str,
    /// The path of `redirect_uri` on the loopback server.
    pub callback_path: &'static str,
    pub scopes: &'static str,
    /// Extra authorization parameters (Spotify: `show_dialog=true`).
    pub extra: &'static [(&'static str, &'static str)],
    pub is_client_id: fn(&str) -> bool,
    /// Client IDs are case-insensitive hex (Spotify) and kept lowercase.
    pub lowercase_client_id: bool,
    pub client_invalid: &'static str,
    pub auth_failed: &'static str,
    pub rate_limit: &'static str,
    pub login_denied: &'static str,
    pub login_timeout: &'static str,
    pub login_required: &'static str,
    pub session_expired: &'static str,
    pub keyring_unavailable: &'static str,
    pub port_busy: &'static str,
}

/// How long the browser sign-in may take.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(180);
/// Refresh this long before the access token expires.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn random_token(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    rand::fill(&mut buffer);
    URL_SAFE_NO_PAD.encode(buffer)
}

/// Constant-time comparison for the `state` parameter.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

struct Pending {
    state: String,
    verifier: String,
    started: Instant,
}

#[derive(Debug, PartialEq)]
pub enum Callback {
    /// Not a callback for the sign-in in progress (wrong path, state or a replay).
    Ignored,
    /// The user cancelled or Spotify refused.
    Denied,
    /// Exchange the code: POST this form to the provider's token URL.
    Exchange(Body),
}

pub struct Auth {
    provider: &'static Provider,
    client_id: String,
    pending: Option<Pending>,
    access: Option<(String, Instant)>,
    refresh: Option<String>,
}

impl Auth {
    pub fn new(provider: &'static Provider) -> Auth {
        Auth { provider, client_id: String::new(), pending: None, access: None, refresh: None }
    }

    fn normalize(&self, client_id: &str) -> String {
        let client_id = client_id.trim();
        if self.provider.lowercase_client_id {
            client_id.to_ascii_lowercase()
        } else {
            client_id.to_string()
        }
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    #[cfg(test)]
    pub fn signing_in(&self) -> bool {
        self.pending.is_some()
    }

    pub fn signed_in(&self) -> bool {
        self.refresh.is_some() || self.access.is_some()
    }

    /// The `state` value the loopback server should accept, if a sign-in is in progress.
    pub fn expected_state(&self) -> Option<String> {
        self.pending.as_ref().map(|p| p.state.clone())
    }

    pub fn refresh_token(&self) -> Option<&str> {
        self.refresh.as_deref()
    }

    /// Start a sign-in; returns the authorization URL to open in the system browser.
    pub fn begin(&mut self, client_id: &str, now: Instant) -> Result<String, Message> {
        let provider = self.provider;
        if !(provider.is_client_id)(client_id.trim()) {
            return Err(Message::new(provider.client_invalid));
        }
        self.disconnect();
        self.client_id = self.normalize(client_id);
        let pending = Pending { state: random_token(32), verifier: random_token(64), started: now };
        let mut query = form_urlencoded::Serializer::new(String::new());
        query
            .append_pair("client_id", &self.client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", provider.redirect_uri)
            .append_pair("state", &pending.state)
            .append_pair("scope", provider.scopes)
            .append_pair("code_challenge_method", "S256")
            .append_pair("code_challenge", &pkce_challenge(&pending.verifier));
        for (name, value) in provider.extra {
            query.append_pair(name, value);
        }
        self.pending = Some(pending);
        Ok(format!("{}?{}", provider.authorize_url, query.finish()))
    }

    /// Resume a saved session from its refresh token.
    pub fn restore(&mut self, client_id: &str, refresh_token: String) -> bool {
        if !(self.provider.is_client_id)(client_id.trim()) || refresh_token.is_empty() {
            return false;
        }
        self.disconnect();
        self.client_id = self.normalize(client_id);
        self.refresh = Some(refresh_token);
        true
    }

    /// `true` once a sign-in in progress has run out of time (and is cancelled).
    pub fn login_expired(&mut self, now: Instant) -> bool {
        if self.pending.as_ref().is_some_and(|p| now.duration_since(p.started) >= LOGIN_TIMEOUT) {
            self.pending = None;
            return true;
        }
        false
    }

    /// Handle `<callback path>?...` from the loopback server (path and query only).
    pub fn callback(&mut self, target: &str) -> Callback {
        let Some(query) = target.strip_prefix(self.provider.callback_path).and_then(|rest| rest.strip_prefix('?')) else {
            return Callback::Ignored;
        };
        let Some(pending) = &self.pending else { return Callback::Ignored };
        let pairs: Vec<(String, String)> = form_urlencoded::parse(query.as_bytes()).into_owned().collect();
        let values = |name: &str| pairs.iter().filter(|(k, _)| k == name).map(|(_, v)| v.as_str()).collect::<Vec<_>>();
        let states = values("state");
        if states.len() != 1 || !same(states[0], &pending.state) {
            return Callback::Ignored;
        }
        let Some(pending) = self.pending.take() else { return Callback::Ignored };
        let codes = values("code");
        if !values("error").is_empty() || codes.len() != 1 || codes[0].is_empty() {
            return Callback::Denied;
        }
        let form = form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", codes[0])
            .append_pair("redirect_uri", self.provider.redirect_uri)
            .append_pair("code_verifier", &pending.verifier)
            .append_pair("client_id", &self.client_id)
            .finish();
        Callback::Exchange(Body::Form(form))
    }

    /// A valid access token, if one is held and not about to expire.
    pub fn access_token(&self, now: Instant) -> Option<&str> {
        self.access.as_ref().filter(|(_, expires)| now + EXPIRY_MARGIN < *expires).map(|(token, _)| token.as_str())
    }

    /// The refresh request to POST to the token URL, if a refresh token is held.
    pub fn refresh_request(&self) -> Option<Body> {
        let refresh = self.refresh.as_ref()?;
        Some(Body::Form(
            form_urlencoded::Serializer::new(String::new())
                .append_pair("grant_type", "refresh_token")
                .append_pair("refresh_token", refresh)
                .append_pair("client_id", &self.client_id)
                .finish(),
        ))
    }

    /// Apply a token endpoint reply. A rejected refresh token (400/401) is dropped.
    pub fn accept(&mut self, response: &Response, now: Instant) -> Result<(), Message> {
        let token = response.body.get("access_token").and_then(Value::as_str).filter(|t| !t.is_empty());
        let expiry = response.body.get("expires_in").and_then(Value::as_f64).filter(|e| *e > 0.0 && *e <= 86_400.0);
        match (response.status, token, expiry) {
            (200, Some(token), Some(expiry)) => {
                self.access = Some((token.to_string(), now + Duration::from_secs_f64(expiry)));
                if let Some(refresh) = response.body.get("refresh_token").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                    self.refresh = Some(refresh.to_string());
                }
                Ok(())
            }
            (status, _, _) => {
                self.access = None;
                if matches!(status, 400 | 401) {
                    self.refresh = None;
                }
                if status == 429 {
                    Err(Message::new(self.provider.rate_limit))
                } else {
                    Err(Message::new(self.provider.auth_failed))
                }
            }
        }
    }

    /// The API rejected the access token: drop it so the next call refreshes.
    pub fn invalidate_access(&mut self) {
        self.access = None;
    }

    pub fn disconnect(&mut self) {
        self.pending = None;
        self.access = None;
        self.refresh = None;
    }
}

/// What a [`Session`] reports to its service, in order.
#[derive(Clone, Debug, PartialEq)]
pub enum Notice {
    Status(Message),
    Failed(Message),
    /// The saved session is gone (rejected refresh token).
    SignedOut,
}

/// Why an authorized call failed.
pub enum CallError {
    /// The service answered with an error status; the caller maps it to a message.
    Http(Response),
    /// No usable token; already reported as a [`Notice`].
    Auth,
}

/// A signed-in OAuth session: browser sign-in through the shared loopback server,
/// refresh token in the keyring, token refresh and one retry on 401.
pub struct Session {
    pub auth: Auth,
    transport: Box<dyn Transport>,
    store: Box<dyn TokenStore>,
    port: u16,
    done_text: String,
    server: Option<Arc<Loopback>>,
    on_callback: Arc<dyn Fn(String) + Send + Sync>,
    notices: Vec<Notice>,
}

impl Session {
    /// `on_callback` receives the callback target on the loopback server thread.
    pub fn new(
        provider: &'static Provider,
        transport: Box<dyn Transport>,
        store: Box<dyn TokenStore>,
        port: u16,
        done_text: String,
        on_callback: impl Fn(String) + Send + Sync + 'static,
    ) -> Session {
        Session {
            auth: Auth::new(provider),
            transport,
            store,
            port,
            done_text,
            server: None,
            on_callback: Arc::new(on_callback),
            notices: Vec::new(),
        }
    }

    pub fn take_notices(&mut self) -> Vec<Notice> {
        std::mem::take(&mut self.notices)
    }

    fn fail(&mut self, key: &'static str) {
        self.notices.push(Notice::Failed(Message::new(key)));
    }

    /// The loopback server, started on first use.
    pub fn server(&mut self) -> Option<Arc<Loopback>> {
        if self.server.is_none() {
            match Loopback::shared(self.port, self.done_text.clone()) {
                Ok(server) => self.server = Some(server),
                Err(()) => self.fail(self.auth.provider.port_busy),
            }
        }
        self.server.clone()
    }

    fn expect_callback(&self) {
        let on_callback = self.on_callback.clone();
        if let Some(server) = &self.server {
            server.expect(self.auth.provider.callback_path, self.auth.expected_state(), move |target| on_callback(target));
        }
    }

    fn cancel_callback(&self) {
        if let Some(server) = &self.server {
            server.cancel(self.auth.provider.callback_path);
        }
    }

    /// Start a browser sign-in; returns the URL to open. A previous session is forgotten.
    pub fn begin(&mut self, client_id: &str, now: Instant) -> Option<String> {
        let previous = self.auth.client_id().to_string();
        match self.auth.begin(client_id, now) {
            Ok(url) => {
                if !previous.is_empty() {
                    self.store.delete(&previous);
                }
                if self.server().is_none() {
                    self.auth.disconnect();
                    return None;
                }
                self.expect_callback();
                Some(url)
            }
            Err(message) => {
                self.notices.push(Notice::Failed(message));
                None
            }
        }
    }

    /// Cancel a sign-in that ran out of time.
    pub fn tick(&mut self, now: Instant) {
        if self.auth.login_expired(now) {
            self.cancel_callback();
            self.fail(self.auth.provider.login_timeout);
        }
    }

    /// Handle the loopback callback; `true` once signed in (the refresh token is saved).
    pub fn callback(&mut self, target: &str, now: Instant) -> bool {
        match self.auth.callback(target) {
            Callback::Ignored => false,
            Callback::Denied => {
                self.fail(self.auth.provider.login_denied);
                false
            }
            Callback::Exchange(body) => {
                let response = self.transport.request(Method::Post, self.auth.provider.token_url, None, Some(body));
                match self.auth.accept(&response, now) {
                    Ok(()) => {
                        self.save_refresh_token();
                        true
                    }
                    Err(message) => {
                        self.notices.push(Notice::Failed(message));
                        false
                    }
                }
            }
        }
    }

    /// Resume the session saved in the keyring; `true` when it is usable.
    pub fn restore(&mut self, client_id: &str, now: Instant) -> bool {
        if self.auth.signed_in() {
            return false;
        }
        let Some(token) = self.store.load(client_id) else { return false };
        self.auth.restore(client_id, token) && self.refresh(now)
    }

    fn save_refresh_token(&mut self) {
        if let Some(token) = self.auth.refresh_token().map(str::to_string) {
            if !self.store.save(self.auth.client_id(), &token) {
                self.notices.push(Notice::Status(Message::new(self.auth.provider.keyring_unavailable)));
            }
        }
    }

    fn refresh(&mut self, now: Instant) -> bool {
        let Some(body) = self.auth.refresh_request() else {
            self.fail(self.auth.provider.login_required);
            return false;
        };
        let previous = self.auth.refresh_token().map(str::to_string);
        let response = self.transport.request(Method::Post, self.auth.provider.token_url, None, Some(body));
        match self.auth.accept(&response, now) {
            Ok(()) => {
                if self.auth.refresh_token().map(str::to_string) != previous {
                    self.save_refresh_token();
                }
                true
            }
            Err(message) => {
                if !self.auth.signed_in() {
                    // Rejected refresh token (revoked or expired): forget it.
                    let client = self.auth.client_id().to_string();
                    self.store.delete(&client);
                    self.notices.push(Notice::SignedOut);
                    self.fail(self.auth.provider.session_expired);
                } else {
                    self.notices.push(Notice::Failed(message));
                }
                false
            }
        }
    }

    /// A valid access token, refreshing it when needed.
    pub fn token(&mut self, now: Instant) -> Option<String> {
        if let Some(token) = self.auth.access_token(now) {
            return Some(token.to_string());
        }
        if !self.auth.signed_in() {
            self.fail(self.auth.provider.login_required);
            return None;
        }
        if !self.refresh(now) {
            return None;
        }
        self.auth.access_token(now).map(str::to_string)
    }

    /// An authorized API call; refreshes the token once on 401.
    pub fn call(&mut self, method: Method, url: &str, body: Option<Body>, now: Instant) -> Result<Response, CallError> {
        for attempt in 0..2 {
            let Some(token) = self.token(now) else { return Err(CallError::Auth) };
            let response = self.transport.request(method, url, Some(&token), body.clone());
            if response.ok() {
                return Ok(response);
            }
            if response.status == 401 && attempt == 0 {
                self.auth.invalidate_access();
                continue;
            }
            return Err(CallError::Http(response));
        }
        Err(CallError::Auth)
    }

    /// Stop waiting for a sign-in callback (the worker is quitting).
    pub fn sign_out_pending(&self) {
        self.cancel_callback();
    }

    /// Forget the account here and in the keyring.
    pub fn sign_out(&mut self) {
        let client = self.auth.client_id().to_string();
        if !client.is_empty() {
            self.store.delete(&client);
        }
        self.auth.disconnect();
        self.cancel_callback();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spotify::auth::PROVIDER as SPOTIFY;
    use serde_json::json;

    fn form(body: &Body) -> Vec<(String, String)> {
        match body {
            Body::Form(form) => form_urlencoded::parse(form.as_bytes()).into_owned().collect(),
            Body::Json(_) => panic!("expected a form"),
        }
    }

    fn get<'a>(pairs: &'a [(String, String)], name: &str) -> Vec<&'a str> {
        pairs.iter().filter(|(k, _)| k == name).map(|(_, v)| v.as_str()).collect()
    }

    fn query(url: &str) -> Vec<(String, String)> {
        let (_, query) = url.split_once('?').unwrap();
        form_urlencoded::parse(query.as_bytes()).into_owned().collect()
    }

    fn reply(status: u16, body: Value) -> Response {
        Response { status, body, retry_after: None }
    }

    #[test]
    fn pkce_known_vector() {
        assert_eq!(pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn login_validates_state_and_replay_and_uses_pkce_without_secret() {
        let now = Instant::now();
        let mut auth = Auth::new(&SPOTIFY);
        let url = auth.begin(&"a".repeat(32), now).unwrap();
        assert!(url.starts_with(SPOTIFY.authorize_url));
        let q = query(&url);
        assert_eq!(get(&q, "redirect_uri"), [SPOTIFY.redirect_uri]);
        assert_eq!(get(&q, "code_challenge_method"), ["S256"]);
        assert_eq!(get(&q, "show_dialog"), ["true"]);
        let state = auth.expected_state().unwrap();
        assert_eq!(get(&q, "state"), [state.as_str()]);
        assert_eq!(get(&q, "code_challenge"), [pkce_challenge(&auth.pending.as_ref().unwrap().verifier).as_str()]);

        assert_eq!(auth.callback("/callback?code=secret&state=incorrect"), Callback::Ignored);
        assert_eq!(auth.callback("/callback?code=secret&state=%C5%82"), Callback::Ignored);
        assert_eq!(auth.callback(&format!("/callback?code=secret&state={state}&state=duplicate")), Callback::Ignored);
        assert_eq!(auth.callback(&format!("/elsewhere?code=secret&state={state}")), Callback::Ignored);
        assert!(auth.signing_in());

        let target = format!("/callback?code=one-time-code&state={state}");
        let Callback::Exchange(body) = auth.callback(&target) else { panic!("expected an exchange") };
        assert_eq!(auth.callback(&target), Callback::Ignored, "replay");
        let f = form(&body);
        assert_eq!(get(&f, "code"), ["one-time-code"]);
        assert_eq!(get(&f, "grant_type"), ["authorization_code"]);
        assert_eq!(get(&f, "code_verifier").len(), 1);
        assert!(get(&f, "client_secret").is_empty());

        auth.accept(&reply(200, json!({"access_token": "access", "refresh_token": "refresh", "expires_in": 3600})), now).unwrap();
        assert_eq!(auth.access_token(now), Some("access"));
        assert_eq!(auth.refresh_token(), Some("refresh"));
        assert_eq!(auth.access_token(now + Duration::from_secs(3550)), None, "refreshes before expiry");
        auth.disconnect();
        assert!(!auth.signed_in() && auth.access_token(now).is_none());
    }

    #[test]
    fn refresh_keeps_refresh_token_and_drops_it_when_rejected() {
        let now = Instant::now();
        let mut auth = Auth::new(&SPOTIFY);
        assert!(auth.restore(&"b".repeat(32), "refresh".into()));
        let f = form(&auth.refresh_request().unwrap());
        assert_eq!(get(&f, "grant_type"), ["refresh_token"]);
        assert_eq!(get(&f, "refresh_token"), ["refresh"]);
        auth.accept(&reply(200, json!({"access_token": "new", "expires_in": 3600})), now).unwrap();
        assert_eq!(auth.refresh_token(), Some("refresh"));
        assert_eq!(
            auth.accept(&reply(200, json!({"access_token": "x", "expires_in": 0})), now),
            Err(Message::new("spotify_auth_failed"))
        );
        assert_eq!(auth.refresh_token(), Some("refresh"), "a malformed reply keeps the session");
        assert_eq!(auth.accept(&reply(429, Value::Null), now), Err(Message::new("spotify_rate_limit")));
        assert_eq!(auth.refresh_token(), Some("refresh"));
        assert!(auth.accept(&reply(400, json!({"error": "invalid_grant"})), now).is_err());
        assert!(auth.refresh_token().is_none() && !auth.signed_in());
    }

    #[test]
    fn denied_expired_and_invalid_logins_do_not_leave_session() {
        let now = Instant::now();
        let mut auth = Auth::new(&SPOTIFY);
        assert_eq!(auth.begin("bad", now), Err(Message::new("spotify_client_invalid")));
        auth.begin(&"a".repeat(32), now).unwrap();
        let state = auth.expected_state().unwrap();
        assert_eq!(auth.callback(&format!("/callback?error=access_denied&state={state}")), Callback::Denied);
        assert!(!auth.signing_in());
        auth.begin(&"A".repeat(32), now).unwrap();
        assert_eq!(auth.client_id(), "a".repeat(32));
        assert!(!auth.login_expired(now + Duration::from_secs(10)));
        assert!(auth.login_expired(now + LOGIN_TIMEOUT));
        assert!(!auth.signing_in() && auth.expected_state().is_none());
    }
}
