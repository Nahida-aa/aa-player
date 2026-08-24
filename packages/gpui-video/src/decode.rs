//! 解码线程引擎：同步拉帧/推音频，经有界通道投递视频，响应暂停/seek 命令。
//!
//! 从 [`crate::controller`] 拆出：公开门面（`PlayerController` 等 UI 可见的
//! 状态机）留在原处；这里集中所有解码侧内部实现——主循环、音频投递助手、
//! 调参常量与探针。两者只通过 `spawn_decode_thread` 的参数和
//! `PlayerCommand`/`FrameMsg` 通道交互。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::channel::mpsc;
use ffmpeg_next::Error as FfmpegError;
use gpui::RenderImage;
use image::{Frame, RgbaImage};
use player_core::{
    AudioChunk, AudioOutput, DecodedFrame, FfmpegSource, MediaEvent, MediaSource, SeekCancelled,
};

use crate::controller::{AudioClockSource, FrameMsg, PlayerCommand};
use crate::stats::ProfileStats;

/// 帧队列容量。
///
/// 这个值不只是「渲染侧背压」，它同时是**解复用领先度**的上限：
/// 音视频包在文件里按 dts 交错排列，解码线程只有在通道有空间时才会
/// 继续读包，所以「解码位置能领先播放位置多少」≈ 通道容量 × 帧时长。
/// 音频缓冲（[`AUDIO_BUFFER`]）的内容只能来自这段领先区间——
/// 容量 3 时领先仅 ~100ms（30fps），音频队列注定贴着实时线跑，
/// FFmpeg 9 的 h264 多线程解码让视频帧就绪节奏更碎，抖动直接打穿
/// 队列造成持续欠载。12 帧 @30fps ≈ 400ms 领先，音频才有余量蓄水。
///
/// 内存代价：BGRA 帧宽×高×4 字节，1080p 下 12 帧 ≈ 100MB。
/// TODO: 打开媒体后按分辨率自适应收窄容量（需要把通道创建挪进解码线程）。
pub(crate) const FRAME_QUEUE_CAP: usize = 12;
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

/// 解码线程：同步拉帧/推音频，经有界通道投递视频，响应暂停/seek 命令。
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_decode_thread(
    path: PathBuf,
    mut tx: mpsc::Sender<FrameMsg>,
    running: Arc<AtomicBool>,
    mut cmd_rx: mpsc::UnboundedReceiver<PlayerCommand>,
    cancel: Arc<AtomicBool>,
    clock_source: Arc<AudioClockSource>,
    stats: Arc<ProfileStats>,
    video_size: Arc<std::sync::Mutex<(u32, u32)>>,
    video_fps: Arc<std::sync::Mutex<f64>>,
) {    std::thread::spawn(move || {
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
        *video_fps.lock().unwrap_or_else(|e| e.into_inner()) = vinfo.fps;
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
                let t_send = Instant::now();
                // 音频续杯：视频帧投递受渲染节奏背压（通道满时每帧要等一个
                // 显示周期 ~33ms），解码线程若干等，音频产出会被挤到贴着
                // 实时线跑（实测 FFmpeg 9 下产出≈消费、零余量），稍有抖动
                // 就欠载。所以投递前先把音频蓄到目标水位。
                //
                // 水位检查在**循环条件**里而不是推送路径里：推送若带背压
                // 睡眠，泵会跟着声卡的消费节奏一毫秒一毫秒地磨，视频帧
                // 就饿死了（实测 events 掉到 1 帧/2s）。这里要的是「以解码
                // 速度灌满、立刻返回」，满与不满由条件判断，不靠睡。
                while running.load(Ordering::Relaxed)
                    && audio
                        .as_ref()
                        .is_some_and(|a| a.queued_duration() < AUDIO_BUFFER)
                {
                    match source.try_next_audio() {
                        Ok(Some(chunk)) => {
                            probe::pump_chunk();
                            deliver_audio(
                                chunk,
                                &audio,
                                &mut audio_seek_target,
                                scrub_paused,
                                start_audio,
                                previewing,
                                false,
                                &running,
                            );
                        }
                        Ok(None) => break,
                        Err(e) => {
                            tracing::debug!(?e, "音频续杯中断（错误留待主流程处理）");
                            break;
                        }
                    }
                }
                if !send_blocking(&mut tx, (render, pts_us, duration_us, preview, frame_gen), &running) {
                    return;
                }
                probe::send_blocked(t_send.elapsed());
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
                    let t_img = Instant::now();
                    let render = decoded_to_render_image(&f);
                    let img_ms = t_img.elapsed().as_millis() as u64;
                    if img_ms >= 10 {
                        tracing::debug!(img_ms, "decoded_to_render_image 耗时");
                    }
                    // 解码+像素转换总耗时（微秒）。
                    stats.record_decoded(t0.elapsed().as_micros() as u64);
                    let pts_us = f.pts.as_micros() as u64;
                    next_frame = Some((render, pts_us, previewing, current_gen));
                }
                Ok(Some(MediaEvent::Audio(chunk))) => {
                    probe::arm_chunk();
                    deliver_audio(
                        chunk,
                        &audio,
                        &mut audio_seek_target,
                        scrub_paused,
                        start_audio,
                        previewing,
                        true,
                        &running,
                    );
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
            probe::next_event_done(t0.elapsed());
        }
    });
}

/// 解码线程时间去向探针（诊断工具，debug 日志可长期保留）。
/// 在解码线程内就地汇总，每 2s 由任一事件触发上报一次：
/// 各阶段累计耗时占比 + 音频入队深度分布，用于定位「音频欠载」时
/// 时间到底花在 next_event（解码慢）还是 send_blocking（渲染背压）。
mod probe {
    use super::Duration;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;
    use std::time::Instant;

    struct Acc {
        next_us: AtomicU64,
        send_us: AtomicU64,
        audio_pushed_ms10: AtomicU64, // 累计推送音频时长×10（毫秒定点数）
        queued_max_ms: AtomicU64,
        played_ms: AtomicU64,         // 声卡累计消费（绝对值，报告算增量）
        dropped: AtomicU64,
        pump_chunks: AtomicU64,       // 续杯泵抽到的音频块数
        arm_chunks: AtomicU64,        // next_event 正常路径交付的音频块数
        events: AtomicU64,
        t_last: AtomicU64, // 上次上报时刻（相对进程锚点的毫秒数）
        last_played: AtomicU64,
    }
    static ACC: Acc = Acc {
        next_us: AtomicU64::new(0),
        send_us: AtomicU64::new(0),
        audio_pushed_ms10: AtomicU64::new(0),
        queued_max_ms: AtomicU64::new(0),
        played_ms: AtomicU64::new(0),
        dropped: AtomicU64::new(0),
        pump_chunks: AtomicU64::new(0),
        arm_chunks: AtomicU64::new(0),
        events: AtomicU64::new(0),
        t_last: AtomicU64::new(0),
        last_played: AtomicU64::new(0),
    };

    const REPORT_INTERVAL_MS: u64 = 2000;

    fn anchor() -> Instant {
        static A: OnceLock<Instant> = OnceLock::new();
        *A.get_or_init(Instant::now)
    }

    fn maybe_report() {
        let now_ms = anchor().elapsed().as_millis() as u64;
        let last = ACC.t_last.load(Ordering::Relaxed);
        if now_ms.wrapping_sub(last) < REPORT_INTERVAL_MS {
            return;
        }
        if ACC
            .t_last
            .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return; // 别的调用已触发上报
        }
        let next = ACC.next_us.swap(0, Ordering::Relaxed);
        let send = ACC.send_us.swap(0, Ordering::Relaxed);
        let pushed = ACC.audio_pushed_ms10.swap(0, Ordering::Relaxed);
        let qmax = ACC.queued_max_ms.swap(0, Ordering::Relaxed);
        let played = ACC.played_ms.load(Ordering::Relaxed);
        let dropped = ACC.dropped.swap(0, Ordering::Relaxed);
        let pump_chunks = ACC.pump_chunks.swap(0, Ordering::Relaxed);
        let arm_chunks = ACC.arm_chunks.swap(0, Ordering::Relaxed);
        let n = ACC.events.load(Ordering::Relaxed);
        // 消费增量 = 本次读数 - 上次读数（绝对计数）。
        let last_played = ACC.last_played.swap(played, Ordering::Relaxed);
        let d_played = played.saturating_sub(last_played);
        tracing::info!(
            next_event_ms = next / 1000,
            send_blocked_ms = send / 1000,
            audio_pushed_ms = pushed / 10,
            card_consumed_ms = d_played,
            audio_queued_peak_ms = qmax,
            pump_chunks,
            arm_chunks,
            dropped,
            events = n,
            "解码线程 2s 时间去向"
        );
    }

    pub fn next_event_done(d: Duration) {
        ACC.next_us.fetch_add(d.as_micros() as u64, Ordering::Relaxed);
        ACC.events.fetch_add(1, Ordering::Relaxed);
        maybe_report();
    }

    pub fn send_blocked(d: Duration) {
        ACC.send_us.fetch_add(d.as_micros() as u64, Ordering::Relaxed);
        maybe_report();
    }

    /// 每次音频 push 调用：记录本 chunk 时长与当前队列深度峰值。
    pub fn audio_pushed(
        sample_count: usize,
        channels: u16,
        sample_rate: u32,
        queued: std::time::Duration,
    ) {
        let ch = channels.max(1) as f64;
        let ms10 = (sample_count as f64 / ch / sample_rate as f64 * 1000.0 * 10.0) as u64;
        ACC.audio_pushed_ms10.fetch_add(ms10, Ordering::Relaxed);
        ACC.queued_max_ms
            .fetch_max(queued.as_millis() as u64, Ordering::Relaxed);
        maybe_report();
    }

    /// 声卡侧消费进度（position 增量在报告里算差值）。
    pub fn audio_chunk(played: Duration, _queued: Duration) {
        ACC.played_ms
            .store(played.as_millis() as u64, Ordering::Relaxed);
    }

    /// 记录音频块被丢弃的路径（seek 过滤 / scrub 静音）。
    pub fn audio_dropped(_why: &'static str) {
        ACC.dropped.fetch_add(1, Ordering::Relaxed);
    }

    pub fn pump_chunk() {
        ACC.pump_chunks.fetch_add(1, Ordering::Relaxed);
    }

    pub fn arm_chunk() {
        ACC.arm_chunks.fetch_add(1, Ordering::Relaxed);
    }
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

/// 把一条音频事件按当前模式入队。
///
/// 「next_event 的 Audio 分支」与「视频投递前的音频续杯泵」共用，
/// 保证两条路径的语义完全一致：
/// - seek 后丢弃目标前音频（避免旧位置声音/时钟超前）；
/// - 暂停中 scrub 只解视频预览帧，不推音频（声卡已 pause 冻结，推入只会堆积）；
/// - 背压：缓冲够深就等，别把整轨解进内存。仅 arm 路径（`backpressure=true`）
///   启用——泵路径靠调用方的循环条件控水位，睡在这里会把主循环拖成
///   声卡节奏（实测视频掉到 1 帧/2s）；seek 后音频是暂停态或拖动预览，
///   队列不被消费，此时也不背压；
/// - 欠载检测：声卡回调发现队列空会置位，这里取走并告警。
#[allow(clippy::too_many_arguments)]
fn deliver_audio(
    chunk: AudioChunk,
    audio: &Option<AudioOutput>,
    audio_seek_target: &mut Option<Duration>,
    scrub_paused: bool,
    start_audio: bool,
    previewing: bool,
    backpressure: bool,
    running: &AtomicBool,
) {
    if let Some(target) = *audio_seek_target {
        if chunk.pts < target {
            probe::audio_dropped("seek_target");
            return;
        }
        *audio_seek_target = None;
    }
    if scrub_paused {
        probe::audio_dropped("scrub_paused");
        return;
    }
    let Some(a) = audio else { return };
    if backpressure && !start_audio && !previewing {
        while running.load(Ordering::Relaxed) && a.queued_duration() > AUDIO_BUFFER {
            std::thread::sleep(AUDIO_BACKOFF);
        }
    }
    a.push_samples(&chunk.samples);
    probe::audio_chunk(a.position(), a.queued_duration());
    let fmt = a.format();
    probe::audio_pushed(
        chunk.samples.len(),
        fmt.channels,
        fmt.sample_rate,
        a.queued_duration(),
    );
    if a.take_underrun() {
        tracing::warn!(
            queued_ms = a.queued_duration().as_millis() as u64,
            "音频欠载：解码跟不上声卡消费"
        );
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
