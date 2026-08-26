//! YUV420 → RGBA 转换与缩放（纯逻辑，平台无关）。
//!
//! 用途：Android 播放链路中 MediaCodec 解码输出为 YUV420（NV12 半平面或
//! I420 平面），此前由 Kotlin `PlaybackPlugin` 逐像素 Java 循环转换 + 缩放
//! （~60 行、无 SIMD、每像素多次边界检查 ByteBuffer.get）——是"解码跟不上
//! 接收"的 CPU 大头。本模块把转换下沉 Rust：纯函数、可单测，桌面与 Android
//! 共用语义（与桌面 [`crate::playback`] 的 scale_rgba 最近邻算法一致）。

/// YUV420 颜色排布（MediaCodec `KEY_COLOR_FORMAT` 的两种常见取值）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Yuv420Layout {
    /// `COLOR_FormatYUV420Planar`（19）：Y 平面 + U 平面 + V 平面（I420）。
    Planar,
    /// `COLOR_FormatYUV420SemiPlanar`（21）：Y 平面 + 交错 UV（NV12，U 在前）。
    SemiPlanar,
}

/// YUV420 → RGBA（最近邻缩放到宽度 ≤ `max_w`，保持宽高比）。
///
/// 输入 `buf` 是一整块输出缓冲区：Y 平面起始于 `buf[0]`，其行跨度为
/// `stride_y`、行数为 `slice_h`；UV 平面接在 `stride_y * slice_h` 之后
/// （Planar：U 与 V 各自宽度 `stride_y`、行数 `slice_h / 2`；SemiPlanar：
/// 单一交错平面，每行 [U,V,U,V,…]）。
///
/// 返回 `(目标宽, 目标高, RGBA 字节)`；参数非法（宽高为 0、缓冲不足、
/// 解析不出有效尺寸）时返回 `None`。
pub fn yuv420_to_rgba_scaled(
    buf: &[u8],
    w: u32,
    h: u32,
    layout: Yuv420Layout,
    stride_y: u32,
    slice_h: u32,
    max_w: u32,
) -> Option<(u32, u32, Vec<u8>)> {
    if w == 0 || h == 0 || stride_y == 0 || slice_h == 0 {
        return None;
    }
    let tw = w.min(max_w.max(1));
    let th = (h * tw / w).max(1);

    let stride_y = stride_y as usize;
    let slice_h = slice_h as usize;
    let y_plane = slice_h * stride_y; // Y 平面字节数
    let uv_stride = match layout {
        Yuv420Layout::SemiPlanar => stride_y,
        Yuv420Layout::Planar => stride_y / 2,
    };
    let uv_height = slice_h / 2;
    if uv_stride == 0 || uv_height == 0 {
        return None;
    }
    let uv_size = uv_stride * uv_height; // 每个色度平面的字节数
    let needed = match layout {
        Yuv420Layout::SemiPlanar => y_plane + uv_size, // 单一交错平面
        Yuv420Layout::Planar => y_plane + uv_size * 2, // U、V 两个平面
    };
    if buf.len() < needed {
        return None;
    }
    let (y_base, uv_base) = (0usize, y_plane);
    let uv_row_gap = if matches!(layout, Yuv420Layout::SemiPlanar) {
        // NV12：UV 交错单平面，行跨度即 uv_stride（= stride_y）
        0
    } else {
        uv_size
    };

    let mut out = Vec::with_capacity(tw as usize * th as usize * 4);
    for oy in 0..th {
        let sy = (oy * h / th) as usize;
        for ox in 0..tw {
            let sx = (ox * w / tw) as usize;
            let y = buf[y_base + sy * stride_y + sx] as i32;
            // 色度按 2x2 块采样（YUV420 语义）
            let (uy, ux) = (sy / 2, sx / 2);
            let u_off = uv_base + uy * uv_stride + ux;
            let (u, v) = match layout {
                Yuv420Layout::SemiPlanar => (buf[u_off] as i32, buf[u_off + 1] as i32),
                Yuv420Layout::Planar => (buf[u_off] as i32, buf[u_off + uv_row_gap] as i32),
            };
            let c = y - 16;
            let d = u - 128;
            let e = v - 128;
            let r = clamp_u8((298 * c + 409 * e + 128) >> 8);
            let g = clamp_u8((298 * c - 100 * d - 208 * e + 128) >> 8);
            let b = clamp_u8((298 * c + 516 * d + 128) >> 8);
            out.extend_from_slice(&[r, g, b, 255]);
        }
    }
    Some((tw, th, out))
}

fn clamp_u8(v: i32) -> u8 {
    if v < 0 {
        0
    } else if v > 255 {
        255
    } else {
        v as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一块 NV12 缓冲区：Y 填 `y_val`，UV 交错填 `(u_val, v_val)`。
    fn nv12_buf(w: u32, h: u32, y_val: u8, u_val: u8, v_val: u8) -> Vec<u8> {
        let stride = w as usize;
        let slice = h as usize;
        let mut buf = vec![0u8; stride * slice + stride * (slice / 2)];
        buf[..stride * slice].fill(y_val);
        for i in (stride * slice..buf.len()).step_by(2) {
            buf[i] = u_val;
            buf[i + 1] = v_val;
        }
        buf
    }

    /// 同一颜色在 Planar（I420）与 SemiPlanar（NV12）布局下 RGBA 应一致。
    #[test]
    fn planar_and_semiplanar_agree() {
        let (w, h) = (8u32, 8u32);
        let (y, u, v) = (32u8, 128u8, 128u8); // 中性灰：R=G=B
        let nv12 = nv12_buf(w, h, y, u, v);
        let (tw, th, rgba_nv12) =
            yuv420_to_rgba_scaled(&nv12, w, h, Yuv420Layout::SemiPlanar, w, h, 8).unwrap();
        assert_eq!((tw, th), (8, 8));

        // I420：Y 平面同 NV12（w*h）；U 平面、V 平面分开（各 w/2 * h/2）
        let y_size = (w * h) as usize;
        let uv_plane = (w as usize / 2) * (h as usize / 2);
        let mut i420 = vec![y; y_size + uv_plane * 2];
        i420[y_size..y_size + uv_plane].fill(u);
        i420[y_size + uv_plane..].fill(v);
        let (_, _, rgba_i420) =
            yuv420_to_rgba_scaled(&i420, w, h, Yuv420Layout::Planar, w, h, 8).unwrap();
        assert_eq!(rgba_nv12, rgba_i420, "两种布局同一颜色应输出相同 RGBA");

        // 中性色：R=G=B
        let pixel = &rgba_nv12[..4];
        assert_eq!(pixel[0], pixel[1]);
        assert_eq!(pixel[1], pixel[2]);
        assert_eq!(pixel[3], 255);
    }

    /// 缩放：16x16 → 宽度 8 → 8x8，且总字节数正确。
    #[test]
    fn scaling_keeps_aspect() {
        let buf = nv12_buf(16, 16, 90, 90, 240);
        let (tw, th, rgba) =
            yuv420_to_rgba_scaled(&buf, 16, 16, Yuv420Layout::SemiPlanar, 16, 16, 8).unwrap();
        assert_eq!((tw, th), (8, 8));
        assert_eq!(rgba.len(), 8 * 8 * 4);
    }

    /// 非紧凑跨距（stride > 行像素数、slice_h = 完整行数）：仍正确采样。
    #[test]
    fn padded_stride() {
        // 8x8 逻辑帧，但行跨度 16（右侧 8 字节填充），slice_h = 8
        let stride = 16u32;
        let slice = 8u32;
        let (w, h) = (8u32, 8u32);
        let mut buf = vec![0u8; (stride * slice + stride * (slice / 2)) as usize];
        for y in 0..slice {
            for x in 0..w {
                buf[(y * stride + x) as usize] = 32; // 中性灰 Y
            }
        }
        // 填充区任意值（不该被采样）
        let uv0 = (stride * slice) as usize;
        for i in (uv0..buf.len()).step_by(2) {
            buf[i] = 128;
            buf[i + 1] = 128;
        }
        let (tw, th, rgba) =
            yuv420_to_rgba_scaled(&buf, w, h, Yuv420Layout::SemiPlanar, stride, slice, 8).unwrap();
        assert_eq!((tw, th), (8, 8));
        assert_eq!(rgba[0], rgba[1]);
        assert_eq!(rgba[1], rgba[2]);
    }

    /// 非法输入返回 None（零尺寸/缓冲不足）。
    #[test]
    fn rejects_invalid() {
        let empty: [u8; 0] = [];
        assert!(yuv420_to_rgba_scaled(&empty, 0, 0, Yuv420Layout::SemiPlanar, 1, 1, 8).is_none());
        assert!(
            yuv420_to_rgba_scaled(&[0u8; 4], 8, 8, Yuv420Layout::SemiPlanar, 8, 8, 8).is_none()
        );
    }
}
