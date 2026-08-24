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
use crate::i18n::{I18n, Lang, StrKey};
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

    /// 快进/快退步长档位（点击更多菜单里的「步长」项时循环切换）。
    ///
    /// 含「1 帧」（按当前 fps 换算）、「1ms」「100ms」等细粒度，以及常用的秒级档。
    pub const SEEK_STEP_OPTIONS: &'static [SeekStep] = &[
        SeekStep::Frames(1),
        SeekStep::Duration(Duration::from_millis(1)),
        SeekStep::Duration(Duration::from_millis(100)),
        SeekStep::Duration(Duration::from_secs(5)),
        SeekStep::Duration(Duration::from_secs(10)),
        SeekStep::Duration(Duration::from_secs(30)),
    ];

    /// 当前快进/快退步长。
    pub fn seek_step(&self) -> SeekStep {
        self.seek_step
    }

    /// 当前步长解析成实际跳转时长（Frames 按 `fps` 换算；fps≤0 时 fallback 30）。
    pub fn seek_step_duration(&self) -> Duration {
        self.seek_step.resolve(self.fps())
    }

    /// 设置快进/快退步长（UI 直接指定，如自定义输入）。
    pub fn set_seek_step(&mut self, step: SeekStep) {
        self.seek_step = step;
    }

    /// 按当前步长向前跳一步（控制条快进按钮 / 键盘右方向键调用）。
    pub fn seek_forward_step(&mut self) {
        self.seek_forward(self.seek_step_duration());
    }

    /// 按当前步长向后跳一步（控制条快退按钮 / 键盘左方向键调用）。
    pub fn seek_backward_step(&mut self) {
        self.seek_backward(self.seek_step_duration());
    }

    /// 循环切换到下一个步长档位（点击「步长」菜单项时调用）。
    pub fn cycle_seek_step(&mut self) {
        let idx = Self::SEEK_STEP_OPTIONS
            .iter()
            .position(|&s| s == self.seek_step)
            .unwrap_or(0);
        self.seek_step = Self::SEEK_STEP_OPTIONS[(idx + 1) % Self::SEEK_STEP_OPTIONS.len()];
    }

    // ----- i18n -----

    /// 当前语言。
    pub fn lang(&self) -> Lang {
        self.i18n.lang()
    }

    /// 取键的译文（按当前语言）。
    pub fn t(&self, key: StrKey) -> &'static str {
        self.i18n.get(key)
    }

    /// 循环切换语言（点击「语言」菜单项时调用）。
    pub fn cycle_lang(&mut self) {
        self.i18n.cycle();
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

    /// 相对当前位置 seek（快进/快退）。`delta_ns` 为相对偏移（纳秒），正向前、
    /// 负向后，按 `[0, duration]` 夹紧后发正式 seek（预测 position + 推进 seek
    /// 代次），不进拖动预览态（快进快退是一次性跳转）。
    pub fn seek_relative(&mut self, delta_ns: i64) {
        if delta_ns >= 0 {
            self.seek_forward(Duration::from_nanos(delta_ns as u64));
        } else {
            self.seek_backward(Duration::from_nanos((-delta_ns) as u64));
        }
    }

    /// 相对当前位置向前 seek（夹到 `[0, duration]`）。
    pub fn seek_forward(&mut self, delta: Duration) {
        let target = (self.position() + delta).min(self.duration);
        self.seek_to(target);
    }

    /// 相对当前位置向后 seek（夹到 `[0, duration]`）。
    pub fn seek_backward(&mut self, delta: Duration) {
        let target = self.position().saturating_sub(delta);
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
