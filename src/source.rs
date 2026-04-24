//! 映像ソース抽象化モジュール
//!
//! カメラに限らない入力を扱えるように、共通の VideoSource トレイトを定義します。

use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{RequestedFormat, RequestedFormatType};
use nokhwa::{Camera, native_api_backend, query};
use std::collections::HashMap;
use std::sync::{Arc, mpsc};
use std::thread;

/// 映像ソースIDの型安全なラッパー
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceId(pub usize);

/// ソース情報（UI表示などのメタデータ）
#[derive(Debug, Clone)]
pub struct SourceInfo {
    pub id: SourceId,
    pub name: String,
}

/// フレームデータ構造体（Arcで共有してコピーを減らす）
#[derive(Debug, Clone)]
pub struct FrameData {
    pub pixels: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
}

/// すべての映像ソースが実装すべき共通トレイト
#[allow(dead_code)]
pub trait VideoSource: Send {
    fn id(&self) -> SourceId;
    fn name(&self) -> &str;
    fn start(&mut self) -> Result<(), String>;
    fn stop(&mut self);
    fn get_frame(&mut self) -> Result<Option<FrameData>, String>;
}

/// Nokhwaベースのカメラソース
#[allow(dead_code)]
pub struct CameraSource {
    id: SourceId,
    name: String,
    index: nokhwa::utils::CameraIndex,
    rx: Option<mpsc::Receiver<Result<FrameData, String>>>,
}

impl CameraSource {
    fn new(id: SourceId, name: String, index: nokhwa::utils::CameraIndex) -> Self {
        Self {
            id,
            name,
            index,
            rx: None,
        }
    }
}

impl VideoSource for CameraSource {
    fn id(&self) -> SourceId {
        self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn start(&mut self) -> Result<(), String> {
        if self.rx.is_some() {
            return Ok(());
        }

        let index = self.index.clone();
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

                let processed = frame_res.and_then(|frame| {
                    let rgb_image = frame
                        .decode_image::<RgbFormat>()
                        .map_err(|e| format!("Frame decode failed: {}", e))?;

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

                if tx.send(processed).is_err() {
                    break;
                }
            }
        });

        self.rx = Some(rx);
        Ok(())
    }

    fn stop(&mut self) {
        self.rx = None;
    }

    fn get_frame(&mut self) -> Result<Option<FrameData>, String> {
        if self.rx.is_none() {
            self.start()?;
        }

        let rx = self
            .rx
            .as_mut()
            .ok_or_else(|| "Source not started".to_string())?;

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
}

/// 複数の映像ソースを統括するマネージャー
pub struct SourceManager {
    sources: HashMap<SourceId, Box<dyn VideoSource>>,
    source_infos: Vec<SourceInfo>,
}

impl SourceManager {
    pub fn new() -> Self {
        let backend = native_api_backend().unwrap_or(nokhwa::utils::ApiBackend::Auto);
        let cameras = query(backend).unwrap_or_default();

        let mut sources: HashMap<SourceId, Box<dyn VideoSource>> = HashMap::new();
        let mut source_infos = Vec::new();

        for (i, info) in cameras.into_iter().enumerate() {
            let id = SourceId(i);
            let name = info.human_name();
            let index = info.index().clone();

            source_infos.push(SourceInfo {
                id,
                name: name.clone(),
            });

            let source = CameraSource::new(id, name, index);
            sources.insert(id, Box::new(source));
        }

        Self {
            sources,
            source_infos,
        }
    }

    pub fn available_sources(&self) -> &[SourceInfo] {
        &self.source_infos
    }

    pub fn open_source(&mut self, source_id: SourceId) -> Result<(), String> {
        let source = self
            .sources
            .get_mut(&source_id)
            .ok_or_else(|| format!("Source {:?} not found", source_id))?;
        source.start()
    }

    #[allow(dead_code)]
    pub fn close_source(&mut self, source_id: SourceId) {
        if let Some(source) = self.sources.get_mut(&source_id) {
            source.stop();
        }
    }

    pub fn get_frame(&mut self, source_id: SourceId) -> Result<Option<FrameData>, String> {
        let source = self
            .sources
            .get_mut(&source_id)
            .ok_or_else(|| format!("Source {:?} not found", source_id))?;
        source.get_frame()
    }

    #[allow(dead_code)]
    pub fn active_source_ids(&self) -> Vec<SourceId> {
        self.source_infos.iter().map(|s| s.id).collect()
    }
}

impl Default for SourceManager {
    fn default() -> Self {
        Self::new()
    }
}
