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

const APP_ICON_SVG: &str = include_str!("../assets/fono8.svg");
const DONE_PAGE_POLICY: &str = "default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'";

/// The page shown in the browser after a sign-in callback, in the style of fono8.com.
/// Self-contained: inline styles and logo, no scripts and no requests.
pub fn done_page(language: &str, title: &str, body: &str) -> String {
    let (language, title, body) = (escape(language), escape(title), escape(body));
    let icon = APP_ICON_SVG.replacen(r#" width="512" height="512""#, "", 1);
    format!(
        r#"<!doctype html>
<html lang="{language}">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="dark">
<title>Fono8 · {title}</title>
<style>
  :root {{ --bg: #07101e; --surface: #0e1a2c; --border: #24334b; --text: #e8f0ff; --muted: #9baecb;
    --accent: #3de8f7; --purple: #9b6dff; --grad: linear-gradient(90deg, var(--accent), var(--purple));
    --font: "Inter", "Ubuntu", "Noto Sans", "Cantarell", "Segoe UI", system-ui, -apple-system, "Helvetica Neue", Arial, sans-serif; }}
  * {{ box-sizing: border-box; }}
  html, body {{ height: 100%; }}
  body {{ margin: 0; display: grid; place-items: center; padding: 24px; color: var(--text);
    font: 400 16px/1.6 var(--font); -webkit-font-smoothing: antialiased; background: var(--bg);
    background-image: radial-gradient(900px 520px at 85% -120px, rgba(155, 109, 255, .16), transparent 70%),
      radial-gradient(800px 500px at 0% 120px, rgba(61, 232, 247, .09), transparent 70%);
    background-repeat: no-repeat; }}
  main {{ width: min(440px, 100%); text-align: center; padding: 40px 32px 34px; border-radius: 14px;
    background: rgba(14, 26, 44, .82); border: 1px solid var(--border); box-shadow: 0 24px 60px rgba(0, 0, 0, .35); }}
  .icon {{ width: 76px; height: 76px; margin: 0 auto 22px; }}
  .icon svg {{ width: 100%; height: 100%; display: block; }}
  h1 {{ margin: 0; font-size: 26px; font-weight: 700; letter-spacing: -.02em; }}
  .rule {{ width: 56px; height: 3px; margin: 14px auto 16px; border-radius: 3px; background: var(--grad); }}
  p {{ margin: 0; color: var(--muted); }}
  footer {{ margin-top: 22px; font-size: 13px; color: var(--muted); letter-spacing: .02em; }}
  footer strong {{ background: var(--grad); -webkit-background-clip: text; background-clip: text; color: transparent; }}
</style>
</head>
<body>
<main>
  <div class="icon" aria-hidden="true">{icon}</div>
  <h1>{title}</h1>
  <div class="rule"></div>
  <p>{body}</p>
  <footer><strong>Fono8</strong></footer>
</main>
</body>
</html>
"#
    )
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

impl Loopback {
    /// Listen on 127.0.0.1:`port` (0 picks a free port).
    pub fn start(port: u16, done_page: String) -> Result<Loopback, ()> {
        let server = Arc::new(tiny_http::Server::http(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).map_err(|_| ())?);
        let port = match server.server_addr() {
            tiny_http::ListenAddr::IP(addr) => addr.port(),
            #[allow(unreachable_patterns)]
            _ => return Err(()),
        };
        let mut random = [0u8; 24];
        rand::fill(&mut random);
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
                    let mut page_policy = false;
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
                            page_policy = true;
                            (200, "text/html; charset=utf-8", done_page.clone().into_bytes())
                        } else {
                            (404, "text/plain", b"Not found.".to_vec())
                        };
                    let mut headers = vec![
                        header("Content-Type", mime),
                        header("Cache-Control", "no-store"),
                        header("Referrer-Policy", "no-referrer"),
                        header("X-Content-Type-Options", "nosniff"),
                        header("X-Frame-Options", "DENY"),
                        header("Connection", "close"),
                    ];
                    if page_policy {
                        // The sign-in page is static: no scripts, no requests, no forms.
                        headers.push(header("Content-Security-Policy", DONE_PAGE_POLICY));
                    }
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
    /// `done_page` (see [`done_page`]) comes from whichever service starts it first, so it must
    /// not name a service.
    pub fn shared(port: u16, done_page: String) -> Result<Arc<Loopback>, ()> {
        static SHARED: OnceLock<Mutex<HashMap<u16, Arc<Loopback>>>> = OnceLock::new();
        if port == 0 {
            return Loopback::start(0, done_page).map(Arc::new);
        }
        let mut servers = SHARED.get_or_init(|| Mutex::new(HashMap::new())).lock().map_err(|_| ())?;
        if let Some(server) = servers.get(&port).filter(|s| !s.closed.load(Ordering::Relaxed)) {
            return Ok(server.clone());
        }
        let server = Arc::new(Loopback::start(port, done_page)?);
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
    fn the_sign_in_page_is_static_escaped_html() {
        let page = done_page("pl", "Logowanie <przyjęte>", "Możesz \"zamknąć\" kartę & wrócić.");
        assert!(page.starts_with("<!doctype html>") && page.contains(r#"<html lang="pl">"#));
        assert!(page.contains("<h1>Logowanie &lt;przyjęte&gt;</h1>"));
        assert!(page.contains("Możesz &quot;zamknąć&quot; kartę &amp; wrócić."));
        assert!(page.contains("<svg") && !page.contains(r#"width="512""#), "inline logo sized by CSS");
        assert!(!page.contains("<script") && !page.contains("https://"), "no scripts, no external resources");
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
        assert!(reply.contains("Content-Type: text/html") && reply.contains("Content-Security-Policy: default-src 'none'"));
        assert!(!reply.contains("one-time") && !reply.contains("s3cret"));
        assert_eq!(spotify_received.recv_timeout(Duration::from_secs(2)).unwrap(), "/callback?state=s3cret&code=one-time");
        assert!(fetch(port, "/callback?state=s3cret&code=one-time", None).starts_with("HTTP/1.1 404"), "replay");
        assert!(fetch(port, "/tidal/callback?state=other&code=t", None).starts_with("HTTP/1.1 200"));
        assert_eq!(tidal_received.recv_timeout(Duration::from_secs(2)).unwrap(), "/tidal/callback?state=other&code=t");
        server.cancel("/callback");
        server.close();
    }
}
