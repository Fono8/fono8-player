//! The main window: title bar, sidebar, home/library pages, queue panel,
//! player bar, status line, context menus and dialogs.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    actions, anchored, deferred, div, linear_color_stop, linear_gradient, prelude::*, px, size, uniform_list, AnimationExt, App,
    Bounds, ClickEvent, Context, CursorStyle, Div, Entity, FocusHandle, KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent,
    PathPromptOptions, Pixels, Point, ResizeEdge, SharedString, Size, Stateful, Subscription, TitlebarOptions,
    UniformListScrollHandle, WeakEntity, Window, WindowBackgroundAppearance, WindowBounds, WindowControlArea, WindowDecorations,
    WindowKind, WindowOptions,
};

use super::shell::{brand_animated, Shell};
use crate::app::ImportMode;
use crate::app::{
    ContextMenu, Dialog, DragList, Fono8, MenuAction, MenuEntry, Page, SettingsTab, WindowRequest, COMPACT_SIZE, FULL_SIZE,
};
use crate::cast::CastState;
use crate::i18n::Message;
use crate::library::{format_clock, format_duration, Track};
use crate::services::{Button as ServiceButton, Service};

use super::text_input::{InputEvent, TextInput};
use super::theme;
use super::widgets::{
    action_button, brand, brand_bars, cover, icon, separator, slider, BrandPose, SliderDrag, SliderKind, Tooltip,
};

actions!(
    fono8,
    [
        PlayPause,
        OpenFolder,
        NewPlaylist,
        FocusSearch,
        PreviousTrack,
        NextTrack,
        ToggleMini,
        HideToTray,
        QuitApp,
        Escape,
        DeleteSelected,
        PlayFocused,
        FocusUp,
        FocusDown,
        FocusUpShift,
        FocusDownShift,
        SelectAllTracks,
    ]
);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("space", PlayPause, Some("Fono8 && !TextInput")),
        KeyBinding::new("secondary-o", OpenFolder, Some("Fono8")),
        KeyBinding::new("secondary-n", NewPlaylist, Some("Fono8")),
        KeyBinding::new("secondary-f", FocusSearch, Some("Fono8")),
        KeyBinding::new("secondary-left", PreviousTrack, Some("Fono8")),
        KeyBinding::new("secondary-right", NextTrack, Some("Fono8")),
        KeyBinding::new("secondary-m", ToggleMini, Some("Fono8")),
        KeyBinding::new("secondary-h", HideToTray, Some("Fono8")),
        KeyBinding::new("secondary-q", QuitApp, Some("Fono8")),
        KeyBinding::new("escape", Escape, Some("Fono8")),
        KeyBinding::new("delete", DeleteSelected, Some("TrackList")),
        KeyBinding::new("enter", PlayFocused, Some("TrackList")),
        KeyBinding::new("up", FocusUp, Some("TrackList")),
        KeyBinding::new("down", FocusDown, Some("TrackList")),
        KeyBinding::new("shift-up", FocusUpShift, Some("TrackList")),
        KeyBinding::new("shift-down", FocusDownShift, Some("TrackList")),
        KeyBinding::new("ctrl-a", SelectAllTracks, Some("TrackList")),
    ]);
}

const TRACK_ROW: f32 = 56.0;
const QUEUE_ROW: f32 = 74.0;
const EDGE: f32 = 6.0;

/// Height and left padding of the title bar (macOS leaves room for the traffic lights).
pub const TITLEBAR_HEIGHT: f32 = 52.;
pub const TITLEBAR_PADDING: f32 = if cfg!(target_os = "macos") { 78. } else { 18. };

pub struct MainView {
    model: Entity<Fono8>,
    search: Entity<TextInput>,
    focus_handle: FocusHandle,
    list_focus: FocusHandle,
    track_scroll: UniformListScrollHandle,
    queue_scroll: UniformListScrollHandle,
    slider: SliderDrag,
    last_title: String,
    /// Texts for artist links, refreshed every render: tooltip prefixes and the font.
    artist_texts: (String, String, SharedString),
    _subscriptions: Vec<Subscription>,
}

pub fn open_main_window(model: Entity<Fono8>, cx: &mut App) {
    let (bounds, maximized, translucent) = {
        let m = model.read(cx);
        (m.window_bounds(), m.window_state.maximized && !m.compact, m.translucent)
    };
    // Linux draws everything itself (client-side decorations); macOS keeps the native
    // traffic lights on a transparent title bar; Windows hides the native bar and uses
    // window control areas for dragging.
    let titlebar = if cfg!(target_os = "linux") {
        None
    } else {
        Some(TitlebarOptions {
            title: Some("Fono8".into()),
            appears_transparent: true,
            traffic_light_position: Some(gpui::point(px(12.), px(18.))),
        })
    };
    let options = WindowOptions {
        window_bounds: Some(if maximized { WindowBounds::Maximized(bounds) } else { WindowBounds::Windowed(bounds) }),
        titlebar,
        focus: true,
        show: true,
        kind: WindowKind::Normal,
        is_movable: true,
        is_resizable: true,
        is_minimizable: true,
        display_id: None,
        window_background: if translucent { WindowBackgroundAppearance::Transparent } else { WindowBackgroundAppearance::Opaque },
        app_id: Some("fono8".into()),
        window_min_size: Some(size(px(420.), px(210.))),
        window_decorations: if cfg!(target_os = "linux") { Some(WindowDecorations::Client) } else { None },
        tabbing_identifier: None,
    };
    let model_for_window = model.clone();
    let handle = cx.open_window(options, move |window, cx| {
        let view = cx.new(|cx| MainView::new(model_for_window.clone(), window, cx));
        let focus = view.read(cx).focus_handle.clone();
        window.focus(&focus);
        cx.new(|cx| Shell::new(view, model_for_window.clone(), cx))
    });
    let Ok(handle) = handle else { return };
    model.update(cx, |m, _| m.window = Some(handle));
    let model_for_close = model.clone();
    let _ = handle.update(cx, |shell, window, cx| {
        shell.main.update(cx, |view, cx| {
            let title = view.model.read(cx).window_title();
            window.set_window_title(&title);
            view.last_title = title;
        });
        window.on_window_should_close(cx, move |window, cx| {
            let bounds = window.bounds();
            let maximized = window.is_maximized();
            let keep_running = model_for_close.update(cx, |m, cx| {
                m.record_window_bounds(bounds, maximized);
                m.save_window_state();
                if m.tray_available() && !m.quitting {
                    true
                } else {
                    m.window = None;
                    m.quit(cx);
                    false
                }
            });
            if keep_running {
                // GPUI quits once the last window is gone, so "close to tray" hides the
                // application on macOS and minimizes the window elsewhere.
                if cfg!(target_os = "macos") {
                    cx.hide();
                } else {
                    window.minimize_window();
                }
                return false;
            }
            true
        });
    });
}

impl MainView {
    fn new(model: Entity<Fono8>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (placeholder, font) = {
            let m = model.read(cx);
            (m.t("search_library"), m.font_family.clone())
        };
        let search = cx.new(|cx| TextInput::new(cx, placeholder, font));
        let mut subscriptions = vec![cx.observe(&model, |_, _, cx| cx.notify())];
        subscriptions.push(cx.subscribe_in(&search, window, |this, input, event, window, cx| match event {
            InputEvent::Changed => {
                let text = input.read(cx).text().to_string();
                this.model.update(cx, |m, cx| {
                    m.set_query(&text);
                    cx.notify();
                });
            }
            InputEvent::Cancelled => {
                input.update(cx, |input, cx| input.set_text(String::new(), cx));
                this.model.update(cx, |m, cx| {
                    m.set_query("");
                    cx.notify();
                });
                window.focus(&this.focus_handle);
            }
            InputEvent::Submitted => {
                window.focus(&this.list_focus);
            }
        }));
        subscriptions.push(cx.observe_window_bounds(window, |this, window, cx| {
            let bounds = window.bounds();
            let maximized = window.is_maximized();
            this.model.update(cx, |m, _| m.record_window_bounds(bounds, maximized));
        }));
        MainView {
            model,
            search,
            focus_handle: cx.focus_handle(),
            list_focus: cx.focus_handle(),
            track_scroll: UniformListScrollHandle::new(),
            queue_scroll: UniformListScrollHandle::new(),
            slider: SliderDrag::default(),
            last_title: String::new(),
            artist_texts: (String::new(), String::new(), SharedString::default()),
            _subscriptions: subscriptions,
        }
    }

    // ----- helpers -------------------------------------------------------

    fn model_action(
        &self,
        f: impl Fn(&mut Fono8, &mut Context<Fono8>) + 'static,
    ) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
        let model = self.model.clone();
        move |_, _, cx| {
            model.update(cx, |m, cx| {
                f(m, cx);
                cx.notify();
            })
        }
    }

    fn update_model(&self, cx: &mut App, f: impl FnOnce(&mut Fono8, &mut Context<Fono8>)) {
        self.model.update(cx, |m, cx| {
            f(m, cx);
            cx.notify();
        });
    }

    fn handle_request(&mut self, request: WindowRequest, window: &mut Window, cx: &mut Context<Self>) {
        match request {
            WindowRequest::ExportPlaylist(id) => self.export_playlist(id, cx),
            WindowRequest::ApplyTranslucency => {
                let translucent = self.model.read(cx).translucent;
                window.set_background_appearance(if translucent {
                    WindowBackgroundAppearance::Transparent
                } else {
                    WindowBackgroundAppearance::Opaque
                });
            }
            WindowRequest::ToggleCompact => self.toggle_compact(window, cx),
            WindowRequest::ResetLayout => {
                self.update_model(cx, |m, _| m.reset_window_layout());
                if window.is_maximized() {
                    window.zoom_window();
                }
                window.resize(size(px(FULL_SIZE.0), px(FULL_SIZE.1)));
            }
            WindowRequest::Quit => self.update_model(cx, |m, cx| m.quit(cx)),
        }
    }

    fn toggle_compact(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let bounds = window.bounds();
        let maximized = window.is_maximized();
        let (target, maximize) = self.model.update(cx, |m, cx| {
            m.record_window_bounds(bounds, maximized);
            m.toggle_compact();
            cx.notify();
            if m.compact {
                (m.window_state.compact_size, false)
            } else {
                // Back to the full window: maximized again if it was maximized before mini mode.
                (m.window_state.full_size, m.window_state.maximized)
            }
        });
        if maximized {
            window.zoom_window();
        }
        window.resize(target);
        if maximize {
            window.zoom_window();
        } else if maximized {
            // Leaving the maximized state is asynchronous on Wayland, and the compositor then
            // restores its own remembered size: apply the target again once it has settled.
            cx.spawn_in(window, async move |_, cx| {
                for _ in 0..25 {
                    cx.background_executor().timer(Duration::from_millis(40)).await;
                    if !cx.update(|window, _| window.is_maximized()).unwrap_or(false) {
                        break;
                    }
                }
                for _ in 0..2 {
                    cx.background_executor().timer(Duration::from_millis(80)).await;
                    let _ = cx.update(|window, _| window.resize(target));
                }
            })
            .detach();
        }
    }

    /// Leave the mini player for the full window and maximize it.
    fn maximize_from_compact(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.update_model(cx, |m, _| m.window_state.maximized = true);
        self.toggle_compact(window, cx);
    }

    fn hide_to_tray(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let available = self.model.read(cx).tray_available();
        let bounds = window.bounds();
        let maximized = window.is_maximized();
        self.update_model(cx, |m, _| {
            m.record_window_bounds(bounds, maximized);
            m.save_window_state();
            if !available {
                m.set_status(Message::new("no_tray"));
            }
        });
        if cfg!(target_os = "macos") && available {
            cx.hide();
        } else {
            window.minimize_window();
        }
    }

    fn choose_folder(&mut self, cx: &mut Context<Self>) {
        let (busy, prompt, directory) = {
            let m = self.model.read(cx);
            (m.scanning(), m.t("choose_folder"), m.last_folder())
        };
        if busy {
            self.update_model(cx, |m, _| m.set_status(Message::new("scan_busy")));
            return;
        }
        let _ = directory;
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(prompt.into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await {
                if let Some(path) = paths.into_iter().next() {
                    let folder = path.to_string_lossy().into_owned();
                    let _ = this.update(cx, |view, cx| view.update_model(cx, |m, _| m.scan(&folder)));
                }
            }
        })
        .detach();
    }

    fn export_playlist(&mut self, playlist: i64, cx: &mut Context<Self>) {
        let (directory, name) = {
            let m = self.model.read(cx);
            (m.last_folder(), m.t("export_filename"))
        };
        let receiver = cx.prompt_for_new_path(&directory, Some(&name));
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = receiver.await {
                let _ = this.update(cx, |view, cx| {
                    view.update_model(cx, |m, _| match m.export_playlist(playlist, &path) {
                        Ok(()) => m.set_status(Message::new("export_done")),
                        Err(error) => {
                            let title = m.t("export_failed");
                            m.show_message(title, error);
                        }
                    })
                });
            }
        })
        .detach();
    }

    fn sync_search(&mut self, cx: &mut Context<Self>) {
        let query = self.model.read(cx).query.clone();
        let (text, placeholder) = {
            let m = self.model.read(cx);
            (query, m.t("search_library"))
        };
        self.search.update(cx, |input, cx| {
            if input.text() != text {
                input.set_text(text, cx);
            }
            input.set_placeholder(placeholder, cx);
        });
    }

    fn track_list_bounds(&self) -> (Bounds<Pixels>, Point<Pixels>) {
        let state = self.track_scroll.0.borrow();
        (state.base_handle.bounds(), state.base_handle.offset())
    }

    fn queue_list_bounds(&self) -> (Bounds<Pixels>, Point<Pixels>) {
        let state = self.queue_scroll.0.borrow();
        (state.base_handle.bounds(), state.base_handle.offset())
    }

    // ----- actions -------------------------------------------------------

    fn on_escape(&mut self, _: &Escape, window: &mut Window, cx: &mut Context<Self>) {
        let handled = self.model.update(cx, |m, _| {
            if m.drag.is_some() {
                m.cancel_drag();
                true
            } else if m.menu.is_some() {
                m.close_menu();
                true
            } else if m.dialog.is_some() {
                m.close_dialog();
                true
            } else if m.cast_open {
                m.close_cast_panel();
                true
            } else {
                false
            }
        });
        if handled {
            self.update_model(cx, |_, _| {});
        } else {
            window.focus(&self.focus_handle);
        }
    }
}

impl Render for MainView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_search(cx);
        let title = self.model.read(cx).window_title();
        {
            let m = self.model.read(cx);
            self.artist_texts = (m.t("artist_show_all"), m.t("artist_search_discover"), m.font_family.clone().into());
        }
        if title != self.last_title {
            window.set_window_title(&title);
            self.last_title = title;
        }
        let viewport = window.viewport_size();
        let width = f32::from(viewport.width);
        let height = f32::from(viewport.height);
        let m = self.model.read(cx);
        let font: SharedString = m.font_family.clone().into();
        let compact = m.compact;
        let translucent = m.translucent;
        let queue_open = m.queue_open;
        let has_menu = m.menu.is_some();
        let has_dialog = m.dialog.is_some();
        let cast_open = m.cast_open;
        let dragging = m.drag.is_some();
        let narrow = width < 800.;
        let wide_queue = width >= 1120.;

        let model = self.model.clone();
        let focus = self.focus_handle.clone();
        let mut root = div()
            .id("fono8-root")
            .key_context("Fono8")
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(12.))
            .border_1()
            .border_color(theme::BORDER)
            .bg(if translucent { theme::root_translucent() } else { theme::BACKGROUND })
            .font_family(font.clone())
            .text_size(px(theme::TEXT_BODY))
            .text_color(theme::TEXT)
            .on_action(cx.listener(|this, _: &PlayPause, _, cx| this.update_model(cx, |m, _| m.toggle_play())))
            .on_action(cx.listener(|this, _: &OpenFolder, _, cx| this.choose_folder(cx)))
            .on_action(cx.listener(|this, _: &NewPlaylist, _, cx| this.update_model(cx, |m, cx| m.new_playlist(cx))))
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                let compact = this.model.read(cx).compact;
                if !compact {
                    this.search.update(cx, |input, cx| {
                        input.focus(window);
                        input.select_all(cx);
                    });
                }
            }))
            .on_action(cx.listener(|this, _: &PreviousTrack, _, cx| this.update_model(cx, |m, _| m.previous())))
            .on_action(cx.listener(|this, _: &NextTrack, _, cx| this.update_model(cx, |m, _| m.next())))
            .on_action(cx.listener(|this, _: &ToggleMini, window, cx| this.toggle_compact(window, cx)))
            .on_action(cx.listener(|this, _: &HideToTray, window, cx| this.hide_to_tray(window, cx)))
            .on_action(cx.listener(|this, _: &QuitApp, _, cx| this.update_model(cx, |m, cx| m.quit(cx))))
            .on_action(cx.listener(Self::on_escape))
            // Click on empty space returns keyboard focus to the window (so Space works after typing).
            .on_mouse_down(MouseButton::Left, move |_, window, _| window.focus(&focus))
            // Resize from the window edges (client-side decorations).
            .capture_any_mouse_down(move |event: &MouseDownEvent, window, cx| {
                if event.button != MouseButton::Left || !cfg!(target_os = "linux") {
                    return;
                }
                let bounds = Bounds { origin: Point::default(), size: window.viewport_size() };
                let x = f32::from(event.position.x);
                let y = f32::from(event.position.y);
                let w = f32::from(bounds.size.width);
                let h = f32::from(bounds.size.height);
                let left = x <= EDGE;
                let right = x >= w - EDGE;
                let top = y <= EDGE;
                let bottom = y >= h - EDGE;
                let edge = match (left, right, top, bottom) {
                    (true, _, true, _) => Some(ResizeEdge::TopLeft),
                    (_, true, true, _) => Some(ResizeEdge::TopRight),
                    (true, _, _, true) => Some(ResizeEdge::BottomLeft),
                    (_, true, _, true) => Some(ResizeEdge::BottomRight),
                    (true, _, _, _) => Some(ResizeEdge::Left),
                    (_, true, _, _) => Some(ResizeEdge::Right),
                    (_, _, true, _) => Some(ResizeEdge::Top),
                    (_, _, _, true) => Some(ResizeEdge::Bottom),
                    _ => None,
                };
                if let Some(edge) = edge {
                    if !window.is_maximized() {
                        window.start_window_resize(edge);
                        cx.stop_propagation();
                    }
                }
            })
            .on_mouse_up(MouseButton::Left, {
                let model = model.clone();
                move |_, _, cx| {
                    if model.read(cx).drag.is_some() {
                        model.update(cx, |m, cx| {
                            m.finish_drag();
                            cx.notify();
                        });
                    }
                }
            })
            .on_mouse_up_out(MouseButton::Left, {
                let model = model.clone();
                move |_, _, cx| {
                    if model.read(cx).drag.is_some() {
                        model.update(cx, |m, cx| {
                            m.cancel_drag();
                            cx.notify();
                        });
                    }
                }
            })
            .on_mouse_move({
                let model = model.clone();
                let track_scroll = self.track_scroll.clone();
                let queue_scroll = self.queue_scroll.clone();
                move |event: &MouseMoveEvent, _, cx| {
                    let drag = model.read(cx).drag;
                    let Some(drag) = drag else { return };
                    let (bounds, offset, row, count) = match drag.list {
                        DragList::Tracks => {
                            let s = track_scroll.0.borrow();
                            (s.base_handle.bounds(), s.base_handle.offset(), TRACK_ROW, model.read(cx).tracks.len())
                        }
                        DragList::Queue => {
                            let s = queue_scroll.0.borrow();
                            (s.base_handle.bounds(), s.base_handle.offset(), QUEUE_ROW, model.read(cx).playback.queue.len())
                        }
                    };
                    if count == 0 {
                        return;
                    }
                    let y = f32::from(event.position.y) - f32::from(bounds.top());
                    let content_y = y - f32::from(offset.y);
                    let target = ((content_y / row).floor().max(0.0) as usize).min(count - 1);
                    model.update(cx, |m, cx| {
                        m.update_drag(target, y);
                        cx.notify();
                    });
                }
            })
            .when(dragging, |this| this.cursor(CursorStyle::ClosedHand));

        root = root.child(self.render_titlebar(compact, narrow, width, &font, window, cx));
        if !compact {
            root = root.child(self.render_workspace(narrow, wide_queue, queue_open, width, height, &font, window, cx));
        }
        root = root.child(self.render_player_bar(compact, width, &font, cx));
        root = root.child(self.render_status_bar(&font, cx));
        if queue_open && !wide_queue && !compact {
            root = root.child(self.render_queue_overlay(width, &font, cx));
        }
        if cast_open {
            root = root.child(self.render_cast_panel(width, height, compact, &font, cx));
        }
        if has_menu {
            root = root.child(self.render_menu(&font, cx));
        }
        if has_dialog {
            root = root.child(self.render_dialog(width, compact, &font, window, cx));
        }
        root
    }
}

impl MainView {
    /// The artists of a track as links: a click shows all their tracks in the library.
    /// `shown` is the display text (with "Unknown artist"), `raw` the stored value.
    fn artist_links(&self, shown: &str, raw: &str, id: (&'static str, usize), size_px: f32) -> Div {
        self.artist_links_to(shown, raw, id, size_px, false)
    }

    /// Artist links that search Discover (`discover`) instead of opening the library view.
    fn artist_links_to(&self, shown: &str, raw: &str, id: (&'static str, usize), size_px: f32, discover: bool) -> Div {
        let (show_all, search_discover, font) = self.artist_texts.clone();
        let mut line = div()
            .flex()
            .flex_row()
            .min_w_0()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(px(size_px))
            .text_color(theme::MUTED);
        let parts = crate::library::artist_parts(raw);
        if raw.trim().is_empty() || shown != raw || parts.is_empty() {
            return line.child(div().truncate().child(shown.to_string()));
        }
        for (index, (name, separator)) in parts.into_iter().enumerate() {
            let model = self.model.clone();
            let artist = name.clone();
            line = line.child(
                div()
                    .id(SharedString::from(format!("{}-{}-{index}", id.0, id.1)))
                    .flex_shrink_0()
                    .px(px(3.))
                    .mx(px(-3.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::ELEVATED).text_color(theme::ACCENT).underline())
                    .active(|s| s.bg(theme::selected_button()))
                    .tooltip(Tooltip::build(
                        format!("{} {name}", if discover { &search_discover } else { &show_all }),
                        font.clone(),
                    ))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        model.update(cx, |m, cx| {
                            if discover {
                                m.discover_search_artist(&artist, cx);
                            } else {
                                m.show_artist(&artist);
                            }
                            cx.notify();
                        });
                    })
                    .child(name.clone()),
            );
            if !separator.is_empty() {
                line = line.child(div().flex_shrink_0().whitespace_nowrap().child(separator));
            }
        }
        line
    }
}

/// The tile of the "My favorites" playlist: a heart on the accent gradient.
fn favorites_tile(size_px: f32) -> Div {
    div()
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .size(px(size_px))
        .rounded(px((size_px / 7.).min(10.)))
        .bg(linear_gradient(135., linear_color_stop(theme::PURPLE, 0.), linear_color_stop(gpui::rgb(0x1c8fa8), 1.)))
        .child(icon("heart-filled", size_px * 0.5, theme::TEXT))
}

fn t_fav(m: &Fono8, on: bool) -> String {
    m.t(if on { "favorites_remove" } else { "favorites_add" })
}

/// Where a track comes from: the streaming service, or `local` for files.
fn source_label(path: &str, local: &str) -> String {
    match crate::playback::remote_kind(path) {
        Some(crate::playback::Remote::YouTube) => Service::YouTube.name().to_string(),
        Some(crate::playback::Remote::Spotify) => Service::Spotify.name().to_string(),
        None if crate::tidal::is_track_path(path) => "TIDAL · 30 s".to_string(),
        None => local.to_string(),
    }
}

// ----- sections ------------------------------------------------------------

impl MainView {
    fn render_titlebar(
        &self,
        compact: bool,
        narrow: bool,
        width: f32,
        font: &SharedString,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let m = self.model.read(cx);
        let query = m.query.clone();
        let sleep_active = m.sleep_timer.active();
        let sleep_open = matches!(m.dialog, Some(Dialog::SleepTimer { .. }));
        let countdown = if sleep_active { m.sleep_timer.countdown() } else { String::new() };
        let t = |key: &str| m.t(key);
        let sleep_tooltip =
            if countdown.is_empty() { t("sleep_timer_title") } else { format!("{}: {}", t("sleep_timer_title"), countdown) };
        let texts = (t("clear_search"), t("mini_tooltip"), t("options"), t("hide_tooltip"), t("close_tooltip"));
        let settings_text = t("settings");
        let settings_open = m.page == Page::Settings;

        let mut bar = div()
            .id("titlebar")
            .flex()
            .flex_row()
            .items_center()
            .flex_shrink_0()
            .h(px(TITLEBAR_HEIGHT))
            .pl(px(TITLEBAR_PADDING))
            .pr(px(10.))
            .gap(px(8.))
            .window_control_area(WindowControlArea::Drag)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    if event.click_count == 2 && this.model.read(cx).compact {
                        // Maximizing the mini player would stretch its layout: go back to the
                        // full window, maximized.
                        this.maximize_from_compact(window, cx);
                    } else if cfg!(target_os = "linux") {
                        if event.click_count == 2 {
                            window.zoom_window();
                        } else {
                            window.start_window_move();
                        }
                    } else if event.click_count == 2 {
                        window.titlebar_double_click();
                    }
                    cx.stop_propagation();
                }),
            )
            .child(brand((!brand_animated(m)).then(|| brand_bars(BrandPose::Still))));
        if !compact {
            if !narrow {
                bar = bar.child(div().w(px(12.)).flex_shrink_0());
            }
            let search_focused = self.search.read(cx).is_focused(window);
            let mut search = div()
                .id("search-box")
                .flex()
                .flex_row()
                .items_center()
                .flex_1()
                .min_w_0()
                .max_w(px(470.))
                .h(px(34.))
                .pl(px(11.))
                .pr(px(4.))
                .gap(px(8.))
                .rounded(px(17.))
                .bg(theme::search_field())
                .border_1()
                .border_color(if search_focused { theme::ACCENT } else { theme::BORDER })
                .on_mouse_down(MouseButton::Left, {
                    let search = self.search.clone();
                    move |_, window, cx| {
                        search.update(cx, |input, _| input.focus(window));
                        cx.stop_propagation();
                    }
                })
                .child(icon("search", 15., theme::MUTED))
                .child(self.search.clone());
            if !query.is_empty() {
                let search_entity = self.search.clone();
                search = search.child(
                    action_button("clear-search", "close", font.clone()).size(28.).tooltip(texts.0.clone()).build({
                        let model = self.model.clone();
                        move |_, window, cx| {
                            search_entity.update(cx, |input, cx| {
                                input.set_text(String::new(), cx);
                                input.focus(window);
                            });
                            model.update(cx, |m, cx| {
                                m.set_query("");
                                cx.notify();
                            });
                        }
                    }),
                );
            }
            bar = bar.child(search);
        }
        if compact || width > 950. {
            bar = bar.child(div().flex_1());
        }
        bar = bar
            .child(
                action_button("sleep-timer", "clock", font.clone())
                    .caption(countdown.clone())
                    .tooltip(sleep_tooltip)
                    .selected(sleep_active || sleep_open)
                    .build(self.model_action(|m, cx| m.open_sleep_timer(cx))),
            )
            .child(
                action_button("mini", "mini", font.clone())
                    .tooltip(texts.1.clone())
                    .selected(compact)
                    .build(cx.listener(|this, _, window, cx| this.toggle_compact(window, cx))),
            )
            .child(
                action_button("settings", "settings", font.clone())
                    .tooltip(settings_text)
                    .selected(settings_open)
                    .build(self.model_action(|m, cx| m.open_settings(SettingsTab::Accounts, cx))),
            )
            .child(
                action_button("options", "more", font.clone())
                    .tooltip(texts.2.clone())
                    .build(self.menu_at_button(|m, pos| m.options_menu(pos))),
            )
            .child(
                action_button("hide", "tray", font.clone())
                    .tooltip(texts.3.clone())
                    .build(cx.listener(|this, _, window, cx| this.hide_to_tray(window, cx))),
            );
        if !cfg!(target_os = "macos") {
            bar = bar.child(action_button("close", "close", font.clone()).tooltip(texts.4.clone()).build(cx.listener(
                |this, _, window, cx| {
                    let tray = this.model.read(cx).tray_available();
                    if tray {
                        this.hide_to_tray(window, cx);
                    } else {
                        this.update_model(cx, |m, cx| m.quit(cx));
                    }
                },
            )));
        }
        bar
    }

    /// Open a menu anchored under the clicked button.
    fn menu_at_button(
        &self,
        open: impl Fn(&mut Fono8, Point<Pixels>) + 'static,
    ) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
        let model = self.model.clone();
        move |event, _, cx| {
            let position = event.position();
            model.update(cx, |m, cx| {
                open(m, Point { x: position.x - px(60.), y: position.y + px(12.) });
                cx.notify();
            });
        }
    }

    fn render_workspace(
        &mut self,
        narrow: bool,
        wide_queue: bool,
        queue_open: bool,
        width: f32,
        height: f32,
        font: &SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let sidebar_width = if narrow { 62. } else { 188. };
        let queue_width = if queue_open && wide_queue { 248. + 8. } else { 0. };
        let content_width = (width - 18. - sidebar_width - 8. - queue_width).max(200.);
        let mut workspace = div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h_0()
            .px(px(9.))
            .gap(px(8.))
            .child(self.render_sidebar(narrow, sidebar_width, font, cx))
            .child(self.render_content(content_width, height, font, window, cx));
        if queue_open && wide_queue {
            workspace = workspace.child(self.render_queue_panel(248., font, cx));
        }
        workspace
    }

    fn render_sidebar(&self, narrow: bool, width: f32, font: &SharedString, cx: &mut Context<Self>) -> Div {
        let m = self.model.read(cx);
        let page = m.page;
        let current = m.current_playlist;
        let busy = m.scanning();
        let playlists = m.playlists.clone();
        let texts = (m.t("home"), m.t("all_tracks"), m.t("your_library"), m.t("new_playlist"), m.t("add_folder"));
        let discover_text = m.t("discover");
        let artist_view = m.artist_filter.is_some();

        let mut list = div().id("playlists").flex().flex_col().flex_1().min_h_0().overflow_y_scroll().gap(px(4.));
        for playlist in playlists {
            let id = playlist.id;
            let selected = page == Page::Library && current == Some(id);
            let image = playlist.cover_path.as_deref().and_then(|p| self.model.update(cx, |m, _| m.artwork.get(p)));
            let subtitle = format!(
                "{}{}",
                if playlist.pinned { "• " } else { "" },
                self.model.read(cx).text("tracks_count", &[("n", playlist.count.into())])
            );
            let mut row = div()
                .id(("playlist", id as u64))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(9.))
                .h(px(54.))
                .px(px(9.))
                .rounded(px(7.))
                .flex_shrink_0()
                .cursor_pointer()
                .bg(if selected { theme::playlist_selected() } else { gpui::transparent_black().into() })
                .when(!selected, |this| this.hover(|s| s.bg(theme::ELEVATED)))
                .tooltip(Tooltip::build(playlist.name.clone(), font.clone()))
                .on_click({
                    let model = self.model.clone();
                    move |event, _, cx| {
                        model.update(cx, |m, cx| {
                            if event.click_count() >= 2 {
                                m.play_playlist(id);
                            } else {
                                m.select_playlist(Some(id));
                            }
                            cx.notify();
                        })
                    }
                })
                .on_mouse_down(MouseButton::Right, {
                    let model = self.model.clone();
                    move |event: &MouseDownEvent, _, cx| {
                        let position = event.position;
                        model.update(cx, |m, cx| {
                            m.select_playlist(Some(id));
                            m.playlist_menu(id, position);
                            cx.notify();
                        });
                        cx.stop_propagation();
                    }
                })
                .child(if playlist.favorite { favorites_tile(36.) } else { cover(image, &playlist.name, 36.) });
            if !narrow {
                row = row.child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .gap(px(4.))
                        .child(
                            div()
                                .text_size(px(theme::TEXT_BODY))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .truncate()
                                .child(playlist.name.clone()),
                        )
                        .child(div().text_size(px(theme::TEXT_SMALL)).text_color(theme::MUTED).truncate().child(subtitle)),
                );
            }
            list = list.child(row);
        }

        let mut header = div().flex().flex_row().items_center().mt(px(15.)).mb(px(4.)).flex_shrink_0();
        if !narrow {
            header = header.child(
                div().flex_1().text_size(px(theme::TEXT_DETAIL)).text_color(theme::MUTED).truncate().child(texts.2.clone()),
            );
        }
        header = header.child(
            action_button("new-playlist", "plus", font.clone())
                .tooltip(texts.3.clone())
                .build(self.model_action(|m, cx| m.new_playlist(cx))),
        );

        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w(px(width))
            .h_full()
            .rounded(px(10.))
            .bg(theme::panel())
            .p(px(if narrow { 8. } else { 12. }))
            .gap(px(6.))
            .child(
                action_button("nav-home", "home", font.clone())
                    .caption(if narrow { String::new() } else { texts.0.clone() })
                    .tooltip(texts.0.clone())
                    .selected(page == Page::Home)
                    .fill_width(true)
                    .build(self.model_action(|m, _| m.show_home())),
            )
            .child(
                action_button("nav-all", "music", font.clone())
                    .caption(if narrow { String::new() } else { texts.1.clone() })
                    .tooltip(texts.1.clone())
                    .selected(page == Page::Library && current.is_none() && !artist_view)
                    .fill_width(true)
                    .build(self.model_action(|m, _| m.select_playlist(None))),
            )
            .child(header)
            .child(list)
            .child(separator())
            .child(
                action_button("add-folder", "folder", font.clone())
                    .caption(if narrow { String::new() } else { texts.4.clone() })
                    .tooltip(texts.4.clone())
                    .enabled(!busy)
                    .fill_width(true)
                    .build(cx.listener(|this, _, _, cx| this.choose_folder(cx))),
            )
            .child(
                action_button("nav-discover", "search", font.clone())
                    .caption(if narrow { String::new() } else { discover_text.clone() })
                    .tooltip(discover_text)
                    .selected(page == Page::Discover)
                    .fill_width(true)
                    .build(self.model_action(|m, cx| m.open_discover(None, cx))),
            )
    }

    fn render_content(
        &mut self,
        content_width: f32,
        height: f32,
        font: &SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let page = self.model.read(cx).page;
        let panel = div().flex().flex_col().flex_1().min_w_0().h_full().rounded(px(10.)).bg(theme::panel()).overflow_hidden();
        match page {
            Page::Home => panel.child(self.render_home(content_width, font, cx)),
            Page::Library => panel.child(self.render_library(content_width, height, font, window, cx)),
            Page::Discover => panel.child(self.render_discover(font, window, cx)),
            Page::Settings => panel.child(self.render_settings(font, window, cx)),
        }
    }

    fn render_home(&self, content_width: f32, font: &SharedString, cx: &mut Context<Self>) -> Stateful<Div> {
        let m = self.model.read(cx);
        let pinned: Vec<_> = m.playlists.iter().filter(|p| p.pinned).cloned().collect();
        let playlists = m.playlists.clone();
        let recent: Vec<Track> = m.recent.iter().take(6).cloned().collect();
        let busy = m.scanning();
        let t = |key: &str| m.t(key);
        let texts = (
            t("home_title"),
            t("home_subtitle"),
            t("add_folder"),
            t("pinned"),
            t("recently_played"),
            t("recent_empty"),
            t("your_playlists"),
            t("home_empty"),
        );
        let count_text = |n: usize| m.text("tracks_count", &[("n", n.into())]);
        let playlist_subtitles: Vec<String> = playlists.iter().map(|p| count_text(p.count)).collect();
        let artist = |value: &str| m.i18n.artist(value);
        let recent_artists: Vec<String> = recent.iter().map(|t| artist(&t.artist)).collect();

        let inner_width = content_width - 44.;
        let columns = if inner_width > 600. { 4. } else { 3. };
        let card_width = (180.0f32).min(((inner_width - 12. * (columns - 1.)) / columns).floor()).max(90.);

        let mut column = div().flex().flex_col().gap(px(18.)).w(px(inner_width.max(100.)));
        column = column.child(
            div()
                .w_full()
                .rounded(px(10.))
                .p(px(20.))
                .flex()
                .flex_col()
                .gap(px(10.))
                .bg(linear_gradient(90., linear_color_stop(gpui::rgb(0x282541), 0.), linear_color_stop(gpui::rgb(0x133345), 1.)))
                .child(div().text_size(px(theme::TEXT_SMALL)).text_color(theme::ACCENT).child("F O N O ∞"))
                .child(
                    div()
                        .text_size(px(if content_width > 560. { 27. } else { 22. }))
                        .font_weight(gpui::FontWeight::BOLD)
                        .child(texts.0.clone()),
                )
                .child(div().text_size(px(theme::TEXT_DETAIL)).text_color(theme::MUTED).child(texts.1.clone()))
                .child(
                    div().flex().child(
                        action_button("home-add-folder", "folder", font.clone())
                            .caption(texts.2.clone())
                            .accent(true)
                            .enabled(!busy)
                            .build(cx.listener(|this, _, _, cx| this.choose_folder(cx))),
                    ),
                ),
        );
        if !pinned.is_empty() {
            column = column.child(div().text_size(px(18.)).font_weight(gpui::FontWeight::BOLD).child(texts.3.clone()));
            let mut flow = div().flex().flex_row().flex_wrap().gap(px(8.)).w_full();
            for playlist in &pinned {
                let id = playlist.id;
                let image = playlist.cover_path.as_deref().and_then(|p| self.model.update(cx, |m, _| m.artwork.get(p)));
                flow = flow.child(
                    div()
                        .id(("pinned", id as u64))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .w(px((300.0f32).min((inner_width - 8.) / 2.)))
                        .h(px(48.))
                        .px(px(8.))
                        .rounded(px(6.))
                        .bg(theme::pinned_card())
                        .hover(|s| s.bg(theme::ELEVATED))
                        .cursor_pointer()
                        .on_click(self.model_action(move |m, _| m.select_playlist(Some(id))))
                        .on_mouse_down(MouseButton::Right, self.playlist_context(id))
                        .child(if playlist.favorite { favorites_tile(34.) } else { cover(image, &playlist.name, 34.) })
                        .child(div().flex_1().min_w_0().text_size(px(theme::TEXT_BODY)).truncate().child(playlist.name.clone())),
                );
            }
            column = column.child(flow);
        }
        column = column.child(div().text_size(px(18.)).font_weight(gpui::FontWeight::BOLD).child(texts.4.clone()));
        if recent.is_empty() {
            column = column.child(div().text_color(theme::MUTED).child(texts.5.clone()));
        } else {
            let mut flow = div().flex().flex_row().flex_wrap().gap(px(12.)).w_full();
            for (index, track) in recent.iter().enumerate() {
                let path = track.path.clone();
                let image = self.model.update(cx, |m, _| m.artwork.get(&track.path));
                flow = flow.child(
                    self.card(
                        ("recent", index as u64),
                        image,
                        &track.title,
                        &track.title,
                        &recent_artists[index],
                        card_width,
                        false,
                    )
                    .on_click(self.model_action({
                        let path = path.clone();
                        move |m, _| m.play_recent(&path)
                    }))
                    .on_mouse_down(MouseButton::Right, {
                        let model = self.model.clone();
                        move |event: &MouseDownEvent, _, cx| {
                            let position = event.position;
                            let path = path.clone();
                            model.update(cx, |m, cx| {
                                m.recent_menu(&path, position);
                                cx.notify();
                            });
                            cx.stop_propagation();
                        }
                    }),
                );
            }
            column = column.child(flow);
        }
        column = column.child(div().text_size(px(18.)).font_weight(gpui::FontWeight::BOLD).child(texts.6.clone()));
        if playlists.is_empty() {
            column = column.child(div().text_color(theme::MUTED).child(texts.7.clone()));
        } else {
            let mut flow = div().flex().flex_row().flex_wrap().gap(px(12.)).w_full();
            for (index, playlist) in playlists.iter().enumerate() {
                let id = playlist.id;
                let image = playlist.cover_path.as_deref().and_then(|p| self.model.update(cx, |m, _| m.artwork.get(p)));
                flow = flow.child(
                    self.card(
                        ("playlist-card", id as u64),
                        image,
                        &playlist.name,
                        &playlist.name,
                        &playlist_subtitles[index],
                        card_width,
                        playlist.favorite,
                    )
                    .on_click(self.model_action(move |m, _| m.select_playlist(Some(id))))
                    .on_mouse_down(MouseButton::Right, self.playlist_context(id)),
                );
            }
            column = column.child(flow);
        }

        div().id("home").flex_1().min_h_0().overflow_y_scroll().p(px(22.)).pb(px(44.)).child(column)
    }

    fn card(
        &self,
        id: impl Into<gpui::ElementId>,
        image: Option<Arc<gpui::RenderImage>>,
        cover_title: &str,
        title: &str,
        subtitle: &str,
        width: f32,
        favorite: bool,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .flex()
            .flex_col()
            .gap(px(7.))
            .w(px(width))
            .h(px(width + 48.))
            .p(px(6.))
            .rounded(px(8.))
            .cursor_pointer()
            .hover(|s| s.bg(theme::ELEVATED))
            .child(if favorite { favorites_tile(width - 12.) } else { cover(image, cover_title, width - 12.) })
            .child(
                div().text_size(px(theme::TEXT_BODY)).font_weight(gpui::FontWeight::SEMIBOLD).truncate().child(title.to_string()),
            )
            .child(div().text_size(px(theme::TEXT_SMALL)).text_color(theme::MUTED).truncate().child(subtitle.to_string()))
    }

    fn playlist_context(&self, id: i64) -> impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static {
        let model = self.model.clone();
        move |event, _, cx| {
            let position = event.position;
            model.update(cx, |m, cx| {
                m.playlist_menu(id, position);
                cx.notify();
            });
            cx.stop_propagation();
        }
    }

    fn render_library(
        &mut self,
        content_width: f32,
        height: f32,
        font: &SharedString,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let m = self.model.read(cx);
        let heading = m.heading();
        let details = m.details();
        let is_playlist = m.current_playlist_row().is_some();
        let (empty_title, empty_body) = m.empty_texts();
        let count = m.tracks.len();
        let first_cover_path = m.tracks.first().map(|t| t.path.clone());
        let can_reorder = m.can_reorder();
        let current_path = m.playback.current().map(str::to_string);
        let drag = m.drag.filter(|d| d.list == DragList::Tracks);
        let current_playlist = m.current_playlist;
        let artist_filter = m.artist_filter.clone();
        let t = |key: &str| m.t(key);
        let search_online_text = t("search_online");
        let texts = (
            if artist_filter.is_some() {
                t("artist")
            } else if is_playlist {
                t("playlist")
            } else {
                t("your_library")
            },
            t("play"),
            t("add_playlist_to_queue"),
            t("pin_playlist"),
            t("playlist_options"),
            t("column_title"),
            t("column_artist"),
            t("column_album"),
            t("source_local"),
            t("reorder_track"),
        );
        let cover_image = first_cover_path.as_deref().and_then(|p| self.model.update(cx, |m, _| m.artwork.get(p)));

        let short = height < 540.;
        let header_height = if short { 112. } else { 150. };
        let margin = if short { 10. } else { 18. };
        let cover_size = header_height - margin * 2.;
        let show_artist = content_width > 670.;
        let show_album = content_width > 850.;

        let mut header_buttons = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.))
            .child(
                action_button("play-all", "play", font.clone())
                    .caption(texts.1.clone())
                    .accent(true)
                    .enabled(count > 0)
                    .build(self.model_action(|m, _| m.start_visible())),
            )
            .child(action_button("enqueue-playlist", "queue", font.clone()).tooltip(texts.2.clone()).build(self.model_action(
                |m, _| {
                    let playlist = m.current_playlist;
                    m.enqueue_playlist(playlist)
                },
            )));
        if let Some(artist) = artist_filter {
            header_buttons = header_buttons.child(
                action_button("search-online", "search", font.clone())
                    .caption(search_online_text)
                    .build(self.model_action(move |m, cx| m.search_online(&artist, cx))),
            );
        }
        if let Some(id) = current_playlist {
            header_buttons = header_buttons
                .child(
                    action_button("pin", "pin", font.clone())
                        .tooltip(texts.3.clone())
                        .build(self.model_action(|m, _| m.toggle_pin())),
                )
                .child(
                    action_button("playlist-options", "more", font.clone())
                        .tooltip(texts.4.clone())
                        .build(self.menu_at_button(move |m, pos| m.playlist_menu(id, pos))),
                );
        }

        let mut header = div()
            .flex()
            .flex_row()
            .items_center()
            .flex_shrink_0()
            .h(px(header_height))
            .p(px(margin))
            .gap(px(18.))
            .bg(linear_gradient(90., linear_color_stop(gpui::rgb(0x27263f), 0.), linear_color_stop(gpui::rgb(0x122a3c), 1.)));
        if content_width > 400. {
            header = header.child(cover(cover_image, &heading, cover_size));
        }
        header = header.child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap(px(if short { 3. } else { 6. }))
                .child(
                    div()
                        .text_size(px(theme::TEXT_SMALL))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme::ACCENT)
                        .child(texts.0.clone()),
                )
                .child(
                    div()
                        .text_size(px(if short {
                            20.
                        } else if content_width > 530. {
                            30.
                        } else {
                            23.
                        }))
                        .font_weight(gpui::FontWeight::BOLD)
                        .truncate()
                        .child(heading.clone()),
                )
                .child(div().text_size(px(theme::TEXT_DETAIL)).text_color(theme::MUTED).truncate().child(details))
                .child(header_buttons),
        );

        let mut columns = div()
            .flex()
            .flex_row()
            .items_center()
            .flex_shrink_0()
            .h(px(34.))
            .pl(px(12.))
            .pr(px(14.))
            .gap(px(10.))
            .text_size(px(theme::TEXT_SMALL))
            .text_color(theme::MUTED)
            .child(div().w(px(22.)).text_center().child("#"))
            .child(div().flex_1().min_w_0().child(texts.5.clone()));
        if show_artist {
            columns = columns.child(div().w(px(130.)).child(texts.6.clone()));
        }
        if show_album {
            columns = columns.child(div().w(px(125.)).child(texts.7.clone()));
        }
        columns =
            columns.child(div().w(px(44.)).flex().justify_end().child(icon("clock", 14., theme::MUTED))).child(div().w(px(22.)));

        let model = self.model.clone();
        let font_for_rows = font.clone();
        let source_text = texts.8.clone();
        let reorder_text = texts.9.clone();
        let list = uniform_list(
            "tracks",
            count,
            cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                let rows: Vec<(usize, Track, bool, String)> = {
                    let m = this.model.read(cx);
                    range
                        .clone()
                        .filter_map(|index| {
                            m.tracks.get(index).map(|track| {
                                (index, track.clone(), m.selected.contains(&track.path), m.i18n.artist(&track.artist))
                            })
                        })
                        .collect()
                };
                rows.into_iter()
                    .map(|(index, track, selected, artist)| {
                        let image = this.model.update(cx, |m, _| m.artwork.get(&track.path));
                        this.track_row(
                            index,
                            track,
                            selected,
                            artist,
                            image,
                            current_path.as_deref(),
                            show_artist,
                            show_album,
                            can_reorder,
                            drag.map(|d| d.source),
                            &source_text,
                            &reorder_text,
                            &font_for_rows,
                            cx,
                        )
                    })
                    .collect()
            }),
        )
        .track_scroll(self.track_scroll.clone())
        .flex_1()
        .min_h_0()
        .w_full();

        let mut list_container = div()
            .id("track-list")
            .key_context("TrackList")
            .track_focus(&self.list_focus)
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .on_action(cx.listener(|this, _: &DeleteSelected, _, cx| this.update_model(cx, |m, _| m.remove_selected())))
            .on_action(cx.listener(|this, _: &PlayFocused, _, cx| this.update_model(cx, |m, _| m.start_selected())))
            .on_action(cx.listener(|this, _: &FocusUp, _, cx| this.update_model(cx, |m, _| m.move_focus(-1, false, false))))
            .on_action(cx.listener(|this, _: &FocusDown, _, cx| this.update_model(cx, |m, _| m.move_focus(1, false, false))))
            .on_action(cx.listener(|this, _: &FocusUpShift, _, cx| this.update_model(cx, |m, _| m.move_focus(-1, false, true))))
            .on_action(cx.listener(|this, _: &FocusDownShift, _, cx| this.update_model(cx, |m, _| m.move_focus(1, false, true))))
            .on_action(cx.listener(|this, _: &SelectAllTracks, _, cx| this.update_model(cx, |m, _| m.select_all())))
            .on_mouse_down(MouseButton::Left, {
                let focus = self.list_focus.clone();
                move |_, window, cx| {
                    window.focus(&focus);
                    cx.stop_propagation();
                }
            })
            .child(list);
        let _ = model;
        if count == 0 {
            list_container = list_container.child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(12.))
                    .px(px(24.))
                    .child(div().text_size(px(18.)).font_weight(gpui::FontWeight::BOLD).text_center().child(empty_title))
                    .child(div().text_color(theme::MUTED).text_center().child(empty_body)),
            );
        }
        if let Some(drag) = drag {
            let (_, offset) = self.track_list_bounds();
            let slot = drag.target + if drag.target > drag.source { 1 } else { 0 };
            let y = slot as f32 * TRACK_ROW + f32::from(offset.y) - 1.5;
            list_container = list_container.child(drop_indicator(y, 12.));
        }

        div().flex().flex_col().flex_1().min_h_0().child(header).child(columns).child(separator()).child(list_container)
    }

    #[allow(clippy::too_many_arguments)]
    fn track_row(
        &self,
        index: usize,
        track: Track,
        selected: bool,
        artist: String,
        image: Option<Arc<gpui::RenderImage>>,
        current_path: Option<&str>,
        show_artist: bool,
        show_album: bool,
        can_reorder: bool,
        drag_source: Option<usize>,
        source_text: &str,
        reorder_text: &str,
        font: &SharedString,
        _cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let is_current = current_path == Some(track.path.as_str());
        let number = if is_current { "♪".to_string() } else { (index + 1).to_string() };
        let source_text = source_label(&track.path, source_text);
        let source_line = if show_artist { source_text } else { format!("{source_text} · {artist}") };
        let model = self.model.clone();
        let mut row = div()
            .id(("track", index as u64))
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .h(px(TRACK_ROW))
            .pl(px(12.))
            .pr(px(14.))
            .gap(px(10.))
            .opacity(if drag_source == Some(index) { 0.45 } else { 1.0 })
            .bg(if selected {
                theme::selected_row()
            } else if index % 2 == 1 {
                theme::stripe_row()
            } else {
                gpui::transparent_black().into()
            })
            .when(!selected, |this| this.hover(|s| s.bg(theme::hover_row())))
            .on_click({
                let model = model.clone();
                move |event: &ClickEvent, _, cx| {
                    let modifiers = event.modifiers();
                    model.update(cx, |m, cx| {
                        if event.click_count() >= 2 {
                            m.play_track(index);
                        } else {
                            m.select_track(index, modifiers.control, modifiers.shift);
                        }
                        cx.notify();
                    });
                }
            })
            .on_mouse_down(MouseButton::Right, {
                let model = model.clone();
                move |event: &MouseDownEvent, _, cx| {
                    let position = event.position;
                    model.update(cx, |m, cx| {
                        let already = m.tracks.get(index).map(|t| m.selected.contains(&t.path)).unwrap_or(false);
                        if !already {
                            m.select_track(index, false, false);
                        }
                        m.track_menu(position);
                        cx.notify();
                    });
                    cx.stop_propagation();
                }
            })
            .child(
                div()
                    .w(px(22.))
                    .text_size(px(theme::TEXT_DETAIL))
                    .text_center()
                    .text_color(if is_current { theme::ACCENT } else { theme::MUTED })
                    .child(number),
            )
            .child(cover(image, if track.album.is_empty() { &track.title } else { &track.album }, 34.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(5.))
                    .child(
                        div()
                            .text_size(px(theme::TEXT_BODY))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(if is_current { theme::ACCENT } else { theme::TEXT })
                            .truncate()
                            .child(track.title.clone()),
                    )
                    .child(div().text_size(px(theme::TEXT_SMALL)).text_color(theme::MUTED).truncate().child(source_line)),
            );
        if show_artist {
            row = row.child(self.artist_links(&artist, &track.artist, ("track-artist", index), 11.).w(px(130.)).flex_shrink_0());
        }
        if show_album {
            row = row.child(
                div()
                    .w(px(125.))
                    .text_size(px(theme::TEXT_DETAIL))
                    .text_color(theme::MUTED)
                    .truncate()
                    .child(track.album.clone()),
            );
        }
        row = row.child(
            div()
                .w(px(44.))
                .text_size(px(theme::TEXT_DETAIL))
                .text_color(theme::MUTED)
                .text_right()
                .child(format_duration(track.duration)),
        );
        let mut handle = div().id(("track-handle", index as u64)).w(px(22.)).h_full().flex().items_center().justify_center();
        if can_reorder {
            handle = handle
                .cursor(CursorStyle::OpenHand)
                .tooltip(Tooltip::build(reorder_text.to_string(), font.clone()))
                .on_mouse_down(MouseButton::Left, {
                    let model = model.clone();
                    move |event: &MouseDownEvent, _, cx| {
                        let y = f32::from(event.position.y);
                        model.update(cx, |m, cx| {
                            m.select_track(index, false, false);
                            m.begin_drag(DragList::Tracks, index, y);
                            cx.notify();
                        });
                        cx.stop_propagation();
                    }
                })
                .child(icon("drag", 14., theme::MUTED));
        }
        row.child(handle)
    }

    fn render_queue_panel(&mut self, width: f32, font: &SharedString, cx: &mut Context<Self>) -> Div {
        let m = self.model.read(cx);
        let queue = m.queue_tracks();
        let count = queue.len();
        let queue_index = m.playback.index;
        let drag = m.drag.filter(|d| d.list == DragList::Queue);
        let t = |key: &str| m.t(key);
        let texts = (
            t("queue"),
            t("clear_queue_tooltip"),
            t("queue_note"),
            t("save_queue_playlist"),
            t("queue_empty"),
            t("source_local"),
            t("reorder_track"),
        );
        let artists: Vec<String> = queue.iter().map(|(_, t)| m.i18n.artist(&t.artist)).collect();

        let font_for_rows = font.clone();
        let source_text = texts.5.clone();
        let reorder_text = texts.6.clone();
        let queue_rows: Arc<Vec<(usize, Track, String)>> =
            Arc::new(queue.into_iter().zip(artists).map(|((i, t), a)| (i, t, a)).collect());
        let list = uniform_list(
            "queue",
            count,
            cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                range
                    .filter_map(|row| queue_rows.get(row).cloned().map(|entry| (row, entry)))
                    .map(|(row, (queue_position, track, artist))| {
                        let image = this.model.update(cx, |m, _| m.artwork.get(&track.path));
                        this.queue_row(
                            row,
                            queue_position,
                            track,
                            artist,
                            image,
                            queue_index,
                            count,
                            drag.map(|d| d.source),
                            &source_text,
                            &reorder_text,
                            &font_for_rows,
                        )
                    })
                    .collect()
            }),
        )
        .track_scroll(self.queue_scroll.clone())
        .flex_1()
        .min_h_0()
        .w_full();

        let mut list_container = div().relative().flex().flex_col().flex_1().min_h_0().child(list);
        if count == 0 {
            list_container = list_container.child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .px(px(10.))
                    .text_center()
                    .text_color(theme::MUTED)
                    .child(texts.4.clone()),
            );
        }
        if let Some(drag) = drag {
            let (_, offset) = self.queue_list_bounds();
            let slot = drag.target + if drag.target > drag.source { 1 } else { 0 };
            let y = slot as f32 * QUEUE_ROW + f32::from(offset.y) - 3.5;
            list_container = list_container.child(drop_indicator(y, 4.));
        }

        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w(px(width))
            .h_full()
            .rounded(px(10.))
            .bg(theme::queue_panel())
            .p(px(12.))
            .gap(px(8.))
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .child(div().flex_1().text_size(px(17.)).font_weight(gpui::FontWeight::BOLD).child(texts.0.clone()))
                    .child(
                        action_button("clear-queue", "trash", font.clone())
                            .tooltip(texts.1.clone())
                            .enabled(count > 0)
                            .build(self.model_action(|m, _| m.clear_queue())),
                    )
                    .child(
                        action_button("close-queue", "close", font.clone())
                            .tooltip(texts.0.clone())
                            .build(self.model_action(|m, _| m.toggle_queue())),
                    ),
            )
            .child(div().text_size(px(theme::TEXT_TINY)).text_color(theme::MUTED).child(texts.2.clone()))
            .child(separator())
            .child(
                action_button("save-queue", "list", font.clone())
                    .caption(texts.3.clone())
                    .enabled(count > 0)
                    .fill_width(true)
                    .build(self.model_action(|m, cx| m.save_queue_playlist(cx))),
            )
            .child(list_container)
    }

    #[allow(clippy::too_many_arguments)]
    fn queue_row(
        &self,
        row: usize,
        queue_position: usize,
        track: Track,
        artist: String,
        image: Option<Arc<gpui::RenderImage>>,
        queue_index: Option<usize>,
        count: usize,
        drag_source: Option<usize>,
        source_text: &str,
        reorder_text: &str,
        font: &SharedString,
    ) -> Stateful<Div> {
        let is_current = queue_index == Some(queue_position);
        let model = self.model.clone();
        let source_text = source_label(&track.path, source_text);
        let tooltip = format!("{}\n{} · {}", track.title, artist, source_text);
        let mut handle = div().id(("queue-handle", row as u64)).w(px(18.)).h_full().flex().items_center().justify_center();
        if count > 1 {
            handle = handle
                .cursor(CursorStyle::OpenHand)
                .tooltip(Tooltip::build(reorder_text.to_string(), font.clone()))
                .on_mouse_down(MouseButton::Left, {
                    let model = model.clone();
                    move |event: &MouseDownEvent, _, cx| {
                        let y = f32::from(event.position.y);
                        model.update(cx, |m, cx| {
                            m.begin_drag(DragList::Queue, queue_position, y);
                            cx.notify();
                        });
                        cx.stop_propagation();
                    }
                })
                .child(icon("drag", 14., theme::MUTED));
        }
        div()
            .id(("queue-row", row as u64))
            .w_full()
            .h(px(QUEUE_ROW))
            .pb(px(4.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .h_full()
                    .pl(px(8.))
                    .pr(px(12.))
                    .py(px(8.))
                    .gap(px(9.))
                    .rounded(px(7.))
                    .cursor_pointer()
                    .opacity(if drag_source == Some(queue_position) { 0.45 } else { 1.0 })
                    .bg(if is_current { theme::queue_current() } else { gpui::transparent_black().into() })
                    .when(!is_current, |this| this.hover(|s| s.bg(theme::ELEVATED)))
                    .child(cover(image, &track.title, 38.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .gap(px(3.))
                            .child(
                                div()
                                    .text_size(px(theme::TEXT_BODY))
                                    .text_color(if is_current { theme::ACCENT } else { theme::TEXT })
                                    .truncate()
                                    .child(track.title.clone()),
                            )
                            .child(self.artist_links(&artist, &track.artist, ("queue-artist", row), 10.))
                            .child(
                                div()
                                    .text_size(px(theme::TEXT_TINY))
                                    .text_color(theme::MUTED)
                                    .truncate()
                                    .child(source_text.clone()),
                            ),
                    )
                    .child(handle),
            )
            .tooltip(Tooltip::build(tooltip, font.clone()))
            .on_click({
                let model = model.clone();
                move |_, _, cx| {
                    model.update(cx, |m, cx| {
                        m.play_queue(queue_position);
                        cx.notify();
                    })
                }
            })
            .on_mouse_down(MouseButton::Right, {
                let model = model.clone();
                move |event: &MouseDownEvent, _, cx| {
                    let position = event.position;
                    model.update(cx, |m, cx| {
                        m.queue_menu(queue_position, position);
                        cx.notify();
                    });
                    cx.stop_propagation();
                }
            })
    }

    fn render_queue_overlay(&mut self, width: f32, font: &SharedString, cx: &mut Context<Self>) -> Stateful<Div> {
        let panel_width = (300.0f32).min(width - 30.);
        div()
            .id("queue-overlay")
            .absolute()
            .left(px(1.))
            .right(px(1.))
            .top(px(53.))
            .bottom(px(130.))
            .bg(theme::queue_overlay())
            .flex()
            .flex_row()
            .justify_end()
            .pr(px(9.))
            .on_click(self.model_action(|m, _| m.toggle_queue()))
            .child(
                div()
                    .id("queue-overlay-panel")
                    .w(px(panel_width))
                    .h_full()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(self.render_queue_panel(panel_width, font, cx)),
            )
    }

    fn render_player_bar(&self, compact: bool, width: f32, font: &SharedString, cx: &mut Context<Self>) -> Div {
        let m = self.model.read(cx);
        let playing = m.playback.playing();
        let favorite = m.current_track.as_ref().map(|t| (t.path.clone(), m.is_favorite(&t.path)));
        let favorite_text = t_fav(m, favorite.as_ref().is_some_and(|(_, on)| *on));
        let song_raw_artist = m.current_track.as_ref().map(|t| t.artist.clone());
        let (song_title, song_artist, song_cover_path) = match &m.current_track {
            Some(track) => (track.title.clone(), m.i18n.artist(&track.artist), Some(track.path.clone())),
            None => (m.t("idle_song"), m.t("idle_artist"), None),
        };
        let position = m.playback.position_ms();
        let duration = m.playback.duration_ms();
        let shuffle = m.playback.shuffle;
        let repeat = m.playback.repeat;
        let muted = m.playback.muted;
        let volume = m.volume();
        let queue_open = m.queue_open;
        let cast_status = m.cast_status();
        let cast_selected = cast_status.state == CastState::Connected || m.cast_open;
        let t = |key: &str| m.t(key);
        let cast_tooltip =
            if cast_status.state == CastState::Connected { m.i18n.message(&cast_status.message) } else { t("cast_title") };
        let texts =
            (t("shuffle"), t("previous_tooltip"), t("play_tooltip"), t("next_tooltip"), t("repeat"), t("queue"), t("mute"));
        let song_cover = song_cover_path.as_deref().and_then(|p| self.model.update(cx, |m, _| m.artwork.get(p)));

        let shown_position =
            if self.slider.kind == Some(SliderKind::Seek) { (self.slider.value * duration as f32) as u64 } else { position };
        let now_playing_width = if compact { (width - 36. - 133.).max(120.) } else { (width * 0.32).min(330.) };

        let now_playing = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .w(px(now_playing_width))
            .flex_shrink_0()
            .child(cover(song_cover, &song_title, 46.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(theme::TEXT_BODY))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .truncate()
                            .child(song_title.clone()),
                    )
                    .child(match &song_raw_artist {
                        Some(raw) => self.artist_links(&song_artist, raw, ("player-artist", 0usize), 10.),
                        None => {
                            div().text_size(px(theme::TEXT_SMALL)).text_color(theme::MUTED).truncate().child(song_artist.clone())
                        }
                    }),
            )
            .when_some(favorite, |this, (path, on)| {
                this.child(
                    action_button("favorite", if on { "heart-filled" } else { "heart" }, font.clone())
                        .size(30.)
                        .selected(on)
                        .tooltip(favorite_text.clone())
                        .build(self.model_action(move |m, _| m.toggle_favorites(vec![path.clone()]))),
                )
            });

        let mut transport = div().flex().flex_row().items_center().gap(px(6.));
        if !compact {
            transport = transport.child(
                action_button("shuffle", "shuffle", font.clone())
                    .tooltip(texts.0.clone())
                    .selected(shuffle)
                    .build(self.model_action(|m, _| m.toggle_shuffle())),
            );
        }
        transport = transport
            .child(
                action_button("previous", "previous", font.clone())
                    .tooltip(texts.1.clone())
                    .build(self.model_action(|m, _| m.previous())),
            )
            .child(
                action_button("play", if playing { "pause" } else { "play" }, font.clone())
                    .tooltip(texts.2.clone())
                    .accent(true)
                    .size(40.)
                    .build(self.model_action(|m, _| m.toggle_play())),
            )
            .child(
                action_button("next", "next", font.clone()).tooltip(texts.3.clone()).build(self.model_action(|m, _| m.next())),
            );
        if !compact {
            transport = transport.child(
                action_button("repeat", "repeat", font.clone())
                    .tooltip(texts.4.clone())
                    .selected(repeat)
                    .build(self.model_action(|m, _| m.toggle_repeat())),
            );
        }

        let volume_slider = |_this: &Self, width_px: f32, cx: &mut Context<Self>| {
            div().w(px(width_px)).flex_shrink_0().child(slider(
                SliderKind::Volume,
                volume as f32 / 100.,
                true,
                cx.entity(),
                |view: &MainView| view.slider,
                |view: &mut MainView, drag, cx| {
                    view.slider = drag;
                    if let Some(SliderKind::Volume) = drag.kind {
                        view.model.update(cx, |m, _| m.set_volume((drag.value * 100.).round() as u32));
                    }
                    cx.notify();
                },
                |view: &mut MainView, value, cx| view.update_model(cx, |m, _| m.set_volume((value * 100.).round() as u32)),
            ))
        };

        let mut top_row = div().flex().flex_row().items_center().flex_1().min_h_0().relative().child(now_playing);
        if compact {
            top_row = top_row.child(div().flex_1()).child(transport);
        } else {
            let right = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(3.))
                .flex_shrink_0()
                .child(
                    action_button("cast", "cast", font.clone())
                        .tooltip(cast_tooltip.clone())
                        .selected(cast_selected)
                        .build(self.model_action(|m, _| m.toggle_cast_panel())),
                )
                .child(
                    action_button("queue-toggle", "queue", font.clone())
                        .tooltip(texts.5.clone())
                        .selected(queue_open)
                        .build(self.model_action(|m, _| m.toggle_queue())),
                )
                .child(
                    action_button("mute", if muted { "muted" } else { "volume" }, font.clone())
                        .tooltip(texts.6.clone())
                        .build(self.model_action(|m, _| m.toggle_mute())),
                )
                .child(volume_slider(
                    self,
                    if width > 1000. {
                        90.
                    } else if width > 720. {
                        65.
                    } else {
                        45.
                    },
                    cx,
                ));
            top_row = top_row.child(div().flex_1().flex().justify_center().child(transport)).child(right);
        }

        let timeline = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .flex_shrink_0()
            .child(div().w(px(31.)).text_size(px(theme::TEXT_TINY)).text_color(theme::MUTED).child(format_clock(shown_position)))
            .child(div().flex_1().child(slider(
                SliderKind::Seek,
                if duration > 0 { position as f32 / duration as f32 } else { 0. },
                duration > 0,
                cx.entity(),
                |view: &MainView| view.slider,
                |view: &mut MainView, drag, cx| {
                    view.slider = drag;
                    cx.notify();
                },
                |view: &mut MainView, value, cx| {
                    view.update_model(cx, |m, _| {
                        let duration = m.playback.duration_ms();
                        m.seek((value * duration as f32) as u64);
                    })
                },
            )))
            .child(
                div()
                    .w(px(31.))
                    .text_size(px(theme::TEXT_TINY))
                    .text_color(theme::MUTED)
                    .text_right()
                    .child(format_clock(duration)),
            );

        let mut bar = div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .when(compact, |this| this.flex_1().min_h_0())
            .h(px(if compact { 133. } else { 100. }))
            .mt(px(if compact { 0. } else { 8. }))
            .px(px(18.))
            .pt(px(9.))
            .pb(px(4.))
            .gap(px(1.))
            .bg(theme::player_bar())
            .child(top_row)
            .child(timeline);
        if compact {
            bar = bar.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .h(px(27.))
                    .gap(px(4.))
                    .flex_shrink_0()
                    .child(
                        action_button("shuffle-mini", "shuffle", font.clone())
                            .size(26.)
                            .tooltip(texts.0.clone())
                            .selected(shuffle)
                            .build(self.model_action(|m, _| m.toggle_shuffle())),
                    )
                    .child(
                        action_button("repeat-mini", "repeat", font.clone())
                            .size(26.)
                            .tooltip(texts.4.clone())
                            .selected(repeat)
                            .build(self.model_action(|m, _| m.toggle_repeat())),
                    )
                    .child(div().flex_1())
                    .child(
                        action_button("cast-mini", "cast", font.clone())
                            .size(26.)
                            .tooltip(cast_tooltip.clone())
                            .selected(cast_selected)
                            .build(self.model_action(|m, _| m.toggle_cast_panel())),
                    )
                    .child(
                        action_button("mute-mini", if muted { "muted" } else { "volume" }, font.clone())
                            .size(26.)
                            .tooltip(texts.6.clone())
                            .build(self.model_action(|m, _| m.toggle_mute())),
                    )
                    .child(volume_slider(self, 85., cx)),
            );
        }
        bar
    }

    /// A button that runs a menu action and applies the window request it returns.
    fn menu_action_button(view: WeakEntity<Self>, action: MenuAction) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
        move |_, window, cx| {
            let _ = view.update(cx, |this, cx| {
                let request = this.model.update(cx, |m, cx| {
                    let request = m.menu_action(action.clone(), cx);
                    cx.notify();
                    request
                });
                if let Some(request) = request {
                    this.handle_request(request, window, cx);
                }
            });
        }
    }

    fn account_button(&self, service: Service, button: &ServiceButton, index: usize, font: &SharedString) -> Stateful<Div> {
        let action = button.action.clone();
        action_button(SharedString::from(format!("account-{}-{index}", service.key())), button.icon, font.clone())
            .caption(button.caption.clone())
            .enabled(button.enabled)
            .accent(button.accent)
            .build(self.model_action(move |m, cx| m.account_action(service, action.clone(), cx)))
    }

    fn page_header(title: String) -> Div {
        div().text_size(px(22.)).font_weight(gpui::FontWeight::BOLD).flex_shrink_0().child(title)
    }

    /// Settings: streaming accounts and general preferences.
    fn render_settings(&self, font: &SharedString, window: &Window, cx: &mut Context<Self>) -> Stateful<Div> {
        let view = cx.entity().downgrade();
        let m = self.model.read(cx);
        let tab = m.settings_tab;
        let t = |key: &str| m.t(key);
        let mut page = div()
            .id("settings")
            .flex()
            .flex_col()
            .size_full()
            .overflow_y_scroll()
            .p(px(22.))
            .gap(px(14.))
            .child(Self::page_header(t("settings")))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(6.))
                    .flex_shrink_0()
                    .child(
                        action_button("settings-accounts", "music", font.clone())
                            .caption(t("settings_accounts"))
                            .selected(tab == SettingsTab::Accounts)
                            .build(self.model_action(|m, cx| m.open_settings(SettingsTab::Accounts, cx))),
                    )
                    .child(
                        action_button("settings-general", "settings", font.clone())
                            .caption(t("settings_general"))
                            .selected(tab == SettingsTab::General)
                            .build(self.model_action(|m, cx| m.open_settings(SettingsTab::General, cx))),
                    ),
            );
        match tab {
            SettingsTab::Accounts => {
                page = page.child(
                    div()
                        .text_size(px(theme::TEXT_BODY))
                        .text_color(theme::MUTED)
                        .flex_shrink_0()
                        .child(t("settings_accounts_intro")),
                );
                let views: Vec<_> = Service::ALL.iter().map(|s| m.account_view(*s)).collect();
                for view in views {
                    let service = view.service;
                    let mut card = div()
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .flex_shrink_0()
                        .p(px(14.))
                        .rounded(px(10.))
                        .bg(theme::SURFACE)
                        .border_1()
                        .border_color(theme::BORDER)
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.))
                                .child(icon(service.icon(), 18., theme::TEXT))
                                .child(div().text_size(px(15.)).font_weight(gpui::FontWeight::SEMIBOLD).child(service.name())),
                        );
                    for line in &view.lines {
                        card = card.child(div().text_size(px(theme::TEXT_BODY)).text_color(theme::MUTED).child(line.clone()));
                    }
                    if let Some((input, button, hint)) = view.setup.clone() {
                        card = card
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(6.))
                                    .child(div().flex_1().max_w(px(420.)).child(self.input_box(input, window, cx)))
                                    .child(self.account_button(service, &button, 9, font)),
                            )
                            .child(div().text_size(px(theme::TEXT_DETAIL)).text_color(theme::MUTED).child(hint));
                    }
                    if !view.buttons.is_empty() {
                        let mut row = div().flex().flex_row().flex_wrap().gap(px(6.));
                        for (index, button) in view.buttons.iter().enumerate() {
                            row = row.child(self.account_button(service, button, index, font));
                        }
                        card = card.child(row);
                    }
                    if !view.status.is_empty() {
                        card = card
                            .child(div().text_size(px(theme::TEXT_DETAIL)).text_color(theme::ACCENT).child(view.status.clone()));
                    }
                    page = page.child(card);
                }
            }
            SettingsTab::General => {
                let preference = m.i18n.preference.clone();
                let translucent = m.translucent;
                let section = |title: String| {
                    div().text_size(px(theme::TEXT_BODY)).font_weight(gpui::FontWeight::SEMIBOLD).flex_shrink_0().child(title)
                };
                let mut languages = div().flex().flex_row().flex_wrap().gap(px(6.)).flex_shrink_0().child(
                    action_button("language-auto", "", font.clone())
                        .caption(t("system_language"))
                        .selected(preference == "auto")
                        .build(Self::menu_action_button(view.clone(), MenuAction::Language("auto".into()))),
                );
                for (code, name) in crate::i18n::LANGUAGES {
                    languages = languages.child(
                        action_button(SharedString::from(format!("language-{code}")), "", font.clone())
                            .caption(name.to_string())
                            .selected(preference == *code)
                            .build(Self::menu_action_button(view.clone(), MenuAction::Language(code.to_string()))),
                    );
                }
                page = page.child(section(t("language"))).child(languages).child(section(t("settings_window"))).child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(6.))
                        .flex_shrink_0()
                        .child(
                            action_button("translucency", if translucent { "check" } else { "" }, font.clone())
                                .caption(t("translucency"))
                                .selected(translucent)
                                .build(Self::menu_action_button(view.clone(), MenuAction::ToggleTranslucent)),
                        )
                        .child(
                            action_button("reset-layout", "refresh", font.clone())
                                .caption(t("reset_window_layout"))
                                .build(Self::menu_action_button(view.clone(), MenuAction::ResetLayout)),
                        ),
                );
                page = page
                    .child(section(t("settings_about")))
                    .child(
                        div().text_size(px(theme::TEXT_DETAIL)).text_color(theme::MUTED).flex_shrink_0().child(m.app_version()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap(px(6.))
                            .flex_shrink_0()
                            .child(action_button("website", "", font.clone()).caption("fono8.com".to_string()).build(
                                Self::menu_action_button(view.clone(), MenuAction::OpenWebsite(crate::app::WEBSITE.into())),
                            ))
                            .child(action_button("release-notes", "", font.clone()).caption(t("release_notes")).build(
                                Self::menu_action_button(view.clone(), MenuAction::OpenWebsite(crate::app::release_notes_url())),
                            )),
                    );
            }
        }
        page
    }

    /// Discover: one search over every streaming service, mixed results, per-service tools.
    fn render_discover(&self, font: &SharedString, window: &Window, cx: &mut Context<Self>) -> Div {
        let m = self.model.read(cx);
        let filter = m.discover.filter;
        let t = |key: &str| m.t(key);
        let busy = m.discover_busy();
        let rows = m.discover_rows();
        let status = m.discover_status();
        let has_more = m.discover_has_more();
        let has_selection = m.discover.has_selection() && !busy;
        let target = m.target_name(m.discover.target);
        let inputs = m.discover_inputs.clone();
        // A service tab that needs an account first (Spotify, TIDAL).
        let account_missing = filter.is_some_and(|s| !m.service_ready(s));
        let spotify_rows = rows.iter().any(|r| r.service == Service::Spotify);
        let texts = (t("discover"), t("discover_all"), t("yt_search"), t("yt_import"), t("yt_my_playlists"), t("spotify_liked"));
        let signed_out_text = match filter {
            Some(Service::Tidal) => t("tidal_discover_signed_out"),
            _ => t("spotify_discover_signed_out"),
        };
        let playlists_text = if filter == Some(Service::Tidal) { t("tidal_collections") } else { texts.4.clone() };
        let show_search = filter.is_none_or(|s| s.searchable());
        let import_mode = (filter == Some(Service::Tidal) && !account_missing).then(|| m.tidal_import_mode());
        let mode_texts = (t("tidal_import_as"), t("tidal_mode_youtube"), t("tidal_mode_spotify"), t("tidal_mode_preview"));
        let (add_text, enqueue_text, more_text, attribution, empty_text, open_accounts) = (
            t("yt_add"),
            t("add_to_queue"),
            t("spotify_more"),
            t("spotify_attribution"),
            t("discover_empty"),
            t("open_account_settings"),
        );

        let mut tabs = div().flex().flex_row().flex_wrap().gap(px(6.)).flex_shrink_0().child(
            action_button("discover-all", "music", font.clone())
                .caption(texts.1.clone())
                .selected(filter.is_none())
                .build(self.model_action(|m, _| m.discover_filter(None))),
        );
        for service in Service::ALL {
            tabs = tabs.child(
                action_button(SharedString::from(format!("discover-{}", service.key())), service.icon(), font.clone())
                    .caption(service.name())
                    .selected(filter == Some(service))
                    .build(self.model_action(move |m, _| m.discover_filter(Some(service)))),
            );
        }
        let mut page =
            div().flex().flex_col().size_full().p(px(22.)).gap(px(10.)).child(Self::page_header(texts.0.clone())).child(tabs);
        if let Some([query, address]) = inputs {
            page = page.when(show_search, |page| {
                page.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .flex_shrink_0()
                        .child(div().flex_1().child(self.input_box(query, window, cx)))
                        .child(
                            action_button("discover-search", "search", font.clone())
                                .caption(texts.2.clone())
                                .enabled(!busy && !account_missing)
                                .build(self.model_action(|m, cx| m.discover_search(cx))),
                        ),
                )
            });
            if let Some(service) = filter.filter(|_| !account_missing) {
                let mut tools = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .flex_shrink_0()
                    .child(div().flex_1().child(self.input_box(address, window, cx)))
                    .child(
                        action_button("discover-import", "", font.clone())
                            .caption(texts.3.clone())
                            .enabled(!busy)
                            .build(self.model_action(move |m, cx| m.discover_import(service, cx))),
                    )
                    .child(
                        action_button("discover-playlists", "list", font.clone())
                            .caption(playlists_text.clone())
                            .enabled(!busy)
                            .build(self.model_action(move |m, _| m.discover_playlists(service))),
                    );
                if service.has_liked() {
                    tools = tools.child(
                        action_button("discover-liked", "plus", font.clone())
                            .caption(texts.5.clone())
                            .enabled(!busy)
                            .build(self.model_action(move |m, _| m.discover_liked(service))),
                    );
                }
                page = page.child(tools);
            }
        }
        if let Some(mode) = import_mode {
            let mut modes = div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap(px(6.))
                .flex_shrink_0()
                .child(div().text_size(px(theme::TEXT_BODY)).text_color(theme::MUTED).child(mode_texts.0.clone()));
            for (id, value, caption) in [
                ("import-youtube", ImportMode::YouTubeFirst, mode_texts.1.clone()),
                ("import-spotify", ImportMode::SpotifyFirst, mode_texts.2.clone()),
                ("import-preview", ImportMode::Preview, mode_texts.3.clone()),
            ] {
                modes = modes.child(
                    action_button(id, if mode == value { "check" } else { "" }, font.clone())
                        .caption(caption)
                        .selected(mode == value)
                        .build(self.model_action(move |m, _| m.set_tidal_import_mode(value))),
                );
            }
            page = page.child(modes);
        }
        if account_missing {
            page = page.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .flex_shrink_0()
                    .child(div().flex_1().text_size(px(theme::TEXT_BODY)).text_color(theme::MUTED).child(signed_out_text))
                    .child(
                        action_button("discover-accounts", "settings", font.clone())
                            .caption(open_accounts.clone())
                            .accent(true)
                            .build(self.model_action(|m, cx| m.open_settings(SettingsTab::Accounts, cx))),
                    ),
            );
        }

        let mut list = div()
            .id("discover-results")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .gap(px(2.))
            .rounded(px(8.))
            .bg(gpui::rgb(0x09182c))
            .p(px(4.));
        if rows.is_empty() {
            list = list.child(div().p(px(16.)).text_size(px(theme::TEXT_BODY)).text_color(theme::MUTED).child(empty_text));
        }
        for (position, row) in rows.into_iter().enumerate() {
            let image = row.cover.as_deref().and_then(|p| self.model.update(cx, |m, _| m.artwork.get(p)));
            let selected = row.selected;
            list = list.child(
                div()
                    .id(("discover-row", position))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .h(px(46.))
                    .px(px(8.))
                    .rounded(px(6.))
                    .flex_shrink_0()
                    .cursor_pointer()
                    .when(!row.enabled, |this| this.opacity(0.5))
                    .bg(if selected { theme::selected_row() } else { gpui::transparent_black().into() })
                    .when(!selected, |this| this.hover(|s| s.bg(theme::ELEVATED)))
                    .on_click({
                        let model = self.model.clone();
                        move |event: &ClickEvent, _, cx| {
                            let modifiers = event.modifiers();
                            model.update(cx, |m, cx| {
                                if event.click_count() >= 2 {
                                    m.discover_activate(position);
                                } else {
                                    m.discover_select(position, modifiers.control, modifiers.shift);
                                }
                                cx.notify();
                            });
                        }
                    })
                    .child(cover(image, &row.title, 34.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .gap(px(2.))
                            .child(div().text_size(px(theme::TEXT_BODY)).truncate().child(row.title.clone()))
                            .child(match &row.artist {
                                Some(raw) => self.artist_links_to(&row.subtitle, raw, ("discover-artist", position), 10., true),
                                None => div()
                                    .text_size(px(theme::TEXT_SMALL))
                                    .text_color(theme::MUTED)
                                    .truncate()
                                    .child(row.subtitle.clone()),
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(4.))
                            .flex_shrink_0()
                            .child(icon(row.service.icon(), 12., theme::MUTED))
                            .child(div().text_size(px(theme::TEXT_SMALL)).text_color(theme::MUTED).child(row.service.name())),
                    ),
            );
        }
        if has_more {
            list = list.child(
                div().flex().justify_center().py(px(4.)).flex_shrink_0().child(
                    action_button("discover-more", "chevron", font.clone())
                        .caption(more_text)
                        .enabled(!busy)
                        .build(self.model_action(|m, _| m.discover_more())),
                ),
            );
        }
        page = page.child(list);
        if spotify_rows {
            page = page.child(div().text_size(px(theme::TEXT_SMALL)).text_color(theme::MUTED).flex_shrink_0().child(attribution));
        }
        page = page.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .flex_shrink_0()
                .child(div().flex_1().min_w_0().max_w(px(320.)).child(
                    action_button("discover-target", "chevron", font.clone()).caption(target).fill_width(true).build(
                        self.menu_at_button(move |m, pos| {
                            let current = m.discover.target;
                            let mut items = vec![MenuEntry::Item {
                                label: m.t("all_tracks"),
                                action: MenuAction::DiscoverTarget(None),
                                enabled: true,
                                checked: Some(current.is_none()),
                            }];
                            for playlist in &m.playlists {
                                items.push(MenuEntry::Item {
                                    label: playlist.name.clone(),
                                    action: MenuAction::DiscoverTarget(Some(playlist.id)),
                                    enabled: true,
                                    checked: Some(current == Some(playlist.id)),
                                });
                            }
                            m.open_menu(pos, items);
                        }),
                    ),
                ))
                .child(div().flex_1())
                .child(
                    action_button("discover-add", "plus", font.clone())
                        .caption(add_text)
                        .enabled(has_selection)
                        .build(self.model_action(|m, _| m.discover_add(false))),
                )
                .child(
                    action_button("discover-enqueue", "queue", font.clone())
                        .caption(enqueue_text)
                        .enabled(has_selection)
                        .build(self.model_action(|m, _| m.discover_add(true))),
                ),
        );
        if busy {
            // Indeterminate progress: a sliding accent segment while a request runs.
            page =
                page.child(div().h(px(3.)).w_full().rounded(px(1.5)).bg(theme::slider_track()).flex_shrink_0().relative().child(
                    div().absolute().top_0().h(px(3.)).w(px(90.)).rounded(px(1.5)).bg(theme::ACCENT).with_animation(
                        "discover-progress",
                        gpui::Animation::new(Duration::from_millis(1100)).repeat().with_easing(gpui::ease_in_out),
                        move |bar, delta| bar.left(gpui::relative((delta * 0.86).clamp(0., 0.86))),
                    ),
                ));
        }
        for (service, text) in status {
            if !text.is_empty() {
                page = page.child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(6.))
                        .flex_shrink_0()
                        .text_size(px(theme::TEXT_DETAIL))
                        .text_color(if busy { theme::ACCENT } else { theme::MUTED })
                        .child(icon(service.icon(), 12., theme::MUTED))
                        .child(div().flex_1().min_w_0().line_clamp(2).child(text)),
                );
            }
        }
        page
    }

    fn render_cast_panel(
        &self,
        width: f32,
        height: f32,
        compact: bool,
        font: &SharedString,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let m = self.model.read(cx);
        let status = m.cast_status();
        let message = m.i18n.message(&status.message);
        let connected = status.state == CastState::Connected;
        let remote_soon = m.remote_beside_cast().filter(|_| connected).map(|remote| m.t(crate::app::cast_soon_key(remote)));
        let t = |key: &str| m.t(key);
        let texts = (
            t("cast_title"),
            t("cast_refresh"),
            t("cast_close"),
            t("cast_device_volume"),
            if status.muted { t("cast_device_unmute") } else { t("cast_device_mute") },
            t("cast_computer"),
            t("cast_hint"),
            if status.scanning { t("cast_searching") } else { t("cast_no_devices") },
        );
        let panel_width = (370.0f32).min(width - 16.);
        let panel_height = (400.0f32).min(height - 16.);
        let shown_volume = if self.slider.kind == Some(SliderKind::CastVolume) {
            (self.slider.value * 100.).round() as u32
        } else {
            status.volume
        };

        let mut header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.))
            .child(div().flex_1().font_weight(gpui::FontWeight::SEMIBOLD).child(texts.0.clone()))
            .child(
                action_button("cast-refresh", "refresh", font.clone())
                    .tooltip(texts.1.clone())
                    .enabled(!status.scanning)
                    .build(self.model_action(|m, _| m.cast_refresh())),
            )
            .child(
                action_button("cast-close", "close", font.clone())
                    .tooltip(texts.2.clone())
                    .build(self.model_action(|m, _| m.close_cast_panel())),
            );
        header = header.flex_shrink_0();

        let mut body = div().flex().flex_col().gap(px(8.)).size_full().child(header).child(
            div()
                .text_size(px(theme::TEXT_DETAIL))
                .text_color(if connected { theme::ACCENT } else { theme::MUTED })
                .line_clamp(if compact { 2 } else { 4 })
                .child(message),
        );
        if let Some(text) = remote_soon {
            body = body.child(
                div().flex_shrink_0().text_size(px(theme::TEXT_DETAIL)).text_color(theme::PURPLE).line_clamp(3).child(text),
            );
        }
        if connected {
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .flex_shrink_0()
                    .child(div().text_size(px(theme::TEXT_SMALL)).text_color(theme::MUTED).child(texts.3.clone()))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.))
                            .child(
                                action_button("cast-mute", if status.muted { "muted" } else { "volume" }, font.clone())
                                    .tooltip(texts.4.clone())
                                    .enabled(status.volume_known)
                                    .selected(status.muted)
                                    .build(self.model_action(|m, _| m.cast_toggle_mute())),
                            )
                            .child(div().flex_1().child(slider(
                                SliderKind::CastVolume,
                                status.volume as f32 / 100.,
                                status.volume_known,
                                cx.entity(),
                                |view: &MainView| view.slider,
                                |view: &mut MainView, drag, cx| {
                                    view.slider = drag;
                                    cx.notify();
                                },
                                |view: &mut MainView, value, cx| {
                                    view.update_model(cx, |m, _| m.cast_volume((value * 100.).round() as u32))
                                },
                            )))
                            .child(
                                div().w(px(36.)).text_right().text_size(px(theme::TEXT_DETAIL)).child(if status.volume_known {
                                    format!("{shown_volume}%")
                                } else {
                                    "…".to_string()
                                }),
                            ),
                    ),
            );
        }
        if !compact || !connected {
            let mut list = div().id("cast-devices").flex().flex_col().flex_1().min_h_0().overflow_y_scroll().gap(px(4.));
            if status.devices.is_empty() {
                list = list.child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .text_size(px(theme::TEXT_DETAIL))
                        .text_color(theme::MUTED)
                        .child(texts.7.clone()),
                );
            }
            for (index, device) in status.devices.iter().enumerate() {
                let id = device.id.clone();
                list = list.child(
                    div()
                        .id(("cast-device", index as u64))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(12.))
                        .h(px(49.))
                        .px(px(8.))
                        .rounded(px(7.))
                        .flex_shrink_0()
                        .cursor_pointer()
                        .hover(|s| s.bg(theme::ELEVATED))
                        .on_click(self.model_action(move |m, _| m.cast_to(Some(id.clone()))))
                        .child(icon("cast", 19., theme::ACCENT))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .gap(px(3.))
                                .child(div().truncate().child(device.name.clone()))
                                .child(
                                    div()
                                        .text_size(px(theme::TEXT_SMALL))
                                        .text_color(theme::MUTED)
                                        .truncate()
                                        .child(device.model.clone()),
                                ),
                        ),
                );
            }
            body = body.child(list);
        }
        body = body.child(
            action_button("cast-local", "volume", font.clone())
                .caption(texts.5.clone())
                .selected(status.state == CastState::Local)
                .enabled(status.state != CastState::Local)
                .fill_width(true)
                .build(self.model_action(|m, _| m.cast_to(None))),
        );
        if !compact {
            body = body
                .child(div().text_size(px(theme::TEXT_SMALL)).text_color(theme::MUTED).flex_shrink_0().child(texts.6.clone()));
        }

        deferred(
            div()
                .id("cast-backdrop")
                .absolute()
                .inset_0()
                .bg(theme::overlay())
                .flex()
                .items_end()
                .justify_end()
                .p(px(8.))
                .on_any_mouse_down({
                    let model = self.model.clone();
                    move |_, _, cx| {
                        model.update(cx, |m, cx| {
                            m.close_cast_panel();
                            cx.notify();
                        });
                        cx.stop_propagation();
                    }
                })
                .child(
                    div()
                        .id("cast-panel")
                        .occlude()
                        .w(px(panel_width))
                        .h(px(panel_height))
                        .p(px(12.))
                        .rounded(px(12.))
                        .bg(theme::SURFACE)
                        .border_1()
                        .border_color(theme::BORDER)
                        .font_family(font.clone())
                        .text_size(px(theme::TEXT_BODY))
                        .text_color(theme::TEXT)
                        .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
                        .child(body),
                ),
        )
        .with_priority(15)
    }

    fn render_status_bar(&self, font: &SharedString, cx: &mut Context<Self>) -> Div {
        let m = self.model.read(cx);
        let status = m.status_text();
        let resize_text = m.t("window_resize");
        div()
            .flex()
            .flex_row()
            .items_center()
            .flex_shrink_0()
            .h(px(22.))
            .pl(px(14.))
            .pr(px(5.))
            .child(div().flex_1().min_w_0().text_size(px(theme::TEXT_TINY)).text_color(theme::MUTED).truncate().child(status))
            .when(cfg!(target_os = "linux"), |this| {
                this.child(
                    div()
                        .id("resize-grip")
                        .size(px(16.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor(CursorStyle::ResizeUpLeftDownRight)
                        .tooltip(Tooltip::build(resize_text, font.clone()))
                        .on_mouse_down(MouseButton::Left, |_, window, cx| {
                            window.start_window_resize(ResizeEdge::BottomRight);
                            cx.stop_propagation();
                        })
                        .child(icon("resize", 10., theme::MUTED)),
                )
            })
    }

    fn render_menu(&self, font: &SharedString, cx: &mut Context<Self>) -> impl IntoElement {
        let menu: ContextMenu = self.model.read(cx).menu.clone().unwrap();
        let mut list = div()
            .occlude()
            .flex()
            .flex_col()
            .min_w(px(190.))
            .max_w(px(320.))
            .p(px(5.))
            .rounded(px(8.))
            .bg(theme::menu_bg())
            .border_1()
            .border_color(theme::menu_border())
            .font_family(font.clone())
            .text_size(px(theme::TEXT_BODY))
            .text_color(theme::TEXT)
            .on_any_mouse_down(|_, _, cx| cx.stop_propagation());
        for (index, entry) in menu.items.iter().enumerate() {
            match entry {
                MenuEntry::Separator => {
                    list = list.child(div().h(px(1.)).my(px(4.)).mx(px(4.)).bg(theme::menu_border()));
                }
                MenuEntry::Item { label, action, enabled, checked } => {
                    let action = action.clone();
                    let mut item = div()
                        .id(("menu-item", index as u64))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .px(px(10.))
                        .py(px(7.))
                        .rounded(px(4.))
                        .opacity(if *enabled { 1.0 } else { 0.4 })
                        .when(*enabled, |this| this.cursor_pointer().hover(|s| s.bg(theme::menu_hover())));
                    item = item.child(
                        div()
                            .w(px(14.))
                            .flex_shrink_0()
                            .when(*checked == Some(true), |this| this.child(icon("check", 14., theme::ACCENT))),
                    );
                    item = item.child(div().flex_1().whitespace_nowrap().child(label.clone()));
                    if *enabled {
                        item = item.on_click(cx.listener(move |this, _, window, cx| {
                            let request = this.model.update(cx, |m, cx| {
                                let request = m.menu_action(action.clone(), cx);
                                cx.notify();
                                request
                            });
                            if let Some(request) = request {
                                this.handle_request(request, window, cx);
                            }
                        }));
                    }
                    list = list.child(item);
                }
                MenuEntry::Submenu { label, enabled, .. } => {
                    let mut item = div()
                        .id(("menu-sub", index as u64))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .px(px(10.))
                        .py(px(7.))
                        .rounded(px(4.))
                        .opacity(if *enabled { 1.0 } else { 0.4 })
                        .when(*enabled, |this| this.cursor_pointer().hover(|s| s.bg(theme::menu_hover())))
                        .child(div().w(px(14.)).flex_shrink_0())
                        .child(div().flex_1().whitespace_nowrap().child(label.clone()))
                        .child(icon("chevron", 14., theme::MUTED));
                    if *enabled {
                        item = item.on_click(self.model_action(move |m, _| m.open_submenu(index)));
                    }
                    list = list.child(item);
                }
            }
        }
        deferred(
            div()
                .id("menu-backdrop")
                .absolute()
                .inset_0()
                .on_any_mouse_down(self.close_menu_listener())
                .child(anchored().position(menu.position).snap_to_window_with_margin(px(8.)).child(list)),
        )
        .with_priority(10)
    }

    fn close_menu_listener(&self) -> impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static {
        let model = self.model.clone();
        move |_, _, cx| {
            model.update(cx, |m, cx| {
                m.close_menu();
                cx.notify();
            });
            cx.stop_propagation();
        }
    }

    fn render_dialog(
        &self,
        width: f32,
        compact: bool,
        font: &SharedString,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let box_width = (380.0f32).min(width - 16.);
        let box_height = f32::from(window.viewport_size().height) - 16.;
        let content = self.dialog_content(compact, font, window, cx);
        deferred(
            div()
                .id("dialog-backdrop")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme::overlay())
                .on_any_mouse_down({
                    let model = self.model.clone();
                    move |_, _, cx| {
                        let closable =
                            matches!(model.read(cx).dialog, Some(Dialog::SleepTimer { .. }) | Some(Dialog::Message { .. }));
                        if closable {
                            model.update(cx, |m, cx| {
                                m.close_dialog();
                                cx.notify();
                            });
                        }
                        cx.stop_propagation();
                    }
                })
                .child(
                    div()
                        .id("dialog-box")
                        .occlude()
                        .w(px(box_width))
                        .max_h(px(box_height))
                        .overflow_y_scroll()
                        .p(px(12.))
                        .rounded(px(12.))
                        .bg(theme::SURFACE)
                        .border_1()
                        .border_color(theme::BORDER)
                        .font_family(font.clone())
                        .text_size(px(theme::TEXT_BODY))
                        .text_color(theme::TEXT)
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
                        .child(content),
                ),
        )
        .with_priority(20)
    }

    fn dialog_content(&self, compact: bool, font: &SharedString, window: &Window, cx: &mut Context<Self>) -> Div {
        let m = self.model.read(cx);
        let Some(dialog) = &m.dialog else { return div() };
        let t = |key: &str| m.t(key);
        match dialog {
            Dialog::Input { title, label, note, input, .. } => {
                let (title, label, note) = (t(title), t(label), note.map(t));
                let input = input.clone();
                let can_submit = !input.read(cx).text().trim().is_empty();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(div().font_weight(gpui::FontWeight::SEMIBOLD).text_size(px(14.)).child(title))
                    .child(div().text_color(theme::MUTED).child(label))
                    .when_some(note, |this, note| {
                        this.child(div().text_size(px(theme::TEXT_DETAIL)).text_color(theme::MUTED).child(note))
                    })
                    .child(self.input_box(input.clone(), window, cx))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap(px(6.))
                            .child(
                                action_button("dialog-cancel", "", font.clone())
                                    .caption(t_cancel(cx, &self.model))
                                    .build(self.model_action(|m, _| m.close_dialog())),
                            )
                            .child(
                                action_button("dialog-ok", "", font.clone())
                                    .caption("OK")
                                    .accent(true)
                                    .enabled(can_submit)
                                    .build(self.model_action(|m, cx| m.dialog_submit(cx))),
                            ),
                    )
            }
            Dialog::Confirm { title, body, .. } => {
                let (title, body, no, yes) = (t(title), t(body), t("key_cancel"), "OK".to_string());
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(div().font_weight(gpui::FontWeight::SEMIBOLD).text_size(px(14.)).child(title))
                    .child(div().text_color(theme::MUTED).child(body))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap(px(6.))
                            .child(
                                action_button("confirm-no", "", font.clone())
                                    .caption(no)
                                    .build(self.model_action(|m, _| m.close_dialog())),
                            )
                            .child(
                                action_button("confirm-yes", "", font.clone())
                                    .caption(yes)
                                    .accent(true)
                                    .build(self.model_action(|m, cx| m.dialog_submit(cx))),
                            ),
                    )
            }
            Dialog::Message { title, body } => {
                let (title, body) = (title.clone(), body.clone());
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(div().font_weight(gpui::FontWeight::SEMIBOLD).text_size(px(14.)).child(title))
                    .child(div().text_color(theme::MUTED).child(body))
                    .child(
                        div().flex().flex_row().justify_end().child(
                            action_button("message-ok", "", font.clone())
                                .caption("OK")
                                .accent(true)
                                .build(self.model_action(|m, _| m.close_dialog())),
                        ),
                    )
            }
            Dialog::SleepTimer { hours, minutes, .. } => {
                let active = m.sleep_timer.active();
                let countdown = m.sleep_timer.countdown();
                let total = m.sleep_timer_minutes(cx);
                let (hours, minutes) = (hours.clone(), minutes.clone());
                let texts = (
                    t("sleep_timer_title"),
                    t("sleep_timer_close"),
                    t("sleep_timer_hint"),
                    t("sleep_timer_minutes_short"),
                    t("sleep_timer_hours_short"),
                    t("sleep_timer_cancel"),
                    if active { t("sleep_timer_replace") } else { t("sleep_timer_start") },
                );
                let mut presets = div().flex().flex_row().gap(px(4.)).w_full();
                for preset in [15u64, 30, 60, 90] {
                    presets = presets.child(
                        div().flex_1().child(
                            action_button(("sleep-preset", preset), "", font.clone())
                                .caption(format!("{preset} {}", texts.3))
                                .selected(total == preset)
                                .fill_width(true)
                                .build(self.model_action(move |m, cx| m.sleep_timer_preset(preset, cx))),
                        ),
                    );
                }
                let mut header = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(div().flex_1().font_weight(gpui::FontWeight::SEMIBOLD).child(texts.0.clone()));
                if active {
                    header = header.child(div().text_color(theme::ACCENT).child(countdown));
                }
                header = header.child(
                    action_button("sleep-close", "close", font.clone())
                        .tooltip(texts.1.clone())
                        .build(self.model_action(|m, _| m.close_dialog())),
                );
                let mut footer = div().flex().flex_row().items_center().gap(px(6.));
                if active {
                    footer = footer.child(
                        action_button("sleep-cancel", "close", font.clone())
                            .caption(texts.5.clone())
                            .build(self.model_action(|m, _| m.sleep_timer_cancel())),
                    );
                }
                footer = footer.child(div().flex_1()).child(
                    action_button("sleep-start", "clock", font.clone())
                        .caption(texts.6.clone())
                        .accent(true)
                        .enabled(total > 0)
                        .build(self.model_action(|m, cx| m.sleep_timer_start(cx))),
                );
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(header)
                    .when(!compact, |this| {
                        this.child(div().text_size(px(theme::TEXT_DETAIL)).text_color(theme::MUTED).child(texts.2.clone()))
                    })
                    .child(presets)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.))
                            .child(div().flex_1().child(self.input_box(hours, window, cx)))
                            .child(div().child(texts.4.clone()))
                            .child(div().flex_1().child(self.input_box(minutes, window, cx)))
                            .child(div().child(texts.3.clone())),
                    )
                    .child(footer)
            }
            Dialog::Metadata { proposals, index, artist, album, error, .. } => {
                let proposal = &proposals[*index];
                let folder_name = std::path::Path::new(&proposal.folder)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let summary = m.text(
                    "metadata_summary",
                    &[
                        ("current", (*index + 1).into()),
                        ("total", proposals.len().into()),
                        ("tracks", Message::count("tracks_count", proposal.track_count()).into()),
                    ],
                );
                let artist_label = m.text(
                    "metadata_field_count",
                    &[("field", t("metadata_artist").into()), ("n", proposal.targets_for("artist").len().into())],
                );
                let album_label = m.text(
                    "metadata_field_count",
                    &[("field", t("metadata_album").into()), ("n", proposal.targets_for("album").len().into())],
                );
                let texts = (
                    t("metadata_title"),
                    t("metadata_notice"),
                    t("metadata_close"),
                    t("metadata_skip"),
                    t("metadata_apply"),
                    t("metadata_save_error"),
                );
                let valid = m.metadata_valid(cx);
                let error = *error;
                let (artist, album) = (artist.clone(), album.clone());
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(div().font_weight(gpui::FontWeight::SEMIBOLD).text_size(px(14.)).child(texts.0.clone()))
                    .child(div().font_weight(gpui::FontWeight::SEMIBOLD).text_size(px(15.)).truncate().child(folder_name))
                    .child(div().child(summary))
                    .when(!compact, |this| {
                        this.child(div().text_size(px(theme::TEXT_DETAIL)).text_color(theme::MUTED).child(texts.1.clone()))
                    })
                    .child(div().text_size(px(theme::TEXT_DETAIL)).text_color(theme::MUTED).child(artist_label))
                    .child(self.input_box(artist, window, cx))
                    .child(div().text_size(px(theme::TEXT_DETAIL)).text_color(theme::MUTED).child(album_label))
                    .child(self.input_box(album, window, cx))
                    .when(error, |this| {
                        this.child(div().text_size(px(theme::TEXT_DETAIL)).text_color(theme::PURPLE).child(texts.5.clone()))
                    })
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.))
                            .child(
                                action_button("meta-close", "", font.clone())
                                    .caption(texts.2.clone())
                                    .build(self.model_action(|m, _| m.close_dialog())),
                            )
                            .child(div().flex_1())
                            .child(
                                action_button("meta-skip", "", font.clone())
                                    .caption(texts.3.clone())
                                    .build(self.model_action(|m, cx| m.metadata_advance(cx))),
                            )
                            .child(
                                action_button("meta-apply", "", font.clone())
                                    .caption(texts.4.clone())
                                    .accent(true)
                                    .enabled(valid)
                                    .build(self.model_action(|m, cx| m.metadata_apply(cx))),
                            ),
                    )
            }
        }
    }

    fn input_box(&self, input: Entity<TextInput>, window: &Window, cx: &mut Context<Self>) -> Div {
        let focused = input.read(cx).is_focused(window);
        let enabled = input.read(cx).enabled;
        div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(34.))
            .px(px(10.))
            .rounded(px(7.))
            .bg(gpui::rgb(0x09182c))
            .border_1()
            .border_color(if focused { theme::ACCENT } else { theme::BORDER })
            .opacity(if enabled { 1.0 } else { 0.6 })
            .on_mouse_down(MouseButton::Left, {
                let input = input.clone();
                move |_, window, cx| {
                    input.update(cx, |input, _| {
                        if input.enabled {
                            input.focus(window);
                        }
                    });
                    cx.stop_propagation();
                }
            })
            .child(input)
    }
}

fn t_cancel(cx: &App, model: &Entity<Fono8>) -> String {
    model.read(cx).t("key_cancel")
}

fn drop_indicator(y: f32, inset: f32) -> Div {
    div()
        .absolute()
        .left(px(inset))
        .right(px(inset))
        .top(px(y.max(1.)))
        .h(px(3.))
        .rounded(px(1.5))
        .bg(theme::ACCENT)
        .child(div().absolute().left(px(-2.)).top(px(-2.)).size(px(7.)).rounded_full().bg(theme::ACCENT))
}

/// Default size for a brand-new window when nothing was saved yet.
#[allow(dead_code)]
pub fn default_size(compact: bool) -> Size<Pixels> {
    if compact {
        size(px(COMPACT_SIZE.0), px(COMPACT_SIZE.1))
    } else {
        size(px(FULL_SIZE.0), px(FULL_SIZE.1))
    }
}
