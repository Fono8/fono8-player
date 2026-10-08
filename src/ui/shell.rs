//! The window root: the main view, reused from the previous frame while it has not
//! changed, with the logo bars drawn on top. Animating the logo then redraws five
//! bars instead of the whole window.

use std::time::{Duration, Instant};

use gpui::{div, prelude::*, px, AnyView, Context, Entity, StyleRefinement, Window};

use super::main_view::{MainView, TITLEBAR_HEIGHT, TITLEBAR_PADDING};
use super::widgets::{brand_bars, BrandPose, BRAND_BARS_HEIGHT};
use crate::app::Fono8;
use crate::meter::{Levels, Motion};

pub struct Shell {
    pub main: Entity<MainView>,
    brand: Entity<BrandMark>,
}

impl Shell {
    pub fn new(main: Entity<MainView>, model: Entity<Fono8>, cx: &mut Context<Self>) -> Shell {
        let brand = cx.new(|cx| BrandMark::new(model, cx));
        Shell { main, brand }
    }
}

impl Render for Shell {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .relative()
            .child(AnyView::from(self.main.clone()).cached(StyleRefinement::default().size_full()))
            .child(self.brand.clone())
    }
}

/// Whether the bars are animated on top of the main view; the title bar then leaves
/// their place empty, otherwise it draws them standing still.
pub fn brand_animated(m: &Fono8) -> bool {
    m.playback.playing() && m.dialog.is_none()
}

/// The moving logo bars: band levels of local audio, a generic wave for other sources.
struct BrandMark {
    model: Entity<Fono8>,
    levels: Levels,
    motion: Motion,
    started: Instant,
    animated: bool,
}

impl BrandMark {
    fn new(model: Entity<Fono8>, cx: &mut Context<Self>) -> BrandMark {
        let levels = model.read(cx).playback.engine.levels();
        // ~30 frames a second while animated (and one more when it stops), otherwise a slow check.
        cx.spawn(async move |this, cx| loop {
            let Ok(animated) = this.update(cx, |mark, cx| {
                let animated = brand_animated(mark.model.read(cx));
                if animated || mark.animated {
                    cx.notify();
                }
                animated
            }) else {
                break;
            };
            cx.background_executor().timer(Duration::from_millis(if animated { 33 } else { 200 })).await;
        })
        .detach();
        BrandMark { model, levels, motion: Motion::default(), started: Instant::now(), animated: false }
    }
}

impl Render for BrandMark {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.animated = brand_animated(self.model.read(cx));
        let mark = div().absolute().left(px(1. + TITLEBAR_PADDING)).top(px(1. + (TITLEBAR_HEIGHT - BRAND_BARS_HEIGHT) / 2.));
        if !self.animated {
            return mark;
        }
        let now = Instant::now();
        let levels = self.motion.step(&self.levels, now).unwrap_or_else(|| crate::meter::wave(now - self.started));
        mark.child(brand_bars(BrandPose::Levels(levels)))
    }
}
