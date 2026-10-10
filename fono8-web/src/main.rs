//! `fono8-web`: the YouTube Music helper process built on `wry`.
//!
//! Hosts two web views (account/library page and the web player) behind a small
//! HTML toolbar, and runs the shared [`fono8_ytm_core::Core`] on top of them.
//! Fono8 talks to it over stdin/stdout with one JSON object per line.

// No console window on Windows; the pipes to Fono8 work without one.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use fono8_ytm_core::protocol::Command;
use fono8_ytm_core::{host_of, navigation_error, scripts, Core, Engine, PageLoad, Purpose, ToolbarState, View};
use serde_json::json;
use tao::dpi::LogicalSize;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::window::{Window, WindowBuilder};
use wry::{PageLoadEvent, WebContext, WebView, WebViewBuilder};

#[cfg(target_os = "linux")]
use gtk::prelude::*;

const TOOLBAR_HEIGHT: f64 = 64.0;

/// Everything that reaches the event loop from other threads or callbacks.
enum UserEvent {
    Command(Command),
    Ipc(String),
    PageLoad(View, PageLoad, String),
    NavigationBlocked(View, String, &'static str),
    Script(Purpose, String),
}

fn debug_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("FONO8_DEBUG").is_some())
}

struct WryEngine {
    window: Window,
    toolbar: WebView,
    account: WebView,
    player: WebView,
    #[cfg(target_os = "linux")]
    stack: gtk::Stack,
    proxy: EventLoopProxy<UserEvent>,
}

impl WryEngine {
    fn view(&self, view: View) -> &WebView {
        match view {
            View::Account => &self.account,
            View::Player => &self.player,
        }
    }

    fn layout(&self) {
        #[cfg(not(target_os = "linux"))]
        {
            use tao::dpi::{LogicalPosition, LogicalSize};
            use wry::Rect;
            let size = self.window.inner_size().to_logical::<f64>(self.window.scale_factor());
            let _ = self.toolbar.set_bounds(Rect {
                position: LogicalPosition::new(0.0, 0.0).into(),
                size: LogicalSize::new(size.width, TOOLBAR_HEIGHT).into(),
            });
            let content = Rect {
                position: LogicalPosition::new(0.0, TOOLBAR_HEIGHT).into(),
                size: LogicalSize::new(size.width, (size.height - TOOLBAR_HEIGHT).max(1.0)).into(),
            };
            let _ = self.account.set_bounds(content);
            let _ = self.player.set_bounds(content);
        }
    }
}

impl Engine for WryEngine {
    fn eval(&mut self, view: View, script: &str, purpose: Purpose) {
        let proxy = self.proxy.clone();
        let _ = self.view(view).evaluate_script_with_callback(script, move |result| {
            let _ = proxy.send_event(UserEvent::Script(purpose.clone(), result));
        });
    }

    fn run(&mut self, view: View, script: &str) {
        let _ = self.view(view).evaluate_script(script);
    }

    fn load_url(&mut self, view: View, url: &str) {
        let _ = self.view(view).load_url(url);
    }

    fn show(&mut self, view: View) {
        #[cfg(target_os = "linux")]
        {
            self.stack.set_visible_child_name(if view == View::Account { "account" } else { "player" });
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = self.account.set_visible(view == View::Account);
            let _ = self.player.set_visible(view == View::Player);
        }
        self.window.set_visible(true);
        self.window.set_focus();
        self.layout();
    }

    fn hide(&mut self) {
        self.window.set_visible(false);
    }

    fn toolbar(&mut self, state: &ToolbarState) {
        let state = json!({
            "notice": state.notice,
            "tooltip": state.tooltip,
            "home": state.home,
            "tabs": state.tabs,
            "current": state.current,
            "warning": state.warning,
        });
        let _ = self.toolbar.evaluate_script(&format!("window.__fono8SetState({state});"));
    }
}

fn main() {
    let mut profile: Option<PathBuf> = None;
    let mut language = "en".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--profile" => profile = args.next().map(PathBuf::from),
            "--language" => language = args.next().unwrap_or_else(|| "en".into()),
            _ => {}
        }
    }
    let persistent = profile.as_ref().map(|dir| prepare_profile(dir)).unwrap_or(false);

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // stdin reader: one JSON command per line.
    let reader_proxy = proxy.clone();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Command>(&line) {
                Ok(command) => {
                    if reader_proxy.send_event(UserEvent::Command(command)).is_err() {
                        break;
                    }
                }
                Err(error) => eprintln!("fono8-web: ignoring malformed command: {error}"),
            }
        }
        // Parent is gone: shut down.
        let _ = reader_proxy.send_event(UserEvent::Command(Command::Quit));
    });

    let window = WindowBuilder::new()
        .with_title("YouTube Music")
        .with_inner_size(LogicalSize::new(900.0, 680.0))
        .with_min_inner_size(LogicalSize::new(480.0, 360.0))
        .with_visible(false)
        .build(&event_loop)
        .expect("helper window");

    let mut context = WebContext::new(if persistent { profile.clone() } else { None });
    let mut engine = build_engine(window, &mut context, &proxy);
    let mut core = Core::new(persistent, &language);
    if debug_enabled() {
        core.set_logger(|message| eprintln!("[fono8-web] {message}"));
    }
    let stdout = std::sync::Mutex::new(std::io::stdout());
    let flush = move |core: &mut Core| {
        if let Ok(mut out) = stdout.lock() {
            for event in core.take_events() {
                if let Ok(line) = serde_json::to_string(&event) {
                    let _ = writeln!(out, "{line}");
                }
            }
            let _ = out.flush();
        }
    };
    core.start(&mut engine);
    flush(&mut core);

    let tick = Duration::from_millis(40);
    let mut next_tick = Instant::now() + tick;
    event_loop.run(move |event, _target, control_flow| {
        *control_flow = ControlFlow::WaitUntil(next_tick);
        match event {
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) | Event::NewEvents(StartCause::Poll) => {
                if Instant::now() >= next_tick {
                    next_tick = Instant::now() + tick;
                    core.tick(&mut engine);
                    *control_flow = ControlFlow::WaitUntil(next_tick);
                }
            }
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                engine.hide();
                core.closed();
            }
            Event::WindowEvent { event: WindowEvent::Resized(_), .. } => engine.layout(),
            Event::UserEvent(UserEvent::Command(Command::Quit)) => {
                core.command(Command::Quit, &mut engine);
                flush(&mut core);
                *control_flow = ControlFlow::Exit;
                return;
            }
            Event::UserEvent(user) => {
                match user {
                    UserEvent::Command(command) => core.command(command, &mut engine),
                    UserEvent::Ipc(message) => core.ipc(&message, &mut engine),
                    UserEvent::PageLoad(view, stage, url) => core.page_load(view, stage, &url, &mut engine),
                    UserEvent::NavigationBlocked(view, host, reason) => core.navigation_blocked(view, &host, reason, &mut engine),
                    UserEvent::Script(purpose, result) => core.script_reply(purpose, &result, &mut engine),
                }
                if Instant::now() >= next_tick {
                    *control_flow = ControlFlow::Poll;
                }
            }
            _ => {}
        }
        flush(&mut core);
    });
}

fn prepare_profile(dir: &PathBuf) -> bool {
    // Never follow a substituted profile directory.
    if dir.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(false) {
        return false;
    }
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    true
}

#[cfg(target_os = "macos")]
const SAFARI_USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.5 Safari/605.1.15";

fn configure<'a>(builder: WebViewBuilder<'a>, view: View, proxy: &EventLoopProxy<UserEvent>, guard: bool) -> WebViewBuilder<'a> {
    let nav_proxy = proxy.clone();
    let load_proxy = proxy.clone();
    let mut builder = builder
        .with_autoplay(true)
        .with_clipboard(false)
        .with_devtools(false)
        .with_navigation_handler(move |url| {
            if let Some(reason) = navigation_error(&url) {
                // Show only the host; paths and queries can contain login identifiers or tokens.
                let host: String = host_of(&url).chars().take(200).collect();
                let _ = nav_proxy.send_event(UserEvent::NavigationBlocked(view, host, reason));
                return false;
            }
            true
        })
        .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
        .with_download_started_handler(|_, _| false)
        .with_on_page_load_handler(move |event, url| {
            let stage = match event {
                PageLoadEvent::Started => PageLoad::Started,
                PageLoadEvent::Finished => PageLoad::Finished,
            };
            let _ = load_proxy.send_event(UserEvent::PageLoad(view, stage, url));
        });
    // WKWebView reports a user agent without Safari's version, and YouTube Music then
    // says the browser is not supported. Present it as the Safari it is built on.
    #[cfg(target_os = "macos")]
    {
        builder = builder.with_user_agent(SAFARI_USER_AGENT);
    }
    if guard {
        // Installed before the website creates or caches MediaSource methods.
        builder = builder.with_initialization_script_for_main_only(scripts::MEDIA_GUARD, true);
    }
    builder
}

#[cfg(target_os = "linux")]
fn build_engine(window: Window, context: &mut WebContext, proxy: &EventLoopProxy<UserEvent>) -> WryEngine {
    use tao::platform::unix::WindowExtUnix;
    use wry::WebViewBuilderExtUnix;

    let vbox = window.default_vbox().expect("gtk vbox");
    let toolbar_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    toolbar_box.set_size_request(-1, TOOLBAR_HEIGHT as i32);
    vbox.pack_start(&toolbar_box, false, false, 0);
    let stack = gtk::Stack::new();
    let account_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let player_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    stack.add_named(&account_box, "account");
    stack.add_named(&player_box, "player");
    vbox.pack_start(&stack, true, true, 0);
    vbox.show_all();

    let ipc_proxy = proxy.clone();
    let toolbar = WebViewBuilder::new()
        .with_html(scripts::toolbar_html())
        .with_ipc_handler(move |request| {
            let _ = ipc_proxy.send_event(UserEvent::Ipc(request.body().to_string()));
        })
        .build_gtk(&toolbar_box)
        .expect("toolbar view");
    let account = configure(WebViewBuilder::new_with_web_context(context), View::Account, proxy, false)
        .build_gtk(&account_box)
        .expect("account view");
    let player = configure(WebViewBuilder::new_with_web_context(context), View::Player, proxy, true)
        .with_url("about:blank")
        .build_gtk(&player_box)
        .expect("player view");
    stack.set_visible_child_name("account");
    WryEngine { window, toolbar, account, player, stack, proxy: proxy.clone() }
}

#[cfg(not(target_os = "linux"))]
fn build_engine(window: Window, context: &mut WebContext, proxy: &EventLoopProxy<UserEvent>) -> WryEngine {
    use tao::dpi::{LogicalPosition, LogicalSize};
    use wry::Rect;

    let size = window.inner_size().to_logical::<f64>(window.scale_factor());
    let toolbar_rect =
        Rect { position: LogicalPosition::new(0.0, 0.0).into(), size: LogicalSize::new(size.width, TOOLBAR_HEIGHT).into() };
    let content_rect = Rect {
        position: LogicalPosition::new(0.0, TOOLBAR_HEIGHT).into(),
        size: LogicalSize::new(size.width, (size.height - TOOLBAR_HEIGHT).max(1.0)).into(),
    };
    let ipc_proxy = proxy.clone();
    let toolbar = WebViewBuilder::new()
        .with_html(scripts::toolbar_html())
        .with_bounds(toolbar_rect)
        .with_ipc_handler(move |request| {
            let _ = ipc_proxy.send_event(UserEvent::Ipc(request.body().to_string()));
        })
        .build_as_child(&window)
        .expect("toolbar view");
    let account = configure(WebViewBuilder::new_with_web_context(context), View::Account, proxy, false)
        .with_bounds(content_rect)
        .build_as_child(&window)
        .expect("account view");
    let player = configure(WebViewBuilder::new_with_web_context(context), View::Player, proxy, true)
        .with_url("about:blank")
        .with_bounds(content_rect)
        .with_visible(false)
        .build_as_child(&window)
        .expect("player view");
    WryEngine { window, toolbar, account, player, proxy: proxy.clone() }
}
