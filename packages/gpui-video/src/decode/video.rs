//! 视频帧出路：BGRA 像素转换与有界通道投递。
//!
//! 从父模块拆出的「视频侧出口」：解码帧 → GPUI 纹理的转换，以及满队列
//! 退避重试的投递循环。音频侧对称逻辑见 [`super::audio`]。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::channel::mpsc;
use gpui::RenderImage;
use image::{Frame, RgbaImage};

use player_core::DecodedFrame;

use super::FrameMsg;

/// 投递队列满时的退避间隔。
pub(super) const SEND_BACKOFF: Duration = Duration::from_millis(2);
/// 解码帧 → GPUI RenderImage（BGRA，与 ffmpeg Pixel::BGRA 一致）。
pub(super) fn decoded_to_render_image(frame: &DecodedFrame) -> Arc<RenderImage> {
    let tight = frame.to_tight_bgra();
    let img =
        RgbaImage::from_raw(frame.width, frame.height, tight).expect("frame byte length mismatch");
    Arc::new(RenderImage::new(smallvec::SmallVec::from_elem(
        Frame::new(img),
        1,
    )))
}
/// 把一帧送进队列，满则退避重试直到成功。返回 false 表示应结束线程。
pub(super) fn send_blocking(
    tx: &mut mpsc::Sender<FrameMsg>,
    item: (Arc<RenderImage>, u64, u64, bool, u64),
    running: &AtomicBool,
) -> bool {
    let mut pending = Some(item);
    while running.load(Ordering::Relaxed) {
        match tx.try_send(pending) {
            Ok(()) => return true,
            Err(e) if e.is_full() => {
                pending = e.into_inner();
                std::thread::sleep(SEND_BACKOFF);
            }
            Err(_) => return false, // 接收端已关闭
        }
    }
    false
}
