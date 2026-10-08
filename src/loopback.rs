//! Loopback-only HTTP server shared by the streaming services: OAuth callbacks
//! (one path per service, each with a one-time `state`) and Spotify's static
//! player page, served from an unguessable path. Never serves credentials.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rand::RngCore;

const PLAYER_HTML: &[u8] = include_bytes!("../assets/spotify/player.html");
const PLAYER_JS: &[u8] = include_bytes!("../assets/spotify/player.js");
const MAX_TARGET: usize = 4096;

type OnCallback = Box<dyn Fn(String) + Send>;

/// A sign-in waiting for its callback: the `state` it must carry and who gets the target.
struct Route {
    state: Option<String>,
    on_callback: OnCallback,
}

type Routes = Arc<Mutex<HashMap<&'static str, Route>>>;

pub struct Loopback {
    server: Arc<tiny_http::Server>,
    port: u16,
    player_path: String,
    routes: Routes,
    closed: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

fn header(name: &str, value: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("static header")
}

/// The route whose path and one-time `state` match `target`; its state is consumed.
fn take_match(target: &str, routes: &Routes) -> Option<&'static str> {
    let (path, query) = target.split_once('?')?;
    let states: Vec<String> =
        form_urlencoded::parse(query.as_bytes()).filter(|(k, _)| k == "state").map(|(_, v)| v.into_owned()).collect();
    let mut routes = routes.lock().ok()?;
    let (key, route) = routes.iter_mut().find(|(key, _)| **key == path)?;
    match (states.as_slice(), route.state.as_deref()) {
        ([state], Some(wanted))
            if state.len() == wanted.len() && state.bytes().zip(wanted.bytes()).fold(0, |a, (x, y)| a | (x ^ y)) == 0 =>
        {
            route.state = None;
            Some(*key)
        }
        _ => None,
    }
}

impl Loopback {
    /// Listen on 127.0.0.1:`port` (0 picks a free port).
    pub fn start(port: u16, done_text: String) -> Result<Loopback, ()> {
        let server = Arc::new(tiny_http::Server::http(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).map_err(|_| ())?);
        let port = match server.server_addr() {
            tiny_http::ListenAddr::IP(addr) => addr.port(),
            #[allow(unreachable_patterns)]
            _ => return Err(()),
        };
        let mut random = [0u8; 24];
        rand::rng().fill_bytes(&mut random);
        let player_path = format!("/player/{}", URL_SAFE_NO_PAD.encode(random));
        let closed = Arc::new(AtomicBool::new(false));
        let routes: Routes = Arc::new(Mutex::new(HashMap::new()));
        let (thread_server, thread_closed, thread_path, thread_routes) =
            (server.clone(), closed.clone(), player_path.clone(), routes.clone());
        let host = format!("127.0.0.1:{port}");
        let thread = std::thread::Builder::new()
            .name("fono8-loopback".into())
            .spawn(move || {
                let script_path = format!("{thread_path}/player.js");
                while !thread_closed.load(Ordering::Relaxed) {
                    let request = match thread_server.recv_timeout(Duration::from_millis(200)) {
                        Ok(Some(request)) => request,
                        Ok(None) => continue,
                        Err(_) => break,
                    };
                    let target = request.url().to_string();
                    let host_ok =
                        request.headers().iter().filter(|h| h.field.equiv("Host")).map(|h| h.value.as_str()).eq([host.as_str()]);
                    let (status, mime, body): (u16, &str, Vec<u8>) =
                        if request.method() != &tiny_http::Method::Get || !host_ok || target.len() > MAX_TARGET {
                            (400, "text/plain", b"Invalid request.".to_vec())
                        } else if target == thread_path {
                            (200, "text/html; charset=utf-8", PLAYER_HTML.to_vec())
                        } else if target == script_path {
                            (200, "text/javascript; charset=utf-8", PLAYER_JS.to_vec())
                        } else if let Some(path) = take_match(&target, &thread_routes) {
                            if let Ok(routes) = thread_routes.lock() {
                                if let Some(route) = routes.get(path) {
                                    (route.on_callback)(target);
                                }
                            }
                            (200, "text/plain; charset=utf-8", done_text.clone().into_bytes())
                        } else {
                            (404, "text/plain", b"Not found.".to_vec())
                        };
                    let headers = vec![
                        header("Content-Type", mime),
                        header("Cache-Control", "no-store"),
                        header("Referrer-Policy", "no-referrer"),
                        header("X-Content-Type-Options", "nosniff"),
                        header("X-Frame-Options", "DENY"),
                        header("Connection", "close"),
                    ];
                    let length = body.len();
                    let response =
                        tiny_http::Response::new(tiny_http::StatusCode(status), headers, &body[..], Some(length), None);
                    let _ = request.respond(response);
                }
            })
            .map_err(|_| ())?;
        Ok(Loopback { server, port, player_path, routes, closed, thread: Mutex::new(Some(thread)) })
    }

    /// The server for `port`, started on first use and shared by every service (port 0: a new one).
    pub fn shared(port: u16, done_text: String) -> Result<Arc<Loopback>, ()> {
        static SHARED: OnceLock<Mutex<HashMap<u16, Arc<Loopback>>>> = OnceLock::new();
        if port == 0 {
            return Loopback::start(0, done_text).map(Arc::new);
        }
        let mut servers = SHARED.get_or_init(|| Mutex::new(HashMap::new())).lock().map_err(|_| ())?;
        if let Some(server) = servers.get(&port).filter(|s| !s.closed.load(Ordering::Relaxed)) {
            return Ok(server.clone());
        }
        let server = Arc::new(Loopback::start(port, done_text)?);
        servers.insert(port, server.clone());
        Ok(server)
    }

    /// Wait for one callback on `path` carrying `state` (`None` cancels a pending sign-in).
    pub fn expect(&self, path: &'static str, state: Option<String>, on_callback: impl Fn(String) + Send + 'static) {
        if let Ok(mut routes) = self.routes.lock() {
            routes.insert(path, Route { state, on_callback: Box::new(on_callback) });
        }
    }

    /// Cancel the sign-in waiting on `path`.
    pub fn cancel(&self, path: &'static str) {
        if let Ok(mut routes) = self.routes.lock() {
            routes.remove(path);
        }
    }

    #[cfg(test)]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The Spotify player page URL (secret path).
    pub fn player_url(&self) -> String {
        format!("http://127.0.0.1:{}{}", self.port, self.player_path)
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        self.server.unblock();
        if let Some(thread) = self.thread.lock().ok().and_then(|mut t| t.take()) {
            let _ = thread.join();
        }
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::mpsc;

    fn fetch(port: u16, target: &str, host: Option<&str>) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let host = host.map(str::to_string).unwrap_or(format!("127.0.0.1:{port}"));
        write!(stream, "GET {target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").unwrap();
        let mut reply = Vec::new();
        let _ = stream.read_to_end(&mut reply);
        String::from_utf8_lossy(&reply).into_owned()
    }

    #[test]
    fn loopback_serves_only_capability_assets_and_valid_callbacks() {
        let server = Loopback::start(0, "Signed in.".into()).unwrap();
        let port = server.port();
        let path = server.player_url().split_once(&format!(":{port}")).unwrap().1.to_string();
        assert!(path.len() > 20);

        let page = fetch(port, &path, None);
        assert!(page.starts_with("HTTP/1.1 200") && page.contains("Fono8 Spotify"), "{page}");
        assert!(page.contains("Cache-Control: no-store"));
        let script = fetch(port, &format!("{path}/player.js"), None);
        assert!(script.starts_with("HTTP/1.1 200") && script.contains("Spotify.Player"));
        for target in ["/", "/player/guessed", "/../Cargo.toml", "/callback?state=bad&code=x", "/callback?code=x"] {
            assert!(fetch(port, target, None).starts_with("HTTP/1.1 404"), "{target}");
        }
        assert!(fetch(port, &path, Some("evil.test")).starts_with("HTTP/1.1 400"));
        assert!(fetch(port, &path, Some("localhost:1")).starts_with("HTTP/1.1 400"));

        let (spotify, spotify_received) = mpsc::channel();
        let (tidal, tidal_received) = mpsc::channel();
        server.expect("/callback", Some("s3cret".into()), move |target| {
            let _ = spotify.send(target);
        });
        server.expect("/tidal/callback", Some("other".into()), move |target| {
            let _ = tidal.send(target);
        });
        assert!(fetch(port, "/callback?state=s3cre&code=x", None).starts_with("HTTP/1.1 404"));
        assert!(fetch(port, "/callback?state=s3cret&state=s3cret&code=x", None).starts_with("HTTP/1.1 404"));
        assert!(fetch(port, "/tidal/callback?state=s3cret&code=x", None).starts_with("HTTP/1.1 404"), "state of another route");
        let reply = fetch(port, "/callback?state=s3cret&code=one-time", None);
        assert!(reply.starts_with("HTTP/1.1 200") && reply.ends_with("Signed in."), "{reply}");
        assert!(!reply.contains("one-time") && !reply.contains("s3cret"));
        assert_eq!(spotify_received.recv_timeout(Duration::from_secs(2)).unwrap(), "/callback?state=s3cret&code=one-time");
        assert!(fetch(port, "/callback?state=s3cret&code=one-time", None).starts_with("HTTP/1.1 404"), "replay");
        assert!(fetch(port, "/tidal/callback?state=other&code=t", None).starts_with("HTTP/1.1 200"));
        assert_eq!(tidal_received.recv_timeout(Duration::from_secs(2)).unwrap(), "/tidal/callback?state=other&code=t");
        server.cancel("/callback");
        server.close();
    }
}
