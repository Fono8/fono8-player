//! Small reusable elements: icon buttons, cover placeholders, sliders, tooltips.

use std::sync::Arc;

use gpui::{
    div, fill, img, linear_color_stop, linear_gradient, point, prelude::*, px, quad, size, svg, AnyElement, AnyView, App,
    BorderStyle, Bounds, ClickEvent, Context, CursorStyle, DispatchPhase, Div, ElementId, Entity, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ObjectFit, Pixels, Point, Render, RenderImage, SharedString, Stateful, Window,
};

use super::theme;

/// A tooltip view, built lazily when the pointer rests on an element.
pub struct Tooltip {
    text: SharedString,
    font: SharedString,
}

impl Tooltip {
    pub fn build(text: impl Into<SharedString>, font: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView {
        let text = text.into();
        let font = font.into();
        move |_, cx| cx.new(|_| Tooltip { text: text.clone(), font: font.clone() }).into()
    }
}

impl Render for Tooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .font_family(self.font.clone())
            .text_size(px(11.))
            .text_color(theme::TEXT)
            .bg(theme::tooltip_bg())
            .border_1()
            .border_color(theme::tooltip_border())
            .rounded(px(5.))
            .px(px(8.))
            .py(px(5.))
            .max_w(px(320.))
            .child(self.text.clone())
    }
}

pub fn icon(name: &str, size_px: f32, color: impl Into<gpui::Hsla>) -> gpui::Svg {
    svg().path(format!("icons/{name}.svg")).size(px(size_px)).flex_shrink_0().text_color(color)
}

/// Builder for the tool buttons used all over the interface.
pub struct ActionButton {
    id: ElementId,
    glyph: Option<String>,
    caption: Option<String>,
    tooltip: Option<String>,
    accent: bool,
    selected: bool,
    enabled: bool,
    size: f32,
    fill_width: bool,
    font: SharedString,
}

pub fn action_button(id: impl Into<ElementId>, glyph: &str, font: impl Into<SharedString>) -> ActionButton {
    ActionButton {
        id: id.into(),
        glyph: if glyph.is_empty() { None } else { Some(glyph.to_string()) },
        caption: None,
        tooltip: None,
        accent: false,
        selected: false,
        enabled: true,
        size: 34.0,
        fill_width: false,
        font: font.into(),
    }
}

impl ActionButton {
    pub fn caption(mut self, caption: impl Into<String>) -> Self {
        let caption = caption.into();
        self.caption = if caption.is_empty() { None } else { Some(caption) };
        self
    }

    pub fn tooltip(mut self, tooltip: impl Into<String>) -> Self {
        let tooltip = tooltip.into();
        self.tooltip = if tooltip.is_empty() { None } else { Some(tooltip) };
        self
    }

    pub fn accent(mut self, accent: bool) -> Self {
        self.accent = accent;
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn size(mut self, size: f32) -> Self {
        self.size = size;
        self
    }

    pub fn fill_width(mut self, fill: bool) -> Self {
        self.fill_width = fill;
        self
    }

    pub fn build(self, on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Stateful<Div> {
        let accent = self.accent;
        let enabled = self.enabled;
        let selected = self.selected;
        let icon_color: gpui::Hsla = if accent {
            theme::BACKGROUND.into()
        } else if selected {
            theme::ACCENT.into()
        } else {
            theme::TEXT.into()
        };
        let text_color: gpui::Hsla = if accent { theme::BACKGROUND.into() } else { theme::TEXT.into() };
        let has_caption = self.caption.is_some();
        let has_glyph = self.glyph.is_some();
        let mut button = div()
            .id(self.id)
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .flex_shrink_0()
            .gap(px(if has_caption && has_glyph { 8. } else { 0. }))
            .h(px(self.size))
            .rounded(px(if accent { self.size / 2. } else { 8. }))
            .font_family(self.font.clone())
            .text_size(px(12.))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(text_color)
            .opacity(if enabled { 1.0 } else { 0.4 })
            .when(has_caption, |this| this.px(px(12.)))
            .when(!has_caption, |this| this.w(px(self.size)))
            .when(self.fill_width, |this| this.w_full())
            .when(accent, |this| this.bg(theme::ACCENT))
            .when(!accent && selected, |this| this.bg(theme::selected_button()))
            .when(enabled, |this| {
                this.cursor_pointer()
                    .hover(move |style| {
                        if accent {
                            style.bg(gpui::rgb(0x8cf4ff))
                        } else if selected {
                            style
                        } else {
                            style.bg(theme::ELEVATED)
                        }
                    })
                    .active(
                        move |style| if accent { style.bg(theme::accent_pressed()) } else { style.bg(theme::selected_button()) },
                    )
            })
            // Clicks on buttons must not start a window move from the title bar underneath.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation());
        if let Some(glyph) = &self.glyph {
            button = button.child(icon(glyph, 19., icon_color));
        }
        if let Some(caption) = self.caption.clone() {
            button = button.child(div().whitespace_nowrap().child(caption));
        }
        if let Some(tooltip) = self.tooltip.clone() {
            button = button.tooltip(Tooltip::build(tooltip, self.font.clone()));
        }
        if enabled {
            button = button.on_click(on_click);
        }
        button
    }
}

fn title_variant(title: &str) -> usize {
    let mut hash: i32 = 0;
    for ch in title.encode_utf16() {
        hash = hash.wrapping_shl(5).wrapping_sub(hash).wrapping_add(ch as i32);
    }
    (hash.unsigned_abs() % 4) as usize
}

/// Album art with the Fono8 vinyl placeholder underneath.
pub fn cover(image: Option<Arc<RenderImage>>, title: &str, size_px: f32) -> Div {
    let variant = title_variant(title);
    let radius = (size_px / 7.).min(10.);
    let base = div().relative().flex_shrink_0().size(px(size_px)).rounded(px(radius)).overflow_hidden().bg(linear_gradient(
        180.,
        linear_color_stop(gpui::rgb(theme::COVER_VARIANTS[variant]), 0.),
        linear_color_stop(gpui::rgb(0x111b30), 1.),
    ));
    match image {
        Some(image) => base.child(img(image).size_full().object_fit(ObjectFit::Cover).rounded(px(radius))),
        None => {
            let disc = size_px * 0.68;
            let inner = disc * 0.66;
            let label = inner * 0.39;
            let hole = label * 0.25;
            base.child(
                div().absolute().inset_0().flex().items_center().justify_center().child(
                    div()
                        .size(px(disc))
                        .rounded_full()
                        .bg(gpui::rgb(0x152030))
                        .border_1()
                        .border_color(gpui::rgb(0x497086))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .size(px(inner))
                                .rounded_full()
                                .border_1()
                                .border_color(gpui::rgb(0x365367))
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(
                                    div()
                                        .size(px(label))
                                        .rounded_full()
                                        .bg(if variant == 1 { theme::PURPLE } else { theme::ACCENT })
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .child(div().size(px(hole.max(1.))).rounded_full().bg(theme::BACKGROUND)),
                                ),
                        ),
                ),
            )
        }
    }
}

/// Which slider a drag belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SliderKind {
    Seek,
    Volume,
    CastVolume,
}

/// State a view keeps for sliders: the active drag and its provisional value.
#[derive(Default, Clone, Copy, Debug)]
pub struct SliderDrag {
    pub kind: Option<SliderKind>,
    pub value: f32,
}

/// A horizontal slider painted with quads; mouse handling is registered per frame.
///
/// `read` returns the value (0..1) and the current drag state from the view,
/// `update` is called with the provisional value during drags and `commit` on release.
pub fn slider<V: 'static>(
    kind: SliderKind,
    value: f32,
    enabled: bool,
    view: Entity<V>,
    drag_state: impl Fn(&V) -> SliderDrag + 'static,
    set_drag: impl Fn(&mut V, SliderDrag, &mut Context<V>) + 'static,
    commit: impl Fn(&mut V, f32, &mut Context<V>) + 'static,
) -> AnyElement {
    let set_drag = Arc::new(set_drag);
    let commit = Arc::new(commit);
    let drag_state = Arc::new(drag_state);
    gpui::canvas(
        move |bounds, window, _cx| window.insert_hitbox(bounds, gpui::HitboxBehavior::Normal),
        move |bounds, hitbox, window, cx| {
            let drag = drag_state(view.read(cx));
            let dragging = drag.kind == Some(kind);
            let shown = if dragging { drag.value } else { value }.clamp(0., 1.);
            let track_h = px(3.);
            let y = bounds.top() + (bounds.size.height - track_h) / 2.;
            let track = Bounds::new(point(bounds.left(), y), size(bounds.size.width, track_h));
            window.paint_quad(quad(track, px(2.), theme::slider_track(), px(0.), gpui::transparent_black(), BorderStyle::Solid));
            let filled = Bounds::new(point(bounds.left(), y), size(bounds.size.width * shown, track_h));
            window.paint_quad(quad(filled, px(2.), theme::ACCENT, px(0.), gpui::transparent_black(), BorderStyle::Solid));
            let hovered = bounds.contains(&window.mouse_position());
            if enabled && (hovered || dragging) {
                let handle = px(10.);
                let x = bounds.left() + (bounds.size.width - handle) * shown;
                let handle_bounds =
                    Bounds::new(point(x, bounds.top() + (bounds.size.height - handle) / 2.), size(handle, handle));
                let color = if dragging { theme::ACCENT } else { theme::TEXT };
                window.paint_quad(fill(handle_bounds, color).corner_radii(px(5.)));
            }
            if !enabled {
                return;
            }
            let ratio = move |position: Point<Pixels>| -> f32 {
                let width = f32::from(bounds.size.width).max(1.);
                ((f32::from(position.x) - f32::from(bounds.left())) / width).clamp(0., 1.)
            };
            // Press: start a drag at the pointer position.
            let view_down = view.clone();
            let set_drag_down = set_drag.clone();
            let hit =
                Bounds::new(point(bounds.left(), bounds.top() - px(4.)), size(bounds.size.width, bounds.size.height + px(8.)));
            window.on_mouse_event(move |event: &MouseDownEvent, phase, _window, cx| {
                if phase != DispatchPhase::Bubble || event.button != MouseButton::Left || !hit.contains(&event.position) {
                    return;
                }
                let value = ratio(event.position);
                view_down.update(cx, |view, cx| set_drag_down(view, SliderDrag { kind: Some(kind), value }, cx));
                cx.stop_propagation();
            });
            if dragging {
                let view_move = view.clone();
                let set_drag_move = set_drag.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, _window, cx| {
                    if phase != DispatchPhase::Bubble {
                        return;
                    }
                    let value = ratio(event.position);
                    view_move.update(cx, |view, cx| set_drag_move(view, SliderDrag { kind: Some(kind), value }, cx));
                });
                let view_up = view.clone();
                let set_drag_up = set_drag.clone();
                let commit_up = commit.clone();
                window.on_mouse_event(move |event: &MouseUpEvent, phase, _window, cx| {
                    if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                        return;
                    }
                    let value = ratio(event.position);
                    view_up.update(cx, |view, cx| {
                        set_drag_up(view, SliderDrag::default(), cx);
                        commit_up(view, value, cx);
                    });
                });
            }
            if hovered || dragging {
                window.set_cursor_style(CursorStyle::PointingHand, &hitbox);
            }
        },
    )
    .h(px(22.))
    .w_full()
    .into_any_element()
}

/// How the five logo bars are drawn.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BrandPose {
    Still,
    /// Bar levels (0..1): the music's bands, or the generic wave for sources we cannot measure.
    Levels([f32; crate::meter::BANDS]),
}

pub const BRAND_BARS_HEIGHT: f32 = 25.;
const BRAND_BARS_WIDTH: f32 = 23.;

/// The five logo bars.
pub fn brand_bars(pose: BrandPose) -> Div {
    let heights = [9.0f32, 19.0, 13.0, 23.0, 11.0];
    let mut bars =
        div().flex().flex_row().items_center().gap(px(2.)).w(px(BRAND_BARS_WIDTH)).h(px(BRAND_BARS_HEIGHT)).flex_shrink_0();
    for (i, height) in heights.iter().enumerate() {
        let color = if i > 2 { theme::PURPLE } else { theme::ACCENT };
        let height = match pose {
            BrandPose::Still => *height,
            BrandPose::Levels(levels) => 4. + 19. * levels[i],
        };
        bars = bars.child(div().w(px(3.)).rounded(px(1.5)).bg(color).h(px(height)));
    }
    bars
}

/// The logo: the bars (or an empty space of their size when they are drawn on top
/// of the window, see `shell`) plus the wordmark.
pub fn brand(bars: Option<Div>) -> Div {
    let bars = bars.unwrap_or_else(|| div().w(px(BRAND_BARS_WIDTH)).h(px(BRAND_BARS_HEIGHT)).flex_shrink_0());
    let wordmark = div()
        .relative()
        .w(px(94. * 20. / 24.))
        .h(px(20.))
        .flex_shrink_0()
        .child(svg().path("icons/wordmark-cyan.svg").absolute().inset_0().size_full().text_color(theme::ACCENT))
        .child(svg().path("icons/wordmark-purple.svg").absolute().inset_0().size_full().text_color(theme::PURPLE));
    div().flex().flex_row().items_center().gap(px(8.)).child(bars).child(wordmark)
}

/// A horizontal hairline.
pub fn separator() -> Div {
    div().w_full().h(px(1.)).bg(theme::BORDER).flex_shrink_0()
}
