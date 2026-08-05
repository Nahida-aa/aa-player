//! 视频帧显示时钟（PlaybackClock）。
//!
//! 把帧的 PTS 映射到「现在该不该显示」。两种模式：
//! - **音频主时钟**（有音轨）：以声卡的播放进度为准（经 player-core 的
//!   `AudioClock`）。声卡按固定采样率消费，比 `Instant` 稳；落后太多丢帧。
//! - **墙钟**（无音轨）：`origin + pts` 模型，落后太多重置原点。
//!
//! 之所以一个类型：有无音轨要等解码线程打开文件后才知道，编译期分不开。

use std::time::{Duration, Instant};

use player_core::AudioClock;

/// 落后多久就直接丢帧（音频主时钟模式）。
const DROP_THRESHOLD: Duration = Duration::from_millis(100);
/// 落后多久就重置时间轴原点（墙钟模式）。
const RESYNC_THRESHOLD: Duration = Duration::from_millis(200);

/// 时钟对某一帧给出的调度决定。
#[derive(Debug, PartialEq, Eq)]
pub enum Schedule {
    /// 还没到点，等待这么久再显示。
    Wait(Duration),
    /// 已到点，立即显示。
    Now,
    /// 落后太多，跳过这一帧（音频主时钟模式）。
    Drop { behind: Duration },
    /// 落后太多，已重置原点；立即显示并附带落后量（墙钟模式）。
    Resynced { behind: Duration },
}

/// 把帧 PTS 映射到显示时刻的时钟。
pub struct PlaybackClock {
    /// 音频主时钟；`None` 或尚未出声时退回墙钟。
    audio: Option<AudioClock>,
    /// seek 锚定偏移（有符号微秒）= 首帧实际 pts − 当时音频位置。
    audio_offset: i64,
    /// 文件总时长（微秒），封顶音频主时钟读数。
    duration_us: u64,
    /// 墙钟模式的时间轴原点。
    origin: Option<Instant>,
}

impl PlaybackClock {
    /// 纯墙钟时钟（无音轨时用）。
    pub fn new() -> Self {
        Self {
            audio: None,
            audio_offset: 0,
            duration_us: 0,
            origin: None,
        }
    }

    /// 设置文件总时长（微秒），用于封顶音频主时钟读数。
    pub fn set_duration(&mut self, duration_us: u64) {
        self.duration_us = duration_us;
    }

    /// 更换音频主时钟句柄，但保留墙钟 origin。
    pub fn set_audio(&mut self, audio: AudioClock) {
        self.audio = Some(audio);
    }

    /// 重置墙钟时间轴原点，下一帧立即显示（seek 后调用）。
    pub fn reset_origin(&mut self) {
        self.origin = None;
    }

    /// 设置音频时钟偏移（有符号微秒）。
    pub fn set_audio_offset(&mut self, offset_us: i64) {
        self.audio_offset = offset_us;
    }

    /// 为 PTS 为 `target` 的帧决定何时显示。
    pub fn schedule(&mut self, target: Duration) -> Schedule {
        // 音频时钟在第一个采样播出前恒为 0，期间用它做基准会把每帧都判成"未来"。
        if let Some(audio) = self.audio.as_ref()
            && audio.started()
        {
            // 有效读数 = 硬件进度 + seek 偏移（可为负）。
            let mut now_us = audio.position().as_micros() as i64 + self.audio_offset;
            // 封顶到文件时长：seek 到近末尾时音频播完会下溢补静音，position 虚高。
            let mut capped = false;
            if self.duration_us > 0 {
                if now_us > self.duration_us as i64 {
                    capped = true;
                }
                now_us = now_us.min(self.duration_us as i64);
            }
            // 封顶生效 = 音频内容已播完，视频应把剩余帧立即显示播完。
            if capped {
                return Schedule::Now;
            }
            let now = Duration::from_micros(now_us.max(0) as u64);
            return schedule_against(target, now, DROP_THRESHOLD, |behind| {
                Schedule::Drop { behind }
            });
        }

        let origin = *self.origin.get_or_insert_with(|| Instant::now() - target);
        let elapsed = origin.elapsed();
        schedule_against(target, elapsed, RESYNC_THRESHOLD, |behind| {
            // 重置原点，以当前帧为新起点继续。
            self.origin = Some(Instant::now() - target);
            Schedule::Resynced { behind }
        })
    }
}

impl Default for PlaybackClock {
    fn default() -> Self {
        Self::new()
    }
}

/// 把 `target` 和当前时钟读数 `now` 一比，给出决定。
fn schedule_against(
    target: Duration,
    now: Duration,
    threshold: Duration,
    on_behind: impl FnOnce(Duration) -> Schedule,
) -> Schedule {
    if target > now {
        Schedule::Wait(target - now)
    } else {
        let behind = now - target;
        if behind > threshold {
            on_behind(behind)
        } else {
            Schedule::Now
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_waits_for_future_frame() {
        let mut clock = PlaybackClock::new();
        assert!(matches!(clock.schedule(Duration::ZERO), Schedule::Now));
        match clock.schedule(Duration::from_secs(1)) {
            Schedule::Wait(d) => {
                assert!(
                    d > Duration::from_millis(900) && d <= Duration::from_secs(1),
                    "期望等待约 1s，实际 {d:?}"
                );
            }
            _ => panic!("未来的帧应当等待"),
        }
    }

    #[test]
    fn audio_clock_drops_when_late() {
        let mut clock = PlaybackClock::new();
        clock.set_duration(1_000_000); // 1s
        // 假音频时钟：已播出 200ms。
        clock.set_audio(AudioClock::for_test(Duration::from_millis(200), true));
        // 目标帧 300ms → 未来，等待 ~100ms。
        assert!(matches!(
            clock.schedule(Duration::from_millis(300)),
            Schedule::Wait(_)
        ));
        // 目标帧 50ms → 落后 150ms > DROP_THRESHOLD(100ms) → Drop。
        assert!(matches!(
            clock.schedule(Duration::from_millis(50)),
            Schedule::Drop { .. }
        ));
    }

    #[test]
    fn audio_clock_capped_by_duration() {
        let mut clock = PlaybackClock::new();
        clock.set_duration(1_000_000); // 1s
        // 音频播到 2s（超过时长，模拟下溢虚高）→ 封顶 → 立即显示。
        clock.set_audio(AudioClock::for_test(Duration::from_secs(2), true));
        assert!(matches!(
            clock.schedule(Duration::from_millis(900)),
            Schedule::Now
        ));
    }
}
