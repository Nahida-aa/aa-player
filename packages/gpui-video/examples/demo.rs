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

    application()
        // 提供内嵌资源（图标/字体），供 svg().path("icons/…") 加载。
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
            // 加载内置字体，保证时间文本正常渲染。
            let _ = assets::Assets.load_fonts(cx);

            let bounds = Bounds::centered(None, size(1280.0.into(), 720.0.into()), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| Player::new(path.clone(), window, cx)),
            )
            .unwrap();
            cx.activate(true);
        });
}
