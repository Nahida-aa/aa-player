//! 播放控制条（PlaybackControls）。
//!
//! 进度条（ui-gpui Slider）+ 播放/暂停按钮 + 时间文本。作为一次性元素，
//! 由拥有控制器和进度条状态的父视图（`Player`）构建。
//!
//! 布局对齐 aa-player 原版：底部半透明黑 overlay，两行——
//! 上行是「播放/暂停 + 时间文本」（右对齐），下行是占满宽度的进度条。

use gpui::{
    Div, Entity, IntoElement, MouseButton, RenderOnce, SharedString, Stateful, Window, div,
    linear_color_stop, linear_gradient, prelude::*, px, rgba, svg, white,
};
use std::time::Duration;

use ui_gpui::{Slider, SliderState};

use crate::controller::PlayerController;

/// 图标资源路径（内嵌 via asset source，`Application::new().with_assets(…)` 提供）。
const ICON_PLAY: &str = "icons/play_filled.svg";
const ICON_PAUSE: &str = "icons/debug_pause.svg";
const ICON_VOLUME_ON: &str = "icons/audio_on.svg";
const ICON_VOLUME_OFF: &str = "icons/audio_off.svg";
const ICON_MORE: &str = "icons/ellipsis_vertical.svg";

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

        let fps = ctrl.fps();
        let time_text = format!(
            "{} / {}",
            timecode(position, fps),
            timecode(duration, fps),
        );
        let icon_path = if paused { ICON_PLAY } else { ICON_PAUSE };
        let muted = ctrl.is_muted();
        let menu_open = ctrl.is_menu_open();
        let volume_icon = if muted { ICON_VOLUME_OFF } else { ICON_VOLUME_ON };

        /// 控制条圆形图标按钮（28px，半透明白底，染白图标）。
        fn icon_btn(id: &'static str, icon: &str) -> Stateful<Div> {
            div()
                .id(id)
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
                        .path(icon)
                        .w(px(16.0))
                        .h(px(16.0))
                        // svg 元素自身必须设 text_color，否则 gpui 不渲染（svg.rs:119）。
                        .text_color(white()),
                )
        }

        // 播放/暂停按钮。
        let mut btn = icon_btn("toggle", icon_path);
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

        // 静音按钮（独立、常用，不放进菜单）。
        let ctrl_for_volume = self.controller.clone();
        let volume_btn = icon_btn("volume", volume_icon).on_mouse_up(
            MouseButton::Left,
            move |_, _, cx| {
                ctrl_for_volume.update(cx, |c, _| c.toggle_mute());
            },
        );

        // 「更多」按钮（竖排三点 kebab），弹出浮层菜单。
        let ctrl_for_more = self.controller.clone();
        let mut more_btn = icon_btn("more", ICON_MORE)
            .relative() // 让内部 anchored 浮层以本按钮为定位参照
            // 用 mouse_down 切换并在按钮上掐断冒泡：这样点按钮不会先触发外层
            // 的「点外部关闭」，从而稳定地开/关菜单（而非开→关抵消）。
            .on_mouse_down(
                MouseButton::Left,
                move |_, _, cx| {
                    cx.stop_propagation();
                    ctrl_for_more.update(cx, |c, _| c.toggle_menu());
                },
            );
        if menu_open {
            // 浮层菜单：直接作为 more_btn(relative) 的 absolute 子元素，相对按钮定位——
            // 贴着按钮正上方、右对齐弹出。比 anchored() 在此处的 Local 模式更可控
            // （anchored 用的 bounds.origin 是自身布局原点而非按钮视觉位置，在 flex
            // 居中下会偏移到左上方）。菜单项第一个控制倍速，其余为占位/可扩展。
            let ctrl_for_menu = self.controller.clone();
            let speed_label = format!("{}x", ctrl.speed());
            let menu = div()
                .id("more-menu")
                .absolute()
                .bottom(px(32.0)) // 菜单底边距按钮顶边 4px 间隔（按钮约 28px 高）
                .right(px(0.0)) // 右对齐按钮
                .flex()
                .flex_col()
                .min_w(px(160.0))
                .overflow_hidden()
                // 无圆角 + 更透明背景（44≈27% 不透明），叠加在控制条渐变之上仍可读。
                .bg(rgba(0x1e1e1e44))
                .text_color(white())
                // 第一个菜单项：循环切换播放速度（1→1.25→1.5→2→0.5→1）。
                // 切换后保持菜单打开，方便连续点击看效果；点外部才关闭。
                .child(menu_item(
                    speed_label,
                    ctrl_for_menu.clone(),
                    |c| {
                        c.cycle_speed();
                    },
                ))
                // 其余菜单项：info 信息面板。点它关闭更多菜单、打开 info 面板。
                .child(menu_item(
                    "info".to_string(),
                    ctrl_for_menu.clone(),
                    |c| {
                        c.close_menu();
                        c.toggle_info();
                    },
                ));
            more_btn = more_btn.child(menu);
        }

        // 两行控制条：上行「按钮 + 时间」，下行「进度条」。
        div()
            .w_full()
            .flex()
            .flex_col()
            .relative() // 让 info 信息面板 absolute 以控制条为定位参照
            .gap(px(2.0))
            .pt(px(6.0))
            .pb(px(8.0))
            .px(px(12.0))
            // 背景从上到下渐变：顶部完全透明 → 底部半透明黑（经典控制条效果）。
            .bg(linear_gradient(
                180.0,
                linear_color_stop(rgba(0x00000000), 0.0),
                linear_color_stop(rgba(0x00000066), 1.0),
            ))
            // 上行：播放按钮 + 时间（左对齐），中间撑开，右侧是静音 + 更多按钮。
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(btn)
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .text_size(px(12.0))
                            .text_color(white())
                            .child(time_text),
                    )
                    .child(div().flex_1().w(px(0.0)))
                    .child(volume_btn)
                    .child(more_btn),
            )
            // 下行：进度条占满宽，细轨道。thumb 12px、轨道 4px。
            .child(
                div()
                    .w_full()
                    .h(px(12.0))
                    .flex()
                    .items_center()
                    .child(
                        Slider::new(&self.progress)
                            .thumb_size(px(12.0))
                            .track_size(px(4.0))
                            .track_color(rgba(0x55555533)),
                    ),
            )
            // info 信息面板：点更多菜单里的 info 项展开，显示当前能分析到的视频信息。
            .when(ctrl.is_info_open(), |this| {
                let (vw, vh) = ctrl.video_size();
                let speed = ctrl.speed();
                this.child(
                    div()
                        .id("info-panel")
                        .absolute()
                        .bottom(px(40.0)) // 浮在控制条上方
                        .right(px(12.0))
                        .flex()
                        .flex_col()
                        .min_w(px(180.0))
                        .overflow_hidden()
                        // 与更多菜单同款风格：无圆角、半透明背景。
                        .bg(rgba(0x1e1e1e88))
                        .text_color(white())
                        .text_size(px(12.0))
                        .child(info_line(format!("分辨率: {}x{}", vw, vh)))
                        .child(info_line(format!("帧率: {:.2} fps", fps)))
                        .child(info_line(format!("倍速: {}x", speed))),
                )
            })
    }
}

/// 浮层菜单项：单行可点击文本。点击时通过 `on_click` 驱动控制器（如切倍速、
/// 关菜单）。`label` 可为动态文本（如「倍速 1.5x」）。
fn menu_item(
    label: impl Into<SharedString>,
    ctrl: Entity<PlayerController>,
    on_click: impl Fn(&mut PlayerController) + 'static,
) -> Stateful<Div> {
    let label: SharedString = label.into();
    div()
        .id(label.clone())
        .px(px(12.0))
        .py(px(6.0))
        .text_size(px(13.0))
        // 掐断 mousedown 冒泡：否则会先触发外层 Player 的「点外部关闭」，
        // 菜单在 mouseup 前就被收走，导致点击既不变速、文字也不刷新。
        .on_mouse_down(
            MouseButton::Left,
            |_, _, cx| {
                cx.stop_propagation();
            },
        )
        .on_mouse_up(
            MouseButton::Left,
            move |_, _, cx| {
                ctrl.update(cx, |c, _| on_click(c));
            },
        )
        .child(label)
}

/// info 信息面板里的单行文本（左对齐、内边距）。
fn info_line(text: String) -> Div {
    div()
        .px(px(12.0))
        .py(px(4.0))
        .child(text)
}

/// 把时长格式化为 `mm:ss:ff,mmm,mmm`。
///
/// - `ff` = 帧，基于 `fps`：帧数 = 小数部分 × fps，clamp 到 `[0, fps-1]`
///   （进位到 fps 整时归零）。`fps <= 0`（帧率未知）时 fallback 到 30。
/// - 第一个 `mmm` = 秒内的毫秒（`total_ms % 1000`）。
/// - 第二个 `mmm` = 当前原始毫秒（总时长/位置，单位毫秒）。
fn timecode(d: Duration, fps: f64) -> String {
    let total = d.as_secs_f64();
    let total_ms = d.as_millis();
    let mm = total as u64 / 60;
    let ss = total as u64 % 60;
    let fps = if fps > 0.0 { fps } else { 30.0 };
    let ff = ((total.fract() * fps).round() as i64).clamp(0, fps as i64 - 1) as u64;
    let ms_in_sec = total_ms % 1000;
    format!(
        "{mm:02}:{ss:02}:{ff:02},{ms_in_sec:03},{total_ms}",
    )
}
