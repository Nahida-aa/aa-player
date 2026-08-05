//! 播放控制条（PlaybackControls）。
//!
//! 进度条（ui-gpui Slider）+ 播放/暂停按钮 + 时间文本。作为一次性元素，
//! 由拥有控制器和进度条状态的父视图（`Player`）构建。
//!
//! 布局对齐 aa-player 原版：底部半透明黑 overlay，两行——
//! 上行是「播放/暂停 + 时间文本」（右对齐），下行是占满宽度的进度条。

use gpui::{
    Entity, IntoElement, MouseButton, RenderOnce, Window, div, prelude::*, px, rgba, svg, white,
};
use ui_gpui::{Slider, SliderState};

use crate::controller::PlayerController;

/// 图标资源路径（内嵌 via asset source，`Application::new().with_assets(…)` 提供）。
const ICON_PLAY: &str = "icons/play_filled.svg";
const ICON_PAUSE: &str = "icons/debug_pause.svg";

/// 播放/暂停按钮回调：驱动控制器切换播放状态。
type ToggleHandler = Box<dyn Fn(&mut PlayerController) + 'static>;

/// 控制条元素（一次性）。
#[derive(IntoElement)]
pub struct PlaybackControls {
    controller: Entity<PlayerController>,
    progress: Entity<SliderState>,
    on_toggle: Option<ToggleHandler>,
}

impl PlaybackControls {
    /// 绑定控制器与进度条状态。
    pub fn new(controller: &Entity<PlayerController>, progress: &Entity<SliderState>) -> Self {
        Self {
            controller: controller.clone(),
            progress: progress.clone(),
            on_toggle: None,
        }
    }

    /// 播放/暂停按钮点击回调（由父视图传入，负责切换并驱动 controller）。
    pub fn on_toggle(
        mut self,
        handler: impl Fn(&mut PlayerController) + 'static,
    ) -> Self {
        self.on_toggle = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for PlaybackControls {
    fn render(self, _window: &mut Window, cx: &mut gpui::App) -> impl IntoElement {
        let ctrl = self.controller.read(cx);
        let position = ctrl.position();
        let duration = ctrl.duration();
        let paused = ctrl.paused();

        let time_text = format!(
            "{:02}:{:02} / {:02}:{:02}",
            position.as_secs() / 60,
            position.as_secs() % 60,
            duration.as_secs() / 60,
            duration.as_secs() % 60,
        );
        let icon_path = if paused { ICON_PLAY } else { ICON_PAUSE };

        // 播放/暂停按钮（zed 图标，染白）。
        let mut btn = div()
            .id("toggle")
            .w(px(28.0))
            .h(px(28.0))
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(rgba(0xffffff22))
            .text_color(white())
            .child(
                svg()
                    .path(icon_path)
                    .w(px(16.0))
                    .h(px(16.0)),
            );
        if let Some(toggle) = self.on_toggle {
            let ctrl = self.controller.clone();
            btn = btn.on_mouse_up(
                MouseButton::Left,
                move |_, _, cx| {
                    let c = ctrl.clone();
                    c.update(cx, |c, _| toggle(c));
                },
            );
        }

        // 两行控制条：上行「按钮 + 时间」，下行「进度条」。
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .pt(px(6.0))
            .pb(px(8.0))
            .px(px(12.0))
            .bg(rgba(0x00000066))
            // 上行：按钮靠左，时间文本右对齐（占满宽）。
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(btn)
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .justify_end()
                            .text_size(px(12.0))
                            .text_color(white())
                            .child(time_text),
                    ),
            )
            // 下行：进度条占满宽。
            .child(
                div()
                    .w_full()
                    .h(px(20.0))
                    .flex()
                    .items_center()
                    .child(Slider::new(&self.progress)),
            )
    }
}
