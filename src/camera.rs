//! カメラ管理モジュール
//!
//! 複数のカメラインスタンスのライフサイクル管理とアクセスを提供します。

use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{RequestedFormat, RequestedFormatType};
use nokhwa::{Camera, native_api_backend, query};
use std::sync::{Arc, mpsc};
use std::thread;

/// カメラIDの型安全なラッパー
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CameraId(pub usize);

/// カメラ情報（メタデータ）
#[derive(Debug, Clone)]
pub struct CameraInfo {
    pub id: CameraId,
    pub name: String,
    pub index: nokhwa::utils::CameraIndex,
}

/// フレームデータ構造体（Arcで共有してコピーを減らす）
#[derive(Debug, Clone)]
pub struct FrameData {
    pub pixels: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
}

/// カメラマネージャー
///
/// 複数のカメラをアクティブに管理し、フレーム取得とカメラの切り替えを処理します。
pub struct CameraManager {
    /// 利用可能なカメラの情報リスト
    available_cameras: Vec<CameraInfo>,
    /// カメラごとのフレーム受信チャネル（バックグラウンドスレッドで取得したフレームを受信）
    active_receivers:
        std::collections::HashMap<CameraId, mpsc::Receiver<Result<FrameData, String>>>,
}

impl CameraManager {
    /// 新しいカメラマネージャーを初期化します
    ///
    /// システムに接続されているすべてのカメラを列挙します。
    pub fn new() -> Self {
        let backend = native_api_backend().unwrap_or(nokhwa::utils::ApiBackend::Auto);
        let cameras = query(backend).unwrap_or_default();

        let available_cameras = cameras
            .into_iter()
            .enumerate()
            .map(|(i, info)| CameraInfo {
                id: CameraId(i),
                name: info.human_name(),
                index: info.index().clone(),
            })
            .collect();

        Self {
            available_cameras,
            active_receivers: std::collections::HashMap::new(),
        }
    }

    /// 利用可能なカメラ情報を取得
    pub fn available_cameras(&self) -> &[CameraInfo] {
        &self.available_cameras
    }

    /// カメラを開く（ストリーミング開始）
    ///
    /// 同じIDのカメラが既に開かれている場合は何もしません。
    pub fn open_camera(&mut self, camera_id: CameraId) -> Result<(), String> {
        if self.active_receivers.contains_key(&camera_id) {
            return Ok(());
        }

        let info = self
            .available_cameras
            .iter()
            .find(|c| c.id == camera_id)
            .ok_or_else(|| format!("Camera {:?} not found", camera_id))?;

        let index = info.index.clone();
        let (tx, rx) = mpsc::sync_channel(2);

        thread::spawn(move || {
            let resolution = nokhwa::utils::Resolution::new(1280, 720);
            let requested = RequestedFormat::new::<RgbFormat>(
                RequestedFormatType::HighestResolution(resolution),
            );

            let mut cam = match Camera::new(index, requested) {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(Err(format!("Failed to create camera: {}", e)));
                    return;
                }
            };

            if let Err(e) = cam.open_stream() {
                let _ = tx.send(Err(format!("Failed to open stream: {}", e)));
                return;
            }

            loop {
                let frame_res = cam
                    .frame()
                    .map_err(|e| format!("Frame capture failed: {}", e));

                let processed_frame = frame_res.and_then(|frame| {
                    let rgb_image = match frame.decode_image::<RgbFormat>() {
                        Ok(img) => img,
                        Err(e) => return Err(format!("Frame decode failed: {}", e)),
                    };

                    let width = rgb_image.width();
                    let height = rgb_image.height();
                    let pixels = rgb_image.into_raw();

                    if width == 0 || height == 0 {
                        Err(format!("Invalid resolution: {}x{}", width, height))
                    } else {
                        Ok(FrameData {
                            pixels: Arc::new(pixels),
                            width,
                            height,
                        })
                    }
                });

                if tx.send(processed_frame).is_err() {
                    break;
                }
            }
        });

        self.active_receivers.insert(camera_id, rx);
        Ok(())
    }

    /// カメラを閉じる
    pub fn close_camera(&mut self, camera_id: CameraId) {
        self.active_receivers.remove(&camera_id);
    }

    /// カメラから最新フレームだけを取得
    pub fn get_frame(&mut self, camera_id: CameraId) -> Result<Option<FrameData>, String> {
        if !self.active_receivers.contains_key(&camera_id) {
            self.open_camera(camera_id)?;
        }

        let rx = self
            .active_receivers
            .get_mut(&camera_id)
            .ok_or_else(|| format!("Camera {:?} not open", camera_id))?;

        let mut latest_frame = None;
        let mut last_error = None;

        while let Ok(result) = rx.try_recv() {
            match result {
                Ok(frame) => latest_frame = Some(frame),
                Err(e) => last_error = Some(e),
            }
        }

        if let Some(e) = last_error {
            return Err(e);
        }
        Ok(latest_frame)
    }

    /// すべてのアクティブなカメラを取得
    pub fn active_camera_ids(&self) -> Vec<CameraId> {
        self.active_receivers.keys().copied().collect()
    }
}

impl Default for CameraManager {
    fn default() -> Self {
        Self::new()
    }
}
