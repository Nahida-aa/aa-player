//! 手动验证音频输出真的能出声（自动化测试没法验证「听得见」）。
//!
//! 用法：cargo run -p player-core --example beep
//! 预期：听到 2 秒 440Hz 的「嘟——」声，并打印时钟读数。

use std::time::Duration;

use player_core::AudioOutput;

fn main() -> anyhow::Result<()> {
    let out = AudioOutput::new()?;
    let fmt = out.format();
    println!("设备: {} Hz, {} 声道", fmt.sample_rate, fmt.channels);

    // 生成 2 秒 440Hz 正弦波，交错填满所有声道。
    let secs = 2.0;
    let total_frames = (fmt.sample_rate as f64 * secs) as usize;
    let mut samples = Vec::with_capacity(total_frames * fmt.channels as usize);
    for i in 0..total_frames {
        let t = i as f64 / fmt.sample_rate as f64;
        // 振幅 0.2，避免太吵。
        let v = (t * 440.0 * std::f64::consts::TAU).sin() as f32 * 0.2;
        for _ in 0..fmt.channels {
            samples.push(v);
        }
    }
    out.push_samples(&samples);
    println!("已排入 {total_frames} 帧，开始播放…");

    // 每 500ms 打一次时钟读数，确认它随播放推进。
    for _ in 0..5 {
        std::thread::sleep(Duration::from_millis(500));
        println!(
            "  position={:?}  剩余队列={} 帧  欠载={}",
            out.position(),
            out.queued_frames(),
            out.take_underrun()
        );
    }

    println!("结束。position 应接近 2.5s，队列应为 0。");
    Ok(())
}
