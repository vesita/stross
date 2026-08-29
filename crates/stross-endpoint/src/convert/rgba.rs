//! RGBA 双线性缩放（纯逻辑，平台无关）。
//!
//! 用途：桌面播放链路中 ffmpeg 解码输出 RGBA 帧后，壳层在转事件推前端前
//! 需要把帧缩放到显示宽度上限（控制 IPC 流量）。此前该缩放是壳层本地最近邻
//! 实现（`apps/stross-gui/src-tauri/src/receive.rs::scale_rgba`）——缩放是
//! 显示路径的计算密集环节，按分层铁律下沉到端点层 `convert`，升级为双线性
//! 插值（近邻在降采样时丢细节、上采样时出锯齿），桌面与 Android 共用语义。
//!
//! 双线性对每输出像素在源图做 2×2 加权采样，成本与最近邻同阶
//! （~O(输出像素数)），远低于三次/兰佐斯，适合 30fps 逐帧实时路径。

/// RGBA 双线性缩放（保持宽高比，目标宽度 ≤ `max_w`）。
///
/// 输入 `src` 为 `w × h × 4` 的 RGBA 字节（alpha 恒 255 也会插值，无副作用）。
/// 返回 `(目标宽, 目标高, RGBA 字节)`；参数非法（宽高为 0、缓冲不足）返回 `None`。
///
/// 采样采用**中心对齐**：目标像素中心映射到源图中心坐标（`dst+0.5 → src+0.5`
/// 的整数倍映射），避免整数除法取整造成的边缘像素重复采样/漂移。
pub fn rgba_scaled(src: &[u8], w: u32, h: u32, max_w: u32) -> Option<(u32, u32, Vec<u8>)> {
    if w == 0 || h == 0 {
        return None;
    }
    let tw = w.min(max_w.max(1));
    let th = (h * tw / w).max(1);
    let needed = (tw * th * 4) as usize;
    if src.len() < (w * h * 4) as usize {
        return None;
    }
    if tw == w && th == h {
        // 无需缩放：直接拷贝，绕开逐像素插值开销
        return Some((tw, th, src[..needed].to_vec()));
    }
    let (w, h) = (w as usize, h as usize);
    let (tw, th) = (tw as usize, th as usize);
    let scale_x = w as f64 / tw as f64;
    let scale_y = h as f64 / th as f64;
    let mut out = Vec::with_capacity(needed);
    let mut row = vec![0u8; tw * 4];
    for oy in 0..th {
        // 中心对齐采样；越界坐标 clamp 到 [0, h-1]（否则负权重外插、边缘过冲）
        let sy = (oy as f64 + 0.5)
            .mul_add(scale_y, -0.5)
            .clamp(0.0, (h - 1) as f64);
        let y0 = sy.floor() as usize;
        let y1 = (y0 + 1).min(h - 1);
        let ty = (sy - y0 as f64) as f32;
        let (row00, row01) = (&src[y0 * w * 4..], &src[y1 * w * 4..]);
        for ox in 0..tw {
            let sx = (ox as f64 + 0.5)
                .mul_add(scale_x, -0.5)
                .clamp(0.0, (w - 1) as f64);
            let x0 = sx.floor() as usize;
            let x1 = (x0 + 1).min(w - 1);
            let tx = (sx - x0 as f64) as f32;
            let i00 = x0 * 4;
            let i01 = x1 * 4;
            for c in 0..4 {
                let top =
                    f32::from(row00[i01 + c]).mul_add(tx, f32::from(row00[i00 + c]) * (1.0 - tx));
                let bot =
                    f32::from(row01[i01 + c]).mul_add(tx, f32::from(row01[i00 + c]) * (1.0 - tx));
                row[ox * 4 + c] = (top * (1.0 - ty) + bot * ty + 0.5) as u8;
            }
        }
        out.extend_from_slice(&row);
    }
    Some((tw as u32, th as u32, out))
}

#[cfg(test)]
mod tests {
    use super::rgba_scaled;

    #[test]
    fn keeps_aspect_and_size() {
        // 1280×720 → 宽度上限 720 → 720×405（保持 16:9）
        let src = vec![0u8; 1280 * 720 * 4];
        let (w, h, out) = rgba_scaled(&src, 1280, 720, 720).unwrap();
        assert_eq!((w, h), (720, 405));
        assert_eq!(out.len(), 720 * 405 * 4);
        // 不超过上限时原样（同尺寸直拷）
        let src2 = vec![9u8; 320 * 240 * 4];
        let (w2, h2, out2) = rgba_scaled(&src2, 320, 240, 480).unwrap();
        assert_eq!((w2, h2), (320, 240));
        assert_eq!(out2, src2);
    }

    #[test]
    fn downscale_averages_quad() {
        // 4×4 → 2×2：输出像素中心落在源 (0.5, 0.5) 等四象限中心，
        // 值为对应 2×2 象限的均值。
        // 构造左上 2×2 = 0，其余 = 255（通道 R；G=B=0）
        let mut src = vec![0u8; 4 * 4 * 4];
        for y in 0..4 {
            for x in 0..4 {
                if x >= 2 || y >= 2 {
                    src[(y * 4 + x) * 4] = 255;
                }
            }
        }
        let (_, _, out) = rgba_scaled(&src, 4, 4, 2).unwrap();
        // 输出 (0,0) 中心 → 源 (0.5, 0.5)：左上象限全 0 → R=0
        assert_eq!(out[0], 0);
        // 输出 (1,0) 中心 → 源 (1.5, 0.5)：右上象限全 255 → R=255
        assert_eq!(out[4], 255);
        // 输出 (0,1) 中心 → 源 (0.5, 1.5)：左下全 255
        assert_eq!(out[8], 255);
        // 输出 (1,1) 中心 → 源 (1.5, 1.5)：右下全 255
        assert_eq!(out[12], 255);
    }

    #[test]
    fn rejects_invalid() {
        assert!(rgba_scaled(&[], 0, 10, 8).is_none());
        assert!(rgba_scaled(&[0u8; 4], 8, 8, 8).is_none());
        assert!(rgba_scaled(&[0u8; 3], 1, 1, 8).is_none());
    }
}
