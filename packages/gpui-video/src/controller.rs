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
///
/// 末尾 `u64` = **seek 代次**：该帧是在哪一次 seek 之后解码投递的。用于渲染侧
/// 丢弃 seek 前已投递进通道的在途旧帧（它们会覆盖预测的 position，造成进度条/
/// thumb 闪回），同时不影响 seek 后的正常帧。
pub type FrameMsg = Option<(Arc<RenderImage>, u64, u64, bool, u64)>;

/// 播放器控制命令（UI → 解码线程）。unbounded：命令不能因背压丢失。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlayerCommand {
    Pause,
    Resume,
    /// 拖动开始：静音（停声卡 + 清队列），与 Preview 解耦，只发一次。
    MuteAudio,
    /// 拖动中：seek 视频出预览帧（不重建音频流、不再静音）。带 seek 代次。
    SeekPreview(Duration, u64),
    /// 松开/点击：完整 seek（重建音频 + 重锚）。带 seek 代次。
    SeekCommit(Duration, u64),
    /// 设置持久静音（作用于音量增益，非拖动临时静音）。
    SetMuted(bool),
    /// 设置播放速度倍率（作用于音频重采样输出率，视频随音频主时钟同步）。
    SetSpeed(f64),
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
    /// 是否静音（持久静音，作用于音量增益，与拖动临时静音正交）。
    muted: bool,
    /// 「更多」菜单是否展开（UI 状态，供控制条条件渲染浮层）。
    menu_open: bool,
    /// 播放速度倍率（1.0=原速）。经 SetSpeed 命令下发到解码线程改音频重采样率。
    speed: f64,
    /// 「info」信息面板是否展开（UI 状态，供控制条条件渲染浮层）。
    info_open: bool,
    /// 最近一次 seek 的代次（每次 seek_preview/seek_to 自增）。帧携带自己所属
    /// 的 seek 代次，`consume_frame` 据此丢弃 seek 前在途的旧帧（不覆盖 position）。
    seek_gen: u64,
    /// 取消标志：发新 Preview 前置 true，中断解码线程里进行中的旧 seek。
    cancel_seek: Arc<AtomicBool>,
    /// 音频主时钟交接点（供渲染侧调度视频）。
    pub clock: Arc<AudioClockSource>,
    /// 性能统计（解码 fps/耗时），仅 debug 时启用。
    pub stats: Arc<ProfileStats>,
    /// 视频原始分辨率 (width, height)，解码线程打开后填入。供组件按视频比例定尺寸。
    video_size: Arc<std::sync::Mutex<(u32, u32)>>,
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
        let video_size = Arc::new(std::sync::Mutex::new((0, 0)));

        spawn_decode_thread(
            path,
            tx,
            running.clone(),
            cmd_rx,
            cancel_seek.clone(),
            clock.clone(),
            stats.clone(),
            video_size.clone(),
        );

        (
            Self {
                latest_frame: None,
                cmd,
                duration: Duration::ZERO,
                position: Duration::ZERO,
                paused: false,
                dragging: false,
                muted: false,
                menu_open: false,
                speed: 1.0,
                info_open: false,
                seek_gen: 0,
                cancel_seek,
                clock,
                stats,
                video_size,
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

    /// 视频原始分辨率 (width, height)。尚未打开（解码线程未填入）时为 (0,0)。
    pub fn video_size(&self) -> (u32, u32) {
        *self.video_size.lock().unwrap_or_else(|e| e.into_inner())
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

    /// 是否静音（持久静音）。
    pub fn is_muted(&self) -> bool {
        self.muted
    }

    /// 切换静音（持久）。同时下发命令让解码线程调音量增益。
    pub fn toggle_mute(&mut self) {
        self.set_muted(!self.muted);
    }

    /// 设置静音（持久）。`muted=true` 增益置 0，否则恢复 1。
    pub fn set_muted(&mut self, muted: bool) {
        if self.muted == muted {
            return;
        }
        self.muted = muted;
        let _ = self.cmd.unbounded_send(PlayerCommand::SetMuted(muted));
    }

    /// 「更多」菜单是否展开。
    pub fn is_menu_open(&self) -> bool {
        self.menu_open
    }

    /// 切换「更多」菜单展开状态。
    pub fn toggle_menu(&mut self) {
        self.menu_open = !self.menu_open;
    }

    /// 关闭「更多」菜单（点击外部时调用）。
    pub fn close_menu(&mut self) {
        self.menu_open = false;
    }

    /// 播放速度档位（点击倍速菜单项时循环切换）。
    pub const SPEED_STEPS: &'static [f64] = &[1.0, 1.25, 1.5, 2.0, 0.5];

    /// 当前播放速度倍率。
    pub fn speed(&self) -> f64 {
        self.speed
    }

    /// 设置播放速度倍率（clamp 到合理范围后下发解码线程）。
    pub fn set_speed(&mut self, speed: f64) {
        let speed = speed.clamp(0.25, 4.0);
        if (speed - self.speed).abs() < f64::EPSILON {
            return;
        }
        self.speed = speed;
        let _ = self.cmd.unbounded_send(PlayerCommand::SetSpeed(speed));
    }

    /// 循环切换到下一个速度档位（点击「倍速」菜单项时调用）。
    pub fn cycle_speed(&mut self) {
        let idx = Self::SPEED_STEPS
            .iter()
            .position(|&s| (s - self.speed).abs() < f64::EPSILON)
            .unwrap_or(0);
        let next = Self::SPEED_STEPS[(idx + 1) % Self::SPEED_STEPS.len()];
        self.set_speed(next);
    }

    /// 「info」信息面板是否展开。
    pub fn is_info_open(&self) -> bool {
        self.info_open
    }

    /// 切换「info」信息面板（点更多菜单里的 info 项时调用）。
    pub fn toggle_info(&mut self) {
        self.info_open = !self.info_open;
    }

    /// 关闭「info」信息面板（点击外部时调用）。
    pub fn close_info(&mut self) {
        self.info_open = false;
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

    /// 跳到指定时刻（正式，松开/点击）。同步更新本地 position（预测）。
    ///
    /// position 在这里**立即预测成 target**：seek 有几十毫秒固有延迟（ffmpeg seek +
    /// 重建声卡流），若等真实解码帧慢慢爬过来，进度条/画面会在 seek 期间停在旧值，
    /// 看起来"卡住一下"。先预测让进度条立刻跳到目标（反馈跟手）。
    ///
    /// 预测的 position 被 seek 前投递进帧通道的旧帧覆盖（thumb 闪回）的问题，由
    /// 渲染循环在 seek 时（检测到音频时钟换代）丢弃在途旧帧来兜底。
    pub fn seek_to(&mut self, target: Duration) {
        let target = target.min(self.duration);
        self.position = target;
        self.dragging = false;
        // 每发一次正式 seek 就推进 seek 代次：seek 前在途的旧帧（代次更小）会在
        // consume_frame 被丢弃，不覆盖这里预测的 position（避免进度条/thumb 闪回）。
        self.seek_gen += 1;
        let _ = self.cmd.unbounded_send(PlayerCommand::SeekCommit(target, self.seek_gen));
    }

    /// 拖动开始：静音（停声卡 + 清队列）。与 Preview 解耦，拖动开始时发一次。
    pub fn mute_audio(&mut self) {
        let _ = self.cmd.unbounded_send(PlayerCommand::MuteAudio);
    }

    /// 拖动中预览 seek：置取消标志中断旧 seek，本地 position 跟手。
    /// 不再静音（静音由拖动开始的 [`mute_audio`](Self::mute_audio) 负责）。
    pub fn seek_preview(&mut self, target: Duration) {
        let target = target.min(self.duration);
        self.position = target;
        self.dragging = true;
        self.cancel_seek.store(true, Ordering::Relaxed);
        self.seek_gen += 1;
        let _ = self
            .cmd
            .unbounded_send(PlayerCommand::SeekPreview(target, self.seek_gen));
    }

    /// 结束拖动：发正式 seek，清拖动态。
    pub fn seek_release(&mut self, target: Duration) {
        self.dragging = false;
        self.seek_to(target);
    }

    /// 渲染循环消费一帧：更新 position/duration/latest_frame。
    pub fn consume_frame(&mut self, item: FrameMsg, cx: &mut gpui::Context<Self>) {
        let Some((render, pts_us, duration_us, _preview, frame_gen)) = item else {
            // EOF：进度条拉满，但解码线程仍活着等 seek 命令。
            if self.duration != Duration::ZERO {
                self.position = self.duration;
            }
            cx.notify();
            return;
        };
        self.duration = Duration::from_micros(duration_us);
        // seek 后在途的旧帧（所属 seek 代次 < 当前代次）会先于真实目标帧到达。
        // 若用它们的 pts 更新 position，会把 `seek_to` 预测的目标覆盖回 seek 前
        // 的位置 —— 连续按方向键时 thumb 闪回"原点"。丢弃这些旧帧的 position。
        // （画面仍可显示——渲染循环已用时钟丢弃了大多数旧帧；这里保证 position
        //   不被旧帧污染，而 `latest_frame` 照常更新。）
        let stale = frame_gen < self.seek_gen;
        if !self.dragging && !stale {
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
    video_size: Arc<std::sync::Mutex<(u32, u32)>>,
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
        // 记录视频原始分辨率，供组件按视频比例定尺寸。
        let vinfo = source.video_info();
        *video_size.lock().unwrap_or_else(|e| e.into_inner()) = (vinfo.width, vinfo.height);
        let mut paused = false;
        // 拖动预览模式：解出的帧标记 preview，渲染侧直接显示。
        let mut previewing = false;
        // seek 后丢弃目标前帧。
        let mut video_seek_target: Option<Duration> = None;
        // seek 后丢弃目标前音频（避免旧位置声音/时钟超前）。
        let mut audio_seek_target: Option<Duration> = None;
        // seek 后首帧锚定偏移。
        let mut pending_anchor = false;
        // seek 后音频是否已满足起播条件。
        let mut start_audio = false;
        // 待发帧（seek 后避免发 seek 前帧，先在下一轮发）。
        let mut next_frame: Option<(Arc<RenderImage>, u64, bool, u64)> = None;
        // 已放完（EOF），只等 seek 命令。
        let mut finished = false;
        // 暂停中 scrub（拖动/跳转）临时允许解码：解出目标帧显示画面，
        // 但保持暂停（不 start 音频、不推进播放）。
        let mut scrub_paused = false;
        // 拖动预览「定格」：preview 模式下解出目标帧后**停住**，不再继续往解码
        // （否则按住 thumb 不松手时，预览会以解码速度一帧帧快进 —— 用户实测的
        // "点 thumb 不滑动画面自动快播"）。等到下一个命令（新的 Preview 或 Commit）
        // 才解除定格、seek 到新目标。
        let mut preview_stall = false;
        // 最近一次执行 seek 的代次；投递的每一帧都打上它，供渲染侧丢弃 seek 前
        // 在途的旧帧（代次更小）——它们会覆盖预测的 position 造成进度条闪回。
        let mut current_gen: u64 = 0;

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
                    PlayerCommand::MuteAudio => {
                        // 拖动开始：静音（停声卡 + 清队列），不设 paused（解码继续）。
                        if let Some(a) = audio.as_ref() {
                            a.pause();
                            a.clear();
                        }
                    }
                    PlayerCommand::SetMuted(m) => {
                        // 持久静音：调音量增益（0=静音），声卡照常跑。
                        // 与拖动的临时 pause 正交，互不干扰。
                        if let Some(a) = audio.as_ref() {
                            a.set_volume(if m { 0.0 } else { 1.0 });
                        }
                    }
                    PlayerCommand::SetSpeed(s) => {
                        // 倍速：转发到媒体源，改音频重采样输出率；
                        // 音频主时钟仍按设备率读数，视频帧调度随之同步。
                        source.set_speed(s);
                    }
                    PlayerCommand::SeekPreview(_, _) | PlayerCommand::SeekCommit(_, _) => {
                        latest_seek = Some((match cmd {
                            PlayerCommand::SeekPreview(t, _) => t,
                            PlayerCommand::SeekCommit(t, _) => t,
                            _ => unreachable!(),
                        }, cmd));
                    }
                }
            }

            // 执行合并后的最新 seek（有则优先于暂停态处理）。
            if let Some((target, cmd)) = latest_seek {
                let t = seek_clamped(target, duration_us);
                // 先执行 seek。被更新的 Preview 抢占取消（SeekCancelled）时，
                // **放弃本次状态设置**，回循环顶部读最新命令重新 seek（对齐
                // player-app playback.rs:341-354）——否则带着半途 seek 的解码器
                // 状态继续读，会解出坏帧（快速拖动时高频抢占尤其明显）。
                if let Err(e) = source.seek(t) {
                    if e.root_cause().downcast_ref::<SeekCancelled>().is_some() {
                        tracing::debug!("seek 被抢占取消，重读最新命令");
                        continue;
                    }
                    tracing::debug!(?e, "seek 失败，继续");
                }
                // 记住本次 seek 的代次：此后投递的帧都打上它，渲染侧据此丢弃更旧帧。
                match cmd {
                    PlayerCommand::SeekPreview(_, g) | PlayerCommand::SeekCommit(_, g) => {
                        current_gen = g;
                    }
                    _ => unreachable!(),
                }
                // seek 会撤销 draining，重新可读；丢弃 seek 前暂存的旧帧。
                finished = false;
                next_frame = None;
                match cmd {
                    PlayerCommand::SeekPreview(..) => {
                        // 拖动中预览：seek 视频出预览帧，不重建音频流。
                        // 静音由拖动开始的 MuteAudio 负责；这里每次 clear 清空
                        // 队列，防止拖动中（声卡已 pause 冻结）解码线程推的音频堆积。
                        if let Some(a) = audio.as_ref() {
                            a.clear();
                        }
                        previewing = true;
                        // 新的预览目标：解除上一帧的定格，重新 seek 出目标帧。
                        preview_stall = false;
                        video_seek_target = None;
                        // 暂停中拖动：临时允许解码出预览帧显示画面。
                        if paused {
                            scrub_paused = true;
                        }
                    }
                    PlayerCommand::SeekCommit(..) => {
                        // 完整 seek：重建声卡流 + 重锚。
                        // **不改变 paused**：暂停时 seek 应保持暂停（只跳位置，
                        // 不自动播放）。播放时 seek 正常恢复（start_audio）。
                        previewing = false;
                        preview_stall = false;
                        pending_anchor = true;
                        video_seek_target = Some(t);
                        // 音频也丢弃目标前内容，避免旧位置声音/时钟超前。
                        audio_seek_target = Some(t);
                        seek_rebuild_audio(&mut audio, &clock_source);
                        if paused {
                            // 暂停中跳转：保持暂停，但临时解码出目标帧显示画面。
                            scrub_paused = true;
                            start_audio = false;
                        } else {
                            start_audio = true;
                        }
                    }
                    _ => unreachable!(),
                }
                continue; // 刚 seek 过，回循环顶部读下一批命令
            }

            // 拖动预览「定格」：preview 模式下已把目标帧送出，停住等下一个命令，
            // 不要继续往后解码（否则按住 thumb 不松手会以解码速度一帧帧快进）。
            if previewing && preview_stall {
                std::thread::sleep(Duration::from_millis(8));
                continue;
            }

            if paused && !scrub_paused {
                std::thread::sleep(Duration::from_millis(8));
                continue;
            }
            if finished {
                std::thread::sleep(Duration::from_millis(8));
                continue;
            }

            // 2) 有暂存帧先发（投递会背压）。
            if let Some((render, pts_us, preview, frame_gen)) = next_frame.take() {
                if !send_blocking(&mut tx, (render, pts_us, duration_us, preview, frame_gen), &running) {
                    return;
                }
                // 预览帧已送出：进入「定格」，不再继续往后解码，画面停在目标帧。
                // 下一个命令（新 Preview / Commit）会解除定格。
                if preview && previewing {
                    preview_stall = true;
                }
                // 暂停中 scrub 已解出目标帧（画面更新），恢复暂停。
                scrub_paused = false;
                // 首个 post-seek 视频帧已送出；若音频缓冲也够，就开播。
                // 暂停时 start_audio 为 false，不会 start。
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
                    next_frame = Some((render, pts_us, previewing, current_gen));
                }
                Ok(Some(MediaEvent::Audio(chunk))) => {
                    // seek 后丢弃目标前音频（避免旧位置声音/时钟超前）。
                    if let Some(target) = audio_seek_target {
                        if chunk.pts < target {
                            continue;
                        }
                        audio_seek_target = None;
                    }
                    // 暂停中 scrub（拖动/跳转）只解视频预览帧，不推音频——
                    // 声卡已 pause 冻结，推入只会堆积。
                    if scrub_paused {
                        continue;
                    }
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
    item: (Arc<RenderImage>, u64, u64, bool, u64),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// player-core 的内置样本视频（含音轨）。
    fn sample_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../player-core/tests/assets/sample.mp4")
    }

    /// 轮询读一帧，直到拿到一帧或超时。返回是否拿到。
    fn try_recv_frame(rx: &mut mpsc::Receiver<FrameMsg>, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            match rx.try_recv() {
                Ok(Some(_)) => return true,
                _ => {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    /// 打开控制器，消费一批帧，触发一次拖动 seek（preview + release），
    /// 验证 seek 后**解码线程不卡死、持续产帧**（对齐之前"拖动后画面/进度条
    /// 不动、声音继续"的 bug——seek 后解码必须继续，不能停）。
    #[test]
    fn seek_preview_release_keeps_frames_flowing() {
        let (mut controller, mut rx) = PlayerController::open(sample_path());

        // 1) 消费若干帧（解码线程在跑）。
        let mut got = 0;
        while got < 30 {
            if try_recv_frame(&mut rx, Duration::from_secs(5)) {
                got += 1;
            } else {
                break;
            }
        }
        assert!(got >= 30, "应解出至少 30 帧，实际 {got}");

        // 2) 拖动 seek（preview 拖动中 + release 提交）。
        controller.seek_preview(Duration::from_secs(2));
        controller.seek_release(Duration::from_secs(2));

        // 3) seek 后继续产帧（解码不卡死）。
        let mut got_after = 0;
        while got_after < 10 {
            if try_recv_frame(&mut rx, Duration::from_secs(5)) {
                got_after += 1;
            } else {
                break;
            }
        }
        assert!(
            got_after >= 10,
            "seek 后应持续产帧（解码不卡死），实际 {got_after}"
        );
    }

    /// 模拟**快速拖动**：短时间内快速连续发几十次 seek_preview（不同目标，
    /// 模拟鼠标快速来回拖），最后 seek_release。验证 seek 覆盖合并 + 抢占
    /// seek 下解码线程不卡死、seek 后持续产帧（对齐之前"快速拖动画面卡住"）。
    #[test]
    fn rapid_drag_does_not_stall_decode() {
        let (mut controller, mut rx) = PlayerController::open(sample_path());

        // 1) 消费若干帧（解码线程在跑）。
        let mut got = 0;
        while got < 20 {
            if try_recv_frame(&mut rx, Duration::from_secs(5)) {
                got += 1;
            } else {
                break;
            }
        }
        assert!(got >= 20, "应解出至少 20 帧，实际 {got}");

        // 2) 快速拖动：1 秒内快速发 50 次 seek_preview，目标在 0..2.5s 之间递增
        //    （模拟快速向右拖），紧接着 seek_release 提交到最终位置。
        for i in 0..50 {
            let t = Duration::from_millis((i * 50) as u64);
            controller.seek_preview(t);
            // 不 sleep：命令在 unbounded channel 堆积，由解码线程覆盖合并，
            // 模拟"拖动比解码线程处理还快"的最坏情况。
        }
        controller.seek_release(Duration::from_millis(2500));

        // 3) 快速拖动结束后应持续产帧（解码线程合并 seek 后不卡死）。
        let mut got_after = 0;
        while got_after < 10 {
            if try_recv_frame(&mut rx, Duration::from_secs(10)) {
                got_after += 1;
            } else {
                break;
            }
        }
        assert!(
            got_after >= 10,
            "快速拖动后应持续产帧（解码不卡死），实际 {got_after}"
        );
    }

    /// 回归测试：**按住 thumb 不松手、不滑动**（进入拖动预览但不再发新命令）。
    ///
    /// 修复前，Preview 模式下解码线程会继续往解码，预览帧以解码速度一帧帧快进，
    /// 画面看起来在自动快播（无声音）。修复后：预览解出目标帧后应**定格**，
    /// 不继续产帧（画面停住），直到下一个命令（新 Preview 或 Release 的 Commit）。
    #[test]
    fn preview_holds_still_freezes_not_fast_forward() {
        let (mut controller, mut rx) = PlayerController::open(sample_path());

        // 1) 消费若干帧（解码线程在跑）。
        let mut got = 0;
        while got < 30 {
            if try_recv_frame(&mut rx, Duration::from_secs(5)) {
                got += 1;
            } else {
                break;
            }
        }
        assert!(got >= 30, "应解出至少 30 帧，实际 {got}");

        // 2) 进入拖动预览：拖动开始（静音）+ 一个 Preview seek 到 4s，然后**停住**，
        //    模拟"点住 thumb 不松手不滑动"。
        controller.mute_audio();
        controller.seek_preview(Duration::from_secs(4));

        // 3) 等预览目标帧送达（画面应该跳过去）。
        assert!(
            try_recv_frame(&mut rx, Duration::from_secs(5)),
            "Preview 应解出目标帧"
        );
        // 再消费掉紧随的 1~2 帧（seek 落点可能先送关键帧前的帧）。
        while got < 34 {
            if try_recv_frame(&mut rx, Duration::from_millis(200)) {
                got += 1;
            } else {
                break;
            }
        }

        // 4) **关键**：此后不再发任何命令（继续"按住不松"），短窗口内不应再有
        //    新帧 —— 画面必须定格，否则就是快进 bug。
        let mut leaked = 0;
        let window = Duration::from_millis(300);
        let deadline = Instant::now() + window;
        while Instant::now() < deadline {
            if try_recv_frame(&mut rx, Duration::from_millis(50)) {
                leaked += 1;
            }
        }
        assert!(
            leaked == 0,
            "按住不滑动时预览应定格（不应再产帧/快进），300ms 内泄漏了 {leaked} 帧"
        );

        // 5) 松开（Commit）：恢复正常播放，应重新持续产帧。
        controller.seek_release(Duration::from_secs(4));
        let mut got_after = 0;
        while got_after < 5 {
            if try_recv_frame(&mut rx, Duration::from_secs(5)) {
                got_after += 1;
            } else {
                break;
            }
        }
        assert!(got_after >= 5, "松开后应恢复产帧，实际 {got_after}");
    }
}
