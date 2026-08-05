//! gpui-video 播放器组件演示。
//!
//! 用法：`cargo run -p gpui-video --example demo -- [视频路径]`
//! 不传路径时播放 player-core 的内置样本。

use std::path::PathBuf;

use gpui::{App, AppContext, Bounds, WindowBounds, WindowOptions, size};
use gpui_platform::application;
use gpui_video::Player;

fn main() {
    env_logger::init();

    // 解析视频路径：优先命令行参数，否则用 player-core 的样本。
    let path: PathBuf = match std::env::args().nth(1) {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../player-core/tests/assets/sample.mp4"),
    };

    application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(960.0.into(), 640.0.into()), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|cx| Player::new(path.clone(), cx)),
        )
        .unwrap();
        cx.activate(true);
    });
}
