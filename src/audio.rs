use std::cell::RefCell;
use std::io::{Read, Write};
use std::num::{NonZeroU32, NonZeroU8};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use crossbeam_channel::Sender;
use ogg::writing::{PacketWriteEndInfo, PacketWriter};
use vorbis_rs::{VorbisBitrateManagementStrategy, VorbisEncoder, VorbisEncoderBuilder};

use crate::config::Codec;
use crate::log::{SharedLog, log_msg};
use crate::stream::{IcecastConfig, IcecastConnection, SharedMetadata};

#[derive(Clone)]
pub struct AudioConfig {
    pub codec: Codec,
    pub sample_rate: u32,
    pub channels: u16,
    pub vorbis_quality: f32,
    pub opus_bitrate_kbps: u32,
}

/// Ducking configuration (from user preferences).
#[derive(Clone)]
pub struct DuckConfig {
    pub threshold: f32,
    pub duck_level: f32,
    pub attack_ms: u32,
    pub release_ms: u32,
    pub hold_ms: u32,
}

#[derive(Clone, Default)]
pub struct AudioLevels {
    pub left: f32,
    pub right: f32,
}

pub type SharedLevels = Arc<Mutex<AudioLevels>>;

// ── OGG Sink ────────────────────────────────────────────────────

struct OggSink {
    buffer: Rc<RefCell<Vec<u8>>>,
}

impl OggSink {
    fn new() -> (Self, Rc<RefCell<Vec<u8>>>) {
        let buffer = Rc::new(RefCell::new(Vec::new()));
        (Self { buffer: buffer.clone() }, buffer)
    }
}

impl Write for OggSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buffer.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// ── Helpers ─────────────────────────────────────────────────────

fn spawn_pw_record(target_serial: u32, rate: u32, channels: u16) -> Result<Child> {
    Command::new("pw-record")
        .args([
            "--raw",
            "--format", "f32",
            "--rate", &rate.to_string(),
            "--channels", &channels.to_string(),
            "--latency", "20ms",
            "--target", &target_serial.to_string(),
            "-",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("Failed to spawn pw-record. Is PipeWire installed?")
}

fn compute_rms(samples: &[f32], channels: u16) -> (f32, f32) {
    if samples.is_empty() || channels == 0 {
        return (0.0, 0.0);
    }
    let mut left_sum = 0.0f64;
    let mut right_sum = 0.0f64;
    let mut count = 0u64;
    for frame in samples.chunks(channels as usize) {
        let left = frame[0] as f64;
        left_sum += left * left;
        let right = if channels > 1 { frame[1] as f64 } else { left };
        right_sum += right * right;
        count += 1;
    }
    if count == 0 {
        return (0.0, 0.0);
    }
    (
        (left_sum / count as f64).sqrt() as f32,
        (right_sum / count as f64).sqrt() as f32,
    )
}

fn deinterleave(interleaved: &[f32], channels: u16) -> Vec<Vec<f32>> {
    let ch = channels as usize;
    let frames = interleaved.len() / ch;
    let mut planar = vec![Vec::with_capacity(frames); ch];
    for frame in interleaved.chunks(ch) {
        for (c, sample) in frame.iter().enumerate() {
            planar[c].push(*sample);
        }
    }
    planar
}

fn bytes_to_samples(bytes: &[u8]) -> Vec<f32> {
    bytes.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn reconnect_icecast(
    config: &IcecastConfig,
    headers: &[u8],
    log: &SharedLog,
    stop: &AtomicBool,
) -> Result<IcecastConnection> {
    const MAX_RETRIES: u32 = 5;
    const RETRY_DELAY: Duration = Duration::from_secs(2);

    for attempt in 1..=MAX_RETRIES {
        if stop.load(Ordering::Relaxed) {
            anyhow::bail!("Reconnection cancelled");
        }
        log_msg(log, &format!("Reconnecting (attempt {attempt}/{MAX_RETRIES})..."));
        std::thread::sleep(RETRY_DELAY);

        match IcecastConnection::connect(config.clone()) {
            Ok(mut conn) => {
                if !headers.is_empty() {
                    conn.send(headers).context("Failed to re-send OGG headers")?;
                }
                log_msg(log, "Reconnected successfully");
                return Ok(conn);
            }
            Err(e) => log_msg(log, &format!("Reconnect failed: {e}")),
        }
    }
    anyhow::bail!("Failed to reconnect after {MAX_RETRIES} attempts")
}

// ── Ducking State ───────────────────────────────────────────────

struct DuckState {
    gain: f32,           // current music gain (0.0-1.0)
    hold_remaining: f32, // seconds remaining in hold
}

impl DuckState {
    fn new() -> Self {
        Self { gain: 1.0, hold_remaining: 0.0 }
    }

    /// Update ducking state and return the current music gain.
    fn update(&mut self, mic_active: bool, mic_rms: f32, cfg: &DuckConfig, dt: f32) -> f32 {
        let ducking = mic_active && mic_rms > cfg.threshold;

        if ducking {
            // Mic is active and above threshold — duck
            self.hold_remaining = cfg.hold_ms as f32 / 1000.0;
            let attack_rate = if cfg.attack_ms > 0 {
                (1.0 - cfg.duck_level) / (cfg.attack_ms as f32 / 1000.0)
            } else {
                f32::MAX
            };
            self.gain = (self.gain - attack_rate * dt).max(cfg.duck_level);
        } else if self.hold_remaining > 0.0 {
            // Hold period — stay ducked
            self.hold_remaining -= dt;
        } else {
            // Release — fade music back up
            let release_rate = if cfg.release_ms > 0 {
                (1.0 - cfg.duck_level) / (cfg.release_ms as f32 / 1000.0)
            } else {
                f32::MAX
            };
            self.gain = (self.gain + release_rate * dt).min(1.0);
        }

        self.gain
    }
}

// ── Music Capture (always-on) ───────────────────────────────────

pub fn run_capture(
    target_serial: u32,
    audio_config: AudioConfig,
    levels: SharedLevels,
    pcm_tx: Sender<Vec<f32>>,
    is_streaming: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    pw_pid: Arc<AtomicU32>,
    log: SharedLog,
) -> Result<()> {
    log_msg(&log, &format!("Music capture started (target: {target_serial})"));
    let mut pw_child = spawn_pw_record(target_serial, audio_config.sample_rate, audio_config.channels)?;
    pw_pid.store(pw_child.id(), Ordering::Relaxed);

    let mut pw_stdout = pw_child.stdout.take().context("Failed to get pw-record stdout")?;
    let buf_size = 512 * audio_config.channels as usize * 4;
    let mut read_buf = vec![0u8; buf_size];

    loop {
        if stop.load(Ordering::Relaxed) { break; }

        let n = match pw_stdout.read(&mut read_buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).context("Failed to read from pw-record"),
        };

        let samples = bytes_to_samples(&read_buf[..n]);
        let (left, right) = compute_rms(&samples, audio_config.channels);
        if let Ok(mut lvl) = levels.lock() {
            lvl.left = left;
            lvl.right = right;
        }

        if is_streaming.load(Ordering::Relaxed) {
            let _ = pcm_tx.try_send(samples);
        }
    }

    pw_pid.store(0, Ordering::Relaxed);
    let _ = pw_child.kill();
    let _ = pw_child.wait();
    log_msg(&log, "Music capture stopped");
    Ok(())
}

// ── Mic Capture (always-on when configured) ─────────────────────

pub fn run_mic_capture(
    target_serial: u32,
    sample_rate: u32,
    levels: SharedLevels,
    mic_tx: Sender<Vec<f32>>,
    is_streaming: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    pw_pid: Arc<AtomicU32>,
    log: SharedLog,
) -> Result<()> {
    log_msg(&log, &format!("Mic capture started (target: {target_serial})"));
    let mut pw_child = spawn_pw_record(target_serial, sample_rate, 1)?; // mono
    pw_pid.store(pw_child.id(), Ordering::Relaxed);

    let mut pw_stdout = pw_child.stdout.take().context("Failed to get mic pw-record stdout")?;
    let buf_size = 512 * 4; // mono f32
    let mut read_buf = vec![0u8; buf_size];

    loop {
        if stop.load(Ordering::Relaxed) { break; }

        let n = match pw_stdout.read(&mut read_buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).context("Failed to read from mic pw-record"),
        };

        let samples = bytes_to_samples(&read_buf[..n]);
        let (rms, _) = compute_rms(&samples, 1);
        if let Ok(mut lvl) = levels.lock() {
            lvl.left = rms;
            lvl.right = rms;
        }

        if is_streaming.load(Ordering::Relaxed) {
            let _ = mic_tx.try_send(samples);
        }
    }

    pw_pid.store(0, Ordering::Relaxed);
    let _ = pw_child.kill();
    let _ = pw_child.wait();
    log_msg(&log, "Mic capture stopped");
    Ok(())
}

// ── Opus / OGG ──────────────────────────────────────────────────

const OPUS_FRAME_SIZE: usize = 960; // 20 ms at 48 kHz
const OPUS_PACKET_MAX: usize = 4000;

fn rand_stream_serial() -> u32 {
    use std::time::SystemTime;
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos()
}

/// Build OGG Opus headers per RFC 7845.
fn build_opus_headers(channels: u16, sample_rate: u32, serial: u32) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut writer = PacketWriter::new(&mut buf);

    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1);
    head.push(channels as u8);
    head.extend_from_slice(&0u16.to_le_bytes()); // pre-skip
    head.extend_from_slice(&sample_rate.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes()); // output gain
    head.push(0); // channel mapping family
    writer.write_packet(head, serial, PacketWriteEndInfo::EndPage, 0)
        .expect("OGG write to Vec is infallible");

    let mut tags = Vec::with_capacity(24);
    tags.extend_from_slice(b"OpusTags");
    let vendor = b"RUMP";
    tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    tags.extend_from_slice(vendor);
    tags.extend_from_slice(&0u32.to_le_bytes()); // no user comments
    writer.write_packet(tags, serial, PacketWriteEndInfo::EndPage, 0)
        .expect("OGG write to Vec is infallible");

    buf
}

// ── Encoder ─────────────────────────────────────────────────────

enum Encoder {
    Vorbis(Box<VorbisState>),
    Opus(OpusState),
}

struct VorbisState {
    encoder: VorbisEncoder<OggSink>,
    sink_buf: Rc<RefCell<Vec<u8>>>,
}

struct OpusState {
    encoder: opus::Encoder,
    serial: u32,
    granule: u64,
    pcm_buf: Vec<f32>,
    packet_scratch: Vec<u8>,
    channels: usize,
}

impl Encoder {
    /// Encode one block of interleaved PCM and append OGG bytes to `out`.
    fn encode(&mut self, music: &[f32], channels: u16, out: &mut Vec<u8>) -> Result<()> {
        match self {
            Encoder::Vorbis(v) => {
                let planar = deinterleave(music, channels);
                let planar_refs: Vec<&[f32]> = planar.iter().map(|c| c.as_slice()).collect();
                v.encoder.encode_audio_block(planar_refs).context("Vorbis encode failed")?;
                let mut sink = v.sink_buf.borrow_mut();
                out.extend_from_slice(&sink);
                sink.clear();
                Ok(())
            }
            Encoder::Opus(o) => {
                o.pcm_buf.extend_from_slice(music);
                let samples_per_frame = OPUS_FRAME_SIZE * o.channels;
                let mut writer = PacketWriter::new(out);
                while o.pcm_buf.len() >= samples_per_frame {
                    let len = o.encoder.encode_float(&o.pcm_buf[..samples_per_frame], &mut o.packet_scratch)
                        .context("Opus encode failed")?;
                    o.pcm_buf.drain(..samples_per_frame);
                    o.granule += OPUS_FRAME_SIZE as u64;
                    writer.write_packet(
                        o.packet_scratch[..len].to_vec(),
                        o.serial,
                        PacketWriteEndInfo::NormalPacket,
                        o.granule,
                    ).context("OGG write failed")?;
                }
                Ok(())
            }
        }
    }

    /// Drain any pending state and append final OGG bytes (with end-of-stream marker for Opus).
    fn finish(self, out: &mut Vec<u8>) -> Result<()> {
        match self {
            Encoder::Vorbis(v) => {
                v.encoder.finish().context("Failed to finish Vorbis encoder")?;
                out.extend_from_slice(&v.sink_buf.borrow());
                Ok(())
            }
            Encoder::Opus(mut o) => {
                let samples_per_frame = OPUS_FRAME_SIZE * o.channels;
                if !o.pcm_buf.is_empty() && o.pcm_buf.len() < samples_per_frame {
                    o.pcm_buf.resize(samples_per_frame, 0.0);
                }
                let mut writer = PacketWriter::new(out);
                while o.pcm_buf.len() >= samples_per_frame {
                    let len = o.encoder.encode_float(&o.pcm_buf[..samples_per_frame], &mut o.packet_scratch)
                        .context("Opus encode failed")?;
                    o.pcm_buf.drain(..samples_per_frame);
                    o.granule += OPUS_FRAME_SIZE as u64;
                    let info = if o.pcm_buf.is_empty() {
                        PacketWriteEndInfo::EndStream
                    } else {
                        PacketWriteEndInfo::NormalPacket
                    };
                    writer.write_packet(o.packet_scratch[..len].to_vec(), o.serial, info, o.granule)
                        .context("OGG write failed")?;
                }
                Ok(())
            }
        }
    }
}

// ── Stream with Mixing (on-demand) ──────────────────────────────

pub fn run_stream(
    audio_config: AudioConfig,
    icecast_config: IcecastConfig,
    pcm_rx: crossbeam_channel::Receiver<Vec<f32>>,
    mic_rx: Option<crossbeam_channel::Receiver<Vec<f32>>>,
    is_mic_toggled: Arc<AtomicBool>,
    is_mic_ptt: Arc<AtomicBool>,
    duck_config: DuckConfig,
    metadata: SharedMetadata,
    stop: Arc<AtomicBool>,
    log: SharedLog,
    error_slot: Arc<Mutex<Option<String>>>,
) -> Result<()> {
    let (encoder, header_bytes) = build_encoder(&audio_config, &log)?;

    let mut conn = IcecastConnection::connect(icecast_config.clone())?;
    log_msg(&log, "Connected to Icecast");

    if !header_bytes.is_empty() {
        conn.send(&header_bytes).context("Failed to send OGG headers")?;
        log_msg(&log, "Streaming started");
    }

    let mut encoder = encoder;
    let mut duck = DuckState::new();
    let frames_per_chunk = 512;
    let dt = frames_per_chunk as f32 / audio_config.sample_rate as f32;
    let mut ogg_buf: Vec<u8> = Vec::with_capacity(8192);

    loop {
        if stop.load(Ordering::Relaxed) { break; }

        let mut music = match pcm_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(s) => s,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        };

        let mic_active = is_mic_toggled.load(Ordering::Relaxed) || is_mic_ptt.load(Ordering::Relaxed);
        if let Some(ref mic_rx) = mic_rx {
            if let Ok(mic_samples) = mic_rx.try_recv() {
                let (mic_rms, _) = compute_rms(&mic_samples, 1);
                let gain = duck.update(mic_active, mic_rms, &duck_config, dt);
                let channels = audio_config.channels as usize;
                let frames = (music.len() / channels).min(mic_samples.len());
                for i in 0..frames {
                    let mic_sample = if mic_active { mic_samples[i] } else { 0.0 };
                    for ch in 0..channels {
                        music[i * channels + ch] = music[i * channels + ch] * gain + mic_sample;
                    }
                }
                for i in (frames * channels)..(music.len()) {
                    music[i] *= gain;
                }
            } else {
                let gain = duck.update(false, 0.0, &duck_config, dt);
                if gain < 1.0 {
                    for s in &mut music { *s *= gain; }
                }
            }
        }

        ogg_buf.clear();
        encoder.encode(&music, audio_config.channels, &mut ogg_buf)?;

        if !ogg_buf.is_empty() {
            if let Err(e) = conn.send(&ogg_buf) {
                log_msg(&log, &format!("Send failed: {e}, reconnecting..."));
                conn = reconnect_icecast(&icecast_config, &header_bytes, &log, &stop)?;
                conn.send(&ogg_buf).context("Failed to send after reconnection")?;
            }
        }

        if let Ok(mut meta) = metadata.lock() {
            if meta.changed {
                meta.changed = false;
                let snap = meta.clone();
                drop(meta);
                let song = snap.display_string();
                if !song.is_empty() {
                    log_msg(&log, &format!("Now playing: {song}"));
                }
                if let Err(e) = conn.update_metadata(&snap) {
                    log_msg(&log, &format!("Metadata update failed: {e}"));
                }
            }
        }
    }

    ogg_buf.clear();
    encoder.finish(&mut ogg_buf)?;
    if !ogg_buf.is_empty() {
        let _ = conn.send(&ogg_buf);
    }

    if let Ok(mut err) = error_slot.lock() {
        *err = None;
    }
    Ok(())
}

fn build_encoder(audio_config: &AudioConfig, log: &SharedLog) -> Result<(Encoder, Vec<u8>)> {
    match audio_config.codec {
        Codec::Opus => {
            log_msg(log, &format!(
                "Encoding: OGG Opus, {}Hz, {}ch, {}kbps",
                audio_config.sample_rate, audio_config.channels, audio_config.opus_bitrate_kbps
            ));
            let channels = match audio_config.channels {
                1 => opus::Channels::Mono,
                _ => opus::Channels::Stereo,
            };
            let mut enc = opus::Encoder::new(audio_config.sample_rate, channels, opus::Application::Audio)
                .context("Failed to create Opus encoder")?;
            enc.set_bitrate(opus::Bitrate::Bits((audio_config.opus_bitrate_kbps * 1000) as i32))
                .context("Failed to set Opus bitrate")?;
            let serial = rand_stream_serial();
            let headers = build_opus_headers(audio_config.channels, audio_config.sample_rate, serial);
            let encoder = Encoder::Opus(OpusState {
                encoder: enc,
                serial,
                granule: 0,
                pcm_buf: Vec::with_capacity(OPUS_FRAME_SIZE * audio_config.channels as usize * 2),
                packet_scratch: vec![0u8; OPUS_PACKET_MAX],
                channels: audio_config.channels as usize,
            });
            Ok((encoder, headers))
        }
        Codec::Vorbis => {
            log_msg(log, &format!(
                "Encoding: OGG Vorbis, {}Hz, {}ch, quality {:.1}",
                audio_config.sample_rate, audio_config.channels, audio_config.vorbis_quality
            ));
            let (sink, sink_buf) = OggSink::new();
            let mut builder = VorbisEncoderBuilder::new(
                NonZeroU32::new(audio_config.sample_rate).context("Invalid sample rate")?,
                NonZeroU8::new(audio_config.channels as u8).context("Invalid channel count")?,
                sink,
            ).context("Failed to create Vorbis encoder builder")?;
            builder.bitrate_management_strategy(VorbisBitrateManagementStrategy::QualityVbr {
                target_quality: audio_config.vorbis_quality,
            });
            let encoder = builder.build().context("Failed to build Vorbis encoder")?;
            let headers: Vec<u8> = sink_buf.borrow_mut().drain(..).collect();
            Ok((Encoder::Vorbis(Box::new(VorbisState { encoder, sink_buf })), headers))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bytes_to_samples() {
        let bytes: Vec<u8> = [1.0f32, -1.0f32, 0.5f32]
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect();
        let samples = bytes_to_samples(&bytes);
        assert_eq!(samples, vec![1.0, -1.0, 0.5]);
    }

    #[test]
    fn test_compute_rms_stereo() {
        // Constant signal of 0.5 on both channels
        let samples = vec![0.5f32, 0.5, 0.5, 0.5, 0.5, 0.5];
        let (l, r) = compute_rms(&samples, 2);
        assert!((l - 0.5).abs() < 0.001);
        assert!((r - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_compute_rms_silence() {
        let samples = vec![0.0f32; 8];
        let (l, r) = compute_rms(&samples, 2);
        assert_eq!(l, 0.0);
        assert_eq!(r, 0.0);
    }

    #[test]
    fn test_compute_rms_empty() {
        let (l, r) = compute_rms(&[], 2);
        assert_eq!(l, 0.0);
        assert_eq!(r, 0.0);
    }

    #[test]
    fn test_deinterleave_stereo() {
        let interleaved = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let planar = deinterleave(&interleaved, 2);
        assert_eq!(planar[0], vec![1.0, 3.0, 5.0]); // left
        assert_eq!(planar[1], vec![2.0, 4.0, 6.0]); // right
    }

    #[test]
    fn test_deinterleave_mono() {
        let interleaved = vec![1.0, 2.0, 3.0];
        let planar = deinterleave(&interleaved, 1);
        assert_eq!(planar[0], vec![1.0, 2.0, 3.0]);
    }

    fn test_duck_config() -> DuckConfig {
        DuckConfig {
            threshold: 0.02,
            duck_level: 0.2,
            attack_ms: 100,
            release_ms: 800,
            hold_ms: 500,
        }
    }

    #[test]
    fn test_duck_state_initial() {
        let duck = DuckState::new();
        assert_eq!(duck.gain, 1.0);
    }

    #[test]
    fn test_duck_state_ducks_on_mic() {
        let mut duck = DuckState::new();
        let cfg = test_duck_config();
        // Simulate several frames of mic active above threshold
        for _ in 0..20 {
            duck.update(true, 0.1, &cfg, 0.01);
        }
        assert!(duck.gain < 0.5, "gain should have ducked: {}", duck.gain);
    }

    #[test]
    fn test_duck_state_recovers() {
        let mut duck = DuckState::new();
        let cfg = test_duck_config();
        // Duck fully
        for _ in 0..50 {
            duck.update(true, 0.1, &cfg, 0.01);
        }
        assert!(duck.gain <= cfg.duck_level + 0.05);
        // Release (mic off, wait past hold)
        for _ in 0..200 {
            duck.update(false, 0.0, &cfg, 0.01);
        }
        assert!(duck.gain > 0.9, "gain should have recovered: {}", duck.gain);
    }

    #[test]
    fn test_duck_state_no_duck_below_threshold() {
        let mut duck = DuckState::new();
        let cfg = test_duck_config();
        // Mic active but below threshold
        duck.update(true, 0.001, &cfg, 0.01);
        assert_eq!(duck.gain, 1.0);
    }
}
