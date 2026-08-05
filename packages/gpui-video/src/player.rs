//! 播放器组合视图（Player）。
//!
//! 拥有 `PlayerController`（无 GUI 播放状态机）+ 进度条 `SliderState`，
//! 启动渲染循环，把视频画面与控制条组合成一个可复用视图。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use gpui::{App, Context, Entity, FocusHandle, Focusable, Render, Window, div, prelude::*};
use ui_gpui::{SliderEvent, SliderState};

use crate::controller::PlayerController;
use crate::controls::PlaybackControls;
use crate::surface::VideoSurface;

/// 播放器视图：给一个视频路径即可用。
pub struct Player {
    /// 无 GUI 播放状态机。
    controller: Entity<PlayerController>,
    /// 进度条状态（0..duration 秒）。
    progress: Entity<SliderState>,
    /// 上一帧（双缓冲回收）。
    previous_frame: Option<Arc<gpui::RenderImage>>,
    /// 渲染循环任务句柄（保活）。
    _render_task: gpui::Task<()>,
    focus_handle: FocusHandle,
}

impl Player {
    /// 打开视频并启动播放器。
    pub fn new(path: PathBuf, cx: &mut Context<Self>) -> Self {
        let (controller, mut rx) = {
            let (c, rx) = PlayerController::open(path);
            (cx.new(|_| c), rx)
        };
        let progress = cx.new(|_| SliderState::new().min(0.0).max(1.0).step(1.0));
        let focus_handle = cx.focus_handle();

        // 渲染循环：异步收帧 → 更新 controller → notify。
        let _render_task = cx.spawn(async move |this, cx| {
            while let Some(item) = rx.next().await {
                this.update(cx, |this, cx| {
                    this.controller.update(cx, |c, cx| c.consume_frame(item, cx));
                })
                .ok();
            }
        });

        // 进度条事件 → 控制器 seek：Change=拖动预览，Release=提交。
        cx.subscribe(&progress, move |this, _slider, event, cx| {
            match event {
                SliderEvent::Change(v) => {
                    this.controller.update(cx, |c, _| {
                        c.seek_preview(Duration::from_secs_f32(v.end()))
                    });
                }
                SliderEvent::Release(v) => {
                    this.controller.update(cx, |c, _| {
                        c.seek_release(Duration::from_secs_f32(v.end()))
                    });
                }
            }
            cx.notify();
        })
        .detach();

        Self {
            controller,
            progress,
            previous_frame: None,
            _render_task,
            focus_handle,
        }
    }
}

impl Focusable for Player {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Player {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 双缓冲回收旧纹理，防止 atlas 无限增长。
        if let Some(frame) = self.controller.read(cx).latest_frame() {
            if let Some(prev) = self.previous_frame.take()
                && prev.id != frame.id
            {
                let _ = window.drop_image(prev);
            }
            self.previous_frame = Some(frame);
        }

        // 进度条绑定控制器状态（0..duration 秒）。拖动时让滑块跟手（由 Slider
        // 自身驱动），外部不覆盖 position。
        let (position, duration, dragging) = {
            let c = self.controller.read(cx);
            (c.position(), c.duration(), c.is_dragging())
        };
        if !dragging {
            let max = duration.as_secs_f32().max(1.0);
            self.progress.update(cx, |s, cx| {
                s.set_max(max, cx);
                s.set_value(position.as_secs_f32().min(max), cx);
            });
        }

        // 画面 + 控制条。
        div()
            .size_full()
            .relative()
            .track_focus(&self.focus_handle(cx))
            .child(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(VideoSurface::new(&self.controller)),
            )
            .child(
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .child(
                        PlaybackControls::new(&self.controller, &self.progress)
                            .on_toggle(|c| {
                                if c.paused() {
                                    c.play();
                                } else {
                                    c.pause();
                                }
                            }),
                    ),
            )
    }
}
