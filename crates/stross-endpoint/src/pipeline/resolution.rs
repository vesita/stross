//! 分辨率规划与自适应调整层。
//!
//! 负责屏幕采集（Wayland、X11、Windows）与摄像头捕获的原生尺寸探测、
//! 偶数对齐（H.264 / YUV420p 编码要求）、保持原生宽高比等比缩放计算，
//! 以及动态分辨率变更时的运行时兜底适配。

use crate::convert::rgba::rgba_scaled_into;

/// 确保尺寸为偶数且至少为 2（H.264 / YUV420p 编码的硬性约束）。
#[inline]
pub fn make_even(v: u32) -> u32 {
    let even = v & !1;
    even.max(2)
}

/// 分辨率规划：明确采集原生尺寸与编码目标尺寸。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolutionPlan {
    /// 采集源原生尺寸 (对齐偶数)
    pub src_width: u32,
    pub src_height: u32,
    /// 目标编码尺寸 (保持原比例、对齐偶数、受约束在上限内)
    pub target_width: u32,
    pub target_height: u32,
}

impl ResolutionPlan {
    /// 根据采集源原生尺寸 `(src_w, src_h)` 与画质上限 `(max_w, max_h)` 计算规划。
    ///
    /// 规则：
    /// 1. 原生尺寸对齐偶数。
    /// 2. 若原生尺寸在上限内，直接沿用原生尺寸（避免不必要的插值模糊）。
    /// 3. 若超出上限，按原生宽高比严格等比缩小，绝不拉伸变形（如 16:10、21:9、手机竖屏均保持原比例）。
    /// 4. 目标宽高均严格对齐偶数且各不超出对应上限。
    pub fn fit(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> Self {
        let sw = make_even(src_w);
        let sh = make_even(src_h);
        let mw = make_even(max_w);
        let mh = make_even(max_h);

        if sw <= mw && sh <= mh {
            return Self {
                src_width: sw,
                src_height: sh,
                target_width: sw,
                target_height: sh,
            };
        }

        // 等比缩放计算
        let scale_w = f64::from(mw) / f64::from(sw);
        let scale_h = f64::from(mh) / f64::from(sh);
        let scale = scale_w.min(scale_h);

        let mut tw = make_even((f64::from(sw) * scale).round() as u32);
        let mut th = make_even((f64::from(sh) * scale).round() as u32);

        // 浮点与偶数舍入保护（保证绝对不超过上限）
        if tw > mw {
            tw = (mw & !1).max(2);
        }
        if th > mh {
            th = (mh & !1).max(2);
        }

        Self {
            src_width: sw,
            src_height: sh,
            target_width: tw,
            target_height: th,
        }
    }

    /// 生成适合 ffmpeg 视频滤镜链的 scale 表达式。
    pub fn ffmpeg_vf_scale(&self) -> String {
        if self.src_width == self.target_width && self.src_height == self.target_height {
            "format=yuv420p".to_string()
        } else {
            format!(
                "scale={}:{}:flags=fast_bilinear,format=yuv420p",
                self.target_width, self.target_height
            )
        }
    }
}

/// 运行时动态尺寸容错自适应缓冲器。
///
/// 保证无论采集驱动送来的帧尺寸是否与初始协商尺寸一致（如用户调整桌面分辨率、
/// 拖拽缩放共享窗口），写向 ffmpeg stdin 的字节流尺寸恒定为 `(expected_w, expected_h)`，
/// 避免 ffmpeg 管道由于尺寸突变而损坏或接收端丢帧。
pub struct DynamicResolutionBuffer {
    expected_w: u32,
    expected_h: u32,
    native_buf: Vec<u8>,
    scaled_buf: Vec<u8>,
}

impl DynamicResolutionBuffer {
    pub fn new(expected_w: u32, expected_h: u32) -> Self {
        let expected_w = make_even(expected_w);
        let expected_h = make_even(expected_h);
        let len = (expected_w as usize) * (expected_h as usize) * 4;
        Self {
            expected_w,
            expected_h,
            native_buf: vec![0u8; len],
            scaled_buf: Vec::new(),
        }
    }

    pub fn expected_size(&self) -> (u32, u32) {
        (self.expected_w, self.expected_h)
    }

    /// 获取最终供写入 ffmpeg 的连续紧密 BGRA 像素切片。
    pub fn current_buffer(&self) -> &[u8] {
        &self.native_buf
    }

    /// 填入来自采集源的新帧。
    ///
    /// - 尺寸一致：按 stride 紧凑化（无缩放开销，memcpy 级）。
    /// - 尺寸偏离：使用定点双线性自适应缩放至 `(expected_w, expected_h)` 填入，绝不丢帧。
    pub fn ingest_frame(&mut self, width: u32, height: u32, stride: usize, data: &[u8]) -> bool {
        if width == 0 || height == 0 {
            return false;
        }

        let ew = self.expected_w;
        let eh = self.expected_h;
        let row_len = (ew as usize) * 4;
        let total_expected_len = (ew as usize) * (eh as usize) * 4;

        if width == ew && height == eh {
            // 尺寸一致（主流路径）：整块或逐行规整 stride
            if data.len() < total_expected_len {
                return false;
            }
            if stride == row_len {
                self.native_buf[..total_expected_len].copy_from_slice(&data[..total_expected_len]);
            } else {
                for y in 0..eh as usize {
                    let src = y * stride;
                    let dst = y * row_len;
                    if src + row_len <= data.len() && dst + row_len <= total_expected_len {
                        self.native_buf[dst..dst + row_len]
                            .copy_from_slice(&data[src..src + row_len]);
                    }
                }
            }
            return true;
        }

        // 尺寸发生动态变化（如分辨率变更/窗口大小改变）：
        // 先规整当前帧为紧排数据，再双线性缩放到 (ew, eh)
        let incoming_row = (width as usize) * 4;
        let incoming_len = (width as usize) * (height as usize) * 4;

        if self.scaled_buf.len() < incoming_len {
            self.scaled_buf.resize(incoming_len, 0);
        }

        if stride == incoming_row {
            if data.len() < incoming_len {
                return false;
            }
            self.scaled_buf[..incoming_len].copy_from_slice(&data[..incoming_len]);
        } else {
            for y in 0..height as usize {
                let src = y * stride;
                let dst = y * incoming_row;
                if src + incoming_row <= data.len() && dst + incoming_row <= incoming_len {
                    self.scaled_buf[dst..dst + incoming_row]
                        .copy_from_slice(&data[src..src + incoming_row]);
                }
            }
        }

        // 双线性定点插值缩放到 expected 尺寸
        rgba_scaled_into(
            &self.scaled_buf[..incoming_len],
            width,
            height,
            &mut self.native_buf,
            ew,
            eh,
        )
        .is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_make_even() {
        assert_eq!(make_even(0), 2);
        assert_eq!(make_even(1), 2);
        assert_eq!(make_even(2), 2);
        assert_eq!(make_even(2259), 2258);
        assert_eq!(make_even(1271), 1270);
        assert_eq!(make_even(1920), 1920);
    }

    #[test]
    fn test_resolution_plan_fit() {
        // 1. 原生 4K 16:9 -> 限制 1080p
        let p1 = ResolutionPlan::fit(3840, 2160, 1920, 1080);
        assert_eq!(p1.src_width, 3840);
        assert_eq!(p1.src_height, 2160);
        assert_eq!(p1.target_width, 1920);
        assert_eq!(p1.target_height, 1080);
        assert_eq!(
            p1.ffmpeg_vf_scale(),
            "scale=1920:1080:flags=fast_bilinear,format=yuv420p"
        );

        // 2. 16:10 屏幕 (2560x1600) -> 限制 1080p，严格保持 16:10 不拉伸
        let p2 = ResolutionPlan::fit(2560, 1600, 1920, 1080);
        assert_eq!(p2.src_width, 2560);
        assert_eq!(p2.src_height, 1600);
        assert_eq!(p2.target_height, 1080);
        assert_eq!(p2.target_width, 1728); // 1728 / 1080 = 1.6 (16:10)
        assert_eq!(p2.target_width % 2, 0);

        // 3. 21:9 带宽屏 (3440x1440) -> 限制 1080p
        let p3 = ResolutionPlan::fit(3440, 1440, 1920, 1080);
        assert_eq!(p3.target_width, 1920);
        assert_eq!(p3.target_height, 804);
        assert_eq!(p3.target_height % 2, 0);

        // 4. 手机竖屏 (1080x2400) -> 限制 1920x1080
        let p4 = ResolutionPlan::fit(1080, 2400, 1920, 1080);
        assert_eq!(p4.target_height, 1080);
        assert_eq!(p4.target_width, 486);
        assert_eq!(p4.target_width % 2, 0);

        // 5. 小于上限 (1366x768) -> 原生直出，不放大模糊
        let p5 = ResolutionPlan::fit(1366, 768, 1920, 1080);
        assert_eq!(p5.target_width, 1366);
        assert_eq!(p5.target_height, 768);
        assert_eq!(p5.ffmpeg_vf_scale(), "format=yuv420p");
    }

    #[test]
    fn test_dynamic_resolution_buffer() {
        let mut buf = DynamicResolutionBuffer::new(100, 100);
        assert_eq!(buf.expected_size(), (100, 100));

        // 正常尺寸喂帧
        let frame1 = vec![0xAB; 100 * 100 * 4];
        assert!(buf.ingest_frame(100, 100, 400, &frame1));
        assert_eq!(buf.current_buffer()[0], 0xAB);

        // 动态尺寸突变（如 50x50 突变输入）
        let frame2 = vec![0x7F; 50 * 50 * 4];
        assert!(buf.ingest_frame(50, 50, 200, &frame2));
        assert_eq!(buf.current_buffer().len(), 100 * 100 * 4);
    }
}
