//! 媒体源抽象与 ffmpeg 实现。
//!
//! [`MediaSource`] 是 player-core 对外的核心边界：调用方（player-app / ocr-lab）
//! 只依赖这个 trait，不关心底层是 ffmpeg、Symphonia 还是别的。未来要换纯 Rust
//! 解码后端，只需再实现一个 `MediaSource` 即可。
//!
//! [`FfmpegSource`] 是当前基于 `ffmpeg-next`（动态链接系统 ffmpeg）的实现，
//! 提供音视频逐单元拉取（视频 BGRA / 音频交错 f32）+ 运行时 seek 的能力。
//!
//! 音视频从**同一个** [`MediaSource::next_event`] 出口交付（见 [`MediaEvent`]），
//! 因为容器里本就只有一条 packet 流；分成两个方法必然要缓冲另一路，
//! 而那个缓冲要么无界（吃光内存）要么会死锁。

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ffmpeg_next::{
    Error, Packet, Rational,
    format::Pixel,
    software::scaling::{context::Context as Scaler, flag::Flags},
    util::frame::Video,
};

use crate::audio_decoder::{AudioDecoder, AudioInfo};
use crate::audio_output::AudioFormat;
use crate::error::Result;
use crate::frame::{AudioChunk, DecodedFrame, VideoInfo};

/// next_event 行为探针：观察包读取节奏与音频事件产出，配合 controller
/// 侧的「解码线程时间去向」日志定位音频欠载类问题。每 2s 汇总一次。
mod probe {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    static PACKETS: AtomicU32 = AtomicU32::new(0);
    static AUDIO_EVENTS: AtomicU32 = AtomicU32::new(0);
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);

    pub fn packet() {
        PACKETS.fetch_add(1, Ordering::Relaxed);
    }

    pub fn audio_event() {
        AUDIO_EVENTS.fetch_add(1, Ordering::Relaxed);
    }

    pub fn maybe_report() {
        let mut last = LAST.lock().unwrap();
        let now = Instant::now();
        match *last {
            Some(t) if now.duration_since(t) < Duration::from_secs(2) => return,
            _ => *last = Some(now),
        }
        let packets = PACKETS.swap(0, Ordering::Relaxed);
        let audio_events = AUDIO_EVENTS.swap(0, Ordering::Relaxed);
        tracing::info!(
            packets,
            audio_events,
            "next_event 2s 统计"
        );
    }
}

/// 从媒体里解出来的一个单元。
///
/// 音频和视频**共享同一条 demux 流**，谁的包先到就先产出谁，
/// 因此调用方拿到的顺序天然接近二者的 PTS 顺序。
///
/// 为什么不是 `next_video()` / `next_audio()` 两个方法：容器里只有一条
/// packet 流，分开拉就必须把另一路的包缓冲起来。而缓冲无上界时，
/// 「只拉音频」会把整条视频攒进内存；有上界时又会死锁（缓冲满了，
/// 但调用方偏偏还在要另一路）。统一出口把这个两难消掉了。
#[derive(Debug, Clone)]
pub enum MediaEvent {
    /// 一帧视频。
    Video(DecodedFrame),
    /// 一块音频（已重采样到打开时指定的设备格式）。
    Audio(AudioChunk),
}

/// [`try_next_audio`](MediaSource::try_next_audio) 单次调用最多跳过（暂存）
/// 的视频包数。30fps 下 32 包 ≈ 1s 的流；压缩包每包几 KB，内存可忽略。
/// 到限即返回，调用方下轮再来——蓄水是持续行为，不必一口气抽完。
const MAX_PUMP_VIDEO_HELD: usize = 32;

/// 媒体源：一个可打开、可逐单元解码、可 seek 的媒体。
pub trait MediaSource {
    /// 打开一个本地文件，只解视频。
    fn open(path: &Path) -> Result<Self>
    where
        Self: Sized;

    /// 打开并同时解码音频，重采样到 `audio` 指定的设备格式。
    ///
    /// 传 `None` 等价于 [`open`](Self::open)。文件本身没有音轨时不算错误，
    /// 只是不会产出 [`MediaEvent::Audio`]。
    fn open_with(path: &Path, audio: Option<AudioFormat>) -> Result<Self>
    where
        Self: Sized;

    /// 视频流元信息（宽高、时长、帧率）。
    fn video_info(&self) -> VideoInfo;

    /// 音频流元信息；没有音轨或未开启音频解码时为 `None`。
    fn audio_info(&self) -> Option<AudioInfo>;

    /// 拉取下一个单元。到文件末尾返回 `Ok(None)`。
    fn next_event(&mut self) -> Result<Option<MediaEvent>>;

    /// 只窥探音频路：立即交付已就绪的音频块，没有则返回 `None`（不阻塞）。
    ///
    /// 与 [`next_event`](Self::next_event) 的区别：完全不碰视频解码器，
    /// 也不读新 packet——只收音频解码器**已经**解出的东西。
    /// 给播放器的「音频续杯」用：视频帧投递被渲染节奏背压时（通道满、
    /// 每帧要等一个显示周期），解码线程不能干等，得把现成音频先推给声卡，
    /// 否则音频产出会被挤到贴着实时线跑，稍有抖动就欠载。
    fn try_next_audio(&mut self) -> Result<Option<AudioChunk>>;

    /// 拉取下一帧视频，丢弃途中遇到的音频。
    ///
    /// 给纯视频场景（逐帧分析、缩略图）的便利方法。开了音频解码时用它
    /// 会把声音悄悄丢掉，所以要播放请用 [`next_event`](Self::next_event)。
    fn next_frame(&mut self) -> Result<Option<DecodedFrame>> {
        loop {
            match self.next_event()? {
                Some(MediaEvent::Video(f)) => return Ok(Some(f)),
                Some(MediaEvent::Audio(_)) => continue,
                None => return Ok(None),
            }
        }
    }

    /// 跳转到指定时间点（最近关键帧）。已解码的残帧需注意在调用方处理。
    fn seek(&mut self, ts: Duration) -> Result<()>;
}

/// seek 被「取消信号」中断（对应 Chromium 的 `CancelPendingSeek`）。
///
/// 拖动快时，解码线程可能正卡在旧 `avformat_seek_file` 上；新 Preview 到达
/// 时主线程置位取消标志，ffmpeg 的 interrupt 回调令旧 seek 以 `AVERROR_EXIT`
/// 干净返回。此时上下文一致、可安全复用，上层应读到最新命令重试 seek，
/// 而不是把「半途而废」的旧 seek 当成功（否则画面会停在错误位置）。
///
/// 用独立错误类型而不是普通 anyhow，让上层能精确区分「被打断」与「真失败」。
#[derive(Debug)]
pub struct SeekCancelled;

impl std::fmt::Display for SeekCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("seek cancelled by newer request")
    }
}

impl std::error::Error for SeekCancelled {}

/// 基于 `ffmpeg-next` 的媒体源实现。
pub struct FfmpegSource {
    input: ffmpeg_next::format::context::Input,
    /// 视频流在输入里的下标。
    stream_index: usize,
    /// 视频流的时间基（PTS 单位 → 秒的换算分母）。
    time_base: Rational,
    /// 视频解码器。
    decoder: ffmpeg_next::decoder::Video,
    /// YUV/原生格式 → BGRA 的缩放/色彩转换上下文。
    scaler: Scaler,
    /// 复用：解码器产出的原始帧（scaler 的输入）。
    raw_frame: Video,
    /// 复用：scaler 输出的 BGRA 帧（GPU 上传源）。
    rgba_frame: Video,
    /// 上次 send_packet 因解码器输入缓冲满(EAGAIN)而未送出的 packet，暂存待重试。
    pending: Option<Packet>,
    /// 音频续杯时被跳过、尚未送进视频解码器的压缩包（FIFO）。
    ///
    /// [`try_next_audio`](Self::try_next_audio) 为了给音频蓄水要向前读包；
    /// 途中的视频包若照常解码，帧就得有地方放（内存大），于是压在队列里
    /// （每包几 KB，有界）。正常读取流程会先排空这里再读新包，顺序不乱。
    video_backlog: std::collections::VecDeque<Packet>,
    width: u32,
    height: u32,
    duration: Duration,
    fps: f64,
    /// 音频解码器；未开启音频或文件无音轨时为 `None`。
    audio: Option<AudioTrack>,
    /// 已 send_eof、正在把解码器内部缓冲吐干净。
    draining: bool,
    /// 取消标志（共享）。打开时通过 `input_with_interrupt` 挂到 `AVFormatContext`：
    /// ffmpeg 在 I/O 阻塞（含 `avformat_seek_file`）期间反复调用回调，返回 `true`
    /// 即中断当前操作。主线程在发**新** Preview 前置 `true`，让进行中的旧 seek
    /// 立即退出，对齐 Chromium 的 `CancelPendingSeek`。
    ///
    /// 每次 `seek` 调用前先清 `false`：这样「当前正在执行的 seek」不会被自己这次
    /// 的取消打断，只有「之后更新的 Preview」置位才会打断它。
    cancel: Arc<AtomicBool>,
}

/// 音频那一路的状态。
struct AudioTrack {
    index: usize,
    decoder: AudioDecoder,
    /// 解码器已 drain 完，接下来该把重采样器里的残留也吐出来。
    flushing_resampler: bool,
}

impl MediaSource for FfmpegSource {
    fn open(path: &Path) -> Result<Self> {
        Self::open_with(path, None)
    }

    fn open_with(path: &Path, audio: Option<AudioFormat>) -> Result<Self> {
        ffmpeg_next::init()?;

        if !path.exists() {
            return Err(anyhow::anyhow!("source not found: {}", path.display()));
        }

        Self::from_input_with(ffmpeg_next::format::input(&path)?, audio)
    }

    fn video_info(&self) -> VideoInfo {
        VideoInfo {
            width: self.width,
            height: self.height,
            duration: self.duration,
            fps: self.fps,
        }
    }

    fn audio_info(&self) -> Option<AudioInfo> {
        self.audio.as_ref().map(|a| a.decoder.info())
    }

    fn next_event(&mut self) -> Result<Option<MediaEvent>> {
        // ffmpeg 官方 demux/decode 状态机：
        //   - receive_frame 返回 EAGAIN  ⇒ 解码器要更多 packet，继续送
        //   - send_packet  返回 EAGAIN  ⇒ 解码器输入缓冲满，先 receive 排空（暂存 packet 重试）
        //   - receive_frame 返回 Eof    ⇒ 仅当已 send_eof（draining）后才表示真正结束
        //
        // 两路解码器共用这一套流程：读到的 packet 按 stream index 分发，
        // 每轮先看看有没有现成的解码结果可以交付。
        // 防死锁：连续多次「send EAGAIN 存 pending + receive EAGAIN 无输出」且
        // 没读到新 packet 时，说明解码器状态卡死（seek + flush 后可能出现）。
        // 若放任会无限循环、解码线程挂死、画面冻结。这里限次后按 EOF 结束本次
        // 调用，让调用方能重新 seek 驱动，而不是永久卡住。
        let mut stalled = 0u32;
        probe::maybe_report();
        loop {
            // 1) 优先把已解出的东西交出去，不必等下一个 packet。
            if let Some(ev) = self.take_ready()? {
                if matches!(ev, MediaEvent::Audio(_)) {
                    probe::audio_event();
                }
                return Ok(Some(ev));
            }

            if self.draining {
                // 两个解码器都吐干净了，还要收走重采样器尾部的残留。
                if let Some(a) = self.audio.as_mut()
                    && a.flushing_resampler
                    && let Some(c) = a.decoder.flush_resampler()?
                {
                    return Ok(Some(MediaEvent::Audio(c)));
                }
                return Ok(None); // 真正结束
            }

            // 2) 喂一个 packet 进去。优先级：EAGAIN 重试 > 续杯攒下的视频包
            //    > 正常读盘——保证包序不乱（backlog 里的包比后续读出的都旧）。
            let packet = match self.pending.take() {
                Some(p) => Some(p),
                None => match self.video_backlog.pop_front() {
                    Some(p) => {
                        probe::packet();
                        Some(p)
                    }
                    None => self.read_packet()?,
                },
            };

            match packet {
                Some(packet) => {
                    probe::packet();
                    self.dispatch(packet)?;
                    // 若 dispatch 又 EAGAIN 存回 pending（没能送进解码器），且本轮
                    // 也没解出帧，说明没进展。累计到阈值即视为卡死，提前结束。
                    if self.pending.is_some() {
                        stalled += 1;
                        if stalled > 64 {
                            tracing::warn!("解码状态卡死（send/receive 均 EAGAIN），按 EOF 结束本次读取");
                            return Ok(None);
                        }
                    } else {
                        stalled = 0; // 送进去了，重置
                    }
                }
                None => {
                    // 文件读完：两路一起进入 draining。
                    // draining 期间重复 send_eof 会返回 Eof，是正常信号，忽略。
                    let _ = self.decoder.send_eof();
                    if let Some(a) = self.audio.as_mut() {
                        a.decoder.send_eof();
                    }
                    self.draining = true;
                }
            }
        }
    }

    fn try_next_audio(&mut self) -> Result<Option<AudioChunk>> {
        if self.audio.is_none() {
            return Ok(None);
        }

        // 1) 解码器里有现成的先交出去。借用即用即还，后面还要碰 self 其他字段。
        if let Some(c) = self.audio.as_mut().unwrap().decoder.receive()? {
            return Ok(Some(c));
        }

        // 2) 解码器见底：继续读包喂它，把解复用位置向前推。
        //    途中的视频包**不解码**（帧没地方放），压进 backlog 等正常流程
        //    处理；有界防失控。这样反复调用就能把音频从「解码位置」一直
        //    抽到「播放位置 + 数百 ms」，而不是被视频帧的就绪节奏锁死在实时线。
        for _ in 0..MAX_PUMP_VIDEO_HELD {
            if self.draining {
                return Ok(None);
            }
            let packet = match self.read_packet() {
                Ok(Some(p)) => p,
                // EOF：不在这里置 draining（那是 next_event/EOF 流程的职责），
                // 泵只管抽，抽不动就返回。
                _ => return Ok(None),
            };
            let index = packet.stream();
            if index == self.stream_index {
                probe::packet();
                self.video_backlog.push_back(packet);
                continue;
            }
            let is_audio = self.audio.as_ref().is_some_and(|a| a.index == index);
            if !is_audio {
                // 字幕等无人认领的流，与 dispatch 的处理一致：丢弃。
                probe::packet();
                continue;
            }
            let accepted = self
                .audio
                .as_mut()
                .unwrap()
                .decoder
                .send(&packet)?;
            probe::packet();
            if !accepted {
                // 音频解码器输入满：包按规矩存 pending，下次先排它。
                self.pending = Some(packet);
                break;
            }
            if let Some(c) = self.audio.as_mut().unwrap().decoder.receive()? {
                return Ok(Some(c));
            }
        }
        Ok(None)
    }

    fn seek(&mut self, ts: Duration) -> Result<()> {
        let ts_us = ts.as_micros() as i64;
        // 开始一次新 seek：清取消标志。这样「本次 seek」不会被自己打断，
        // 只有之后更新的 Preview 置位才会中断它（抢占取消）。
        self.cancel.store(false, Ordering::Relaxed);
        // avformat_seek_file：stream_index=-1 表示 ts 单位是微秒（AV_TIME_BASE）。
        // 若期间主线程置位了 cancel（新 Preview 到达），ffmpeg 的 interrupt 回调
        // 令本次 seek 以 AVERROR_EXIT 干净返回——上下文一致、可安全重试。
        match self.input.seek(ts_us, ..ts_us) {
            Ok(()) => {}
            Err(Error::Exit) => {
                // 被更新的 seek 请求抢占：不要当成功（否则会停在错误半途位置），
                // 也不要当真失败。返回哨兵错误，让上层读到最新命令重新 seek。
                return Err(SeekCancelled.into());
            }
            Err(e) => return Err(e.into()),
        }
        // seek 后必须 flush 解码器，丢弃残留守帧，否则花屏。
        self.decoder.flush();
        if let Some(a) = self.audio.as_mut() {
            a.decoder.flush();
            a.flushing_resampler = false;
        }
        // 暂存的 packet 属于 seek 前的位置，留着会在新位置插进一段旧内容。
        self.pending = None;
        // 续杯攒下的视频包同理：全是旧位置的压缩包，必须丢弃。
        self.video_backlog.clear();
        // seek 到文件中间后又能继续读了，draining 状态必须撤销，
        // 否则播到过结尾再 seek 回去会立刻又报 EOF。
        self.draining = false;
        Ok(())
    }
}

impl FfmpegSource {
    /// 从一个已打开的输入上下文构造。
    ///
    /// 与 [`MediaSource::open`] 的区别只在于「怎么拿到 `Input`」：
    /// `open` 负责本地路径校验，这里则接受任何来源（含测试里构造的流），
    /// 好处是解码逻辑可以脱离文件系统单独测试。
    pub(crate) fn from_input_with(
        input: ffmpeg_next::format::context::Input,
        audio_format: Option<AudioFormat>,
    ) -> Result<Self> {
        // 找最佳视频流。
        let stream = input
            .streams()
            .best(ffmpeg_next::media::Type::Video)
            .ok_or_else(|| anyhow::anyhow!("no video stream in source"))?;
        let stream_index = stream.index();
        let time_base = stream.time_base();

        let ctx = ffmpeg_next::codec::context::Context::from_parameters(stream.parameters())?;
        let decoder = ctx.decoder().video()?;

        let (width, height) = (decoder.width(), decoder.height());
        let scaler = Scaler::get(
            decoder.format(),
            width,
            height,
            Pixel::BGRA,
            width,
            height,
            Flags::BILINEAR,
        )?;

        // 时长：优先流时长，回退到容器时长（均基于 time_base，转秒）。
        let duration = Duration::from_secs_f64(
            stream.duration().unsigned_abs() as f64 * f64::from(time_base.numerator())
                / f64::from(time_base.denominator()),
        );
        let fps = {
            let r = stream.avg_frame_rate();
            if r.denominator() != 0 {
                r.numerator() as f64 / r.denominator() as f64
            } else {
                0.0
            }
        };

        // 音频是可选的：没开启、或文件本身无音轨，都只是"没有声音"，
        // 不该让打开失败——纯视频素材是常见输入。
        let audio = match audio_format {
            Some(fmt) => match input.streams().best(ffmpeg_next::media::Type::Audio) {
                Some(s) => {
                    let index = s.index();
                    Some(AudioTrack {
                        index,
                        decoder: AudioDecoder::new(&s, fmt)?,
                        flushing_resampler: false,
                    })
                }
                None => None,
            },
            None => None,
        };

        Ok(Self {
            input,
            stream_index,
            time_base,
            decoder,
            scaler,
            raw_frame: Video::empty(),
            rgba_frame: Video::empty(),
            pending: None,
            video_backlog: std::collections::VecDeque::new(),
            width,
            height,
            duration,
            fps,
            audio,
            draining: false,
            cancel: Arc::new(AtomicBool::new(false)),
        })
    }

    /// 带「可中断 seek」的打开：拖动中能取消进行中的 seek（对齐 Chromium
    /// `CancelPendingSeek`）。
    ///
    /// 与 [`MediaSource::open_with`] 的区别只在于底层 `Input` 用
    /// `input_with_interrupt` 打开，并把 `cancel` 标志挂到 ffmpeg 的
    /// interrupt 回调上。普通打开（无取消需求，如 ocr-lab 抽帧）仍走
    /// [`MediaSource::open_with`]，不受影响。
    pub fn open_with_interrupt(
        path: &Path,
        audio_format: Option<AudioFormat>,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self> {
        ffmpeg_next::init()?;

        if !path.exists() {
            return Err(anyhow::anyhow!("source not found: {}", path.display()));
        }

        // 闭包捕获 cancel 的一个 clone；ffmpeg 在每次 I/O 阻塞时调用它，
        // 返回 true 即中断当前操作。回调发生在解码线程（同一个线程），
        // `Arc<AtomicBool>` 是 Send+Sync，安全。
        let cb = cancel.clone();
        let input = ffmpeg_next::format::input_with_interrupt(path, move || {
            cb.load(Ordering::Relaxed)
        })?;

        let mut this = Self::from_input_with(input, audio_format)?;
        this.cancel = cancel; // 用调用方提供的同一份，UI 侧持有另一个 clone
        Ok(this)
    }

    /// 取取消标志的 clone（供 UI 侧在发新 Preview 前置位）。
    pub fn cancel_handle(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    /// 设置播放速度倍率（转发到音频重采样器）。无音轨时静默忽略——
    /// 视频流无变速概念，音频主时钟不存在时本就是墙钟、按真实时间走。
    pub fn set_speed(&mut self, speed: f64) {
        if let Some(a) = self.audio.as_mut() {
            a.decoder.set_speed(speed);
        }
    }

    /// 看看两个解码器里有没有已经解好、可以直接交付的东西。
    ///
    /// 视频优先只是个任意选择：同一轮里两者都有产出的情况很少，
    /// 且调用方本来就要按 PTS 自行调度，谁先出来不影响正确性。
    fn take_ready(&mut self) -> Result<Option<MediaEvent>> {
        match self.decoder.receive_frame(&mut self.raw_frame) {
            Ok(()) => return Ok(Some(MediaEvent::Video(self.frame_to_decoded()?))),
            Err(Error::Other { errno }) if errno == ffmpeg_next::error::EAGAIN => {}
            // Eof 只说明**这一路**吐完了，另一路可能还有货，所以不能就此返回 None。
            Err(Error::Eof) => {}
            Err(e) => return Err(e.into()),
        }

        if let Some(a) = self.audio.as_mut() {
            match a.decoder.receive()? {
                Some(c) => return Ok(Some(MediaEvent::Audio(c))),
                // receive 把 EAGAIN 和 Eof 都映射成 None。draining 阶段的 None
                // 就意味着解码器空了，该轮到重采样器交尾巴了。
                None if self.draining => a.flushing_resampler = true,
                None => {}
            }
        }

        Ok(None)
    }

    /// 把一个 packet 送进它所属的解码器。
    ///
    /// 输入满（EAGAIN）时暂存到 `pending`，下一轮先 receive 排空再重试
    /// ——**不能丢弃**，丢一个音频包就是一段静音，丢一个视频包会花屏到下个关键帧。
    fn dispatch(&mut self, packet: Packet) -> Result<()> {
        let index = packet.stream();

        if index == self.stream_index {
            match self.decoder.send_packet(&packet) {
                Ok(()) => {}
                Err(Error::Other { errno }) if errno == ffmpeg_next::error::EAGAIN => {
                    self.pending = Some(packet);
                }
                Err(e) => return Err(e.into()),
            }
            return Ok(());
        }

        if let Some(a) = self.audio.as_mut()
            && index == a.index
            && !a.decoder.send(&packet)?
        {
            self.pending = Some(packet);
        }
        // 其余流（字幕等）无人认领，丢弃。
        Ok(())
    }

    /// 读出下一个 packet（不分流）；返回 `Ok(None)` 表示文件真正读完。
    ///
    /// 分流交给 [`dispatch`](Self::dispatch)：在这里过滤会让「跳过的包」和
    /// 「没有的包」混在一起，音频那一路就永远等不到自己的数据。
    ///
    /// 这里刻意不用 `input.packets()` 迭代器：它把「真 EOF」和「I/O 错误」
    /// 都折叠成 `None`（见 rust-ffmpeg `PacketIter::next`），调用方无从区分，
    /// 于是读盘出错会被静默当成「正常播完」。直接驱动 [`Packet::read`]
    /// 才能拿到原始错误码，从而分三类处理：
    ///   - `Eof`         → 真正结束
    ///   - `InvalidData` → 单个坏包，解复用器可以重新同步，跳过继续
    ///   - 其它          → 终止性错误（I/O 失败、读取被取消），向上报错
    ///
    /// 注意终止性错误**不能重试**：它会被记录进 `AVIOContext->pb->error`，
    /// 之后每次 `av_read_frame` 都返回同一个错误。实测旧写法遇到连接中断时
    /// 会 98% CPU 空转且永不退出——回归测试见本文件底部的
    /// `read_error_surfaces_instead_of_looking_like_eof`。
    fn read_packet(&mut self) -> Result<Option<Packet>> {
        loop {
            let mut packet = Packet::empty();
            match packet.read(&mut self.input) {
                Ok(()) => return Ok(Some(packet)),
                Err(Error::Eof) => return Ok(None),
                // 坏包：解复用器能越过 AVERROR_INVALIDDATA 重新同步，跳过即可。
                Err(Error::InvalidData) => continue,
                // interrupt 回调令本次 I/O 中断（ffmpeg 返回 AVERROR_EXIT）。
                // 这正是「可抢占 seek」机制的一部分：它不只是 seek 会被打断，
                // 普通读帧（`av_read_frame`）在 I/O 阻塞时同样会被打断——因为
                // 新 Preview 到达时主线程置了 cancel=true。这不是故障，而是
                // 「有更新的 seek 要处理」的信号：归一成 SeekCancelled，让上层
                // 回到命令循环跳到最新目标。否则会被误判为致命错误、解码线程退出。
                // 注意：只有 cancel 置位才会触发 Exit；正常播放 cancel 恒为 false，
                // 不会走到这里，真实 I/O 错误仍走下面的 Err 分支如实上报。
                Err(Error::Exit) => return Err(SeekCancelled.into()),
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// 把当前 raw_frame 转 BGRA 后拷成 [`DecodedFrame`]，并换算 PTS 为 Duration。
    fn frame_to_decoded(&mut self) -> Result<DecodedFrame> {
        // 原始帧 → BGRA（scaler 在 rgba_frame 为空时自动 alloc）。
        self.scaler.run(&self.raw_frame, &mut self.rgba_frame)?;

        let width = self.width;
        let height = self.height;
        let stride = self.rgba_frame.stride(0);
        // 注意：PTS 必须取自解码出的原始帧 raw_frame，而非 scaler 输出帧 rgba_frame。
        // swscale 生成的输出帧不带时间戳，rgba_frame.timestamp() 恒为 None。
        // 负值钳 0：B 帧开头的流首帧 best-effort PTS 可能为负，Duration 不接受。
        let pts = match self.raw_frame.timestamp() {
            Some(ts) => Duration::from_secs_f64(
                (ts as f64 * f64::from(self.time_base.numerator())
                    / f64::from(self.time_base.denominator()))
                .max(0.0),
            ),
            None => Duration::ZERO,
        };

        // 复制像素（含 stride 填充）。GPU 上传时由调用方决定要不要紧凑化。
        let data = self.rgba_frame.data(0).to_vec();

        Ok(DecodedFrame {
            data,
            stride,
            width,
            height,
            pts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把测试素材重封装成 MPEG-TS 字节流（`-c copy`，不重新编码）。
    ///
    /// 为什么必须是 TS：mp4 的 `moov` 在文件尾，数据截断后**打开阶段**就失败，
    /// 根本走不到解码循环，也就测不到 `next_frame` 的错误处理。
    /// TS 是流式容器，头部信息在最前面，残缺数据照样能开。
    ///
    /// 不把 .ts 存进仓库：它只服务于这一条测试，且能从 sample.mp4 无损推导。
    /// ffmpeg 命令不可用时返回 None，让测试跳过而不是误报失败。
    fn sample_as_mpegts() -> Option<Vec<u8>> {
        let sample =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/assets/sample.mp4");
        let out = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(sample)
            .args(["-c", "copy", "-f", "mpegts", "pipe:1"])
            .output()
            .ok()?;
        (out.status.success() && !out.stdout.is_empty()).then_some(out.stdout)
    }

    /// 读取中断必须报错，不能伪装成「正常播完」。
    ///
    /// 背景：旧实现用 `input.packets()` 迭代器，而 rust-ffmpeg 的
    /// `PacketIter::next` 把**所有**错误都折叠成 `None`，于是网络中断/读盘失败
    /// 和真 EOF 长得一模一样。后果不止是误报播完：`next_frame` 收到 `None` 会
    /// 去 `send_eof` 进入 draining，而终止性错误已被记进 `AVIOContext->pb->error`
    /// 且是**粘性**的，之后每次 `av_read_frame` 都返回它，外层循环便一直空转
    /// ——实测跑满 98% CPU 且永不退出。
    ///
    /// 怎么造出这个错误：本地文件造不出来。截断文件、FIFO 关闭写端、concat 分片
    /// 被截短，在操作系统看来**都是干净的 EOF**——OS 无法表达「本该还有数据」。
    /// 只有声明了预期长度的协议才能发现缺斤少两，故这里起一个假 HTTP 服务：
    /// 响应头写完整的 `Content-Length`，实际只发 1/4 就断开。
    ///
    /// 注意这里走 [`FfmpegSource::from_input_with`] 而非 `open`：`open` 只接受
    /// 存在的本地路径，这是有意的约束，不该为了测试去放宽它。
    /// `open_with_interrupt` 与 `input_with_interrupt` 打开的文件必须能正常解码与
    /// seek——interrupt 回调挂上去不应影响正常路径（否则拖动外的播放全坏）。
    ///
    /// 这是可抢占 seek 重构的回归护栏：确认「带取消标志打开」不引入功能回退。
    #[test]
    fn open_with_interrupt_decodes_and_seeks() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/assets/sample.mp4");
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut src = FfmpegSource::open_with_interrupt(&path, None, cancel)
            .expect("带 interrupt 打开应成功");

        // 正常解码若干帧。
        let mut frames = 0u32;
        while frames < 30 {
            match src.next_frame() {
                Ok(Some(_)) => frames += 1,
                Ok(None) => break,
                Err(e) => panic!("解码失败: {e}"),
            }
        }
        assert!(frames >= 30, "应至少解出 30 帧，实际 {frames}");

        // 没有被取消过：seek 必须成功（不返回 SeekCancelled）。
        src.seek(Duration::from_millis(500))
            .expect("未取消时 seek 应成功");
        // seek 后仍能继续解帧。
        assert!(src.next_frame().unwrap().is_some(), "seek 后应能继续解帧");
    }

    #[test]
    fn read_error_surfaces_instead_of_looking_like_eof() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let Some(data) = sample_as_mpegts() else {
            eprintln!("跳过：ffmpeg 命令不可用，无法生成 TS");
            return;
        };
        let total = data.len();
        // 只发一小部分就断开，确保失败发生在「读到一半」而非一开始。
        let cut = total / 4;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        let server = std::thread::spawn(move || {
            // 只服务一个连接就退出。用 incoming() 的第一个即可。
            if let Some(Ok(mut stream)) = listener.incoming().next() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let hdr = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\n\
                     Content-Type: video/mp2t\r\nAccept-Ranges: none\r\n\r\n"
                );
                if stream.write_all(hdr.as_bytes()).is_ok() {
                    let _ = stream.write_all(&data[..cut]);
                    // 承诺了 total 字节却只发 cut 就关闭：http 协议层会发现
                    // 「还差一大截」，报连接中断而非当成干净 EOF。
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
            }
        });

        let url = format!("http://{addr}/sample.ts");

        // 解码放子线程 + 超时：错误被吞掉时的症状是**死循环空转**，
        // 直接在测试线程跑会把 CI 挂死。看门狗把「卡死」转成明确的失败信息。
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let verdict = (|| {
                ffmpeg_next::init().map_err(|e| format!("ffmpeg init 失败: {e}"))?;
                let input = ffmpeg_next::format::input(&url)
                    .map_err(|e| format!("截断的 TS 应该能正常打开，却失败了: {e}"))?;
                let mut src = FfmpegSource::from_input_with(input, None)
                    .map_err(|e| format!("构造失败: {e}"))?;

                let mut count = 0u32;
                loop {
                    match src.next_frame() {
                        Ok(Some(_)) => count += 1,
                        Ok(None) => {
                            return Err(format!(
                                "读取被中断却报告成正常 EOF（{count} 帧后），错误被吞了"
                            ));
                        }
                        Err(_) => return Ok(count), // 期望路径：错误如实上报
                    }
                    if count > 1_000 {
                        return Err("解出的帧数远超样本总量，疑似死循环".into());
                    }
                }
            })();
            let _ = tx.send(verdict);
        });

        let verdict = rx
            .recv_timeout(Duration::from_secs(30))
            .unwrap_or_else(|_| Err("解码线程 30s 未返回：错误很可能被吞掉后陷入空转".into()));
        let _ = server.join();

        match verdict {
            Ok(count) => assert!(count < 300, "连接中途就断了，不该解出完整的 {count} 帧"),
            Err(msg) => panic!("{msg}"),
        }
    }
}
