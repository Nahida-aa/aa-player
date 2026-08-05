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

/// 性能统计上报窗口（秒）。
const STATS_WINDOW_SECS: u64 = 2;

/// 播放器视图：给一个视频路径即可用。
pub struct Player {
    /// 无 GUI 播放状态机。
    controller: Entity<PlayerController>,
    /// 进度条状态（0..duration 毫秒）。
    progress: Entity<SliderState>,
    /// 上一帧（双缓冲回收）。
    previous_frame: Option<Arc<gpui::RenderImage>>,
    /// 渲染循环任务句柄（保活）。
    _render_task: gpui::Task<()>,
    focus_handle: FocusHandle,
}

impl Player {
    /// 打开视频并启动播放器。`window` 用于绑定渲染任务生命周期（关窗自动取消）。
    pub fn new(path: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (controller, mut rx) = {
            let (c, rx) = PlayerController::open(path);
            (cx.new(|_| c), rx)
        };
        let progress = cx.new(|_| SliderState::new().min(0.0).max(1.0).step(1.0));
        let focus_handle = cx.focus_handle();
        let dragging = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // 音频时钟交接点（渲染循环调度用）。克隆进异步任务，避免跨实体读。
        let clock_source = controller.read(cx).clock.clone();
        // 性能统计（解码在 worker 里记，渲染侧记显示/漂移）。
        let stats = controller.read(cx).stats.clone();

        // 渲染循环：异步收帧 → 按播放时钟调度 → 更新 controller → notify。
        // 用 spawn_in(window) 绑定窗口生命周期：关窗即取消，避免任务泄漏。
        let dragging_render = dragging.clone();
        let stats_render = stats.clone();
        let _render_task = cx.spawn_in(window, async move |this, cx| {
            let mut clock = PlaybackClock::new();
            // 记住音频时钟代次，换代才换时钟柄（保留墙钟 origin）。
            let mut audio_generation: u64 = 0;
            while let Some(item) = rx.next().await {
                // 从音频时钟交接点取 (代次, seek偏移, 时钟)，调度视频帧。
                let (generation, offset_us, audio) = clock_source.get_with_generation();
                if generation != audio_generation {
                    audio_generation = generation;
                    if let Some(a) = audio.as_ref() {
                        clock.set_audio(a.clone());
                    }
                    // seek 发生：重置墙钟 origin。
                    clock.reset_origin();
                }
                clock.set_audio_offset(offset_us);

                // 预览帧或拖动中：直接显示，不走音频时钟调度。
                // （拖动时声卡静音、音频时钟冻结，正常帧会被 Wait/Drop 卡住；
                //  拖动中所有帧直接显示，避免残留正常帧用冻结时钟卡住画面。）
                let is_preview = matches!(&item, Some((_, _, _, true)));
                let show = if is_preview || dragging_render.load(std::sync::atomic::Ordering::Relaxed) {
                    true
                } else {
                    match &item {
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
                        _ => true, // EOF 等
                    }
                };

                if show {
                    // 统计漂移：在移动 item 前取 pts 和音频位置。
                    let drift_us = match &item {
                        Some((_, pts_us, _, false)) => audio
                            .as_ref()
                            .map(|a| a.position().as_micros() as i64 - *pts_us as i64),
                        _ => None,
                    };
                    this.update(cx, |this, cx| {
                        this.controller.update(cx, |c, cx| c.consume_frame(item, cx));
                    })
                    .ok();
                    // 统计：显示帧数 + 帧间隔 + 音画漂移（丢帧不计入）。
                    stats_render.record_displayed();
                    if let Some(drift) = drift_us {
                        stats_render.record_av_sync(drift);
                    }
                }
            }
        });

        // 性能统计上报：周期取快照，卡顿/失步打日志。
        let _stats_task = cx.spawn_in(window, async move |_, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(STATS_WINDOW_SECS))
                    .await;
                let snap = stats.take_snapshot(STATS_WINDOW_SECS);
                if snap.is_janky() {
                    tracing::warn!(
                        decoded_fps = snap.decoded_fps,
                        displayed_fps = snap.displayed_fps,
                        avg_interval_ms = snap.avg_interval_ms,
                        p99_interval_ms = snap.p99_interval_ms,
                        max_interval_ms = snap.max_interval_ms,
                        on_time_pct = snap.on_time_pct,
                        avg_decode_us = snap.avg_decode_us,
                        "检测到卡顿"
                    );
                }
                if snap.is_av_out_of_sync() {
                    tracing::warn!(
                        av_mean_ms = snap.av_sync_mean_ms,
                        av_rms_ms = snap.av_sync_rms_ms,
                        av_max_lag_ms = snap.av_sync_max_lag_ms,
                        av_max_lead_ms = snap.av_sync_max_lead_ms,
                        av_bad_pct = snap.av_sync_bad_pct,
                        "音画失步"
                    );
                }
            }
        });

        // 进度条事件 → 控制器 seek：Change=拖动预览，Release=提交。
        // 单位：Slider 值 = 毫秒（step=1 → 1ms 步进），seek 精确到毫秒。
        // 拖动标志用原子量同步给渲染循环（独立 async task 读）。
        let dragging_change = dragging.clone();
        let dragging_release = dragging.clone();
        cx.subscribe(&progress, move |this, _slider, event, cx| {
            match event {
                SliderEvent::Change(v) => {
                    dragging_change.store(true, std::sync::atomic::Ordering::Relaxed);
                    this.controller.update(cx, |c, _| {
                        c.seek_preview(Duration::from_millis(v.end() as u64))
                    });
                }
                SliderEvent::Release(v) => {
                    dragging_release.store(false, std::sync::atomic::Ordering::Relaxed);
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
