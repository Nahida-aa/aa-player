//! 无 GUI 的播放状态机（PlayerController）。
//!
//! 复用 `player-core`（FFmpeg）做解码，管理播放时钟、暂停、seek，并把最新帧
//! 转成 GPUI `RenderImage` 供渲染层消费。不依赖任何 UI。
//!
//! V2 加入**音频主时钟同步**：解码线程把音频采样直接推给声卡（cpal，
//! 经 player-core 的 `AudioOutput`），渲染侧用音频播放进度调度视频帧，
//! 音画同步。无音轨时退墙钟。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures::channel::mpsc;
use ffmpeg_next::Error as FfmpegError;
use gpui::RenderImage;
use image::{Frame, RgbaImage};
use player_core::{
    AudioClock, AudioOutput, DecodedFrame, FfmpegSource, MediaEvent, MediaSource, SeekCancelled,
};

use crate::stats::ProfileStats;

/// 帧队列容量。故意浅：渲染侧按 PTS 等待，队列几乎总是满的（背压）。
const FRAME_QUEUE_CAP: usize = 3;
/// 投递队列满时的退避间隔。
const SEND_BACKOFF: Duration = Duration::from_millis(2);
/// 声卡队列里最多缓冲多少音频。超过就先别解，形成背压。
const AUDIO_BUFFER: Duration = Duration::from_millis(400);
/// seek 重建声卡流后，至少缓冲这么多音频才允许 `start()`。
const AUDIO_START_MIN: Duration = Duration::from_millis(80);
/// 音频缓冲满时的退避间隔。
const AUDIO_BACKOFF: Duration = Duration::from_millis(5);
/// seek 时离文件末尾保留的安全余量（微秒），避 ffmpeg 末尾阻塞。
const SEEK_END_MARGIN_US: u64 = 1_000_000;

/// 发往渲染侧的一帧：图像、显示时刻（PTS 微秒）、文件总时长（微秒）、是否预览帧。
/// `None` 表示流结束（EOF 或错误）。
///
/// `preview` = Preview seek 解出（拖动中），渲染侧应**直接显示**，不走音频时钟
/// 同步（拖动时声卡被静音，音频时钟冻结，正常调度会卡住画面）。
pub type FrameMsg = Option<(Arc<RenderImage>, u64, u64, bool)>;

/// 播放器控制命令（UI → 解码线程）。unbounded：命令不能因背压丢失。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerCommand {
    Pause,
    Resume,
    /// 拖动中：静音 + seek 视频出预览帧（不重建音频流）。
    SeekPreview(Duration),
    /// 松开/点击：完整 seek（重建音频 + 重锚）。
    SeekCommit(Duration),
}

/// 音频主时钟的交接点。
///
/// 声卡只能在解码线程里打开（cpal `Stream` 不是 `Send`），但渲染 task 需要读
/// 它的进度。用 `Mutex<Option<AudioClock>>` + `generation` 把这个空档接上：
/// - `attach` 在解码线程确认有音轨 / seek 重建流后调用，`generation++`。
/// - 渲染侧 `get_with_generation` 读到 (generation, seek_offset, clock)，
///   换代才重建时钟（保留墙钟 origin），seek 偏移用于对齐首帧。
pub struct AudioClockSource {
    clock: std::sync::Mutex<Option<AudioClock>>,
    generation: AtomicU64,
    /// seek 锚定偏移（有符号微秒）= 首帧实际 pts − 当时音频位置。
    seek_offset_us: AtomicI64,
}

impl Default for AudioClockSource {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioClockSource {
    pub fn new() -> Self {
        Self {
            clock: std::sync::Mutex::new(None),
            generation: AtomicU64::new(0),
            seek_offset_us: AtomicI64::new(0),
        }
    }

    /// 解码线程确认有音轨后（或 seek 重建流后）把时钟交出来。
    pub fn attach(&self, clock: AudioClock) {
        *self.clock.lock().unwrap_or_else(|e| e.into_inner()) = Some(clock);
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// 记录 seek 锚定偏移（有符号微秒）。
    pub fn set_seek_offset(&self, us: i64) {
        self.seek_offset_us.store(us, Ordering::Relaxed);
    }

    /// 取当前音频时钟、代次与 seek 偏移；尚未就位（或无音轨）时时钟为 `None`。
    pub fn get_with_generation(&self) -> (u64, i64, Option<AudioClock>) {
        let generation = self.generation.load(Ordering::Relaxed);
        let offset = self.seek_offset_us.load(Ordering::Relaxed);
        let clock = self.clock.lock().unwrap_or_else(|e| e.into_inner()).clone();
        (generation, offset, clock)
    }
}

/// 解码帧 → GPUI RenderImage（BGRA，与 ffmpeg Pixel::BGRA 一致）。
fn decoded_to_render_image(frame: &DecodedFrame) -> Arc<RenderImage> {
    let tight = frame.to_tight_bgra();
    let img =
        RgbaImage::from_raw(frame.width, frame.height, tight).expect("frame byte length mismatch");
    Arc::new(RenderImage::new(smallvec::SmallVec::from_elem(
        Frame::new(img),
        1,
    )))
}

/// 播放器控制器：无 GUI 的播放状态机。
///
/// 由调用方包成 `Entity<PlayerController>`，UI 侧读它的状态并驱动命令。
pub struct PlayerController {
    /// 解码线程推来的最新一帧。
    latest_frame: Option<Arc<RenderImage>>,
    /// 控制命令通道（发给解码线程）。
    cmd: mpsc::UnboundedSender<PlayerCommand>,
    /// 文件总时长（首帧到达后确定）。
    duration: Duration,
    /// 当前播放位置。
    position: Duration,
    /// 是否暂停。
    paused: bool,
    /// 是否正在拖动进度条。
    dragging: bool,
    /// 取消标志：发新 Preview 前置 true，中断解码线程里进行中的旧 seek。
    cancel_seek: Arc<AtomicBool>,
    /// 音频主时钟交接点（供渲染侧调度视频）。
    pub clock: Arc<AudioClockSource>,
    /// 性能统计（解码 fps/耗时），仅 debug 时启用。
    pub stats: Arc<ProfileStats>,
    /// 关窗时停止解码线程。
    _running: Arc<AtomicBool>,
}

impl PlayerController {
    /// 打开视频并启动解码线程。
    ///
    /// 返回控制器和一个帧接收端：渲染循环（GPUI async task）持有接收端，
    /// 消费解码线程投递的帧，调 [`consume_frame`](Self::consume_frame) 更新状态。
    pub fn open(path: PathBuf) -> (Self, mpsc::Receiver<FrameMsg>) {
        let (tx, rx) = mpsc::channel(FRAME_QUEUE_CAP);
        let (cmd, cmd_rx) = mpsc::unbounded();
        let running = Arc::new(AtomicBool::new(true));
        let cancel_seek = Arc::new(AtomicBool::new(false));
        let clock = Arc::new(AudioClockSource::new());
        let stats = Arc::new(ProfileStats::default());

        spawn_decode_thread(
            path,
            tx,
            running.clone(),
            cmd_rx,
            cancel_seek.clone(),
            clock.clone(),
            stats.clone(),
        );

        (
            Self {
                latest_frame: None,
                cmd,
                duration: Duration::ZERO,
                position: Duration::ZERO,
                paused: false,
                dragging: false,
                cancel_seek,
                clock,
                stats,
                _running: running,
            },
            rx,
        )
    }

    // ----- 查询 -----

    pub fn latest_frame(&self) -> Option<Arc<RenderImage>> {
        self.latest_frame.clone()
    }

    pub fn duration(&self) -> Duration {
        self.duration
    }

    pub fn position(&self) -> Duration {
        self.position
    }

    pub fn paused(&self) -> bool {
        self.paused
    }

    pub fn is_dragging(&self) -> bool {
        self.dragging
    }

    // ----- 控制 -----

    pub fn play(&mut self) {
        if self.paused {
            self.paused = false;
            let _ = self.cmd.unbounded_send(PlayerCommand::Resume);
        }
    }

    pub fn pause(&mut self) {
        if !self.paused {
            self.paused = true;
            let _ = self.cmd.unbounded_send(PlayerCommand::Pause);
        }
    }

    pub fn toggle(&mut self) {
        if self.paused {
            self.play();
        } else {
            self.pause();
        }
    }

    /// 跳到指定时刻（正式，松开/点击）。同步更新本地 position。
    pub fn seek_to(&mut self, target: Duration) {
        let target = target.min(self.duration);
        self.position = target;
        self.dragging = false;
        let _ = self.cmd.unbounded_send(PlayerCommand::SeekCommit(target));
    }

    /// 拖动中预览 seek：置取消标志中断旧 seek，本地 position 跟手。
    pub fn seek_preview(&mut self, target: Duration) {
        let target = target.min(self.duration);
        self.position = target;
        self.dragging = true;
        self.cancel_seek.store(true, Ordering::Relaxed);
        let _ = self.cmd.unbounded_send(PlayerCommand::SeekPreview(target));
    }

    /// 结束拖动：发正式 seek，清拖动态。
    pub fn seek_release(&mut self, target: Duration) {
        self.dragging = false;
        self.seek_to(target);
    }

    /// 渲染循环消费一帧：更新 position/duration/latest_frame。
    pub fn consume_frame(&mut self, item: FrameMsg, cx: &mut gpui::Context<Self>) {
        let Some((render, pts_us, duration_us, _preview)) = item else {
            // EOF：进度条拉满，但解码线程仍活着等 seek 命令。
            if self.duration != Duration::ZERO {
                self.position = self.duration;
            }
            cx.notify();
            return;
        };
        self.duration = Duration::from_micros(duration_us);
        if !self.dragging {
            self.position = Duration::from_micros(pts_us);
        }
        self.latest_frame = Some(render);
        cx.notify();
    }

    /// 取消标志的 clone（供 seek 抢占；解码线程持有另一份）。
    pub fn cancel_handle(&self) -> Arc<AtomicBool> {
        self.cancel_seek.clone()
    }
}

/// 解码线程：同步拉帧/推音频，经有界通道投递视频，响应暂停/seek 命令。
#[allow(clippy::too_many_arguments)]
fn spawn_decode_thread(
    path: PathBuf,
    mut tx: mpsc::Sender<FrameMsg>,
    running: Arc<AtomicBool>,
    mut cmd_rx: mpsc::UnboundedReceiver<PlayerCommand>,
    cancel: Arc<AtomicBool>,
    clock_source: Arc<AudioClockSource>,
    stats: Arc<ProfileStats>,
) {
    std::thread::spawn(move || {
        // 声卡打不开不该让整个播放失败——没有声音总比放不了强。
        let audio = match AudioOutput::new() {
            Ok(o) => Some(o),
            Err(e) => {
                tracing::warn!(?e, "打开音频设备失败，将以无声模式播放");
                None
            }
        };
        let audio_format = audio.as_ref().map(|a| a.format());

        // 可中断 seek 打开（带音频解码）。
        let mut source = match FfmpegSource::open_with_interrupt(&path, audio_format, cancel) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(?e, "打开媒体失败");
                return;
            }
        };

        // 只有确实有音轨时才让时钟切到音频主时钟。
        let mut audio = audio.filter(|_| source.audio_info().is_some());
        if let Some(a) = audio.as_ref() {
            clock_source.attach(a.clock());
            tracing::info!("音频主时钟已启用");
        } else {
            tracing::info!("无音轨，使用墙钟");
        }

        let duration_us = source.video_info().duration.as_micros() as u64;
        let mut paused = false;
        // 拖动预览模式：解出的帧标记 preview，渲染侧直接显示。
        let mut previewing = false;
        // seek 后丢弃目标前帧。
        let mut video_seek_target: Option<Duration> = None;
        // seek 后首帧锚定偏移。
        let mut pending_anchor = false;
        // seek 后音频是否已满足起播条件。
        let mut start_audio = false;
        // 待发帧（seek 后避免发 seek 前帧，先在下一轮发）。
        let mut next_frame: Option<(Arc<RenderImage>, u64, bool)> = None;
        // 已放完（EOF），只等 seek 命令。
        let mut finished = false;

        loop {
            if !running.load(Ordering::Relaxed) {
                return;
            }

            // 1) 处理命令。**Seek 覆盖合并**：拖动会积压多个 seek，只保留最新一个
            // 再执行（每个 ffmpeg seek 都慢，逐个执行会让解码线程全耗在 seek 上、
            // 画面卡住）。Preview 只留最新；Commit 是最终位置，后到覆盖且优先。
            let mut latest_seek: Option<(Duration, PlayerCommand)> = None;
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    PlayerCommand::Pause => {
                        paused = true;
                        if let Some(a) = audio.as_ref() {
                            a.pause();
                        }
                    }
                    PlayerCommand::Resume => {
                        paused = false;
                        if let Some(a) = audio.as_ref() {
                            a.resume();
                        }
                    }
                    PlayerCommand::SeekPreview(_) | PlayerCommand::SeekCommit(_) => {
                        latest_seek = Some((match cmd {
                            PlayerCommand::SeekPreview(t) => t,
                            PlayerCommand::SeekCommit(t) => t,
                            _ => unreachable!(),
                        }, cmd));
                    }
                }
            }

            // 执行合并后的最新 seek（有则优先于暂停态处理）。
            if let Some((target, cmd)) = latest_seek {
                let t = seek_clamped(target, duration_us);
                match cmd {
                    PlayerCommand::SeekPreview(_) => {
                        // 拖动中预览：静音（停声卡）+ seek 视频出预览帧，不重建音频流。
                        if let Some(a) = audio.as_ref() {
                            a.pause();
                            a.clear();
                        }
                        if let Err(e) = source.seek(t) {
                            tracing::debug!(?e, "Preview seek 被打断/失败，重试最新命令");
                        }
                        previewing = true;
                        video_seek_target = None;
                    }
                    PlayerCommand::SeekCommit(_) => {
                        // 完整 seek：重建声卡流 + 重锚，进入正常播放。
                        if let Err(e) = source.seek(t) {
                            tracing::debug!(?e, "Commit seek 失败");
                        }
                        previewing = false;
                        pending_anchor = true;
                        start_audio = true;
                        video_seek_target = Some(t);
                        seek_rebuild_audio(&mut audio, &clock_source);
                        paused = false;
                    }
                    _ => unreachable!(),
                }
                continue; // 刚 seek 过，回循环顶部读下一批命令
            }

            if paused {
                std::thread::sleep(Duration::from_millis(8));
                continue;
            }
            if finished {
                std::thread::sleep(Duration::from_millis(8));
                continue;
            }

            // 2) 有暂存帧先发（投递会背压）。
            if let Some((render, pts_us, preview)) = next_frame.take() {
                if !send_blocking(&mut tx, (render, pts_us, duration_us, preview), &running) {
                    return;
                }
                // 首个 post-seek 视频帧已送出；若音频缓冲也够，就开播。
                try_start_audio(&audio, &mut start_audio);
                continue;
            }

            // 3) 拉一个单元。
            let t0 = Instant::now();
            match source.next_event() {
                Ok(Some(MediaEvent::Video(f))) => {
                    // seek 后丢弃目标前帧。
                    if let Some(target) = video_seek_target {
                        if f.pts < target {
                            continue;
                        }
                        video_seek_target = None;
                    }
                    // seek 后首帧：用实际 pts − 当时音频位置作锚定偏移。
                    if pending_anchor {
                        pending_anchor = false;
                        let audio_pos_us = audio
                            .as_ref()
                            .map(|a| a.position().as_micros() as i64)
                            .unwrap_or(0);
                        let anchor = f.pts.as_micros() as i64 - audio_pos_us;
                        clock_source.set_seek_offset(anchor);
                    }
                    let render = decoded_to_render_image(&f);
                    // 解码+像素转换总耗时（微秒）。
                    stats.record_decoded(t0.elapsed().as_micros() as u64);
                    let pts_us = f.pts.as_micros() as u64;
                    next_frame = Some((render, pts_us, previewing));
                }
                Ok(Some(MediaEvent::Audio(chunk))) => {
                    if let Some(a) = audio.as_ref() {
                        // 背压：音频缓冲够深就等，别把整轨解进内存。
                        // seek 后音频是暂停态或拖动预览，队列不被消费，此时不背压。
                        if !start_audio && !previewing {
                            while running.load(Ordering::Relaxed)
                                && a.queued_duration() > AUDIO_BUFFER
                            {
                                std::thread::sleep(AUDIO_BACKOFF);
                            }
                        }
                        a.push_samples(&chunk.samples);
                        if a.take_underrun() {
                            tracing::warn!("音频欠载：解码跟不上声卡消费");
                        }
                    }
                }
                Ok(None) => {
                    // EOF：等声卡缓冲播完，然后通知渲染侧，但线程不退出（可再 seek）。
                    if let Some(a) = audio.as_ref() {
                        drain_audio(a, &running);
                    }
                    let _ = tx.try_send(None);
                    finished = true;
                }
                Err(e) => {
                    // 被更新的 Preview 抢占取消：interrupt 回调也会打断普通读帧。
                    if e.root_cause().downcast_ref::<SeekCancelled>().is_some() {
                        tracing::debug!("next_event 被抢占取消，重读命令");
                        continue;
                    }
                    // 单个坏帧/坏包（如 NAL 损坏）：可恢复，跳过继续——解复用器/
                    // 解码器能越过坏帧重新同步到下一个关键帧，不应终止整个播放。
                    if matches!(
                        e.root_cause().downcast_ref::<FfmpegError>(),
                        Some(FfmpegError::InvalidData)
                    ) {
                        tracing::debug!(?e, "坏帧跳过");
                        continue;
                    }
                    tracing::error!(?e, "解码失败");
                    let _ = tx.try_send(None);
                    return;
                }
            }
        }
    });
}

/// seek 目标夹到 [0, duration-1s]，避 ffmpeg 末尾阻塞。
fn seek_clamped(t: Duration, duration_us: u64) -> Duration {
    let margin = SEEK_END_MARGIN_US;
    let max_us = duration_us.saturating_sub(margin);
    let us = (t.as_micros() as u64).min(max_us);
    Duration::from_micros(us)
}

/// 音频缓冲满足条件（≥ AUDIO_START_MIN）就 `start()`。
fn try_start_audio(audio: &Option<AudioOutput>, start_audio: &mut bool) {
    if !*start_audio {
        return;
    }
    let Some(a) = audio.as_ref() else { return };
    if a.queued_duration() >= AUDIO_START_MIN {
        *start_audio = false;
        a.start();
    }
}

/// 声卡硬件时钟不能倒带：seek 后重建流（计数器归零、先不启动），
/// 等缓冲填够再 start，再把新时钟句柄交回渲染侧。
fn seek_rebuild_audio(audio: &mut Option<AudioOutput>, clock_source: &Arc<AudioClockSource>) {
    *audio = match AudioOutput::new_paused() {
        Ok(o) => Some(o),
        Err(e) => {
            tracing::warn!(?e, "seek 后重开音频设备失败，将以无声模式播放");
            None
        }
    };
    if let Some(a) = audio.as_ref() {
        clock_source.attach(a.clock());
    }
}

/// 等声卡把缓冲里剩下的采样播完（结尾不掐音）。
fn drain_audio(audio: &AudioOutput, running: &AtomicBool) {
    if audio.is_paused() {
        audio.start();
    }
    while running.load(Ordering::Relaxed) && audio.queued_frames() > 0 {
        std::thread::sleep(AUDIO_BACKOFF);
    }
}

/// 把一帧送进队列，满则退避重试直到成功。返回 false 表示应结束线程。
fn send_blocking(
    tx: &mut mpsc::Sender<FrameMsg>,
    item: (Arc<RenderImage>, u64, u64, bool),
    running: &AtomicBool,
) -> bool {
    let mut pending = Some(item);
    while running.load(Ordering::Relaxed) {
        match tx.try_send(pending) {
            Ok(()) => return true,
            Err(e) if e.is_full() => {
                pending = e.into_inner();
                std::thread::sleep(SEND_BACKOFF);
            }
            Err(_) => return false, // 接收端已关闭
        }
    }
    false
}
