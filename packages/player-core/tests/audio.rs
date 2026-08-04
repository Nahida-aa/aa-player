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

use player_core::{AudioChunk, AudioDecoder, AudioFormat};

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
