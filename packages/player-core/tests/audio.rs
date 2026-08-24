//! 音频解码链路的集成测试。
//!
//! 用同一个样本（sample.mp4，AAC 单声道 48kHz / 10s）。
//!
//! 这里刻意**不只测「解出了非空数据」**。空/非空只能证明代码跑通，
//! 证明不了声音是对的：声道搞反、重采样比率写错、平面数据当交错读，
//! 都能产出一大堆非空 f32，却是噪声。所以核心断言是对波形本身的检查
//! ——主频、峰值、总时长——这些错了必然能听出来。
//!
//! 参考值来自 `ffmpeg -i sample.mp4 -vn -f f32le -ar 48000 -ac 1`：
//! 主频 172 Hz、峰值 0.515、时长 10.005s。
//!
//! 运行：cargo test -p player-core --test audio

use std::path::PathBuf;
use std::time::Duration;

use player_core::{AudioChunk, AudioDecoder, AudioFormat, FfmpegSource, MediaEvent, MediaSource};

/// 素材里的主频（Hz）。用 ffmpeg 解出的参考 PCM 做 FFT 得到。
const DOMINANT_HZ: f64 = 172.0;
/// 素材时长（秒）。
const SAMPLE_SECS: f64 = 10.0;

fn sample_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/assets/sample.mp4")
}

/// 解出整条音轨。这是 Task #9（统一的音视频接口）落地前的临时驱动逻辑：
/// 手动 demux，只喂音频流的 packet。
fn decode_all(target: AudioFormat) -> Vec<AudioChunk> {
    use ffmpeg_next::{Error, Packet};

    ffmpeg_next::init().expect("ffmpeg init");
    let mut input = ffmpeg_next::format::input(&sample_path()).expect("open");
    let stream = input
        .streams()
        .best(ffmpeg_next::media::Type::Audio)
        .expect("样本应当有音频流");
    let index = stream.index();
    let mut dec = AudioDecoder::new(&stream, target).expect("建解码器");

    let mut chunks = Vec::new();
    let mut pending: Option<Packet> = None;
    let mut eof_sent = false;

    loop {
        if !eof_sent {
            let packet = match pending.take() {
                Some(p) => Some(p),
                None => loop {
                    let mut p = Packet::empty();
                    match p.read(&mut input) {
                        Ok(()) if p.stream() == index => break Some(p),
                        Ok(()) => continue, // 视频包，跳过
                        Err(Error::Eof) => break None,
                        Err(e) => panic!("读包失败: {e}"),
                    }
                },
            };
            match packet {
                // send 返回 false = 解码器输入满，暂存重试
                Some(p) => {
                    if !dec.send(&p).expect("send") {
                        pending = Some(p);
                    }
                }
                None => {
                    dec.send_eof();
                    eof_sent = true;
                }
            }
        }

        match dec.receive().expect("receive") {
            Some(c) => chunks.push(c),
            None if eof_sent => break,
            None => continue,
        }

        assert!(chunks.len() < 10_000, "块数远超预期，疑似死循环");
    }

    // 重采样器尾部残留也要取出来，否则结尾会短一小截。
    while let Some(c) = dec.flush_resampler().expect("flush") {
        chunks.push(c);
    }
    chunks
}

/// 把所有块拼成单声道 f32（多声道时取第一声道）。
fn to_mono(chunks: &[AudioChunk]) -> Vec<f32> {
    let ch = chunks.first().map_or(1, |c| c.channels).max(1) as usize;
    chunks
        .iter()
        .flat_map(|c| c.samples.iter().step_by(ch).copied())
        .collect()
}

/// 朴素 DFT 求指定频率处的幅度。
///
/// 为什么不做完整 FFT：测试里只需要「某几个频点上能量对不对」，
/// 单点 DFT 十几行就够，比引入 FFT 依赖划算。
fn magnitude_at(samples: &[f32], hz: f64, rate: u32) -> f64 {
    let n = samples.len().min(rate as usize); // 只用头 1 秒，够分辨了
    let (mut re, mut im) = (0.0f64, 0.0f64);
    for (i, &s) in samples.iter().take(n).enumerate() {
        // 加汉宁窗抑制频谱泄漏，否则相邻频点会互相污染，
        // 「主频最强」这个断言就会变得不稳。
        let w = 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / n as f64).cos();
        let theta = std::f64::consts::TAU * hz * i as f64 / rate as f64;
        re += s as f64 * w * theta.cos();
        im -= s as f64 * w * theta.sin();
    }
    (re * re + im * im).sqrt() / n as f64
}

#[test]
fn decodes_audio_with_expected_waveform() {
    let target = AudioFormat {
        sample_rate: 48_000,
        channels: 1,
    };
    let chunks = decode_all(target);
    assert!(!chunks.is_empty(), "应当解出音频块");

    let mono = to_mono(&chunks);
    let secs = mono.len() as f64 / target.sample_rate as f64;
    assert!(
        (secs - SAMPLE_SECS).abs() < 0.2,
        "解出的音频总时长 {secs:.3}s，应接近 {SAMPLE_SECS}s"
    );

    // 峰值：参考值 0.515。太小说明增益丢了，超过 1.0 说明溢出（会削波）。
    let peak = mono.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    assert!(
        (0.3..=1.0).contains(&peak),
        "峰值 {peak:.3} 不合理（参考 0.515）"
    );

    // 主频必须显著强于邻近频点。这一条是真正能抓住"解码错了"的断言：
    // 数据结构对不上（平面当交错、声道错位）会让波形变成噪声，
    // 噪声的频谱是平的，压不出这个比值。
    let at_main = magnitude_at(&mono, DOMINANT_HZ, target.sample_rate);
    for probe in [80.0, 400.0, 1000.0, 5000.0] {
        let other = magnitude_at(&mono, probe, target.sample_rate);
        assert!(
            at_main > other * 3.0,
            "{DOMINANT_HZ}Hz 应远强于 {probe}Hz，实际 {at_main:.5} vs {other:.5}"
        );
    }
}

#[test]
fn chunk_pts_advances_monotonically() {
    let target = AudioFormat {
        sample_rate: 48_000,
        channels: 1,
    };
    let chunks = decode_all(target);

    let mut prev = Duration::ZERO;
    for (i, c) in chunks.iter().enumerate() {
        assert!(c.pts >= prev, "第 {i} 块 PTS 倒退：{:?} < {prev:?}", c.pts);
        prev = c.pts;
    }
    // 最后一块应当接近片尾，说明 PTS 走完了全程而不是一直贴着 0。
    let last = chunks.last().expect("非空").pts.as_secs_f64();
    assert!(last > SAMPLE_SECS - 1.0, "末块 PTS 只到 {last:.3}s，太靠前");
}

#[test]
fn resamples_to_device_rate_and_channels() {
    // 设备常见的 44.1kHz 立体声：采样率与声道数都与素材（48k 单声道）不同。
    // 这一条守的是"重采样真的发生了"——若比率算错，时长会整体偏移；
    // 若上混没做，交错读取会把相邻采样错当成两个声道，听感是变调。
    let target = AudioFormat {
        sample_rate: 44_100,
        channels: 2,
    };
    let chunks = decode_all(target);
    assert!(!chunks.is_empty());

    for c in &chunks {
        assert_eq!(c.channels, 2, "声道数应为设备的 2");
        assert_eq!(c.sample_rate, 44_100, "采样率应为设备的 44100");
        assert_eq!(c.samples.len() % 2, 0, "交错数据长度必须是声道数的整数倍");
    }

    let frames: usize = chunks.iter().map(|c| c.frames()).sum();
    let secs = frames as f64 / 44_100.0;
    assert!(
        (secs - SAMPLE_SECS).abs() < 0.2,
        "重采样后时长 {secs:.3}s 应仍是 {SAMPLE_SECS}s——变了就说明比率算错"
    );

    // 单声道上混到立体声后，主频不该变。变了通常意味着采样率换算搞反
    // （48/44.1 写成 44.1/48），听起来就是整体升调或降调。
    let mono = to_mono(&chunks);
    let at_main = magnitude_at(&mono, DOMINANT_HZ, 44_100);
    let at_shifted = magnitude_at(&mono, DOMINANT_HZ * 48_000.0 / 44_100.0, 44_100);
    assert!(
        at_main > at_shifted,
        "主频仍应是 {DOMINANT_HZ}Hz，而非被比率写反后偏移的那个"
    );
}

// ---------------------------------------------------------------------------
// MediaSource 统一出口
// ---------------------------------------------------------------------------

fn device_format() -> AudioFormat {
    AudioFormat {
        sample_rate: 48_000,
        channels: 2,
    }
}

/// 把整个媒体拉完，分别收集音视频的 PTS。
fn drain_events(src: &mut FfmpegSource) -> (Vec<Duration>, Vec<Duration>, Duration) {
    let (mut video, mut audio) = (Vec::new(), Vec::new());
    let mut audio_total = Duration::ZERO;
    loop {
        match src.next_event().expect("next_event") {
            Some(MediaEvent::Video(f)) => video.push(f.pts),
            Some(MediaEvent::Audio(c)) => {
                audio_total += c.duration();
                audio.push(c.pts);
            }
            None => break,
        }
        assert!(
            video.len() + audio.len() < 10_000,
            "产出的单元数远超样本总量，疑似死循环"
        );
    }
    (video, audio, audio_total)
}

#[test]
fn source_yields_both_streams_when_audio_enabled() {
    let mut src =
        FfmpegSource::open_with(&sample_path(), Some(device_format())).expect("open_with");

    let info = src.audio_info().expect("样本有音轨，应报告音频信息");
    assert_eq!(info.sample_rate, 48_000);
    assert_eq!(info.channels, 1, "源是单声道（输出才是设备的 2 声道）");

    let (video, audio, audio_total) = drain_events(&mut src);

    // 10s @ 30fps ≈ 300 帧，与纯视频路径应当一致——加了音频不该少解视频。
    assert!(
        (280..=320).contains(&video.len()),
        "视频帧数 {} 不在预期区间",
        video.len()
    );
    assert!(!audio.is_empty(), "应当产出音频块");
    assert!(
        (audio_total.as_secs_f64() - SAMPLE_SECS).abs() < 0.2,
        "音频总时长 {:.3}s 应接近 {SAMPLE_SECS}s",
        audio_total.as_secs_f64()
    );
}

/// 两路必须**交错**产出，而不是先把一路全吐完再吐另一路。
///
/// 这是统一出口最容易写错的地方：只要 demux 循环里有一处按流过滤，
/// 就会变成「先解完整条视频，再解音频」。功能测试全都能过（数量、时长都对），
/// 但播放时需要把整条视频缓存在内存里才能等到第一块音频——
/// 短样本看不出来，长片直接 OOM。
#[test]
fn streams_are_interleaved_not_batched() {
    let mut src =
        FfmpegSource::open_with(&sample_path(), Some(device_format())).expect("open_with");

    // 记录产出顺序，看两种类型是否穿插。
    let mut order = Vec::new();
    loop {
        match src.next_event().expect("next_event") {
            Some(MediaEvent::Video(_)) => order.push('v'),
            Some(MediaEvent::Audio(_)) => order.push('a'),
            None => break,
        }
    }

    // 统计类型切换次数。真交错时会有几百次；一路吐完再吐另一路只有 1 次。
    let switches = order.windows(2).filter(|w| w[0] != w[1]).count();
    assert!(
        switches > 50,
        "音视频应当交错产出，实际只切换了 {switches} 次（看起来是分批吐的）"
    );

    // 头部也不该被某一路独占：前 20 个单元里两种都该出现。
    let head: String = order.iter().take(20).collect();
    assert!(
        head.contains('v') && head.contains('a'),
        "开头 20 个单元应两种都有，实际是 {head}"
    );
}

/// 两路的 PTS 必须同步推进，不能一路跑到片尾另一路还在开头。
#[test]
fn audio_and_video_pts_advance_together() {
    let mut src =
        FfmpegSource::open_with(&sample_path(), Some(device_format())).expect("open_with");

    let mut last_v = Duration::ZERO;
    let mut last_a = Duration::ZERO;
    let mut worst_gap = 0.0f64;
    let (mut n_v, mut n_a) = (0usize, 0usize);

    loop {
        match src.next_event().expect("next_event") {
            Some(MediaEvent::Video(f)) => {
                last_v = f.pts;
                n_v += 1;
            }
            Some(MediaEvent::Audio(c)) => {
                last_a = c.pts;
                n_a += 1;
            }
            None => break,
        }
        // 两路都开始产出之后才比较，否则启动阶段的空档会误判。
        if last_v > Duration::ZERO && last_a > Duration::ZERO {
            let gap = (last_v.as_secs_f64() - last_a.as_secs_f64()).abs();
            worst_gap = worst_gap.max(gap);
        }
    }

    // 先确认两路都真的产出过。少了这一条，"音频完全没解出来"会让上面的
    // 比较一次都不执行，worst_gap 停在 0，测试反而绿灯通过。
    assert!(
        n_v > 0 && n_a > 0,
        "两路都该有产出，实际 video={n_v} audio={n_a}"
    );

    // 容器本身的交错间隔通常在几百毫秒内。放宽到 1s 仍能抓住"分批吐"这种
    // 量级的错误（那会让间隔一路涨到接近整个片长）。
    assert!(
        worst_gap < 1.0,
        "音视频 PTS 最大偏离 {worst_gap:.3}s，两路没有同步推进"
    );
}

#[test]
fn audio_stays_off_unless_requested() {
    // 默认 open 不解音频：ocr-lab 那种纯抽帧场景不该为音频付出代价。
    let mut src = FfmpegSource::open(&sample_path()).expect("open");
    assert!(src.audio_info().is_none(), "未开启音频时不该报告音频信息");

    let (video, audio, _) = drain_events(&mut src);
    assert!(audio.is_empty(), "未开启音频时不该产出音频块");
    assert!((280..=320).contains(&video.len()));
}

/// `next_frame` 在开了音频时也要照常工作——它会把音频丢掉，但不能卡住。
#[test]
fn next_frame_skips_audio_without_stalling() {
    let mut src =
        FfmpegSource::open_with(&sample_path(), Some(device_format())).expect("open_with");

    let mut count = 0;
    while src.next_frame().expect("next_frame").is_some() {
        count += 1;
        assert!(count < 1_000, "疑似死循环");
    }
    assert!(
        (280..=320).contains(&count),
        "开了音频后 next_frame 仍应解出约 300 帧，实际 {count}"
    );
}

/// seek 之后两路都要回到新位置，且不能因为之前播到过结尾就直接报 EOF。
#[test]
fn seek_resets_both_streams() {
    let mut src =
        FfmpegSource::open_with(&sample_path(), Some(device_format())).expect("open_with");

    // 先一路播到结尾，把 draining 状态点亮。
    let _ = drain_events(&mut src);

    src.seek(Duration::from_secs(2)).expect("seek");

    let mut got_v = None;
    let mut got_a = None;
    for _ in 0..500 {
        match src.next_event().expect("next_event") {
            Some(MediaEvent::Video(f)) => got_v = got_v.or(Some(f.pts)),
            Some(MediaEvent::Audio(c)) => got_a = got_a.or(Some(c.pts)),
            None => break,
        }
        if got_v.is_some() && got_a.is_some() {
            break;
        }
    }

    // 关键：播到过结尾之后 seek 回去，必须还能继续解。
    // 若 draining 标志没撤销，这里会立刻拿到 None。
    let v = got_v.expect("seek 后应能解出视频帧（draining 状态没撤销？）");
    let a = got_a.expect("seek 后应能解出音频块");
    // seek 落到最近关键帧，允许比目标早一些。
    assert!(
        v.as_secs_f64() < 3.0 && a.as_secs_f64() < 3.0,
        "seek 到 2s 后，首个单元应在附近，实际 video={v:?} audio={a:?}"
    );
}

/// 回归：**seek 到 0 之后音频必须继续产出**。
///
/// 实测症状链：暂停→点击进度条向过去跳（目标被钳到 0）→恢复播放，
/// 音频时钟冻死在 0、永远静音；FFmpeg 打印「Could not update timestamps
/// for discarded samples」。本测试在媒体源层隔离该场景：播放一段后
/// seek(0)，后续事件流里必须还有音频块。
#[test]
fn seek_to_zero_keeps_audio_flowing() {
    let mut src =
        FfmpegSource::open_with(&sample_path(), Some(device_format())).expect("open_with");

    // 播一小段，离开文件头部。
    for i in 0..60 {
        if src.next_event().expect("next_event").is_none() {
            break;
        }
        let _ = i;
    }

    src.seek(Duration::ZERO).expect("seek(0)");

    // seek 后拉一大批事件，统计音频块。
    let mut audio_chunks = 0usize;
    let mut first_audio_pts = None;
    let mut video_frames = 0usize;
    for _ in 0..600 {
        match src.next_event().expect("next_event") {
            Some(MediaEvent::Video(_)) => video_frames += 1,
            Some(MediaEvent::Audio(c)) => {
                audio_chunks += 1;
                first_audio_pts = first_audio_pts.or(Some(c.pts));
            }
            None => break,
        }
    }

    assert!(video_frames > 0, "seek(0) 后应继续解出视频帧");
    assert!(
        audio_chunks > 0,
        "seek(0) 后 {video_frames} 帧视频里一块音频都没有——解码器干涸"
    );
    eprintln!("seek(0) 后：视频 {video_frames} 帧，音频 {audio_chunks} 块，首块 pts={first_audio_pts:?}");
}
