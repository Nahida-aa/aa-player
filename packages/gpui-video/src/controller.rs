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
use std::time::Duration;

use futures::channel::mpsc;
use gpui::RenderImage;
use player_core::AudioClock;

use crate::decode::{FRAME_QUEUE_CAP, spawn_decode_thread};
use crate::i18n::{I18n, Lang};
use crate::stats::ProfileStats;

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

/// 快进/快退步长的表示。
///
/// - [`SeekStep::Frames`]：按视频帧数跳转（1 帧 = `1/fps` 秒）。fps 未知时
///   fallback 到 30，故「1 帧」在未知 fps 下约等于 33ms。
/// - [`SeekStep::Duration`]：固定时长（如 1ms、5s），与帧率无关。
///
/// 用枚举而非纯 `Duration`，是因为「跳 1 帧」这种步长无法写成固定时长——
/// 它依赖当前视频的 fps，必须在 seek 时按 fps 换算。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekStep {
    /// N 帧（按 fps 换算成时长）。
    Frames(u32),
    /// 固定时长。
    Duration(Duration),
}

impl SeekStep {
    /// 解析成实际跳转时长：`Frames(n)` → `n/fps` 秒（fps≤0 用 30 兜底）；
    /// `Duration(d)` 原样返回。
    fn resolve(self, fps: f64) -> Duration {
        match self {
            SeekStep::Frames(n) => {
                let fps = if fps > 0.0 { fps } else { 30.0 };
                Duration::from_secs_f64(n as f64 / fps)
            }
            SeekStep::Duration(d) => d,
        }
    }

    /// 菜单/按钮显示的简短标签，如「1帧」「1ms」「5s」（按语言）。
    /// `ms`/`s` 两种语言通用；「帧」在英文下显示 `frame`。
    pub fn label(self, lang: Lang) -> String {
        match self {
            SeekStep::Frames(n) => {
                let unit = if lang == Lang::Zh { "帧" } else { "f" };
                format!("{n}{unit}")
            }
            SeekStep::Duration(d) => {
                if d < Duration::from_secs(1) {
                    format!("{}ms", d.as_millis())
                } else {
                    format!("{}s", d.as_secs())
                }
            }
        }
    }
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
    /// 快进/快退步长（点击控制条左右箭头一次跳转的量）。单源状态，由 UI
    /// （更多菜单的「步长」项）切换；控制条按钮与键盘方向键共用它。
    /// 可为「N 帧」（按当前 fps 换算成时长）或「固定时长」（如 1ms）。
    seek_step: SeekStep,
    /// 当前界面语言（UI 状态，供控制条菜单项按语言取文本）。
    i18n: I18n,
    /// 「info」信息面板是否展开（UI 状态，供控制条条件渲染浮层）。
    info_open: bool,
    /// 最近一次 seek 的代次（每次 seek_preview/seek_to 自增）。帧携带自己所属
    /// 的 seek 代次，`consume_frame` 据此丢弃 seek 前在途的旧帧（不覆盖 position）。
    seek_gen: u64,
    /// 最近一次真正下发的预览时刻（预览节流用）。拖动时鼠标移动事件可达
    /// 60+/s，每次都 demux seek 会把音频切成 16ms 碎片——碎片里 AAC 中途
    /// 进入的收敛帧占大头，听感就是持续的滋滋/咔咔电音。节流后每个预览
    /// 有 ~45ms 可闻窗，碎片变成可辨识的位置采样（vlc 拖动手感）。
    last_preview_sent: Option<std::time::Instant>,
    /// 取消标志：发新 Preview 前置 true，中断解码线程里进行中的旧 seek。
    cancel_seek: Arc<AtomicBool>,
    /// 音频主时钟交接点（供渲染侧调度视频）。
    pub clock: Arc<AudioClockSource>,
    /// 性能统计（解码 fps/耗时），仅 debug 时启用。
    pub stats: Arc<ProfileStats>,
    /// 视频原始分辨率 (width, height)，解码线程打开后填入。供组件按视频比例定尺寸。
    video_size: Arc<std::sync::Mutex<(u32, u32)>>,
    /// 视频平均帧率 (fps)，解码线程打开后填入。供时间码 `mm:ss:ff` 的帧字段计算。
    video_fps: Arc<std::sync::Mutex<f64>>,
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
        let video_fps = Arc::new(std::sync::Mutex::new(0.0));

        spawn_decode_thread(
            path,
            tx,
            running.clone(),
            cmd_rx,
            cancel_seek.clone(),
            clock.clone(),
            stats.clone(),
            video_size.clone(),
            video_fps.clone(),
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
                seek_step: SeekStep::Duration(Duration::from_secs(5)),
                i18n: I18n::default(),
                info_open: false,
                seek_gen: 0,
                last_preview_sent: None,
                cancel_seek,
                clock,
                stats,
                video_size,
            video_fps,
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

    /// 视频平均帧率（fps）。未知时为 0，调用方应 fallback 到合理默认值。
    pub fn fps(&self) -> f64 {
        *self.video_fps.lock().unwrap_or_else(|e| e.into_inner())
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
}

mod commands;
