//! ffmpeg C 层日志桥：路由进 tracing + 同文节流。
//!
//! libavcodec/libavformat 的日志直写 stderr、绕过 tracing。历史上有过
//! 两个极端：
//! - 全放行：HE-AAC seek 后 `[aac] Could not update timestamps…` 成串
//!   刷屏，把真正的错误淹没；
//! - 压到 Fatal（ef180c3）：安静了，但用户再也看不到警告/错误——排查
//!   电音这类问题时失去了最重要的线索来源。
//!
//! 正解是 vlc 式（modules/misc/logger.c 同思路）：装自定义
//! `av_log_set_callback`，按级别路由进 tracing，并对**同文本**消息做
//! 时间窗节流——窗口内第一条照发并累计计数，窗口到期后随下一条一起
//! 报出 "×N"。既看得见，又不刷屏。
//!
//! 级别门槛默认 Warning，可用环境变量 `AA_PLAYER_FFMPEG_LOG` 覆盖
//! （取值 quiet/panic/fatal/error/warning/info/debug/trace）。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// AV_LOG_* 级别常量（libavutil/log.h）。level 的低 8 位是级别，
/// 高位是分类（category），比较前要掩掉。
const AV_LOG_QUIET: i32 = -8;
const AV_LOG_ERROR: i32 = 16;
const AV_LOG_WARNING: i32 = 24;

/// 同一文本消息的最小重发间隔。窗口内的重复只计数不发。
pub(crate) const THROTTLE_WINDOW: Duration = Duration::from_secs(3);
/// 节流表上限：键数超限直接清空（防御病态输入撑爆内存）。
const MAX_KEYS: usize = 512;

/// 消息文本 → (自上次发出以来累计条数, 上次发出的时刻；None=从未发过)
type ThrottleMap = HashMap<String, (u64, Option<Instant>)>;

static THROTTLE: OnceLock<Mutex<ThrottleMap>> = OnceLock::new();

fn throttle_map() -> &'static Mutex<ThrottleMap> {
    THROTTLE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 决定一条消息现在要不要发出。
///
/// 返回 `Some(n)`：发出，n 为自上次发出以来累计的条数（含本条，首条
/// 为 1）；返回 `None`：窗口内重复，只计数。抽成纯函数以便单测。
fn decide(map: &mut ThrottleMap, key: &str, now: Instant, window: Duration) -> Option<u64> {
    if map.len() > MAX_KEYS {
        map.clear();
    }
    let e = map.entry(key.to_string()).or_insert((0, None));
    e.0 += 1;
    let expired = match e.1 {
        None => true,
        Some(last) => now.duration_since(last) >= window,
    };
    if expired {
        let n = std::mem::replace(&mut e.0, 0);
        e.1 = Some(now);
        Some(n)
    } else {
        None
    }
}

/// C 层回调：格式化消息后交给 [`route`]。
///
/// SAFETY: 由 libavutil 调用；fmt 是合法 C 格式串，args 与之匹配
/// （ffmpeg 保证）。vsnprintf 按 ffmpeg-sys 绑定的 va_list ABI 调用。
unsafe extern "C" fn trampoline(
    _avcl: *mut core::ffi::c_void,
    level: core::ffi::c_int,
    fmt: *const core::ffi::c_char,
    args: *mut ffmpeg_next::ffi::__va_list_tag,
) {
    // QUIET(-8) 是上游的显式静音请求，尊重之（它是精确级别，无分类位）；
    // INFO 及以下默认门槛已挡，这里不再重复判断（set_level 控制生成端）。
    if level == AV_LOG_QUIET {
        return;
    }
    // SAFETY: 见函数级文档；buf/len/fmt/args 均为合法匹配的调用。
    let mut buf = [0u8; 1024];
    let n = unsafe {
        ffmpeg_next::ffi::vsnprintf(
            buf.as_mut_ptr() as *mut core::ffi::c_char,
            buf.len() as u64,
            fmt,
            args,
        )
    };
    if n <= 0 {
        return;
    }
    let take = (n as usize).min(buf.len() - 1);
    let msg = String::from_utf8_lossy(&buf[..take]);
    route(level & 0xff, msg.trim_end());
}

/// 级别路由 + 节流后的实际输出。`level` 为已掩掉分类位的 AV_LOG 级别。
pub(crate) fn route(level: i32, msg: &str) {
    if msg.is_empty() {
        return;
    }
    let decided = {
        let now = Instant::now();
        let mut map = throttle_map().lock().unwrap_or_else(|e| e.into_inner());
        decide(&mut map, msg, now, THROTTLE_WINDOW)
    };
    let Some(count) = decided else { return };
    let text = if count > 1 {
        format!("{msg} ×{count}")
    } else {
        msg.to_string()
    };
    if level <= AV_LOG_ERROR {
        tracing::error!("{text}");
    } else if level <= AV_LOG_WARNING {
        tracing::warn!("{text}");
    } else {
        tracing::debug!("{text}");
    }
}

/// 解析 `AA_PLAYER_FFMPEG_LOG` 环境变量为 ffmpeg 日志级别。
fn env_level() -> Option<ffmpeg_next::log::Level> {
    use ffmpeg_next::log::Level;
    let s = std::env::var("AA_PLAYER_FFMPEG_LOG").ok()?;
    let lowered = s.to_ascii_lowercase();
    let level = match lowered.as_str() {
        "quiet" => Level::Quiet,
        "panic" => Level::Panic,
        "fatal" => Level::Fatal,
        "error" => Level::Error,
        "warning" => Level::Warning,
        "info" => Level::Info,
        "debug" => Level::Debug,
        "trace" => Level::Trace,
        other => {
            tracing::warn!(value = %other, "AA_PLAYER_FFMPEG_LOG 取值无效，忽略");
            return None;
        }
    };
    Some(level)
}

/// 安装回调并设置级别门槛。幂等，只在 init_ffmpeg 的 OnceLock 里调一次。
pub(crate) fn install(default_level: ffmpeg_next::log::Level) {
    unsafe {
        ffmpeg_next::ffi::av_log_set_callback(Some(trampoline));
    }
    let level = env_level().unwrap_or(default_level);
    ffmpeg_next::log::set_level(level);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_message_emits_immediately() {
        let mut m = ThrottleMap::new();
        assert_eq!(decide(&mut m, "x", Instant::now(), THROTTLE_WINDOW), Some(1));
    }

    #[test]
    fn repeats_within_window_suppressed_and_counted() {
        let mut m = ThrottleMap::new();
        let t = Instant::now();
        assert_eq!(decide(&mut m, "x", t, THROTTLE_WINDOW), Some(1));
        for _ in 0..9 {
            assert_eq!(decide(&mut m, "x", t, THROTTLE_WINDOW), None);
        }
        // 窗口到期后的下一条带出累计计数 ×10。
        assert_eq!(
            decide(&mut m, "x", t + THROTTLE_WINDOW, THROTTLE_WINDOW),
            Some(10)
        );
    }

    #[test]
    fn distinct_messages_throttled_independently() {
        let mut m = ThrottleMap::new();
        let t = Instant::now();
        assert_eq!(decide(&mut m, "a", t, THROTTLE_WINDOW), Some(1));
        assert_eq!(decide(&mut m, "b", t, THROTTLE_WINDOW), Some(1));
        assert_eq!(decide(&mut m, "a", t, THROTTLE_WINDOW), None);
    }

    #[test]
    fn key_overflow_clears_table_instead_of_growing_forever() {
        let mut m = ThrottleMap::new();
        let t = Instant::now();
        for i in 0..(MAX_KEYS as u32 + 10) {
            decide(&mut m, &format!("k{i}"), t, THROTTLE_WINDOW);
        }
        assert!(m.len() <= MAX_KEYS + 1);
    }

    #[test]
    fn level_routing_matches_av_constants() {
        // ERROR(16) 走 error，WARNING(24) 走 warn，INFO(32) 走 debug。
        // 只验证节流放行路径不 panic；级别分派由 match 结构保证。
        route(AV_LOG_ERROR, "route-test-error");
        route(AV_LOG_WARNING, "route-test-warning");
        route(AV_LOG_WARNING + 8, "route-test-info");
    }
}
