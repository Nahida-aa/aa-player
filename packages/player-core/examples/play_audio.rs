//! 播放一个媒体文件的音轨（无视频），用于人工听感验证。
//!
//! 运行：cargo run -p player-core --example play_audio -- <文件路径>
//! 不给路径时用测试样本。

use std::time::Duration;

use ffmpeg_next::{Error, Packet};
use player_core::{AudioDecoder, AudioOutput};

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| format!("{}/tests/assets/sample.mp4", env!("CARGO_MANIFEST_DIR")));

    let out = AudioOutput::new()?;
    let fmt = out.format();
    println!("设备: {} Hz, {} 声道", fmt.sample_rate, fmt.channels);

    ffmpeg_next::init()?;
    let mut input = ffmpeg_next::format::input(&path)?;
    let stream = input
        .streams()
        .best(ffmpeg_next::media::Type::Audio)
        .ok_or_else(|| anyhow::anyhow!("没有音频流"))?;
    let index = stream.index();
    let mut dec = AudioDecoder::new(&stream, fmt)?;
    let info = dec.info();
    println!(
        "音轨: {} Hz, {} 声道, {:.2}s",
        info.sample_rate,
        info.channels,
        info.duration.as_secs_f64()
    );

    let mut pending: Option<Packet> = None;
    let mut eof_sent = false;
    let mut pushed = Duration::ZERO;

    loop {
        // 背压：队列里攒够 0.5s 就先别解了，免得把整轨都塞进内存。
        // 真实播放器里这一步同样必要，否则长片会把 RAM 吃光。
        while out.queued_frames() > fmt.sample_rate as usize / 2 {
            std::thread::sleep(Duration::from_millis(10));
        }

        if !eof_sent {
            let packet = match pending.take() {
                Some(p) => Some(p),
                None => loop {
                    let mut p = Packet::empty();
                    match p.read(&mut input) {
                        Ok(()) if p.stream() == index => break Some(p),
                        Ok(()) => continue,
                        Err(Error::Eof) => break None,
                        Err(e) => return Err(e.into()),
                    }
                },
            };
            match packet {
                Some(p) => {
                    if !dec.send(&p)? {
                        pending = Some(p);
                    }
                }
                None => {
                    dec.send_eof();
                    eof_sent = true;
                }
            }
        }

        match dec.receive()? {
            Some(c) => {
                pushed += c.duration();
                out.push_samples(&c.samples);
                if out.take_underrun() {
                    println!(
                        "  欠载！已送入 {:.2}s，设备时钟 {:.2}s，队列 {} 帧，超前 {:.3}s",
                        pushed.as_secs_f64(),
                        out.position().as_secs_f64(),
                        out.queued_frames(),
                        pushed.as_secs_f64() - out.position().as_secs_f64(),
                    );
                }
            }
            None if eof_sent => break,
            None => continue,
        }
    }
    while let Some(c) = dec.flush_resampler()? {
        pushed += c.duration();
        out.push_samples(&c.samples);
    }
    println!("已送入 {:.2}s 音频，等待播完…", pushed.as_secs_f64());

    // 等**设备时钟**追上已送入的时长，而不是等队列长度归零。
    //
    // 这两者差着一个回调周期：队列刚被最后一次回调取空的那一刻，
    // 这批采样其实还在声卡缓冲里没出声。此时退出会截掉尾巴，
    // 并且那次回调因取不满而补了静音，会点亮 underrun ——
    // 看着像"解码跟不上"，实则是收尾姿势不对。
    // 播放途中有没有欠载，才是真正需要关心的信号；收尾时最后一次回调
    // 必然取不满一整个缓冲，那是正常的结束姿势，不该混为一谈。
    let starved_midway = out.take_underrun();

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
