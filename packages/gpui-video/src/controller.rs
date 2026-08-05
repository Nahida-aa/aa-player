//! 无 GUI 的播放状态机（PlayerController）。
//!
//! 复用 `player-core`（FFmpeg）做解码，管理播放时钟、暂停、seek，并把最新帧
//! 转成 GPUI `RenderImage` 供渲染层消费。不依赖任何 UI。
//!
//! V1 只做视频（纯视频 + 墙钟节流）；音频主时钟同步留待 V2。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::channel::mpsc;
use gpui::RenderImage;
use image::{Frame, RgbaImage};
use player_core::{DecodedFrame, FfmpegSource, MediaEvent, MediaSource};

/// 帧队列容量。故意浅：渲染侧按 PTS 等待，队列几乎总是满的（背压）。
const FRAME_QUEUE_CAP: usize = 3;
/// 投递队列满时的退避间隔。
const SEND_BACKOFF: Duration = Duration::from_millis(2);

/// 发往渲染侧的一帧：图像、显示时刻（PTS 微秒）、文件总时长（微秒）。
/// `None` 表示流结束（EOF 或错误）。
pub type FrameMsg = Option<(Arc<RenderImage>, u64, u64)>;

/// 播放器控制命令（UI → 解码线程）。unbounded：命令不能因背压丢失。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerCommand {
    Pause,
    Resume,
    /// 拖动中：seek 视频出预览帧。
    SeekPreview(Duration),
    /// 松开/点击：完整 seek。
    SeekCommit(Duration),
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

        spawn_decode_thread(path, tx, running.clone(), cmd_rx, cancel_seek.clone());

        (
            Self {
                latest_frame: None,
                cmd,
                duration: Duration::ZERO,
                position: Duration::ZERO,
                paused: false,
                dragging: false,
                cancel_seek,
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
        // 置取消标志：让解码线程里进行中的旧 seek 立即被 interrupt 打断。
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
        let Some((render, pts_us, duration_us)) = item else {
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

/// 解码线程：同步拉帧，经有界通道投递，响应暂停/seek 命令。
#[allow(clippy::too_many_arguments)]
fn spawn_decode_thread(
    path: PathBuf,
    mut tx: mpsc::Sender<FrameMsg>,
    running: Arc<AtomicBool>,
    mut cmd_rx: mpsc::UnboundedReceiver<PlayerCommand>,
    cancel: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        // 用可中断 seek 的打开方式。
        let mut source = match FfmpegSource::open_with_interrupt(&path, None, cancel) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(?e, "打开媒体失败");
                return;
            }
        };
        let mut paused = false;
        let fps = source.video_info().fps.max(1.0);
        let frame_interval = Duration::from_secs_f64(1.0 / fps);
        let mut next_frame_at: Option<Instant> = None;

        while running.load(Ordering::Relaxed) {
            // 1) 处理命令。
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    PlayerCommand::Pause => paused = true,
                    PlayerCommand::Resume => {
                        paused = false;
                        // 恢复时重置节流基准，避免把暂停时长当落后。
                        next_frame_at = None;
                    }
                    PlayerCommand::SeekPreview(t) => {
                        if let Err(e) = source.seek(t) {
                            tracing::debug!(?e, "Preview seek 被打断/失败，重试最新命令");
                        }
                        next_frame_at = None;
                    }
                    PlayerCommand::SeekCommit(t) => {
                        if let Err(e) = source.seek(t) {
                            tracing::debug!(?e, "Commit seek 失败");
                        }
                        paused = false;
                        next_frame_at = None;
                    }
                }
            }

            if paused {
                // 暂停时仍响应命令；退避避免忙等。
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }

            // 2) 拉一帧。
            match source.next_event() {
                Ok(Some(MediaEvent::Video(f))) => {
                    let render = decoded_to_render_image(&f);
                    let pts_us = f.pts.as_micros() as u64;
                    let dur_us = source.video_info().duration.as_micros() as u64;

                    // 3) 墙钟节流：按帧间隔推进显示，避免解码跑在显示前太多。
                    if let Some(at) = next_frame_at {
                        let now = Instant::now();
                        if now < at {
                            std::thread::sleep(at - now);
                        }
                    }
                    next_frame_at = Some(Instant::now() + frame_interval);

                    // 4) 投递；队列满则退避重试（不丢帧）。
                    loop {
                        match tx.try_send(Some((render.clone(), pts_us, dur_us))) {
                            Ok(()) => break,
                            Err(e) if e.is_full() => {
                                std::thread::sleep(SEND_BACKOFF);
                            }
                            Err(_) => {
                                // 渲染侧已关闭（关窗）。
                                return;
                            }
                        }
                    }
                }
                Ok(Some(MediaEvent::Audio(_))) => {
                    // V1 只做视频，丢弃音频。
                    continue;
                }
                Ok(None) => {
                    // EOF：投递结束信号，然后等命令（可再 seek）。
                    let _ = tx.try_send(None);
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => {
                    tracing::error!(?e, "解码失败");
                    let _ = tx.try_send(None);
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    });
}
