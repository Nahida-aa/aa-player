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

use ffmpeg_next::Error as FfmpegError;
use futures::channel::mpsc;
use gpui::RenderImage;
use player_core::{AudioOutput, FfmpegSource, MediaEvent, MediaSource, SeekCancelled};

use crate::controller::{AudioClockSource, FrameMsg, PlayerCommand};
use crate::stats::ProfileStats;

mod audio;
mod probe;
mod video;

use audio::{AUDIO_BUFFER, deliver_audio, drain_audio, seek_reset_audio, try_start_audio};
use video::{decoded_to_render_image, send_blocking};

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
/// seek 时离文件末尾保留的安全余量（微秒），避 ffmpeg 末尾阻塞。
const SEEK_END_MARGIN_US: u64 = 1_000_000;

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
        // 拖动预览「定格」：preview 模式下解过目标帧（+快进余量）后**停住**，
        // 不再继续往解码。完全不许前进会让拖动画面一跳一跳（每次 seek 只出
        // 一帧，中间全冻着）；vlc/mpv 的顺滑感来自预览期间画面持续流动。
        // 折中：越过目标后允许再解 PREVIEW_CREEP 的帧——拖动时表现为跟手的
        // 连续小段运动；按住不动时最多多走 350ms 就停（不可感知），不会
        // 变成无限快播。
        const PREVIEW_CREEP_US: u64 = 350_000;
        let mut preview_stall = false;
        // 最近一次 Preview 的目标（微秒）；定格判定 = pts 越过 target+creep。
        let mut preview_target_us: u64 = 0;
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
                        // 清理 scrub 残留：空格若按在暂停中跳转的收尾中途，
                        // 不清理会让音频投递被 deliver_audio 持续丢弃。
                        scrub_paused = false;
                        if let Some(a) = audio.as_ref() {
                            a.resume();
                            // 暂停中 commit 过的流：设备被 Pause 冻结、队列被
                            // 清空，resume() 只解冻设备。起播必须走统一协议——
                            // 置 start_audio，让 try_start_audio 在缓冲攒够
                            // AUDIO_START_MIN 后才真正出声。否则这条路径绕过
                            // 起播协议，曾致「暂停→向后跳转→播放」静音。
                            // 拖动预览除外：拖动静音语义优先（队列被 preview
                            // 反复 clear，不该出声）。
                            if !previewing {
                                start_audio = true;
                            }
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
                        latest_seek = Some((
                            match cmd {
                                PlayerCommand::SeekPreview(t, _) => t,
                                PlayerCommand::SeekCommit(t, _) => t,
                                _ => unreachable!(),
                            },
                            cmd,
                        ));
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
                        // 拖动中预览：seek 视频出预览帧，不动音频设备。
                        // 静音由拖动开始的 MuteAudio 负责；这里每次 clear 清空
                        // 队列，防止拖动中（声卡已 pause 冻结）解码线程推的音频堆积。
                        if let Some(a) = audio.as_ref() {
                            a.clear();
                        }
                        previewing = true;
                        // 新的预览目标：解除上一帧的定格，重新 seek 出目标帧。
                        preview_stall = false;
                        video_seek_target = None;
                        preview_target_us = t.as_micros() as u64;
                        // 暂停中拖动：临时允许解码出预览帧显示画面。
                        if paused {
                            scrub_paused = true;
                        }
                    }
                    PlayerCommand::SeekCommit(..) => {
                        // 完整 seek：善后音频 + 重锚。
                        // **不改变 paused**：暂停时 seek 应保持暂停（只跳位置，
                        // 不自动播放）。播放时 seek 正常恢复（start_audio）。
                        previewing = false;
                        preview_stall = false;
                        pending_anchor = true;
                        video_seek_target = Some(t);
                        // 音频也丢弃目标前内容，避免旧位置声音/时钟超前。
                        audio_seek_target = Some(t);
                        seek_reset_audio(&audio, &clock_source);
                        if paused {
                            // 暂停中跳转：保持暂停，但临时解码出目标帧显示画面。
                            scrub_paused = true;
                            start_audio = false;
                        } else {
                            start_audio = true;
                        }
                        // 音频流状态转换观测点：静音类 bug（起播协议被绕过、
                        // commit 目标值异常）靠这条日志定罪。
                        tracing::debug!(target = ?t, paused, start_audio, "seek 音频善后");
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
                //
                // 门闸丢弃期（暂停 scrub / 拖动预览）必须停泵：此间每块音频
                // 都会在 deliver_audio 入队前被丢掉，队列读数恒 0，水位条件
                // 永不满足——泵会以解码速度把整条剩余音轨拉完丢光直到 EOF
                // （实测 10s 样本一次窗口丢了 469 块）。用户随后恢复播放时
                // 解码器已见底：静音直到文件尾。这就是「暂停→点击进度条向
                // 过去跳→恢复后无声」的完整病灶。
                while running.load(Ordering::Relaxed)
                    && !scrub_paused
                    && !previewing
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
                if !send_blocking(
                    &mut tx,
                    (render, pts_us, duration_us, preview, frame_gen),
                    &running,
                ) {
                    return;
                }
                probe::send_blocked(t_send.elapsed());
                // 预览帧持续送出直到越过目标 + 快进余量，然后「定格」：
                // 不再继续往后解码。下一个命令（新 Preview / Commit）会解除定格。
                // （此前是送出第一帧就定格——拖动画面一跳一跳的根源。）
                if preview && previewing && pts_us >= preview_target_us + PREVIEW_CREEP_US {
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

/// seek (jump) 目标夹到 [0, duration-1s]，避 ffmpeg 末尾阻塞。
fn seek_clamped(t: Duration, duration_us: u64) -> Duration {
    let margin = SEEK_END_MARGIN_US;
    let max_us = duration_us.saturating_sub(margin);
    let us = (t.as_micros() as u64).min(max_us);
    Duration::from_micros(us)
}
