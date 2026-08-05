//! gpui-video —— 可复用的 GPUI 视频播放器组件。
//!
//! 分层：
//! - [`controller`]：无 GUI 播放状态机（解码线程/时钟/seek/pause）。
//! - [`surface`]：视频画面渲染元素。
//! - [`controls`]：控制条（进度条/播放暂停/时间）。
//! - [`player`]：组合视图。

pub mod controller;
pub mod controls;
pub mod player;
pub mod surface;

pub use controller::PlayerController;
pub use player::Player;
pub use surface::VideoSurface;
