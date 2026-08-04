//! 播放器视图：把解码帧上屏。
//!
//! 渲染模式照抄 zed 的 `remote_video_track_view.rs`：双缓冲 +
//! `drop_image` 回收纹理，避免 sprite atlas 泄漏。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::StreamExt;
use gpui::{Context, EventEmitter, IntoElement, Render, RenderImage, Styled, Task, Window, div};
use tracing::{info, warn};

use crate::playback::{self, PlaybackClock, Schedule};
use crate::stats::ProfileStats;

/// 统计上报间隔（秒）。
const STATS_WINDOW_SECS: u64 = 2;

/// 播放器视图：持有一帧最新的解码画面。
pub struct PlayerView {
    /// 解码线程推来、待渲染的最新帧。
    latest_frame: Option<Arc<RenderImage>>,
    /// 双缓冲：当前已渲染的帧，用于下一帧渲染时回收旧纹理。
    current_rendered: Option<Arc<RenderImage>>,
    previous_rendered: Option<Arc<RenderImage>>,
    /// 后台渲染任务句柄（持有以保活）。
    _render_task: Task<()>,
}

/// 解码线程结束（EOF）时发出，便于 UI 提示。
#[derive(Debug)]
pub struct PlaybackEnded;

impl EventEmitter<PlaybackEnded> for PlayerView {}

impl PlayerView {
    pub fn new(path: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (tx, mut rx) = playback::frame_channel();
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
        );

        // 渲染 task：异步收帧，按 PTS 精确节流显示，绝不阻塞 executor。
        let stats_render = stats.clone();
        let _render_task = cx.spawn_in(window, async move |this, cx| {
            // 时钟初始为墙钟；解码线程把音频主时钟交上来后再切换。
            // 文件无音轨时它会一直是墙钟，那也是正确的行为。
            let mut clock = PlaybackClock::new();
            // 音频主时钟就位后保存句柄，用于算音画漂移。
            // `Option` 而非每次 `get()`：避免每帧都重新探测且能稳定读数。
            let mut audio_clock: Option<player_core::AudioClock> = None;
            while let Some(item) = rx.next().await {
                let Some((render, pts_us)) = item else {
                    break; // EOF
                };
                let pts = Duration::from_micros(pts_us);

                // 时钟可能在本轮等待期间就位，每帧重新探测一次。
                if let Some(c) = clock_source.get()
                    && audio_clock.is_none()
                {
                    audio_clock = Some(c.clone());
                    clock = PlaybackClock::with_audio(c.clone());
                }

                match clock.schedule(pts) {
                    // 用 GPUI timer 精确等待（对齐事件循环，
                    // 比 worker 线程的 thread::sleep 更平滑）。
                    Schedule::Wait(d) => cx.background_executor().timer(d).await,
                    Schedule::Now => {}
                    // 音频主时钟下落后太多：跳过这一帧，让画面追上声音，
                    // 不能反过来把已经播出去的音频拽慢。
                    Schedule::Drop { behind } => {
                        warn!(behind_ms = behind.as_millis(), "画面落后音频，丢帧追赶");
                        continue;
                    }
                    Schedule::Resynced { behind } => playback::log_resync(behind, pts),
                }

                this.update(cx, |this, cx| {
                    this.latest_frame = Some(render);
                    cx.notify();
                })
                .ok();

                if profiling {
                    stats_render.record_displayed();
                    // 音画漂移：在真正显示的这一刻，用音频时钟读数减帧 PTS。
                    // 丢帧（Drop）不计入——主动丢帧是追赶手段，不是同步误差。
                    if let Some(c) = audio_clock.as_ref() {
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

        Self {
            latest_frame: None,
            current_rendered: None,
            previous_rendered: None,
            _render_task,
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

impl Render for PlayerView {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // 双缓冲回收：把上一帧的纹理 drop 掉，防止 sprite atlas 无限增长。
        if let Some(current) = self.current_rendered.take() {
            if let Some(prev) = self.previous_rendered.take() {
                if prev.id != current.id {
                    let _ = window.drop_image(prev);
                }
            }
            self.previous_rendered = Some(current);
        }

        let Some(frame) = self.latest_frame.clone() else {
            return div().size_full().into_any_element();
        };
        self.current_rendered = Some(frame.clone());

        gpui::img(frame).size_full().into_any_element()
    }
}
