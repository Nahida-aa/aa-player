//! gpui-video —— 可复用的 GPUI 视频播放器组件。
//!
//! 分层：
//! - [`controller`]：无 GUI 播放状态机（解码线程/音频输出/seek/pause）。
//! - [`playback_clock`]：视频帧显示时钟（音频主时钟 / 墙钟）。
//! - [`surface`]：视频画面渲染元素。
//! - [`controls`]：控制条（进度条/播放暂停/时间）。
//! - [`player`]：组合视图。

pub mod controller;
pub mod controls;
pub mod i18n;
pub mod playback_clock;
pub mod player;
pub mod stats;
pub mod surface;

pub use controller::{AudioClockSource, PlayerController};
pub use i18n::{I18n, Lang, StrKey};
pub use playback_clock::{PlaybackClock, Schedule};
pub use player::{PlaybackEnded, Player, TimeFormat};
pub use stats::{ProfileStats, Snapshot};
pub use surface::VideoSurface;
