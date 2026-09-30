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
use crate::stream::{IcecastConfig, IcecastConnection, SharedMetadata, TrackMetadata, send_admin_metadata};

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

/// Frames per capture chunk. Whole chunks keep L/R aligned (a short pipe read
/// can end mid-frame) and give the ducker a fixed time step.
const CHUNK_FRAMES: usize = 512;

/// Fill `buf` exactly. Ok(false) when pw-record has exited.
fn read_chunk(src: &mut impl Read, buf: &mut [u8], log: &SharedLog, what: &str) -> Result<bool> {
    match src.read_exact(buf) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            log_msg(log, &format!("{what} exited"));
            Ok(false)
        }
        Err(e) => Err(e).with_context(|| format!("Failed to read from {what}")),
    }
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

/// Sleep up to `d`, returning early (false) if `stop` is raised.
fn sleep_unless_stopped(d: Duration, stop: &AtomicBool) -> bool {
    let step = Duration::from_millis(100);
    let mut left = d;
    while !left.is_zero() {
        if stop.load(Ordering::Relaxed) { return false; }
        let s = left.min(step);
        std::thread::sleep(s);
        left -= s;
    }
    !stop.load(Ordering::Relaxed)
}

/// Reconnect until it works or the user stops the stream. An uplink that
/// gives up after a few seconds turns a server restart into dead air.
fn reconnect_icecast(
    config: &IcecastConfig,
    headers: &[u8],
    log: &SharedLog,
    stop: &AtomicBool,
) -> Result<IcecastConnection> {
    const MAX_DELAY: Duration = Duration::from_secs(30);
    let mut delay = Duration::from_secs(2);

    for attempt in 1.. {
        log_msg(log, &format!("Reconnecting in {}s (attempt {attempt})...", delay.as_secs()));
        if !sleep_unless_stopped(delay, stop) {
            anyhow::bail!("Reconnection cancelled");
        }
        match IcecastConnection::connect(config) {
            Ok(mut conn) => match conn.send(headers) {
                Ok(()) => {
                    log_msg(log, "Reconnected");
                    return Ok(conn);
                }
                Err(e) => log_msg(log, &format!("Reconnect failed: {e:#}")),
            },
            Err(e) => log_msg(log, &format!("Reconnect failed: {e:#}")),
        }
        delay = (delay * 2).min(MAX_DELAY);
    }
    unreachable!()
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

#[allow(clippy::too_many_arguments)]
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
    let mut read_buf = vec![0u8; CHUNK_FRAMES * audio_config.channels as usize * 4];

    loop {
        if stop.load(Ordering::Relaxed) { break; }

        if !read_chunk(&mut pw_stdout, &mut read_buf, &log, "pw-record")? { break; }

        let samples = bytes_to_samples(&read_buf);
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

#[allow(clippy::too_many_arguments)]
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
    let mut read_buf = vec![0u8; CHUNK_FRAMES * 4]; // mono f32

    loop {
        if stop.load(Ordering::Relaxed) { break; }

        if !read_chunk(&mut pw_stdout, &mut read_buf, &log, "mic pw-record")? { break; }

        let samples = bytes_to_samples(&read_buf);
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
/// Close an OGG page every 5 packets (100 ms). The writer only emits a page
/// when one is closed, so this bounds both latency and burst size.
const OPUS_PACKETS_PER_PAGE: u32 = 5;

fn rand_stream_serial() -> u32 {
    use std::time::SystemTime;
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos()
}

/// Write the OGG Opus header pages (RFC 7845) through the stream's writer,
/// so the audio pages that follow continue its page sequence.
fn write_opus_headers(
    writer: &mut PacketWriter<'static, Vec<u8>>,
    channels: u16,
    sample_rate: u32,
    pre_skip: u16,
    serial: u32,
    meta: &TrackMetadata,
) {
    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1);
    head.push(channels as u8);
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&sample_rate.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes()); // output gain
    head.push(0); // channel mapping family
    writer.write_packet(head, serial, PacketWriteEndInfo::EndPage, 0)
        .expect("OGG write to Vec is infallible");

    // Titles travel here: Icecast's updinfo does not reach Ogg Opus
    // listeners, so a track change starts a new chained stream (run_stream).
    let comments: Vec<String> = [("TITLE", &meta.title), ("ARTIST", &meta.artist)]
        .into_iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    let mut tags = Vec::with_capacity(64);
    tags.extend_from_slice(b"OpusTags");
    let vendor = b"RUMP";
    tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    tags.extend_from_slice(vendor);
    tags.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for c in &comments {
        tags.extend_from_slice(&(c.len() as u32).to_le_bytes());
        tags.extend_from_slice(c.as_bytes());
    }
    writer.write_packet(tags, serial, PacketWriteEndInfo::EndPage, 0)
        .expect("OGG write to Vec is infallible");
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
    /// Lives for the whole stream: it buffers packets until a page is closed,
    /// and carries the page sequence number. A writer per encode() call would
    /// drop every packet it had not yet paged out.
    writer: PacketWriter<'static, Vec<u8>>,
    serial: u32,
    granule: u64,
    packets_in_page: u32,
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
                while o.pcm_buf.len() >= samples_per_frame {
                    let len = o.encoder.encode_float(&o.pcm_buf[..samples_per_frame], &mut o.packet_scratch)
                        .context("Opus encode failed")?;
                    o.pcm_buf.drain(..samples_per_frame);
                    o.granule += OPUS_FRAME_SIZE as u64;
                    o.packets_in_page += 1;
                    let info = if o.packets_in_page >= OPUS_PACKETS_PER_PAGE {
                        o.packets_in_page = 0;
                        PacketWriteEndInfo::EndPage
                    } else {
                        PacketWriteEndInfo::NormalPacket
                    };
                    o.writer.write_packet(o.packet_scratch[..len].to_vec(), o.serial, info, o.granule)
                        .context("OGG write failed")?;
                }
                out.append(o.writer.inner_mut());
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
                // Pad to whole frames, and to at least one: the end-of-stream
                // flag rides on a packet, and it also flushes any packets
                // still waiting in the current page.
                let samples_per_frame = OPUS_FRAME_SIZE * o.channels;
                let frames = o.pcm_buf.len().div_ceil(samples_per_frame).max(1);
                o.pcm_buf.resize(frames * samples_per_frame, 0.0);
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
                    o.writer.write_packet(o.packet_scratch[..len].to_vec(), o.serial, info, o.granule)
                        .context("OGG write failed")?;
                }
                out.append(o.writer.inner_mut());
                Ok(())
            }
        }
    }
}

// ── Stream with Mixing (on-demand) ──────────────────────────────

/// Stream until stopped. Stopping mid-reconnect surfaces as an error from
/// the loop; that is a normal exit, not a failure to show the user.
#[allow(clippy::too_many_arguments)]
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
    let stopped = stop.clone();
    match stream_loop(audio_config, icecast_config, pcm_rx, mic_rx, is_mic_toggled, is_mic_ptt, duck_config, metadata, stop, log, error_slot) {
        Err(_) if stopped.load(Ordering::Relaxed) => Ok(()),
        r => r,
    }
}

#[allow(clippy::too_many_arguments)]
fn stream_loop(
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
    log_msg(&log, &describe(&audio_config));

    // The title playing at connect time goes into the first OpusTags.
    let initial = take_metadata(&metadata, true).unwrap_or_default();
    let (mut encoder, mut header_bytes) = build_encoder(&audio_config, &initial)?;

    let mut conn = IcecastConnection::connect(&icecast_config)?;
    log_msg(&log, "Connected to Icecast");
    conn.send(&header_bytes).context("Failed to send OGG headers")?;
    log_msg(&log, "Streaming started");

    let mut sent_song = String::new();
    announce(&initial, &mut sent_song, &encoder, &icecast_config, &log);

    let mut duck = DuckState::new();
    let dt = CHUNK_FRAMES as f32 / audio_config.sample_rate as f32;
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
                for s in &mut music[frames * channels..] {
                    *s *= gain;
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
            send_or_reconnect(&mut conn, &ogg_buf, &icecast_config, &header_bytes, &log, &stop)?;
        }

        if let Some(meta) = take_metadata(&metadata, false) {
            if announce(&meta, &mut sent_song, &encoder, &icecast_config, &log)
                && matches!(encoder, Encoder::Opus(_))
            {
                // Chain a new logical stream: end the current one, then
                // headers whose OpusTags carry the new title.
                let (next, headers) = build_encoder(&audio_config, &meta)?;
                ogg_buf.clear();
                std::mem::replace(&mut encoder, next).finish(&mut ogg_buf)?;
                send_or_reconnect(&mut conn, &ogg_buf, &icecast_config, &header_bytes, &log, &stop)?;
                header_bytes = headers;
                if let Err(e) = conn.send(&header_bytes) {
                    log_msg(&log, &format!("Send failed: {e:#}"));
                    // A reconnect sends the (new) headers itself.
                    conn = reconnect_icecast(&icecast_config, &header_bytes, &log, &stop)?;
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

/// Send, and on failure reconnect (which re-sends `headers`) and resend.
fn send_or_reconnect(
    conn: &mut IcecastConnection,
    data: &[u8],
    config: &IcecastConfig,
    headers: &[u8],
    log: &SharedLog,
    stop: &AtomicBool,
) -> Result<()> {
    if let Err(e) = conn.send(data) {
        log_msg(log, &format!("Send failed: {e:#}"));
        *conn = reconnect_icecast(config, headers, log, stop)?;
        conn.send(data).context("Failed to send after reconnection")?;
    }
    Ok(())
}

/// Take a metadata snapshot if it changed (or unconditionally with `always`),
/// clearing the changed flag.
fn take_metadata(metadata: &SharedMetadata, always: bool) -> Option<TrackMetadata> {
    let mut m = metadata.lock().ok()?;
    if !always && !m.changed { return None; }
    m.changed = false;
    Some(m.clone())
}

/// Log a new title and, for Vorbis, push it to Icecast's admin interface on a
/// side thread (it opens a connection; the audio loop must not wait on it).
/// Returns false when there is nothing new to announce: playerctl repeats
/// itself on play/pause, and each Opus announcement costs a stream chain.
fn announce(
    meta: &TrackMetadata,
    sent_song: &mut String,
    encoder: &Encoder,
    config: &IcecastConfig,
    log: &SharedLog,
) -> bool {
    let song = meta.display_string();
    if song.is_empty() || song == *sent_song { return false; }
    *sent_song = song.clone();
    log_msg(log, &format!("Now playing: {song}"));
    if matches!(encoder, Encoder::Vorbis(_)) {
        let (config, log) = (config.clone(), log.clone());
        std::thread::spawn(move || {
            if let Err(e) = send_admin_metadata(&config, &song) {
                log_msg(&log, &format!("Metadata update failed: {e:#}"));
            }
        });
    }
    true
}

fn describe(audio_config: &AudioConfig) -> String {
    match audio_config.codec {
        Codec::Opus => format!(
            "Encoding: OGG Opus, {}Hz, {}ch, {}kbps",
            audio_config.sample_rate, audio_config.channels, audio_config.opus_bitrate_kbps
        ),
        Codec::Vorbis => format!(
            "Encoding: OGG Vorbis, {}Hz, {}ch, quality {:.1}",
            audio_config.sample_rate, audio_config.channels, audio_config.vorbis_quality
        ),
    }
}

fn build_encoder(audio_config: &AudioConfig, meta: &TrackMetadata) -> Result<(Encoder, Vec<u8>)> {
    match audio_config.codec {
        Codec::Opus => {
            let channels = match audio_config.channels {
                1 => opus::Channels::Mono,
                _ => opus::Channels::Stereo,
            };
            let mut enc = opus::Encoder::new(audio_config.sample_rate, channels, opus::Application::Audio)
                .context("Failed to create Opus encoder")?;
            enc.set_bitrate(opus::Bitrate::Bits((audio_config.opus_bitrate_kbps * 1000) as i32))
                .context("Failed to set Opus bitrate")?;
            enc.set_signal(opus::Signal::Music).context("Failed to set Opus signal type")?;
            // Decoders drop this many leading samples: the encoder's warm-up.
            let pre_skip = enc.get_lookahead().context("Failed to read Opus lookahead")? as u16;
            let serial = rand_stream_serial();
            let mut writer = PacketWriter::new(Vec::new());
            write_opus_headers(&mut writer, audio_config.channels, audio_config.sample_rate, pre_skip, serial, meta);
            let headers = std::mem::take(writer.inner_mut());
            let encoder = Encoder::Opus(OpusState {
                encoder: enc,
                writer,
                serial,
                granule: 0,
                packets_in_page: 0,
                pcm_buf: Vec::with_capacity(OPUS_FRAME_SIZE * audio_config.channels as usize * 2),
                packet_scratch: vec![0u8; OPUS_PACKET_MAX],
                channels: audio_config.channels as usize,
            });
            Ok((encoder, headers))
        }
        Codec::Vorbis => {
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

    /// Split an OGG byte stream into (header_type, granule, sequence) per page.
    fn ogg_pages(bytes: &[u8]) -> Vec<(u8, u64, u32)> {
        let mut pages = Vec::new();
        let mut i = 0;
        while i + 27 <= bytes.len() {
            assert_eq!(&bytes[i..i + 4], b"OggS", "page misaligned at {i}");
            let segs = bytes[i + 26] as usize;
            let body: usize = bytes[i + 27..i + 27 + segs].iter().map(|&b| b as usize).sum();
            let granule = u64::from_le_bytes(bytes[i + 6..i + 14].try_into().unwrap());
            let seq = u32::from_le_bytes(bytes[i + 18..i + 22].try_into().unwrap());
            pages.push((bytes[i + 5], granule, seq));
            i += 27 + segs + body;
        }
        assert_eq!(i, bytes.len(), "trailing partial page");
        pages
    }

    #[test]
    fn test_opus_stream_emits_audio_pages() {
        let cfg = AudioConfig {
            codec: Codec::Opus, sample_rate: 48000, channels: 2,
            vorbis_quality: 0.4, opus_bitrate_kbps: 128,
        };
        let (mut enc, headers) = build_encoder(&cfg, &TrackMetadata::default()).unwrap();
        let mut stream = headers.clone();

        // One second in the 512-frame chunks run_capture delivers.
        let chunk: Vec<f32> = (0..512 * 2).map(|i| ((i as f32) * 0.05).sin() * 0.3).collect();
        let mut out = Vec::new();
        let mut sent_before_finish = 0;
        for _ in 0..(48000 / 512) {
            out.clear();
            enc.encode(&chunk, 2, &mut out).unwrap();
            sent_before_finish += out.len();
            stream.extend_from_slice(&out);
        }
        out.clear();
        enc.finish(&mut out).unwrap();
        stream.extend_from_slice(&out);

        // Audio must leave while streaming, not only at finish().
        assert!(sent_before_finish > 10_000, "only {sent_before_finish} bytes during streaming");

        let pages = ogg_pages(&stream);
        assert_eq!(pages[0].0 & 0x02, 0x02, "first page is BOS");
        assert!(pages.len() >= 2 + 9, "expected ~10 audio pages, got {}", pages.len() - 2);
        for (k, p) in pages.iter().enumerate() {
            assert_eq!(p.2 as usize, k, "page sequence must be continuous");
            if k > 0 { assert_eq!(p.0 & 0x02, 0, "only the first page is BOS"); }
        }
        assert_eq!(pages.last().unwrap().0 & 0x04, 0x04, "last page is EOS");
        let granules: Vec<u64> = pages[2..].iter().map(|p| p.1).collect();
        assert!(granules.windows(2).all(|w| w[0] < w[1]), "granules increase");
    }

    #[test]
    fn test_opus_title_change_chains_stream() {
        let cfg = AudioConfig {
            codec: Codec::Opus, sample_rate: 48000, channels: 2,
            vorbis_quality: 0.4, opus_bitrate_kbps: 96,
        };
        let a = TrackMetadata { artist: "Angelo".into(), title: "First".into(), changed: false };
        let b = TrackMetadata { artist: String::new(), title: "It's Two".into(), changed: false };
        let chunk: Vec<f32> = (0..CHUNK_FRAMES * 2).map(|i| ((i as f32) * 0.03).sin() * 0.3).collect();

        let (mut enc, mut stream) = build_encoder(&cfg, &a).unwrap();
        for _ in 0..40 { enc.encode(&chunk, 2, &mut stream).unwrap(); }
        // What run_stream does on a title change.
        let (next, headers) = build_encoder(&cfg, &b).unwrap();
        std::mem::replace(&mut enc, next).finish(&mut stream).unwrap();
        stream.extend_from_slice(&headers);
        for _ in 0..40 { enc.encode(&chunk, 2, &mut stream).unwrap(); }
        enc.finish(&mut stream).unwrap();

        if let Ok(path) = std::env::var("RUMP_DUMP_OGG") { std::fs::write(path, &stream).unwrap(); }

        let pages = ogg_pages(&stream);
        let bos: Vec<usize> = pages.iter().enumerate().filter(|(_, p)| p.0 & 0x02 != 0).map(|(i, _)| i).collect();
        let eos: Vec<usize> = pages.iter().enumerate().filter(|(_, p)| p.0 & 0x04 != 0).map(|(i, _)| i).collect();
        assert_eq!(bos.len(), 2, "two logical streams");
        assert_eq!(eos, vec![bos[1] - 1, pages.len() - 1], "each ends with EOS, the first just before the second BOS");
        let tags = |needle: &[u8]| stream.windows(needle.len()).any(|w| w == needle);
        assert!(tags(b"TITLE=First") && tags(b"ARTIST=Angelo") && tags(b"TITLE=It's Two"));
    }

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
