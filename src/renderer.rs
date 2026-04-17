//! マルチビュー描画モジュール
//!
//! 複数のカメラフレームをグリッド状に合成・レンダリングします。
//! rayon による並列処理で高速レンダリングを実現します。

use crate::layout::LayoutType;
use rayon::prelude::*;

/// フレームデータ（RGB形式）
#[derive(Debug, Clone)]
pub struct FrameData {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl FrameData {
    /// RGB形式のピクセルを取得（(R, G, B)タプル）
    fn get_rgb(&self, x: u32, y: u32) -> Option<(u8, u8, u8)> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let idx = ((y * self.width + x) * 3) as usize;
        if idx + 2 < self.pixels.len() {
            Some((self.pixels[idx], self.pixels[idx + 1], self.pixels[idx + 2]))
        } else {
            None
        }
    }

    /// 出力座標を16:9クロップ領域へ線形マップしてRGBピクセルを取得
    fn get_rgb_cropped_16_9(
        &self,
        out_x: usize,
        out_y: usize,
        out_w: usize,
        out_h: usize,
    ) -> Option<(u8, u8, u8)> {
        if self.width == 0 || self.height == 0 {
            return None;
        }
        if out_w == 0 || out_h == 0 {
            return None;
        }

        // ターゲット: 16:9
        let target_width = self.width;
        let target_height = target_width * 9 / 16;

        // クロップ領域が高さ制限を超える場合は高さを基準にスケール
        let (crop_width, crop_height) = if target_height > self.height {
            let w = self.height * 16 / 9;
            (w, self.height)
        } else {
            (target_width, target_height)
        };

        if crop_width == 0 || crop_height == 0 {
            return None;
        }

        // センター配置でクロップ
        let offset_x = (self.width.saturating_sub(crop_width)) / 2;
        let offset_y = (self.height.saturating_sub(crop_height)) / 2;

        let src_x_in_crop = if out_w <= 1 {
            0
        } else {
            ((out_x as u64) * ((crop_width - 1) as u64) / ((out_w - 1) as u64)) as u32
        };
        let src_y_in_crop = if out_h <= 1 {
            0
        } else {
            ((out_y as u64) * ((crop_height - 1) as u64) / ((out_h - 1) as u64)) as u32
        };

        let src_x = offset_x + src_x_in_crop;
        let src_y = offset_y + src_y_in_crop;

        self.get_rgb(src_x, src_y)
    }
}

/// マルチビューレンダラー
///
/// 複数のカメラフレームをグリッドレイアウトに従って合成します。
/// rayon による並列処理で高速レンダリングを実現します。
pub struct MultiViewRenderer {
    /// レイアウト設定
    layout_type: LayoutType,
    /// 各ビューのフレームデータ
    frame_cache: Vec<Option<FrameData>>,
    /// フレームバッファプール（メモリ再利用）
    output_buffer: Vec<u8>,
}

impl MultiViewRenderer {
    /// 新しいレンダラーを作成
    pub fn new(layout_type: LayoutType) -> Self {
        let view_count = layout_type.view_count();
        let frame_cache = vec![None; view_count];
        // 最大サイズ 1920x1080 でバッファを事前割り当て（RGBA, 4バイト/ピクセル）
        let output_buffer = vec![0u8; 1920 * 1080 * 4];

        Self {
            layout_type,
            frame_cache,
            output_buffer,
        }
    }

    /// フレームデータをキャッシュ
    pub fn cache_frame(&mut self, view_index: usize, frame: FrameData) {
        if view_index < self.frame_cache.len() {
            // フレーム解像度の妥当性チェック（0より大きく、32ビット符号なし整数の範囲内）
            if frame.width == 0 || frame.height == 0 {
                eprintln!("ERROR: Camera frame has invalid resolution: {}x{}", frame.width, frame.height);
                return;
            }
            // フレームサイズチェック（RGB形式: width*height*3）
            let expected_size = (frame.width as usize) * (frame.height as usize) * 3;
            if frame.pixels.len() < expected_size {
                eprintln!(
                    "ERROR: Camera frame data is incomplete: expected {} bytes, got {}",
                    expected_size,
                    frame.pixels.len()
                );
                return;
            }
            self.frame_cache[view_index] = Some(frame);
        }
    }

    /// 指定ビューのキャッシュをクリア（空枠表示）
    pub fn clear_frame(&mut self, view_index: usize) {
        if view_index < self.frame_cache.len() {
            self.frame_cache[view_index] = None;
        }
    }

    /// 全フレームを指定された総サイズにレンダリング
    ///
    /// グリッドレイアウトに従って、複数のカメラフレームを1つの画像に合成します。
    /// rayon による並列処理で高速化されています。
    pub fn render(&mut self, output_width: usize, output_height: usize) -> &[u8] {
        // 出力バッファサイズチェック
        let required_size = output_width * output_height * 4;
        if required_size > self.output_buffer.len() {
            eprintln!(
                "ERROR: Output size {}x{} (requires {} bytes) exceeds buffer capacity ({}). Resizing.",
                output_width,
                output_height,
                required_size,
                self.output_buffer.len()
            );
            self.output_buffer.resize(required_size, 0);
        }

        let (cols, rows) = self.layout_type.dimensions();
        let view_width = output_width / cols;
        let view_height = output_height / rows;

        // 使用範囲だけ初期化
        self.output_buffer[..required_size]
            .par_iter_mut()
            .for_each(|pixel| *pixel = 0);

        // 各ビューをレンダリング
        for view_idx in 0..self.frame_cache.len() {
            let row = view_idx / cols;
            let col = view_idx % cols;

            let offset_x = col * view_width;
            let offset_y = row * view_height;
            let has_frame = self.frame_cache[view_idx].is_some();

            // フレームデータをコピーして、borrow を解放
            if let Some(frame) = &self.frame_cache[view_idx] {
                let frame_copy = FrameData {
                    pixels: frame.pixels.clone(),
                    width: frame.width,
                    height: frame.height,
                };

                self.render_view_parallel(
                    output_width,
                    &frame_copy,
                    offset_x,
                    offset_y,
                    view_width,
                    view_height,
                );
            } else {
                // フレームがない場合は黒で埋める
                self.fill_black_parallel(output_width, offset_x, offset_y, view_width, view_height);
            }

            // OBSライクに各枠の境界線を描画（空枠は明るく強調）
            let border = if has_frame {
                [70, 70, 70, 255]
            } else {
                [180, 180, 180, 255]
            };
            self.draw_view_border(
                output_width,
                output_height,
                offset_x,
                offset_y,
                view_width,
                view_height,
                border,
                2,
            );
        }

        &self.output_buffer[..required_size]
    }

    fn draw_view_border(
        &mut self,
        output_width: usize,
        output_height: usize,
        offset_x: usize,
        offset_y: usize,
        width: usize,
        height: usize,
        rgba: [u8; 4],
        thickness: usize,
    ) {
        if width == 0 || height == 0 {
            return;
        }

        for t in 0..thickness {
            if width <= t * 2 || height <= t * 2 {
                break;
            }

            let left = offset_x + t;
            let right = offset_x + width - 1 - t;
            let top = offset_y + t;
            let bottom = offset_y + height - 1 - t;

            for x in left..=right {
                self.set_pixel(output_width, output_height, x, top, rgba);
                self.set_pixel(output_width, output_height, x, bottom, rgba);
            }
            for y in top..=bottom {
                self.set_pixel(output_width, output_height, left, y, rgba);
                self.set_pixel(output_width, output_height, right, y, rgba);
            }
        }
    }

    fn set_pixel(
        &mut self,
        output_width: usize,
        output_height: usize,
        x: usize,
        y: usize,
        rgba: [u8; 4],
    ) {
        if x >= output_width || y >= output_height {
            return;
        }

        let idx = (y * output_width + x) * 4;
        if idx + 3 < self.output_buffer.len() {
            self.output_buffer[idx] = rgba[0];
            self.output_buffer[idx + 1] = rgba[1];
            self.output_buffer[idx + 2] = rgba[2];
            self.output_buffer[idx + 3] = rgba[3];
        }
    }

    /// 単一ビューを並列レンダリング（行単位で並列化、16:9アスペクト比保持）
    fn render_view_parallel(
        &mut self,
        output_width: usize,
        frame: &FrameData,
        offset_x: usize,
        offset_y: usize,
        view_width: usize,
        view_height: usize,
    ) {
        // 各行を並列処理
        let rows: Vec<Vec<u8>> = (0..view_height)
            .into_par_iter()
            .map(|y| {
                let mut row_data = vec![0u8; view_width * 4];
                for x in 0..view_width {
                    let (r_cam, g_cam, b_cam) = frame
                        .get_rgb_cropped_16_9(x, y, view_width, view_height)
                        .unwrap_or((0, 0, 0));

                    let idx = x * 4;
                    row_data[idx] = r_cam;
                    row_data[idx + 1] = g_cam;
                    row_data[idx + 2] = b_cam;
                    row_data[idx + 3] = 255;
                }
                row_data
            })
            .collect();

        // 各行を出力バッファに書き込み
        for (y, row_data) in rows.into_iter().enumerate() {
            let dest_y = offset_y + y;
            let base_idx = (dest_y * output_width + offset_x) * 4;
            self.output_buffer[base_idx..base_idx + row_data.len()].copy_from_slice(&row_data);
        }
    }

    /// 領域を黒で埋める（並列化版）
    fn fill_black_parallel(
        &mut self,
        output_width: usize,
        offset_x: usize,
        offset_y: usize,
        width: usize,
        height: usize,
    ) {
        // 複数行ごとにまとめて処理
        let chunk_height = (height / num_cpus::get()).max(1);

        (0..height).step_by(chunk_height).for_each(|y_start| {
            let y_end = (y_start + chunk_height).min(height);
            for y in y_start..y_end {
                for x in 0..width {
                    let idx = ((offset_y + y) * output_width + (offset_x + x)) * 4;
                    if idx + 3 < self.output_buffer.len() {
                        self.output_buffer[idx] = 0;
                        self.output_buffer[idx + 1] = 0;
                        self.output_buffer[idx + 2] = 0;
                        self.output_buffer[idx + 3] = 255;
                    }
                }
            }
        });
    }
}
