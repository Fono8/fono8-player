//! Bounded HTTPS for the streaming services: only the hosts a service declares,
//! no redirects, a request timeout, a size limit on replies and no logging of
//! credentials or bodies.

use std::time::Duration;

use serde_json::Value;

const MAX_REPLY: u64 = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    /// `application/x-www-form-urlencoded`
    Form(String),
    Json(Value),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Response {
    /// HTTP status, or 0 when the request did not complete.
    pub status: u16,
    /// The parsed JSON reply; `Null` for empty or non-JSON bodies.
    pub body: Value,
    /// `Retry-After` seconds on 429 replies.
    pub retry_after: Option<u64>,
}

impl Response {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

pub trait Transport: Send {
    fn request(&self, method: Method, url: &str, bearer: Option<&str>, body: Option<Body>) -> Response;
}

/// `true` for `https://` URLs on one of `hosts`.
pub fn allowed(url: &str, hosts: &[&str]) -> bool {
    match url::Url::parse(url) {
        Ok(url) => {
            url.scheme() == "https"
                && url.username().is_empty()
                && url.password().is_none()
                && url.port().is_none()
                && url.host_str().is_some_and(|host| hosts.contains(&host))
        }
        Err(_) => false,
    }
}

pub struct Https {
    agent: ureq::Agent,
    hosts: &'static [&'static str],
    /// The `Accept` header for API calls (TIDAL: JSON:API).
    accept: Option<&'static str>,
}

impl Https {
    pub fn new(hosts: &'static [&'static str]) -> Https {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(15)))
            .max_redirects(0)
            .http_status_as_error(false)
            .user_agent(concat!("Fono8/", env!("CARGO_PKG_VERSION")))
            .build()
            .new_agent();
        Https { agent, hosts, accept: None }
    }

    pub fn accept(mut self, accept: &'static str) -> Https {
        self.accept = Some(accept);
        self
    }
}

impl Transport for Https {
    fn request(&self, method: Method, url: &str, bearer: Option<&str>, body: Option<Body>) -> Response {
        let failed = Response { status: 0, body: Value::Null, retry_after: None };
        if !allowed(url, self.hosts) {
            return failed;
        }
        let authorization = bearer.map(|token| format!("Bearer {token}"));
        let result = match method {
            Method::Get => {
                let mut request = self.agent.get(url);
                if let Some(accept) = self.accept {
                    request = request.header("Accept", accept);
                }
                if let Some(value) = &authorization {
                    request = request.header("Authorization", value);
                }
                request.call()
            }
            Method::Post | Method::Put => {
                let mut request = if method == Method::Post { self.agent.post(url) } else { self.agent.put(url) };
                if let Some(value) = &authorization {
                    request = request.header("Authorization", value);
                }
                match body {
                    Some(Body::Form(form)) => {
                        request.header("Content-Type", "application/x-www-form-urlencoded").send(form.as_bytes())
                    }
                    Some(Body::Json(value)) => {
                        request.header("Content-Type", "application/json").send(value.to_string().as_bytes())
                    }
                    None => request.send_empty(),
                }
            }
        };
        let Ok(mut response) = result else { return failed };
        let status = response.status().as_u16();
        let retry_after = response.headers().get("Retry-After").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse().ok());
        let body = match response.body_mut().with_config().limit(MAX_REPLY).read_to_vec() {
            Ok(bytes) if !bytes.is_empty() => serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            Ok(_) => Value::Null,
            Err(_) => return failed,
        };
        Response { status, body, retry_after }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_declared_https_hosts() {
        const HOSTS: &[&str] = &["accounts.spotify.com", "api.spotify.com"];
        assert!(allowed("https://accounts.spotify.com/api/token", HOSTS));
        assert!(allowed("https://api.spotify.com/v1/me", HOSTS));
        for url in [
            "http://api.spotify.com/v1/me",
            "https://api.spotify.com.evil.test/v1/me",
            "https://user@api.spotify.com/v1/me",
            "https://api.spotify.com:444/v1/me",
            "https://open.spotify.com/",
            "file:///etc/passwd",
        ] {
            assert!(!allowed(url, HOSTS), "{url}");
        }
        let reply = Https::new(HOSTS).request(Method::Get, "https://example.com/", None, None);
        assert_eq!(reply.status, 0);
    }
}
