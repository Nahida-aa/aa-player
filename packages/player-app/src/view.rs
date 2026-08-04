//! 播放器视图：把解码帧上屏。
//!
//! 渲染模式照抄 zed 的 `remote_video_track_view.rs`：双缓冲 +
//! `drop_image` 回收纹理，避免 sprite atlas 泄漏。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::StreamExt;
use gpui::{
    Context, EventEmitter, FocusHandle, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Render, RenderImage, Task, Window, div, green, prelude::*, px,
    relative, rgba, white,
};
use tracing::{info, warn};

use crate::playback::{self, PlaybackClock, Schedule};
use crate::stats::ProfileStats;

/// 统计上报间隔（秒）。
const STATS_WINDOW_SECS: u64 = 2;

/// 方向键 seek 的步进。
const SEEK_STEP: Duration = Duration::from_secs(5);

/// 进度条左右留白（像素）。轨道宽度 = 窗口宽 − 2×此值；
/// 点击映射用同一常量换算，保证轨道/填充/点击三者对齐。
const PROGRESS_INSET: f32 = 12.0;

/// 拖动进度条时是否实时 seek（画面/声音随鼠标跳，拖到哪看到哪）。
/// `false` = 拖动中只移动进度条视觉，松开才跳到最终位置。
/// 默认 `true`（对齐主流播放器的实时跟随手感）。将来可接配置文件。
const LIVE_SEEK: bool = true;

/// 播放器视图：持有一帧最新的解码画面，并接收键盘/鼠标控制。
pub struct PlayerView {
    /// 解码线程推来、待渲染的最新帧。
    latest_frame: Option<Arc<RenderImage>>,
    /// 双缓冲：当前已渲染的帧，用于下一帧渲染时回收旧纹理。
    current_rendered: Option<Arc<RenderImage>>,
    previous_rendered: Option<Arc<RenderImage>>,
    /// 后台渲染任务句柄（持有以保活）。
    _render_task: Task<()>,
    /// 控制命令通道：暂停/继续/seek 发给解码线程。
    cmd: playback::CommandSender,
    /// 文件总时长（首帧到达后确定）。
    duration: Duration,
    /// 当前播放位置（随显示帧更新）。
    position: Duration,
    /// 是否暂停（仅 UI 侧镜像，真正的暂停在解码线程）。
    paused: bool,
    /// 是否正在拖动进度条。用 `Arc<AtomicBool>` 以便渲染循环（独立 async task）
    /// 读取——拖动中直接显示预览帧，不走音频时钟调度。
    dragging: Arc<AtomicBool>,
    /// 键盘焦点句柄：让本视图能收到按键。
    focus_handle: FocusHandle,
}

/// 解码线程结束（EOF）时发出，便于 UI 提示。
#[derive(Debug)]
pub struct PlaybackEnded;

impl EventEmitter<PlaybackEnded> for PlayerView {}

impl PlayerView {
    pub fn new(path: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (tx, mut rx) = playback::frame_channel();
        let (cmd, cmd_rx) = playback::command_channel();
        let running = Arc::new(AtomicBool::new(true));
        // 仅当 debug 级别开启时才统计（`RUST_LOG=player_app=debug`），
        // 避免常态下为统计付出原子操作与定时任务开销。
        let stats = Arc::new(ProfileStats::default());
        let profiling = tracing::enabled!(tracing::Level::DEBUG);

        // 音频主时钟的交接点：解码线程确认有音轨后填入，渲染侧异步切过去。
        let clock_source = Arc::new(playback::AudioClockSource::default());

        // 关窗时停止解码线程。
        let running_on_release = running.clone();
        cx.on_release(move |_, _cx| {
            running_on_release.store(false, Ordering::Relaxed);
        })
        .detach();

        playback::spawn_decode_thread(
            path,
            tx,
            running.clone(),
            stats.clone(),
            clock_source.clone(),
            cmd_rx,
        );

        let focus_handle = cx.focus_handle();

        // 拖动标志：渲染循环（独立 async task）要读它来决定是否直接显示预览帧。
        let dragging = Arc::new(AtomicBool::new(false));

        // 渲染 task：异步收帧，按 PTS 精确节流显示，绝不阻塞 executor。
        let stats_render = stats.clone();
        let dragging_render = dragging.clone();
        let _render_task = cx.spawn_in(window, async move |this, cx| {
            // 时钟初始为墙钟；解码线程把音频主时钟交上来后再切换。
            let mut clock = PlaybackClock::new();
            // 记住当前音频时钟的代次；只有换代（attach / seek 重建）才换时钟柄，
            // 避免每帧重建把墙钟 origin 清零（启动时音频未出声走墙钟，
            // origin 必须在首帧定一次，否则画面不受节流地提前刷出）。
            let mut audio_gen: u64 = 0;
            while let Some(item) = rx.next().await {
                let Some((render, pts_us, duration_us, is_preview)) = item else {
                    // EOF：进度条拉满，但**不退出循环**——解码线程在 EOF 后仍活着
                    // 等 seek 命令，播完再点进度条能回到中间重播，这里要继续消费帧。
                    this.update(cx, |this, cx| {
                        if this.duration != Duration::ZERO {
                            this.position = this.duration;
                        }
                        cx.notify();
                    })
                    .ok();
                    continue;
                };
                let pts = Duration::from_micros(pts_us);

                // 音频时钟换代时更新句柄；没换代则沿用，墙钟 origin 得以保持。
                let (clock_gen, offset_us, audio) = clock_source.get_with_generation();
                if clock_gen != audio_gen {
                    audio_gen = clock_gen;
                    if let Some(c) = audio.as_ref() {
                        clock.set_audio(c.clone());
                    }
                    // seek 发生了：重置墙钟 origin（避免 seek 到中间首帧被判"落后 N 秒"
                    // 触发 Resynced）。**不要清空帧队列**——那会与解码线程竞态，
                    // 把 seek 后刚发的新帧也误清掉，导致画面停在旧帧等待（用户实测
                    // seek 到近末尾后卡 7 秒）。seek 前旧帧由 seek_target 丢弃 + 渲染
                    // Drop 自然处理。
                    clock.reset_origin();
                }
                // seek 偏移由解码线程用"首个 post-seek 视频帧的实际 pts − 当时
                // 音频位置"设定（可为负），每帧都读（原子读，廉价）并应用。
                clock.set_audio_offset(offset_us);
                // 文件时长用于封顶音频主时钟（seek 到近末尾时音频下溢会虚高）。
                clock.set_duration(duration_us);

                // 预览帧（Preview seek 解出）或拖动中：直接显示，不走音频时钟调度
                // （拖动时 MuteAudio 停声卡，音频时钟冻结，正常帧会被 Wait/Drop
                // 卡住；预览帧位置与音频不匹配也会被 Drop）。拖动中所有帧直接显示，
                // 避免残留的正常帧用冻结的音频时钟 Wait 卡住画面。
                if !is_preview && !dragging_render.load(Ordering::Relaxed) {
                    match clock.schedule(pts) {
                        // 用 GPUI timer 精确等待。但**封顶等待时长**：若某帧被判 1 秒后
                        // 才显示，几乎必然是 seek 后残留的旧帧（pts 远大于新 offset）——
                        // 真等会卡住画面数秒、音频趁机超前（用户实测卡 7s）。直接丢弃。
                        Schedule::Wait(d) => {
                            if d.as_millis() > 1000 {
                                continue; // 旧帧/时钟错位，丢弃
                            }
                            cx.background_executor().timer(d).await;
                        }
                        Schedule::Now => {}
                        // 音频主时钟下落后太多：跳过这一帧，让画面追上声音，
                        // 不能反过来把已经播出去的音频拽慢。
                        Schedule::Drop { behind } => {
                            // 只有较大落后才打日志：小 behind 是音频主时钟下视频追赶的
                            // 正常行为，逐帧 warn 会刷屏。
                            if behind.as_millis() >= 500 {
                                warn!(
                                    behind_ms = behind.as_millis(),
                                    pts_ms = pts_us / 1000,
                                    offset_ms = offset_us / 1000,
                                    audio_ms = audio.as_ref().map(|c| c.position().as_millis()).unwrap_or_default(),
                                    "画面落后音频，丢帧追赶"
                                );
                            }
                            continue;
                        }
                        Schedule::Resynced { behind } => playback::log_resync(behind, pts),
                    }
                }

                this.update(cx, |this, cx| {
                    this.latest_frame = Some(render);
                    // 进度：首帧也确认总时长。
                    this.duration = Duration::from_micros(duration_us);
                    // 拖动中不覆盖 position，让进度条跟随鼠标。
                    if !this.dragging.load(Ordering::Relaxed) {
                        this.position = pts;
                    }
                    cx.notify();
                })
                .ok();

                if profiling {
                    stats_render.record_displayed();
                    // 音画漂移：在真正显示的这一刻，用音频时钟读数减帧 PTS。
                    if let Some(c) = audio.as_ref() {
                        let drift_us = c.position().as_micros() as i64 - pts_us as i64;
                        stats_render.record_av_sync(drift_us);
                    }
                }
            }
            this.update(cx, |_, cx| cx.emit(PlaybackEnded)).ok();
        });

        if profiling {
            Self::spawn_stats_reporter(stats, window, cx);
        }

        // 抢键盘焦点，让空格/方向键直接可用。
        window.focus(&focus_handle, cx);

        Self {
            latest_frame: None,
            current_rendered: None,
            previous_rendered: None,
            _render_task,
            cmd,
            duration: Duration::ZERO,
            position: Duration::ZERO,
            paused: false,
            dragging,
            focus_handle,
        }
    }

    /// 周期性上报播放统计，并直接给出"流畅/卡顿"的结论。
    fn spawn_stats_reporter(stats: Arc<ProfileStats>, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |_, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(STATS_WINDOW_SECS))
                    .await;
                let snap = stats.take_snapshot(STATS_WINDOW_SECS);

                // 注意：decoded 与 displayed 之差**不是**丢帧率——两者天然相差
                // 队列深度与统计窗口边界。真正的丢帧已不存在（解码侧会重试到
                // 送出为止），所以只报原始速率，不再算那个会骗人的 drop_rate：
                // 旧实现里 decoded 归零时 drop_rate 恒为 0%，正是它掩盖了真实丢帧。
                if snap.is_janky() {
                    warn!(
                        decoded_fps = snap.decoded_fps,
                        displayed_fps = snap.displayed_fps,
                        avg_interval_ms = snap.avg_interval_ms,
                        p99_interval_ms = snap.p99_interval_ms,
                        max_interval_ms = snap.max_interval_ms,
                        on_time_pct = snap.on_time_pct,
                        avg_decode_us = snap.avg_decode_us,
                        hist = ?snap.hist,
                        "检测到卡顿"
                    );
                } else {
                    info!(
                        decoded_fps = snap.decoded_fps,
                        displayed_fps = snap.displayed_fps,
                        avg_interval_ms = snap.avg_interval_ms,
                        p99_interval_ms = snap.p99_interval_ms,
                        max_interval_ms = snap.max_interval_ms,
                        on_time_pct = snap.on_time_pct,
                        avg_decode_us = snap.avg_decode_us,
                        "播放流畅"
                    );
                }

                // 音画同步：与卡顿正交——画面可能很流畅但整体偏音。
                // 无音轨时 av_sync_* 全为 0，这条日志依然打印但都是 0，无害；
                // 想看真实同步质量需在带音频设备的环境跑（headless 测不到）。
                if snap.is_av_out_of_sync() {
                    warn!(
                        mean_ms = snap.av_sync_mean_ms,
                        rms_ms = snap.av_sync_rms_ms,
                        max_lag_ms = snap.av_sync_max_lag_ms,
                        max_lead_ms = snap.av_sync_max_lead_ms,
                        bad_pct = snap.av_sync_bad_pct,
                        "音画失步"
                    );
                } else {
                    info!(
                        mean_ms = snap.av_sync_mean_ms,
                        rms_ms = snap.av_sync_rms_ms,
                        max_lag_ms = snap.av_sync_max_lag_ms,
                        max_lead_ms = snap.av_sync_max_lead_ms,
                        bad_pct = snap.av_sync_bad_pct,
                        "音画同步"
                    );
                }
            }
        })
        .detach();
    }
}

impl PlayerView {
    /// 发送暂停/恢复命令并镜像本地 paused 状态。
    fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        let cmd = if self.paused {
            playback::PlaybackCommand::Pause
        } else {
            playback::PlaybackCommand::Resume
        };
        let _ = self.cmd.unbounded_send(cmd);
    }

    /// 相对当前位置向后 seek（夹到 [0, duration]）。
    fn seek_backward(&mut self, delta: Duration) {
        self.seek_to(self.position.saturating_sub(delta));
    }

    /// 相对当前位置向前 seek（夹到 [0, duration]）。
    fn seek_forward(&mut self, delta: Duration) {
        self.seek_to(self.position.saturating_add(delta));
    }

    /// 跳到指定时间点（夹到 [0, duration]）。**正式 seek**（Commit）：
    /// 重建音频 + 重锚，进入正常播放。用于点击/键盘/松开。
    fn seek_to(&mut self, target: Duration) {
        let target = target.min(self.duration);
        self.position = target;
        let _ = self.cmd.unbounded_send(playback::PlaybackCommand::Seek(
            target,
            playback::SeekKind::Commit,
        ));
    }

    /// 拖动中的**预览 seek**（Preview）：只 seek 视频出预览帧，画面跟手，
    /// 不重建音频流。松开时才发 Commit 正式 seek。
    /// 同时更新本地 position，让进度条跟随鼠标。
    fn seek_preview(&mut self, target: Duration) {
        let target = target.min(self.duration);
        self.position = target;
        let _ = self.cmd.unbounded_send(playback::PlaybackCommand::Seek(
            target,
            playback::SeekKind::Preview,
        ));
    }

    /// 把窗口内 x 坐标映射到播放时间。
    ///
    /// 轨道相对窗口左右各缩进 [`PROGRESS_INSET`] 像素，所以换算要减去缩进、
    /// 再除以轨道宽（窗口宽 − 2×缩进），与 fill 的 `relative(比例)` 对齐。
    fn x_to_time(&self, x: gpui::Pixels, window: &mut Window) -> Option<Duration> {
        let bounds = window.bounds();
        let window_w = bounds.size.width;
        let track_w = window_w - px(PROGRESS_INSET * 2.0);
        if track_w == px(0.0) {
            return None;
        }
        // Pixels/Pixels → f32 比例。
        let frac = ((x - bounds.origin.x - px(PROGRESS_INSET)) / track_w).clamp(0.0, 1.0);
        Some(self.duration.mul_f32(frac))
    }

    /// 点击进度条：立即精确 seek 到点击位置（也作为拖动开始）。
    fn seek_click(&mut self, x: gpui::Pixels, window: &mut Window) {
        if let Some(target) = self.x_to_time(x, window) {
            self.seek_to(target);
        }
    }

    /// 拖动进度条移动（按下后移动 / 拖动中）。
    ///
    /// - `live_seek`：每次移动都发 Preview（seek 实测 ~10ms，够快），画面跟随
    ///   鼠标扫过的位置；覆盖合并保证解码线程只执行最新 Preview。
    /// - 否则只更新进度条视觉，不发给解码线程（松开时才跳）。
    fn seek_drag(&mut self, x: gpui::Pixels, window: &mut Window) {
        let Some(target) = self.x_to_time(x, window) else { return };
        // 进度条始终跟随鼠标。
        self.position = target.min(self.duration);
        if LIVE_SEEK {
            self.seek_preview(target);
        }
        // 非 live：position 已更新（进度条跟手），不发命令，松开才 seek。
    }

    /// 拖动结束（鼠标松开）：**总是**发 Commit 正式 seek（重建音频 + 重锚，
    /// 进入正常播放）。live 模式拖动中发的是 Preview，松开这里补最终 Commit。
    fn seek_release(&mut self, x: gpui::Pixels, window: &mut Window) {
        if let Some(target) = self.x_to_time(x, window) {
            self.seek_to(target);
        }
    }

    /// 处理按键。返回 true 表示已消费。
    fn on_key(&mut self, event: &KeyDownEvent) -> bool {
        match event.keystroke.key.as_str() {
            "space" => {
                self.toggle_pause();
                true
            }
            "left" => {
                self.seek_backward(SEEK_STEP);
                true
            }
            "right" => {
                self.seek_forward(SEEK_STEP);
                true
            }
            _ => false,
        }
    }
}

impl Render for PlayerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 双缓冲回收：把上一帧的纹理 drop 掉，防止 sprite atlas 无限增长。
        if let Some(current) = self.current_rendered.take() {
            if let Some(prev) = self.previous_rendered.take() {
                if prev.id != current.id {
                    let _ = window.drop_image(prev);
                }
            }
            self.previous_rendered = Some(current);
        }

        // 进度条已填充比例。
        let fill_pct = if self.duration.is_zero() {
            0.0
        } else {
            (self.position.as_secs_f64() / self.duration.as_secs_f64()).clamp(0.0, 1.0) as f32
        };
        let time_text = format!(
            "{:02}:{:02} / {:02}:{:02}",
            self.position.as_secs() / 60,
            self.position.as_secs() % 60,
            self.duration.as_secs() / 60,
            self.duration.as_secs() % 60,
        );

        let root = div()
            .id("player")
            .track_focus(&self.focus_handle)
            .key_context("player")
            .size_full()
            .relative()
            .on_key_down(cx.listener(|this, e, _, cx| {
                this.on_key(e);
                cx.notify();
            }));

        let Some(frame) = self.latest_frame.clone() else {
            return root.into_any_element();
        };
        self.current_rendered = Some(frame.clone());
        let image = gpui::img(frame).size_full();

        // 底部控制条：时间文本 + 进度条，两行都占满整窗宽度。
        // 进度条占满整行宽 → 点击映射到 `window.bounds()` 宽度即精确对齐进度条，
        // 不会因为右侧时间文本占据横向空间而把"满进度"错位到窗口最右。
        let bar = div()
            .id("progress")
            .absolute()
            .bottom_0()
            .left_0()
            .right_0()
            .pt(px(6.0))
            .pb(px(8.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .bg(rgba(0x00000066))
            // 点击命中区 = 整个控制条（含时间文本行），比 4px 轨道粗得多，
            // 不容易点到无效区域。x_to_time 用同一 PROGRESS_INSET 换算，
            // 横向映射仍精确对齐轨道（轨道左右各缩进 12px）。
            // 按下：立即精确 seek 到点击位置（Commit，正常跳转走 schedule）。
            // **不设 dragging**——点击不该算拖动，否则渲染循环会把 Commit 帧当
            // 预览直接显示（无 PTS 节流、不更新 position），跳转反而卡。
            .on_mouse_down(MouseButton::Left, cx.listener(|this, e: &MouseDownEvent, window, cx| {
                this.seek_click(e.position.x, window);
                cx.notify();
            }))
            // 拖动中（左键按住移动才叫拖动）：设 dragging，发 Preview（画面预览
            // 跟手，不重建音频）；进度条跟随鼠标。
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, window, cx| {
                if e.pressed_button == Some(MouseButton::Left) {
                    if !this.dragging.load(Ordering::Relaxed) {
                        this.dragging.store(true, Ordering::Relaxed);
                        // 拖动开始：静音（停声卡，避免声音卡顿/与画面抢资源），
                        // 且立即跳一次（Preview），否则画面停在按下位置。
                        let _ = this.cmd.unbounded_send(playback::PlaybackCommand::MuteAudio);
                        this.seek_drag(e.position.x, window);
                    } else {
                        this.seek_drag(e.position.x, window);
                    }
                    cx.notify();
                }
            }))
            // 松开：结束拖动；发 Commit（正式 seek，重建音频 + 重锚进入正常播放）。
            .on_mouse_up(MouseButton::Left, cx.listener(|this, e: &MouseUpEvent, window, cx| {
                if this.dragging.load(Ordering::Relaxed) {
                    this.dragging.store(false, Ordering::Relaxed);
                    this.seek_release(e.position.x, window);
                    cx.notify();
                }
            }))
            .child(
                // 时间文本行：占满整行宽，右对齐，不参与进度条布局。
                div()
                    .id("time")
                    .w_full()
                    .px(px(12.0))
                    .flex()
                    .justify_end()
                    .child(div().text_color(white()).child(time_text)),
            )
            // 进度条行：左右留内边距，让轨道不占满整窗宽。
            .child(
                div()
                    .id("bar_row")
                    .w_full()
                    .px(px(PROGRESS_INSET))
                    .child(
                        // 轨道：宽度 = 整行宽 − 2×内边距。fill 的 relative(比例)
                        // 相对轨道宽计算，100% 恰好填满，轨道/填充对齐。
                        div()
                            .id("bar")
                            .w_full()
                            .h(px(4.0))
                            .bg(rgba(0xffffff33))
                            .rounded_full()
                            .child(
                                div()
                                    .id("fill")
                                    .h_full()
                                    .w(relative(fill_pct))
                                    .bg(green())
                                    .rounded_full(),
                            ),
                    ),
            );

        let mut content = root.child(image).child(bar);

        // 暂停遮罩。
        if self.paused {
            content = content.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .text_color(white())
                            .text_2xl()
                            .child("⏸ 已暂停"),
                    ),
            );
        }

        content.into_any_element()
    }
}
