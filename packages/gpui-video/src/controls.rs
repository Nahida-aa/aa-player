//! 播放控制条（PlaybackControls）。
//!
//! 进度条（ui-gpui Slider）+ 播放/暂停按钮 + 时间文本。作为一次性元素，
//! 由拥有控制器和进度条状态的父视图（`Player`）构建。

use std::time::Duration;

use gpui::{Entity, IntoElement, MouseButton, RenderOnce, Window, div, prelude::*, px, rgb};
use ui_gpui::{Slider, SliderState};

use crate::controller::PlayerController;

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

        // 播放/暂停按钮标签。
        let play_label = if paused { "▶" } else { "⏸" };

        let mut controls = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .w_full()
            .px_2()
            .py_1()
            .bg(rgb(0x000000cc));

        // 播放/暂停按钮。
        let mut btn = div()
            .id("toggle")
            .w(px(28.0))
            .h(px(28.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(rgb(0xffffff22))
            .child(play_label);
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
        controls = controls.child(btn);

        // 进度条（ui-gpui Slider）：0..duration 秒，值 = 当前位置。
        let progress_el = Slider::new(&self.progress);
        controls = controls.child(div().flex_1().h(px(20.0)).child(progress_el));

        // 时间文本。
        controls = controls.child(div().text_size(px(12.0)).child(time_text));

        controls
    }
}

/// 让 `Duration` 在此模块有用途标注（进度条换算辅助，V1 简化）。
#[allow(dead_code)]
fn _secs(d: Duration) -> f32 {
    d.as_secs_f32()
}
