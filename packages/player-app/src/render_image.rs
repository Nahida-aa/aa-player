//! 解码帧 → GPUI 纹理的转换。

use std::sync::Arc;

use gpui::RenderImage;
use image::{Frame, RgbaImage};
use player_core::DecodedFrame;

/// 把 player-core 解码出的 BGRA 帧转成 GPUI 可渲染的 RenderImage。
///
/// GPUI 的 RenderImage 内部按 **BGRA** 解释字节（见 zed crates/gpui/src/assets.rs），
/// 与 ffmpeg Pixel::BGRA 一致。RgbaImage 仅作容器，字节序保持 BGRA 不动。
pub fn decoded_to_render_image(frame: &DecodedFrame) -> Arc<RenderImage> {
    // 紧密打包（去掉 ffmpeg 的行 stride 填充），长度 = w*h*4。
    let tight = frame.to_tight_bgra();
    let img =
        RgbaImage::from_raw(frame.width, frame.height, tight).expect("frame byte length mismatch");
    Arc::new(RenderImage::new(smallvec::SmallVec::from_elem(
        Frame::new(img),
        1,
    )))
}
