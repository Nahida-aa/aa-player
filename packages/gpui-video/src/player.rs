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
use crate::playback_clock::{PlaybackClock, Schedule};
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
        let progress = cx.new(|_| SliderState::new().min(0.0).max(1.0).step(1.0));        let focus_handle = cx.focus_handle();

        // 音频时钟交接点（渲染循环调度用）。克隆进异步任务，避免跨实体读。
        let clock_source = controller.read(cx).clock.clone();

        // 渲染循环：异步收帧 → 按播放时钟调度 → 更新 controller → notify。
        let _render_task = cx.spawn(async move |this, cx| {
            let mut clock = PlaybackClock::new();
            // 记住音频时钟代次，换代才换时钟柄（保留墙钟 origin）。
            let mut audio_generation: u64 = 0;
            while let Some(item) = rx.next().await {
                // 从音频时钟交接点取 (代次, seek偏移, 时钟)，调度视频帧。
                let (generation, offset_us, audio) = clock_source.get_with_generation();
                if generation != audio_generation {
                    audio_generation = generation;
                    if let Some(a) = audio {
                        clock.set_audio(a);
                    }
                    // seek 发生：重置墙钟 origin。
                    clock.reset_origin();
                }
                clock.set_audio_offset(offset_us);

                // preview 帧（拖动中）：直接显示，不走音频时钟调度。
                let show = match &item {
                    Some((_, _, dur_us, true)) => {
                        clock.set_duration(*dur_us);
                        true
                    }
                    Some((_, pts_us, dur_us, false)) => {
                        clock.set_duration(*dur_us);
                        match clock.schedule(Duration::from_micros(*pts_us)) {
                            Schedule::Now | Schedule::Resynced { .. } => true,
                            // 未来帧：等点到再显示。封顶 1s——超过几乎必是 seek 后
                            // 残留旧帧，真等会卡住画面数秒、音频趁机超前。
                            Schedule::Wait(d) => {
                                if d.as_millis() > 1000 {
                                    false // 旧帧/时钟错位，丢弃
                                } else {
                                    cx.background_executor().timer(d).await;
                                    true
                                }
                            }
                            // 落后太多：丢帧，让画面追上声音。
                            Schedule::Drop { .. } => false,
                        }
                    }
                    None => true, // EOF：更新状态（进度条拉满）
                };

                if show {
                    this.update(cx, |this, cx| {
                        this.controller.update(cx, |c, cx| c.consume_frame(item, cx));
                    })
                    .ok();
                }
            }
        });

        // 进度条事件 → 控制器 seek：Change=拖动预览，Release=提交。
        // 单位：Slider 值 = 毫秒（step=1 → 1ms 步进），seek 精确到毫秒。
        cx.subscribe(&progress, move |this, _slider, event, cx| {
            match event {
                SliderEvent::Change(v) => {
                    this.controller.update(cx, |c, _| {
                        c.seek_preview(Duration::from_millis(v.end() as u64))
                    });
                }
                SliderEvent::Release(v) => {
                    this.controller.update(cx, |c, _| {
                        c.seek_release(Duration::from_millis(v.end() as u64))
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
            // 进度条值域 = 视频时长（毫秒）。
            let max_ms = duration.as_millis() as f32;
            self.progress.update(cx, |s, cx| {
                s.set_max(max_ms, cx);
                s.set_value((position.as_millis() as f32).min(max_ms), cx);
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
