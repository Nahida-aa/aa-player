//! 隔离实验：只测 MediaSource::next_event 的吞吐，不涉及 GUI/声卡。
//! 用法：cargo run -p player-core --example pump_bench -- /path/to/video.mp4 [秒数]
//!
//! 判读：audio_xreal（音频产出倍速）应远大于 1（解码比实时快得多）；
//! 若 ≈1 或 <1，说明解码层本身跟不上实时 → 问题在 MediaSource/FFmpeg；
//! 若远大于 1，说明解码层没问题 → 问题在 controller 的调度/背压。

use std::time::{Duration, Instant};

use player_core::{audio_output::AudioFormat, media_source::MediaSource, FfmpegSource, MediaEvent};

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("用法: pump_bench <video> [秒数]");
    let budget = std::env::args()
        .nth(2)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(10);
    let budget = Duration::from_secs(budget);

    // 模拟真实设备格式（PipeWire 默认 48k 立体声）。
    let fmt = AudioFormat { sample_rate: 48000, channels: 2 };
    let mut src = FfmpegSource::open_with(std::path::Path::new(&path), Some(fmt))?;
    if let Some(a) = src.audio_info() {
        println!("音轨: {}Hz {}ch", a.sample_rate, a.channels);
    }

    let t0 = Instant::now();
    let mut video = 0u64;
    let mut audio_frames = 0u64; // 声卡帧数（一帧=一次采样点/声道组）
    let mut decode_us = 0u128;
    let mut events: Vec<(char, u128)> = Vec::new(); // (类型, 相对时刻µs) 用于看节奏

    while t0.elapsed() < budget {
        let e0 = Instant::now();
        match src.next_event()? {
            Some(MediaEvent::Video(_)) => {
                video += 1;
                decode_us += e0.elapsed().as_micros();
                events.push(('V', e0.duration_since(t0).as_micros()));
            }
            Some(MediaEvent::Audio(c)) => {
                audio_frames += (c.samples.len() / fmt.channels as usize) as u64;
                decode_us += e0.elapsed().as_micros();
                events.push(('A', e0.duration_since(t0).as_micros()));
            }
            None => break,
        }
    }
    let wall = t0.elapsed().as_secs_f64();
    let audio_secs = audio_frames as f64 / fmt.sample_rate as f64;
    // 音频 PTS 推进 vs 实际耗时：>1 表示解得比实时快。
    println!(
        "wall={wall:.2}s video_frames={video} ({:.1}fps) audio={audio_secs:.2}s \
         (x{:.2} 实时) 解码占用CPU={:.1}%",
        video as f64 / wall,
        audio_secs / wall,
        100.0 * decode_us as f64 / (wall * 1_000_000.0)
    );

    // 每 500ms 窗口内的音频毫秒数——看音频产出是否有周期性断流。
    let mut win_start = 0u128;
    let mut i = 0usize;
    let mut audio_ms_in_win = 0f64;
    let mut rows: Vec<String> = Vec::new();
    let mut last_v = 0u32;
    while i < events.len() {
        let (kind, ts) = events[i];
        if ts - win_start > 500_000 {
            rows.push(format!(
                "  [{:>5}.{:+03}s] A={:>5.0}ms V={last_v}",
                win_start / 1_000_000,
                format!("{:03}", win_start / 1_000 % 1000),
                audio_ms_in_win
            ));
            win_start = ts;
            audio_ms_in_win = 0.0;
            last_v = 0;
        }
        if kind == 'A' {
            audio_ms_in_win += 1024.0 * 1000.0 / 44100.0; // AAC 每帧 1024 样本@44.1k
        } else {
            last_v += 1;
        }
        i += 1;
    }
    for r in rows.iter().take(20) {
        println!("{r}");
    }
    Ok(())
}
