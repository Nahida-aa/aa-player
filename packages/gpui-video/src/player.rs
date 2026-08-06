//! 播放器组合视图（Player）。
//!
//! 拥有 `PlayerController`（无 GUI 播放状态机）+ 进度条 `SliderState`，
//! 启动渲染循环，把视频画面与控制条组合成一个可复用视图。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyDownEvent, MouseButton, Render,
    Window, div, prelude::*,
};
use ui_gpui::{SliderEvent, SliderState};

use crate::controller::PlayerController;
use crate::controls::PlaybackControls;
use crate::playback_clock::{PlaybackClock, Schedule};
use crate::surface::VideoSurface;

/// 性能统计上报窗口（秒）。
const STATS_WINDOW_SECS: u64 = 2;
/// 键盘方向键 seek 步长（秒）。
const SEEK_STEP: Duration = Duration::from_secs(5);

/// 解码结束（EOF）时发出，供外部监听（如播放列表自动切下一集）。
#[derive(Debug)]
pub struct PlaybackEnded;

impl EventEmitter<PlaybackEnded> for Player {}

/// 控制条时间文本的显示格式。
///
/// - [`TimeFormat::Frame`]：`mm:ss:ff`（ff = 帧），默认。
/// - [`TimeFormat::FrameMillis`]：`mm:ss:ff,mmm,mmm`（第一个 mmm = 秒内毫秒，
///   第二个 mmm = 当前原始毫秒/总位置）。供 player-app 等需要更精细时间戳的场景。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimeFormat {
    /// `mm:ss:ff`（ff = 帧）。
    #[default]
    Frame,
    /// `mm:ss:ff,mmm,mmm`。
    FrameMillis,
}

/// 播放器视图：给一个视频路径即可用。
pub struct Player {
    /// 无 GUI 播放状态机。
    controller: Entity<PlayerController>,
    /// 进度条状态（0..duration 毫秒）。
    progress: Entity<SliderState>,
    /// 视频画面等比上限；None 填满容器（letterbox）。
    max_size: Option<gpui::Pixels>,
    /// 上一帧（双缓冲回收）。
    previous_frame: Option<Arc<gpui::RenderImage>>,
    /// 渲染循环任务句柄（保活）。
    _render_task: gpui::Task<()>,
    focus_handle: FocusHandle,
    /// 控制条是否可见：默认隐藏，鼠标移入控制区才显示（移出后隐藏）。
    /// 菜单/info 打开时强制可见，避免失焦即收起。
    controls_visible: bool,
    /// 控制条时间文本格式，默认 [`TimeFormat::Frame`]（`mm:ss:ff`）。
    time_format: TimeFormat,
}

impl Player {
    /// 打开视频并启动播放器。`window` 用于绑定渲染任务生命周期（关窗自动取消）。
    pub fn new(path: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {        let (controller, mut rx) = {
            let (c, rx) = PlayerController::open(path);
            (cx.new(|_| c), rx)
        };
        let progress = cx.new(|_| {
            SliderState::new()
                .min(0.0)
                .max(1.0)
                .step(1.0)
                // 播放器进度条用 OnHover 模式：默认不显示 thumb，悬停轨道才显示，
                // 拖动时只变色不变大（视频播放器常见风格）。
                .thumb_mode(ui_gpui::ThumbMode::OnHover)
        });
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
                    // **不要在这里清空帧通道/丢弃当前帧**：seek 后解码线程重建音频
                    // （generation++）随之投递的第一个 post-seek 帧正是要显示的目标帧
                    // （暂停中 seek 只投这一帧，丢了画面就不动）。seek 前在途的旧帧
                    // 由 `consume_frame` 的 seek 代次检查（frame_gen < seek_gen）丢弃
                    // position、由下方时钟调度丢弃画面（reset 后旧帧判 Wait>1s/Drop）。
                }
                clock.set_audio_offset(offset_us);

                // 预览帧或拖动中：直接显示，不走音频时钟调度。
                // （拖动时声卡静音、音频时钟冻结，正常帧会被 Wait/Drop 卡住；
                //  拖动中所有帧直接显示，避免残留正常帧用冻结时钟卡住画面。）
                let is_preview = matches!(&item, Some((_, _, _, true, _)));
                let show = if is_preview || dragging_render.load(std::sync::atomic::Ordering::Relaxed) {
                    true
                } else {
                    match &item {
                        Some((_, pts_us, dur_us, false, _)) => {
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
                        Some((_, pts_us, _, false, _)) => audio
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
            // 渲染循环结束（解码通道关闭 / 窗口关闭）：通知外部播放结束。
            this.update(cx, |_, cx| cx.emit(PlaybackEnded)).ok();
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
                    // 拖动开始（第一次 Change）：静音一次（对齐 player-app 的 MuteAudio）。
                    if !dragging_change.swap(true, std::sync::atomic::Ordering::Relaxed) {
                        this.controller.update(cx, |c, _| c.mute_audio());
                    }
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
            max_size: None,
            previous_frame: None,
            _render_task,
            focus_handle,
            controls_visible: false,
            time_format: TimeFormat::default(),
        }
    }

    /// 设置视频画面等比上限：组件按视频原始宽高比缩放，最大边不超 `max`。
    /// 未设则视频填满父容器（letterbox）。
    pub fn max_size(mut self, max: gpui::Pixels) -> Self {
        self.max_size = Some(max);
        self
    }

    /// 设置控制条时间文本格式。默认 [`TimeFormat::Frame`]（`mm:ss:ff`）。
    pub fn time_format(mut self, fmt: TimeFormat) -> Self {
        self.time_format = fmt;
        self
    }

    /// 程序化触发一次拖动 seek（拖动中 preview + 松手 release）。
    ///
    /// 供外部（自动拖动脚本 / 键盘快捷键 / 测试）驱动，等价于在进度条上
    /// 按住拖到 `target` 再松开。`target` 单位秒。
    pub fn seek(&mut self, target: Duration, cx: &mut Context<Self>) {
        self.controller.update(cx, |c, _| c.seek_preview(target));
        self.controller.update(cx, |c, _| c.seek_release(target));
        cx.notify();
    }

    /// 拖动中 preview（按住不松、移动进度条）。配合 [`seek`](Self::seek) 用：
    /// 连续多次 `seek_preview` 模拟快速拖动中的移动，最后 `seek` 提交。
    pub fn seek_preview(&mut self, target: Duration, cx: &mut Context<Self>) {
        self.controller.update(cx, |c, _| c.seek_preview(target));
        cx.notify();
    }

    /// 播放/暂停切换（键盘空格、外部控制）。
    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        let paused = self.controller.read(cx).paused();
        self.controller
            .update(cx, |c, _| if paused { c.play() } else { c.pause() });
        cx.notify();
    }

    /// 相对当前位置向前 seek（键盘方向键，夹到 [0, duration]）。
    pub fn seek_forward(&mut self, delta: Duration, cx: &mut Context<Self>) {
        let pos = self.controller.read(cx).position();
        let dur = self.controller.read(cx).duration();
        let target = (pos + delta).min(dur);
        self.seek_to(target, cx);
    }

    /// 相对当前位置向后 seek（键盘方向键，夹到 [0, duration]）。
    pub fn seek_backward(&mut self, delta: Duration, cx: &mut Context<Self>) {
        let pos = self.controller.read(cx).position();
        let target = pos.saturating_sub(delta);
        self.seek_to(target, cx);
    }

    /// 正式 seek（Commit），供点击/键盘/松开。同步 position。
    fn seek_to(&mut self, target: Duration, cx: &mut Context<Self>) {
        self.controller.update(cx, |c, _| c.seek_to(target));
        cx.notify();
    }

    /// 键盘快捷键处理（对齐 player-app view.rs:401-417）。
    fn on_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "space" => self.toggle(cx),
            "left" => self.seek_backward(SEEK_STEP, cx),
            "right" => self.seek_forward(SEEK_STEP, cx),
            _ => {}
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
            .key_context("player")
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| {
                this.on_key(e, cx);
                cx.notify();
            }))
            // 鼠标移入播放区（移动即触发）：显示控制条。
            .on_mouse_move(cx.listener(|this, _event, _window, cx| {
                if !this.controls_visible {
                    this.controls_visible = true;
                    cx.notify();
                }
            }))
            // 鼠标移出播放区：隐藏（菜单/info 打开时保持显示）。
            .on_mouse_exit(cx.listener(|this, _event, _window, cx| {
                let keep = this.controller.read(cx).is_menu_open()
                    || this.controller.read(cx).is_info_open();
                if !keep {
                    this.controls_visible = false;
                    cx.notify();
                }
            }))
            // 点击播放区（浮层外）关闭「更多」菜单与「info」面板。按钮/菜单项
            // 自身在 mouse_down 已 stop_propagation，故点它们不会触发这里。
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _event, _window, cx| {
                if this.controller.read(cx).is_menu_open() {
                    this.controller.update(cx, |c, _| c.close_menu());
                }
                if this.controller.read(cx).is_info_open() {
                    this.controller.update(cx, |c, _| c.close_info());
                }
            }))
            .child(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(match self.max_size {
                        Some(max) => VideoSurface::new(&self.controller).max_size(max),
                        None => VideoSurface::new(&self.controller),
                    }),
            )
            .when(self.controls_visible, |this| {
                this.child(
                    div()
                        .absolute()
                        .bottom_0()
                        .left_0()
                        .right_0()
                        .child(
                            PlaybackControls::new(&self.controller, &self.progress)
                                .time_format(self.time_format)
                                .on_toggle(|c| {
                                    if c.paused() {
                                        c.play();
                                    } else {
                                        c.pause();
                                    }
                                }),
                        ),
                )
            })
    }
}
