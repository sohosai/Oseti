use mp4::{
    AvcConfig, Bytes as Mp4Bytes, FourCC, MediaConfig, Mp4Config, Mp4Sample, Mp4Writer,
    TrackConfig, TrackType,
};
use openh264::encoder::{Encoder, FrameType};
use openh264::formats::{RgbSliceU8, YUVBuffer};
use openh264::nal_units;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::thread;

use crate::source::FrameData;

const MOVIE_TIMESCALE: u32 = 90_000;
const SAMPLE_DURATION: u32 = 3_000;

#[derive(Debug)]
struct EncodedFrame {
    bytes: Vec<u8>,
    is_sync: bool,
    width: u16,
    height: u16,
}

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
                let (io_tx, io_rx) = mpsc::sync_channel::<EncodedFrame>(30);

                let filename = format!(
                    "{}_{}.mp4",
                    chrono::Local::now().format("%Y%m%d_%H%M%S"),
                    target.name()
                );
                let filepath = self.save_dir.join(filename);

                let writer_path = filepath.clone();
                let writer_handle = thread::spawn(move || {
                    let mut pending_frames: Vec<EncodedFrame> = Vec::new();
                    let mut writer: Option<Mp4Writer<BufWriter<File>>> = None;
                    let mut track_id = 0;
                    let mut next_start_time = 0u64;

                    while let Ok(frame) = io_rx.recv() {
                        pending_frames.push(frame);

                        if writer.is_none() {
                            if let Some((config, avc_config)) = build_mp4_config(&pending_frames) {
                                let file = match File::create(&writer_path) {
                                    Ok(file) => file,
                                    Err(e) => {
                                        eprintln!("Failed to create recording file: {}", e);
                                        return;
                                    }
                                };

                                println!("Started recording to {:?}", writer_path);

                                let mut mp4_writer =
                                    match Mp4Writer::write_start(BufWriter::new(file), &config) {
                                        Ok(writer) => writer,
                                        Err(e) => {
                                            eprintln!("Failed to start MP4 writer: {:?}", e);
                                            return;
                                        }
                                    };

                                let track_config = TrackConfig {
                                    track_type: TrackType::Video,
                                    timescale: MOVIE_TIMESCALE,
                                    language: String::from("und"),
                                    media_conf: MediaConfig::AvcConfig(avc_config),
                                };

                                if let Err(e) = mp4_writer.add_track(&track_config) {
                                    eprintln!("Failed to add MP4 track: {:?}", e);
                                    return;
                                }

                                writer = Some(mp4_writer);
                                track_id = 1;

                                if let Some(writer) = writer.as_mut() {
                                    for pending in pending_frames.drain(..) {
                                        if let Err(e) = write_mp4_sample(
                                            writer,
                                            track_id,
                                            &pending,
                                            &mut next_start_time,
                                        ) {
                                            eprintln!("Failed to write MP4 sample: {:?}", e);
                                            return;
                                        }
                                    }
                                }
                            }
                            continue;
                        }

                        if let Some(writer) = writer.as_mut() {
                            let frame = pending_frames.pop().expect("frame just pushed");
                            if let Err(e) =
                                write_mp4_sample(writer, track_id, &frame, &mut next_start_time)
                            {
                                eprintln!("Failed to write MP4 sample: {:?}", e);
                                return;
                            }
                        }
                    }

                    if let Some(mut writer) = writer {
                        if let Err(e) = writer.write_end() {
                            eprintln!("Failed to finalize MP4 file: {:?}", e);
                            return;
                        }

                        let mut writer = writer.into_writer();
                        if let Err(e) = writer.flush() {
                            eprintln!("Recording flush error: {}", e);
                        }
                        println!("Stopped recording to {:?}", writer_path);
                    }
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

                        if let Some(ref mut enc) = encoder {
                            let rgb = RgbSliceU8::new(frame.pixels.as_slice(), (target_width, target_height));
                            let yuv = YUVBuffer::from_rgb8_source(rgb);
                            if let Ok(bitstream) = enc.encode(&yuv) {
                                let bytes = bitstream.to_vec();
                                let is_sync = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);
                                let _ = io_tx.try_send(EncodedFrame {
                                    bytes,
                                    is_sync,
                                    width: target_width as u16,
                                    height: target_height as u16,
                                });
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

fn strip_annexb_start_code(nal: &[u8]) -> &[u8] {
    if nal.starts_with(&[0, 0, 0, 1]) {
        &nal[4..]
    } else if nal.starts_with(&[0, 0, 1]) {
        &nal[3..]
    } else {
        nal
    }
}

fn nal_units_without_start_codes(data: &[u8]) -> Vec<&[u8]> {
    nal_units(data)
        .map(strip_annexb_start_code)
        .filter(|nal| !nal.is_empty())
        .collect()
}

fn frame_to_mp4_sample_bytes(data: &[u8]) -> Vec<u8> {
    let mut sample = Vec::new();
    for nal in nal_units_without_start_codes(data) {
        sample.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        sample.extend_from_slice(nal);
    }
    sample
}

fn build_mp4_config(frames: &[EncodedFrame]) -> Option<(Mp4Config, AvcConfig)> {
    let first_frame = frames.first()?;
    let mut seq_param_set = None;
    let mut pic_param_set = None;

    for frame in frames {
        for nal in nal_units_without_start_codes(&frame.bytes) {
            match nal.first().copied().map(|byte| byte & 0x1F) {
                Some(7) if seq_param_set.is_none() => seq_param_set = Some(nal.to_vec()),
                Some(8) if pic_param_set.is_none() => pic_param_set = Some(nal.to_vec()),
                _ => {}
            }

            if seq_param_set.is_some() && pic_param_set.is_some() {
                break;
            }
        }

        if seq_param_set.is_some() && pic_param_set.is_some() {
            break;
        }
    }

    let seq_param_set = seq_param_set?;
    let pic_param_set = pic_param_set?;

    let config = Mp4Config {
        major_brand: FourCC { value: *b"isom" },
        minor_version: 512,
        compatible_brands: vec![
            FourCC { value: *b"isom" },
            FourCC { value: *b"iso2" },
            FourCC { value: *b"avc1" },
            FourCC { value: *b"mp41" },
        ],
        timescale: MOVIE_TIMESCALE,
    };

    let avc_config = AvcConfig {
        width: first_frame.width,
        height: first_frame.height,
        seq_param_set,
        pic_param_set,
    };

    Some((config, avc_config))
}

fn write_mp4_sample(
    writer: &mut Mp4Writer<BufWriter<File>>,
    track_id: u32,
    frame: &EncodedFrame,
    next_start_time: &mut u64,
) -> Result<(), mp4::Error> {
    let sample_bytes = frame_to_mp4_sample_bytes(&frame.bytes);
    let sample = Mp4Sample {
        start_time: *next_start_time,
        duration: SAMPLE_DURATION,
        rendering_offset: 0,
        is_sync: frame.is_sync,
        bytes: Mp4Bytes::from(sample_bytes),
    };

    writer.write_sample(track_id, &sample)?;
    *next_start_time += SAMPLE_DURATION as u64;
    Ok(())
}
