//! 播放一个媒体文件的音轨（丢弃画面），用于人工听感验证。
//!
//! 运行：cargo run -p player-core --example play_audio -- <文件路径>
//! 不给路径时用测试样本。

use std::time::Duration;

use player_core::{AudioOutput, FfmpegSource, MediaEvent, MediaSource};

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| format!("{}/tests/assets/sample.mp4", env!("CARGO_MANIFEST_DIR")));

    let out = AudioOutput::new()?;
    let fmt = out.format();
    println!("设备: {} Hz, {} 声道", fmt.sample_rate, fmt.channels);

    let mut src = FfmpegSource::open_with(std::path::Path::new(&path), Some(fmt))?;
    match src.audio_info() {
        Some(i) => println!(
            "音轨: {} Hz, {} 声道, {:.2}s",
            i.sample_rate,
            i.channels,
            i.duration.as_secs_f64()
        ),
        None => {
            println!("该文件没有音轨");
            return Ok(());
        }
    }

    let mut pushed = Duration::ZERO;
    let mut frames = 0u32;

    loop {
        // 背压：队列攒够 0.5s 就先歇着，免得把整轨都塞进内存。
        // 长片没有这一步会直接吃光 RAM。
        while out.queued_frames() > fmt.sample_rate as usize / 2 {
            std::thread::sleep(Duration::from_millis(10));
        }

        match src.next_event()? {
            Some(MediaEvent::Audio(c)) => {
                pushed += c.duration();
                out.push_samples(&c.samples);
            }
            // 这个示例只关心声音，画面解出来直接丢。
            Some(MediaEvent::Video(_)) => frames += 1,
            None => break,
        }
    }

    println!(
        "已送入 {:.2}s 音频（顺带解了 {frames} 帧画面）",
        pushed.as_secs_f64()
    );

    // 播放途中有没有欠载才是要关心的信号；收尾时最后一次回调必然
    // 取不满一整个缓冲，那是正常的结束姿势，不该混为一谈。
    let starved_midway = out.take_underrun();

    // 等**设备时钟**追上已送入的时长，而不是等队列长度归零：
    // 队列刚被取空那一刻，这批采样其实还在声卡缓冲里没出声。
    while out.position() < pushed {
        std::thread::sleep(Duration::from_millis(20));
    }

    println!(
        "设备时钟 {:.2}s（已送入 {:.2}s），播放途中欠载: {}",
        out.position().as_secs_f64(),
        pushed.as_secs_f64(),
        starved_midway
    );
    Ok(())
}
