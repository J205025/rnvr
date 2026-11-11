use iced::window;

/// 產生 NVR 監控攝影機專屬 RGBA 圖示 (深色圓角底座 + 藍光高清鏡頭 + 右上角 REC 錄影紅燈)
pub fn render_nvr_icon_rgba(size: u32) -> Vec<u8> {
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let s = size as f32;

    for y in 0..size {
        for x in 0..size {
            let px = (x as f32 + 0.5) / s * 256.0;
            let py = (y as f32 + 0.5) / s * 256.0;

            let idx = ((y * size + x) * 4) as usize;

            // 1. 圓角矩形底板 (x: 16..240, y: 16..240, rx: 48)
            let bg_dist = sd_rounded_box(px - 128.0, py - 128.0, 112.0, 112.0, 48.0);
            if bg_dist > 1.5 {
                continue;
            }

            let bg_alpha = (0.5 - bg_dist).clamp(0.0, 1.0);
            // 漸層深藍灰背景
            let t = ((px + py) / 512.0).clamp(0.0, 1.0);
            let mut r = lerp(30.0, 15.0, t);
            let mut g = lerp(41.0, 23.0, t);
            let mut b = lerp(59.0, 42.0, t);

            // 外框天藍光邊 (stroke width = 6)
            let border_dist = bg_dist.abs() - 3.0;
            if border_dist < 1.0 {
                let ba = (0.5 - border_dist).clamp(0.0, 1.0);
                r = lerp(r, 56.0, ba);
                g = lerp(g, 189.0, ba);
                b = lerp(b, 248.0, ba);
            }

            // 2. 攝影機鏡頭中心 (128, 132)
            let dx = px - 128.0;
            let dy = py - 132.0;
            let d_center = (dx * dx + dy * dy).sqrt();

            // 攝影機外環 (r = 72)
            if d_center < 73.0 {
                let a = (72.5 - d_center).clamp(0.0, 1.0);
                r = lerp(r, 51.0, a);
                g = lerp(g, 65.0, a);
                b = lerp(b, 85.0, a);

                // 外環金屬邊框
                let ring_edge = (d_center - 72.0).abs() - 3.0;
                if ring_edge < 1.0 {
                    let ra = (0.5 - ring_edge).clamp(0.0, 1.0);
                    r = lerp(r, 148.0, ra);
                    g = lerp(g, 163.0, ra);
                    b = lerp(b, 184.0, ra);
                }
            }

            // 光學藍玻璃鏡片 (r = 56)
            if d_center < 57.0 {
                let a = (56.5 - d_center).clamp(0.0, 1.0);
                let lt = (d_center / 56.0).clamp(0.0, 1.0);
                let lr = lerp(2.0, 8.0, lt);
                let lg = lerp(132.0, 47.0, lt);
                let lb = lerp(199.0, 73.0, lt);
                r = lerp(r, lr, a);
                g = lerp(g, lg, a);
                b = lerp(b, lb, a);

                let inner_ring = (d_center - 56.0).abs() - 2.0;
                if inner_ring < 1.0 {
                    let ia = (0.5 - inner_ring).clamp(0.0, 1.0);
                    r = lerp(r, 56.0, ia);
                    g = lerp(g, 189.0, ia);
                    b = lerp(b, 248.0, ia);
                }
            }

            // 深層光圈瞳孔 (r = 28)
            if d_center < 29.0 {
                let a = (28.5 - d_center).clamp(0.0, 1.0);
                r = lerp(r, 9.0, a);
                g = lerp(g, 21.0, a);
                b = lerp(b, 38.0, a);
            }

            // 左上光學反光高光點 (110, 114, r = 11)
            let h1_d = ((px - 110.0).powi(2) + (py - 114.0).powi(2)).sqrt();
            if h1_d < 12.0 {
                let a = (11.5 - h1_d).clamp(0.0, 1.0) * 0.88;
                r = lerp(r, 224.0, a);
                g = lerp(g, 242.0, a);
                b = lerp(b, 254.0, a);
            }

            // 右下副反光點 (144, 148, r = 5.5)
            let h2_d = ((px - 144.0).powi(2) + (py - 148.0).powi(2)).sqrt();
            if h2_d < 6.5 {
                let a = (6.0 - h2_d).clamp(0.0, 1.0) * 0.65;
                r = lerp(r, 125.0, a);
                g = lerp(g, 211.0, a);
                b = lerp(b, 252.0, a);
            }

            // 3. 右上角 REC 錄影狀態紅燈 (196, 60)
            let rec_d = ((px - 196.0).powi(2) + (py - 60.0).powi(2)).sqrt();
            if rec_d < 19.0 {
                let halo_a = (18.5 - rec_d).clamp(0.0, 1.0) * 0.32;
                r = lerp(r, 239.0, halo_a);
                g = lerp(g, 68.0, halo_a);
                b = lerp(b, 68.0, halo_a);
            }
            if rec_d < 12.0 {
                let dot_a = (11.5 - rec_d).clamp(0.0, 1.0);
                r = lerp(r, 239.0, dot_a);
                g = lerp(g, 68.0, dot_a);
                b = lerp(b, 68.0, dot_a);
            }

            rgba[idx] = r.round().clamp(0.0, 255.0) as u8;
            rgba[idx + 1] = g.round().clamp(0.0, 255.0) as u8;
            rgba[idx + 2] = b.round().clamp(0.0, 255.0) as u8;
            rgba[idx + 3] = (bg_alpha * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }

    rgba
}

/// 建立 Iced 視窗專屬圖示 (64x64)
pub fn build_window_icon() -> Option<window::Icon> {
    let rgba = render_nvr_icon_rgba(64);
    window::icon::from_rgba(rgba, 64, 64).ok()
}

fn sd_rounded_box(px: f32, py: f32, bx: f32, by: f32, r: f32) -> f32 {
    let qx = px.abs() - bx + r;
    let qy = py.abs() - by + r;
    let out_dist = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    let in_dist = qx.max(qy).min(0.0);
    out_dist + in_dist - r
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::BufWriter;
    use std::path::Path;

    #[test]
    fn test_generate_and_save_app_icon_png() {
        let icon = build_window_icon();
        assert!(icon.is_some(), "應成功建立 64x64 視窗圖示");

        // 同時輸出 256x256 高解析度圖示至 assets/rnvr.png 供 Ubuntu 桌面捷徑使用
        let assets_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
        fs::create_dir_all(&assets_dir).unwrap();
        let png_path = assets_dir.join("rnvr.png");

        let size = 256u32;
        let rgba = render_nvr_icon_rgba(size);
        let file = File::create(&png_path).unwrap();
        let w = BufWriter::new(file);
        let mut encoder = png::Encoder::new(w, size, size);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&rgba).unwrap();

        assert!(png_path.exists());
    }
}
