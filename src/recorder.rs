use openh264::encoder::Encoder;
use rayon::prelude::*;
use std::fs::File;
use std::io::Write;

use crate::camera::FrameData;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::thread;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordingTarget {
    Input(usize),
    Preview,
    Program,
}

impl RecordingTarget {
    pub fn name(&self) -> String {
        match self {
            RecordingTarget::Input(idx) => format!("Input_{}", idx + 1),
            RecordingTarget::Preview => "Preview".to_string(),
            RecordingTarget::Program => "Program".to_string(),
        }
    }
}

pub struct RecordingSession {
    target: RecordingTarget,
    tx: mpsc::SyncSender<Arc<FrameData>>,
    handle: Option<thread::JoinHandle<()>>,
}

pub struct RecorderManager {
    sessions: HashMap<RecordingTarget, RecordingSession>,
    pub save_dir: PathBuf,
    pub show_settings: bool,
    pub record_configs: HashMap<RecordingTarget, bool>,
}

impl RecorderManager {
    pub fn new() -> Self {
        let save_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            sessions: HashMap::new(),
            save_dir,
            show_settings: false,
            record_configs: HashMap::new(),
        }
    }

    pub fn is_recording(&self) -> bool {
        !self.sessions.is_empty()
    }

    pub fn is_recording_target(&self, target: &RecordingTarget) -> bool {
        self.sessions.contains_key(target)
    }

    pub fn start_selected(&mut self) {
        for (target, enabled) in &self.record_configs {
            if *enabled && !self.sessions.contains_key(target) {
                let (tx, rx) = mpsc::sync_channel::<Arc<FrameData>>(5); // バッファを持たせる

                let _target_clone = *target;
                let save_dir = self.save_dir.clone();
                let name = target.name();

                let handle = thread::spawn(move || {
                    let filename = format!(
                        "{}_{}.h264",
                        chrono::Local::now().format("%Y%m%d_%H%M%S"),
                        name
                    );
                    let filepath = save_dir.join(filename);

                    let mut file = match File::create(&filepath) {
                        Ok(f) => f,
                        Err(e) => {
                            eprintln!("Failed to create recording file: {}", e);
                            return;
                        }
                    };

                    println!("Started recording to {:?}", filepath);

                    let mut encoder: Option<Encoder> = None;
                    let mut target_width = 1280;
                    let mut target_height = 720;

                    while let Ok(frame) = rx.recv() {
                        let width = frame.width as usize;
                        let height = frame.height as usize;

                        // 初期化
                        if encoder.is_none() {
                            target_width = width;
                            target_height = height;
                            match Encoder::new() {
                                Ok(e) => encoder = Some(e),
                                Err(err) => {
                                    eprintln!("Failed to create encoder: {:?}", err);
                                    break;
                                }
                            }
                        }

                        // openh264 は YUV420p を要求する
                        // RGB -> YUV420p 高速変換 (Rayon利用)
                        let pixels = &frame.pixels;
                        let y_len = target_width * target_height;
                        let uv_len = (target_width / 2) * (target_height / 2);

                        let mut yuv_buf = vec![0u8; y_len + uv_len * 2];

                        // Y平面の変換
                        let (y_plane, uv_planes) = yuv_buf.split_at_mut(y_len);
                        let (u_plane, v_plane) = uv_planes.split_at_mut(uv_len);

                        y_plane
                            .par_chunks_mut(target_width)
                            .enumerate()
                            .for_each(|(y, row)| {
                                for x in 0..target_width {
                                    let idx = (y * width + x) * 3;
                                    if idx + 2 < pixels.len() {
                                        let r = pixels[idx] as f32;
                                        let g = pixels[idx + 1] as f32;
                                        let b = pixels[idx + 2] as f32;
                                        let y_val = (0.299 * r + 0.587 * g + 0.114 * b) as u8;
                                        row[x] = y_val;
                                    }
                                }
                            });

                        // UV平面の変換
                        u_plane
                            .par_chunks_mut(target_width / 2)
                            .enumerate()
                            .for_each(|(y, row)| {
                                let src_y = y * 2;
                                for x in 0..target_width / 2 {
                                    let src_x = x * 2;
                                    let idx = (src_y * width + src_x) * 3;
                                    if idx + 2 < pixels.len() {
                                        let r = pixels[idx] as f32;
                                        let g = pixels[idx + 1] as f32;
                                        let b = pixels[idx + 2] as f32;

                                        let u_val =
                                            (-0.147 * r - 0.289 * g + 0.436 * b + 128.0) as u8;
                                        row[x] = u_val;
                                    }
                                }
                            });

                        // V平面の変換
                        v_plane
                            .par_chunks_mut(target_width / 2)
                            .enumerate()
                            .for_each(|(y, row)| {
                                let src_y = y * 2;
                                for x in 0..target_width / 2 {
                                    let src_x = x * 2;
                                    let idx = (src_y * width + src_x) * 3;
                                    if idx + 2 < pixels.len() {
                                        let r = pixels[idx] as f32;
                                        let g = pixels[idx + 1] as f32;
                                        let b = pixels[idx + 2] as f32;

                                        let v_val =
                                            (0.615 * r - 0.515 * g - 0.100 * b + 128.0) as u8;
                                        row[x] = v_val;
                                    }
                                }
                            });

                        if let Some(ref mut enc) = encoder {
                            use openh264::formats::YUVBuffer;
                            let yuv = YUVBuffer::from_vec(yuv_buf, target_width, target_height);
                            if let Ok(bitstream) = enc.encode(&yuv) {
                                let mut bits = Vec::new();
                                bitstream.write_vec(&mut bits);
                                let _ = file.write_all(&bits);
                            }
                        }
                    }

                    println!("Stopped recording to {:?}", filepath);
                });

                self.sessions.insert(
                    *target,
                    RecordingSession {
                        target: *target,
                        tx,
                        handle: Some(handle),
                    },
                );
            }
        }
    }

    pub fn stop_all(&mut self) {
        // tx を drop するとスレッドの rx が Err になり終了する
        self.sessions.clear();
    }

    pub fn dispatch_frame(&self, target: RecordingTarget, frame: Arc<FrameData>) {
        if let Some(session) = self.sessions.get(&target) {
            let _ = session.tx.try_send(frame); // バッファが一杯の場合はスキップ (ノンブロッキング)
        }
    }
}
