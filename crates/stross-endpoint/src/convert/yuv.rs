//! YUV420 → RGBA 转换与缩放（纯逻辑，平台无关）。
//!
//! 用途：Android 播放链路中 MediaCodec 解码输出为 YUV420（NV12 半平面或
//! I420 平面），此前由 Kotlin `PlaybackPlugin` 逐像素 Java 循环转换 + 缩放
//! （~60 行、无 SIMD、每像素多次边界检查 ByteBuffer.get）——是"解码跟不上
//! 接收"的 CPU 大头。本模块把转换下沉 Rust：纯函数、可单测，桌面与 Android
//! 共用语义（Y 亮度双线性插值 + 色度 2×2 块最近邻，与
//! [`crate::convert::rgba::rgba_scaled`] 同一中心对齐采样约定）。

/// YUV420 颜色排布（MediaCodec `KEY_COLOR_FORMAT` 的两种常见取值）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Yuv420Layout {
    /// `COLOR_FormatYUV420Planar`（19）：Y 平面 + U 平面 + V 平面（I420）。
    Planar,
    /// `COLOR_FormatYUV420SemiPlanar`（21）：Y 平面 + 交错 UV（NV12，U 在前）。
    SemiPlanar,
}

/// YUV420 → RGBA（双线性缩放到宽度 ≤ `max_w`，保持宽高比）。
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

    const FP: u64 = 12;
    let scale_x = ((w as u64) << FP) / tw as u64;
    let scale_y = ((h as u64) << FP) / th as u64;

    // 预计算水平采样参数（消除逐行内循环的重复乘除法）
    #[derive(Clone, Copy)]
    struct YuvXSample {
        x0: usize,
        x1: usize,
        fx: u32,
        w_fx: u32,
        ux: usize,
    }
    let mut x_samples = Vec::with_capacity(tw as usize);
    for ox in 0..tw {
        let sx = ((ox as u64) * scale_x)
            .saturating_add(scale_x / 2)
            .saturating_sub(1 << (FP - 1));
        let x0 = ((sx >> FP) as usize).min(w as usize - 1);
        let x1 = (x0 + 1).min(w as usize - 1);
        let fx = (sx & ((1 << FP) - 1)) as u32;
        let w_fx = (1 << FP) - fx;
        x_samples.push(YuvXSample {
            x0,
            x1,
            fx,
            w_fx,
            ux: x0 / 2,
        });
    }

    let dst_stride = tw as usize * 4;
    let mut out = vec![0u8; tw as usize * th as usize * 4];
    for oy in 0..th {
        // 中心对齐采样（与 rgba::rgba_scaled 同约定，12 位定点）；越界 clamp 防负权重外插
        let sy = ((oy as u64) * scale_y)
            .saturating_add(scale_y / 2)
            .saturating_sub(1 << (FP - 1));
        let y0 = ((sy >> FP) as usize).min(h as usize - 1);
        let y1 = (y0 + 1).min(h as usize - 1);
        let fy = (sy & ((1 << FP) - 1)) as u32;
        let w_fy = (1 << FP) - fy;
        let row0 = y_base + y0 * stride_y;
        let row1 = y_base + y1 * stride_y;
        let uy = y0 / 2;
        let uv_row_off = uv_base + uy * uv_stride;
        let out_row = &mut out[oy as usize * dst_stride..oy as usize * dst_stride + dst_stride];

        for (ox, sample) in x_samples.iter().enumerate() {
            let x0 = sample.x0;
            let x1 = sample.x1;
            let fx = sample.fx;
            let w_fx = sample.w_fx;

            // 亮度对细节敏感：Y 12 位定点双线性插值
            let y00 = u32::from(buf[row0 + x0]);
            let y01 = u32::from(buf[row0 + x1]);
            let y10 = u32::from(buf[row1 + x0]);
            let y11 = u32::from(buf[row1 + x1]);
            let top = (y00 * w_fx + y01 * fx + 2048) >> FP;
            let bot = (y10 * w_fx + y11 * fx + 2048) >> FP;
            let y_val = ((top * w_fy + bot * fy + 2048) >> FP) as i32;

            // 色度按 2x2 块采样（YUV420 语义；块坐标取插值格点左下）
            let ux = sample.ux;
            let (u, v) = match layout {
                Yuv420Layout::SemiPlanar => (
                    i32::from(buf[uv_row_off + ux * 2]),
                    i32::from(buf[uv_row_off + ux * 2 + 1]),
                ),
                Yuv420Layout::Planar => (
                    i32::from(buf[uv_row_off + ux]),
                    i32::from(buf[uv_row_off + ux + uv_size]),
                ),
            };
            let c = y_val - 16;
            let d = u - 128;
            let e = v - 128;
            let r = clamp_u8((298 * c + 409 * e + 128) >> 8);
            let g = clamp_u8((298 * c - 100 * d - 208 * e + 128) >> 8);
            let b = clamp_u8((298 * c + 516 * d + 128) >> 8);
            let dst_px = ox * 4;
            out_row[dst_px] = r;
            out_row[dst_px + 1] = g;
            out_row[dst_px + 2] = b;
            out_row[dst_px + 3] = 255;
        }
    }
    Some((tw, th, out))
}

const fn clamp_u8(v: i32) -> u8 {
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

/// BGRA（小端：B,G,R,A 各一字节）→ YUV420p（I420 平面），BT.601 全范围。
///
/// Wayland 屏幕采集（portal+pipewire SHM 路径）产出 BGRA 帧；ffmpeg
/// rawvideo 输入需要 yuv420p。逐像素转换 + 2x2 色度抽样（4:2:0）。
///
/// 输入：`bgra` 为 `stride` 行跨度的 BGRA 数据；输出 `out` 必须容纳
/// `w*h + w*h/2` 字节（Y + U + V 平面）。
pub fn bgra_to_yuv420p(
    bgra: &[u8],
    stride: usize,
    w: usize,
    h: usize,
    out: &mut [u8],
) -> Result<(), String> {
    let y_size = w * h;
    let uv_size = w * h / 4;
    if out.len() < y_size + uv_size * 2 {
        return Err(format!(
            "输出缓冲不足: {} < {}",
            out.len(),
            y_size + uv_size * 2
        ));
    }
    if bgra.len() < stride * h {
        return Err(format!("输入缓冲不足: {} < {}", bgra.len(), stride * h));
    }
    let (y_plane, rest) = out.split_at_mut(y_size);
    let (u_plane, v_plane) = rest.split_at_mut(uv_size);

    // Y 平面：全分辨率；U/V 平面：2x2 平均抽样（BT.601 全范围整数系数，
    // 系数和为 0/256：Y=(77,150,29)，U=(-43,-85,128)，V=(128,-107,-21)）
    for j in 0..h {
        let row = &bgra[j * stride..j * stride + w * 4];
        let yrow = &mut y_plane[j * w..(j + 1) * w];
        for (i, ypx) in yrow.iter_mut().enumerate() {
            let px = i * 4;
            let (b, g, r) = (
                i32::from(row[px]),
                i32::from(row[px + 1]),
                i32::from(row[px + 2]),
            );
            *ypx = ((77 * r + 150 * g + 29 * b) >> 8).clamp(0, 255) as u8;
        }
        // 偶数行时累计色度（2x2 平均）
        if j % 2 == 0 && j + 1 < h {
            let row2 = &bgra[(j + 1) * stride..(j + 1) * stride + w * 4];
            let uv_offset = (j / 2) * (w / 2);
            let urow = &mut u_plane[uv_offset..uv_offset + (w / 2)];
            let vrow = &mut v_plane[uv_offset..uv_offset + (w / 2)];
            for i in (0..w).step_by(2) {
                let px0 = i * 4;
                let px1 = px0 + 4;
                let b_sum = i32::from(row[px0])
                    + i32::from(row[px1])
                    + i32::from(row2[px0])
                    + i32::from(row2[px1]);
                let g_sum = i32::from(row[px0 + 1])
                    + i32::from(row[px1 + 1])
                    + i32::from(row2[px0 + 1])
                    + i32::from(row2[px1 + 1]);
                let r_sum = i32::from(row[px0 + 2])
                    + i32::from(row[px1 + 2])
                    + i32::from(row2[px0 + 2])
                    + i32::from(row2[px1 + 2]);
                // 平均后换算（/4 得像素均值，再 /256 得系数缩放）
                let u = ((-43 * r_sum - 85 * g_sum + 128 * b_sum) / (4 * 256) + 128).clamp(0, 255);
                let v = ((128 * r_sum - 107 * g_sum - 21 * b_sum) / (4 * 256) + 128).clamp(0, 255);
                urow[i / 2] = u as u8;
                vrow[i / 2] = v as u8;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod bgra_tests {
    use super::*;

    #[test]
    fn bgra_to_yuv420p_known_value() {
        // 纯红 (B=0,G=0,R=255)：Y≈76, U≈84, V≈255
        let w = 2;
        let h = 2;
        let stride = w * 4;
        let mut bgra = vec![0u8; stride * h];
        for px in bgra.chunks_mut(4) {
            px[0] = 0; // B
            px[1] = 0; // G
            px[2] = 255; // R
            px[3] = 255; // A
        }
        let mut out = vec![0u8; w * h + w * h / 2];
        bgra_to_yuv420p(&bgra, stride, w, h, &mut out).unwrap();
        // 全红：所有 Y 相同、U 相同、V 相同
        let y0 = out[0];
        let u0 = out[w * h];
        let v0 = out[w * h + w * h / 4];
        assert!(out[..w * h].iter().all(|&x| x == y0));
        assert!((i32::from(y0) - 77).abs() <= 2, "Y={y0}");
        assert!((i32::from(u0) - 85).abs() <= 4, "U={u0}");
        assert!((i32::from(v0) - 255).abs() <= 2, "V={v0}");
    }

    #[test]
    fn bgra_to_yuv420p_black() {
        let w = 4;
        let h = 4;
        let stride = w * 4;
        let bgra = vec![0u8; stride * h]; // 全黑（含 alpha=0，但转换只用 BGR）
        let mut out = vec![0u8; w * h + w * h / 2];
        bgra_to_yuv420p(&bgra, stride, w, h, &mut out).unwrap();
        // 黑 = Y 全 0，U/V = 128（消色差）
        assert!(out[..w * h].iter().all(|&x| x == 0));
        assert!(out[w * h..].iter().all(|&x| x == 128));
    }

    #[test]
    fn bgra_to_yuv420p_undersized_output() {
        let mut out = vec![0u8; 4];
        assert!(bgra_to_yuv420p(&[0u8; 64], 8, 2, 2, &mut out).is_err());
    }
}

/// BGRA → YUV420p 双线性缩放（Wayland 采集帧 → 编码目标分辨率）。
///
/// portal 交付显示器原生尺寸（如 1920×1080），而编码目标由 [`Quality`]
/// （如 MEDIUM 1280×720）决定——先双线性缩放到目标分辨率，再按
/// [`bgra_to_yuv420p`] 转 I420（色度 2×2 平均）。
///
/// 双线性较最近邻在屏幕文本/UI 下混叠明显更小（屏幕共享的常态内容）。
pub fn bgra_to_yuv420p_scaled(
    bgra: &[u8],
    src_stride: usize,
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
    out: &mut [u8],
) -> Result<(), String> {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return Err("尺寸非法".into());
    }
    // 目标分辨率 BGRA 缓冲 → 复用 bgra_to_yuv420p（其内做 2×2 色度）。
    // 双线性插值（中心对齐 + 越界 clamp，与 rgba::rgba_scaled 同约定）：
    // 屏幕文本/UI 是屏幕共享的常态内容，最近邻缩放锯齿明显。
    let mut scaled = vec![0u8; dst_w * dst_h * 4];
    let dst_stride = dst_w * 4;
    const FP: u64 = 12;
    let scale_x = ((src_w as u64) << FP) / dst_w as u64;
    let scale_y = ((src_h as u64) << FP) / dst_h as u64;

    // 预计算水平采样参数（消除逐行内循环的重复乘除法）
    #[derive(Clone, Copy)]
    struct ScaledXSample {
        i00_off: usize,
        i01_off: usize,
        fx: u32,
        w_fx: u32,
    }
    let mut x_samples = Vec::with_capacity(dst_w);
    for i in 0..dst_w {
        let sx = ((i as u64 * scale_x).saturating_add(scale_x / 2)).saturating_sub(1 << (FP - 1));
        let x0 = ((sx >> FP) as usize).min(src_w - 1);
        let x1 = (x0 + 1).min(src_w - 1);
        let fx = (sx & ((1 << FP) - 1)) as u32;
        let w_fx = (1 << FP) - fx;
        x_samples.push(ScaledXSample {
            i00_off: x0 * 4,
            i01_off: x1 * 4,
            fx,
            w_fx,
        });
    }

    for j in 0..dst_h {
        let sy = ((j as u64 * scale_y).saturating_add(scale_y / 2)).saturating_sub(1 << (FP - 1));
        let y0 = ((sy >> FP) as usize).min(src_h - 1);
        let y1 = (y0 + 1).min(src_h - 1);
        let fy = (sy & ((1 << FP) - 1)) as u32;
        let w_fy = (1 << FP) - fy;
        let r0 = y0 * src_stride;
        let r1 = y1 * src_stride;
        let dst_row = &mut scaled[j * dst_stride..j * dst_stride + dst_stride];

        for (i, sample) in x_samples.iter().enumerate() {
            let i00 = r0 + sample.i00_off;
            let i01 = r0 + sample.i01_off;
            let i10 = r1 + sample.i00_off;
            let i11 = r1 + sample.i01_off;
            let fx = sample.fx;
            let w_fx = sample.w_fx;
            let d = i * 4;

            for c in 0..4 {
                let top =
                    (u32::from(bgra[i00 + c]) * w_fx + u32::from(bgra[i01 + c]) * fx + 2048) >> FP;
                let bot =
                    (u32::from(bgra[i10 + c]) * w_fx + u32::from(bgra[i11 + c]) * fx + 2048) >> FP;
                dst_row[d + c] = ((top * w_fy + bot * fy + 2048) >> FP) as u8;
            }
        }
    }
    bgra_to_yuv420p(&scaled, dst_stride, dst_w, dst_h, out)
}

#[cfg(test)]
mod scaled_tests {
    use super::*;

    #[test]
    fn scaled_down_keeps_color() {
        // 1280x720 全红 → 缩到 640x360：仍应全红（Y≈76 U≈84 V≈255）
        let (sw, sh) = (1280usize, 720usize);
        let stride = sw * 4;
        let mut bgra = vec![0u8; stride * sh];
        for px in bgra.chunks_mut(4) {
            px[2] = 255; // R
            px[3] = 255;
        }
        let (dw, dh) = (640usize, 360usize);
        let mut out = vec![0u8; dw * dh + dw * dh / 2];
        bgra_to_yuv420p_scaled(&bgra, stride, sw, sh, dw, dh, &mut out).unwrap();
        assert!((i32::from(out[0]) - 77).abs() <= 2, "Y={}", out[0]);
        let u = out[dw * dh];
        let v = out[dw * dh + dw * dh / 4];
        assert!((i32::from(u) - 85).abs() <= 4, "U={u}");
        assert!((i32::from(v) - 255).abs() <= 2, "V={v}");
    }
}
