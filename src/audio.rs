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
use vorbis_rs::{VorbisBitrateManagementStrategy, VorbisEncoderBuilder};

use crate::log::{SharedLog, log_msg};
use crate::stream::{IcecastConfig, IcecastConnection, SharedMetadata};

#[derive(Clone)]
pub struct AudioConfig {
    pub sample_rate: u32,
    pub channels: u16,
    pub vorbis_quality: f32,
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
    log_msg(&log, &format!(
        "Encoding: OGG Vorbis, {}Hz, {}ch, quality {:.1}",
        audio_config.sample_rate, audio_config.channels, audio_config.vorbis_quality
    ));

    let (sink, sink_buffer) = OggSink::new();
    let mut builder = VorbisEncoderBuilder::new(
        NonZeroU32::new(audio_config.sample_rate).context("Invalid sample rate")?,
        NonZeroU8::new(audio_config.channels as u8).context("Invalid channel count")?,
        sink,
    ).context("Failed to create Vorbis encoder builder")?;

    builder.bitrate_management_strategy(VorbisBitrateManagementStrategy::QualityVbr {
        target_quality: audio_config.vorbis_quality,
    });

    let mut encoder = builder.build().context("Failed to build Vorbis encoder")?;
    let header_bytes: Vec<u8> = sink_buffer.borrow_mut().drain(..).collect();

    let mut conn = IcecastConnection::connect(icecast_config.clone())?;
    log_msg(&log, "Connected to Icecast");

    if !header_bytes.is_empty() {
        conn.send(&header_bytes).context("Failed to send OGG headers")?;
        log_msg(&log, "Streaming started");
    }

    let mut duck = DuckState::new();
    let frames_per_chunk = 512;
    let dt = frames_per_chunk as f32 / audio_config.sample_rate as f32;

    loop {
        if stop.load(Ordering::Relaxed) { break; }

        // Receive music PCM (blocking with timeout)
        let mut music = match pcm_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(s) => s,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        };

        // Try to receive mic PCM and mix
        let mic_active = is_mic_toggled.load(Ordering::Relaxed) || is_mic_ptt.load(Ordering::Relaxed);
        if let Some(ref mic_rx) = mic_rx {
            if let Ok(mic_samples) = mic_rx.try_recv() {
                // Compute mic RMS for ducking
                let (mic_rms, _) = compute_rms(&mic_samples, 1);
                let gain = duck.update(mic_active, mic_rms, &duck_config, dt);

                // Mix: apply gain to music, add mic (mono → stereo)
                let channels = audio_config.channels as usize;
                let music_frames = music.len() / channels;
                let mic_frames = mic_samples.len();
                let frames = music_frames.min(mic_frames);

                for i in 0..frames {
                    let mic_sample = if mic_active { mic_samples[i] } else { 0.0 };
                    for ch in 0..channels {
                        music[i * channels + ch] = music[i * channels + ch] * gain + mic_sample;
                    }
                }
                // Apply gain to any remaining music frames (beyond mic data)
                for i in (frames * channels)..(music.len()) {
                    music[i] *= gain;
                }
            } else {
                // No mic data — just update duck state toward release
                let gain = duck.update(false, 0.0, &duck_config, dt);
                if gain < 1.0 {
                    for s in &mut music {
                        *s *= gain;
                    }
                }
            }
        }

        // Encode
        let planar = deinterleave(&music, audio_config.channels);
        let planar_refs: Vec<&[f32]> = planar.iter().map(|ch| ch.as_slice()).collect();
        encoder.encode_audio_block(planar_refs).context("Vorbis encode failed")?;

        let ogg_bytes: Vec<u8> = sink_buffer.borrow_mut().drain(..).collect();
        if !ogg_bytes.is_empty() {
            if let Err(e) = conn.send(&ogg_bytes) {
                log_msg(&log, &format!("Send failed: {e}, reconnecting..."));
                conn = reconnect_icecast(&icecast_config, &header_bytes, &log, &stop)?;
                conn.send(&ogg_bytes).context("Failed to send after reconnection")?;
            }
        }

        // Metadata
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

    encoder.finish().context("Failed to finish Vorbis encoder")?;
    let final_bytes: Vec<u8> = sink_buffer.borrow_mut().drain(..).collect();
    if !final_bytes.is_empty() {
        let _ = conn.send(&final_bytes);
    }

    if let Ok(mut err) = error_slot.lock() {
        *err = None;
    }
    Ok(())
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
