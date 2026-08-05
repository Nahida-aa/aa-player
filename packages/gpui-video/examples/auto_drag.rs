//! 自动拖动演示：打开**真实窗口**，自动播放几秒后，脚本自动触发几次
//! 快速拖动 seek（画面/进度条/时间文本跟着动），供肉眼观察拖动行为。
//!
//! 用法：`cargo run -p gpui-video --example auto_drag -- [视频路径]`
//! 不传路径播放 player-core 内置样本。

use std::path::PathBuf;
use std::time::Duration;

use gpui::{App, AppContext, Bounds, WindowBounds, WindowOptions, size};
use gpui_platform::application;
use gpui_video::Player;

fn main() {
    env_logger::init();

    let path: PathBuf = match std::env::args().nth(1) {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../player-core/tests/assets/sample.mp4"),
    };

    application()
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
            let _ = assets::Assets.load_fonts(cx);

            let bounds = Bounds::centered(None, size(960.0.into(), 640.0.into()), cx);
            let window = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        ..Default::default()
                    },
                    |window, cx| cx.new(|cx| Player::new(path.clone(), window, cx)),
                )
                .unwrap();
            let player = window
                .update(cx, |_, window, _| window.root::<Player>())
                .unwrap()
                .unwrap()
                .unwrap();

            // 自动拖动脚本：先播 4s，然后每 1.5s 快速拖动一次（不同目标）。
            let player2 = player.clone();
            cx.spawn(async move |cx| {
                let mut step = 0u32;
                // 目标序列（秒）：快速来回拖。
                let targets = [10.0f32, 20.0, 5.0, 30.0, 15.0, 40.0, 25.0, 2.0];
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(if step == 0 { 4000 } else { 1500 }))
                        .await;
                    let t = targets[(step as usize) % targets.len()];
                    // 连续发多次 preview（模拟快速拖动中的连续移动），最后 release。
                    for i in 0..5 {
                        let target = t * (i + 1) as f32 / 5.0;
                        player2.update(cx, |player, cx| {
                            player.seek_preview(Duration::from_secs_f32(target), cx);
                        });
                    }
                    player2.update(cx, |player, cx| {
                        player.seek(Duration::from_secs_f32(t), cx);
                    });
                    eprintln!("[auto-drag] step {step}: 拖动到 {t}s");
                    step += 1;
                }
            })
            .detach();

            cx.activate(true);
        });
}
