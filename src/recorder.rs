use openh264::encoder::Encoder;
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::thread;

use crate::source::FrameData;

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
    tx: Option<mpsc::SyncSender<Arc<FrameData>>>,
    encode_handle: Option<thread::JoinHandle<()>>,
    writer_handle: Option<thread::JoinHandle<()>>,
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
                let (tx, rx) = mpsc::sync_channel::<Arc<FrameData>>(5);
                let (io_tx, io_rx) = mpsc::sync_channel::<Vec<u8>>(30);

                // mp4化前の段階として、現状は Annex-B H.264 を保存する
                let filename = format!(
                    "{}_{}.h264",
                    chrono::Local::now().format("%Y%m%d_%H%M%S"),
                    target.name()
                );
                let filepath = self.save_dir.join(filename);

                let writer_path = filepath.clone();
                let writer_handle = thread::spawn(move || {
                    let file = match File::create(&writer_path) {
                        Ok(f) => f,
                        Err(e) => {
                            eprintln!("Failed to create recording file: {}", e);
                            return;
                        }
                    };

                    println!("Started recording to {:?}", writer_path);
                    let mut writer = BufWriter::new(file);

                    while let Ok(data) = io_rx.recv() {
                        if let Err(e) = writer.write_all(&data) {
                            eprintln!("Recording write error: {}", e);
                            break;
                        }
                    }

                    if let Err(e) = writer.flush() {
                        eprintln!("Recording flush error: {}", e);
                    }
                    println!("Stopped recording to {:?}", writer_path);
                });

                let encode_handle = thread::spawn(move || {
                    let mut encoder: Option<Encoder> = None;
                    let mut target_width = 1280;
                    let mut target_height = 720;

                    while let Ok(frame) = rx.recv() {
                        let width = frame.width as usize;
                        let height = frame.height as usize;

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

                        let pixels = &frame.pixels;
                        let y_len = target_width * target_height;
                        let uv_len = (target_width / 2) * (target_height / 2);
                        let mut yuv_buf = vec![0u8; y_len + uv_len * 2];

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
                                let _ = io_tx.try_send(bits);
                            }
                        }
                    }
                });

                self.sessions.insert(
                    *target,
                    RecordingSession {
                        tx: Some(tx),
                        encode_handle: Some(encode_handle),
                        writer_handle: Some(writer_handle),
                    },
                );
            }
        }
    }

    pub fn stop_all(&mut self) {
        let mut sessions: Vec<RecordingSession> = self.sessions.drain().map(|(_, s)| s).collect();

        for session in &mut sessions {
            let _ = session.tx.take();
        }

        for mut session in sessions {
            if let Some(handle) = session.encode_handle.take() {
                let _ = handle.join();
            }
            if let Some(handle) = session.writer_handle.take() {
                let _ = handle.join();
            }
        }
    }

    pub fn dispatch_frame(&self, target: RecordingTarget, frame: Arc<FrameData>) {
        if let Some(session) = self.sessions.get(&target) {
            if let Some(tx) = &session.tx {
                let _ = tx.try_send(frame);
            }
        }
    }
}
