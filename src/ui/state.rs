use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use gtk4::prelude::*;
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;

use crate::audio::{AudioConfig, AudioLevels, DuckConfig, SharedLevels};
use crate::config::Config;
use crate::log::{SharedLog, log_msg};
use crate::ptt;
use crate::stream::{IcecastConfig, SharedMetadata, TrackMetadata};

pub(crate) struct AppState {
    // Music capture
    pub capture_thread: Option<JoinHandle<anyhow::Result<()>>>,
    pub capture_stop: Option<Arc<AtomicBool>>,
    pub pw_pid: Arc<AtomicU32>,
    pub is_streaming_flag: Arc<AtomicBool>,
    pub pcm_rx: Option<crossbeam_channel::Receiver<Vec<f32>>>,
    pub current_device: String,

    // Mic capture
    pub mic_capture_thread: Option<JoinHandle<anyhow::Result<()>>>,
    pub mic_capture_stop: Option<Arc<AtomicBool>>,
    pub mic_pw_pid: Arc<AtomicU32>,
    pub mic_rx: Option<crossbeam_channel::Receiver<Vec<f32>>>,
    pub mic_levels: SharedLevels,
    pub current_mic_device: String,

    // PTT (two separate flags: toggle via button, hold via evdev)
    pub is_mic_toggled: Arc<AtomicBool>,
    pub is_mic_ptt: Arc<AtomicBool>,
    pub ptt_thread: Option<JoinHandle<()>>,
    pub ptt_stop: Option<Arc<AtomicBool>>,

    // Metadata
    pub metadata_thread: Option<JoinHandle<()>>,
    pub metadata_stop: Arc<AtomicBool>,

    // Stream
    pub is_streaming: bool,
    pub stream_thread: Option<JoinHandle<anyhow::Result<()>>>,
    pub stream_stop: Option<Arc<AtomicBool>>,

    pub timer_seconds: u32,
    pub levels: SharedLevels,
    pub metadata: SharedMetadata,
    pub error: Arc<Mutex<Option<String>>>,
    pub log: SharedLog,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            capture_thread: None,
            capture_stop: None,
            pw_pid: Arc::new(AtomicU32::new(0)),
            is_streaming_flag: Arc::new(AtomicBool::new(false)),
            pcm_rx: None,
            current_device: String::new(),

            mic_capture_thread: None,
            mic_capture_stop: None,
            mic_pw_pid: Arc::new(AtomicU32::new(0)),
            mic_rx: None,
            mic_levels: Arc::new(Mutex::new(AudioLevels::default())),
            current_mic_device: String::new(),

            is_mic_toggled: Arc::new(AtomicBool::new(false)),
            is_mic_ptt: Arc::new(AtomicBool::new(false)),
            ptt_thread: None,
            ptt_stop: None,

            metadata_thread: None,
            metadata_stop: Arc::new(AtomicBool::new(false)),

            is_streaming: false,
            stream_thread: None,
            stream_stop: None,

            timer_seconds: 0,
            levels: Arc::new(Mutex::new(AudioLevels::default())),
            metadata: Arc::new(Mutex::new(TrackMetadata::default())),
            error: Arc::new(Mutex::new(None)),
            log: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

pub(crate) fn rms_to_display(rms: f32) -> f64 {
    if rms <= 0.0 { return 0.0; }
    let db = 20.0 * rms.log10();
    ((db + 60.0) / 60.0).clamp(0.0, 1.0) as f64
}

pub(crate) fn reset_to_idle(
    btn: &gtk4::Button,
    timer: &gtk4::Label,
    title: &libadwaita::WindowTitle,
    subtitle: &str,
) {
    btn.set_icon_name("media-playback-start-symbolic");
    btn.set_css_classes(&["circular", "suggested-action"]);
    btn.set_tooltip_text(Some("Start Streaming"));
    timer.set_label("00:00:00");
    title.set_subtitle(subtitle);
}

pub(crate) fn kill_pw(pid: &AtomicU32) {
    let p = pid.swap(0, Ordering::Relaxed);
    if p != 0 {
        let _ = signal::kill(Pid::from_raw(p as i32), Signal::SIGTERM);
    }
}

pub(crate) fn is_mic_active(s: &AppState) -> bool {
    s.is_mic_toggled.load(Ordering::Relaxed) || s.is_mic_ptt.load(Ordering::Relaxed)
}

pub(crate) fn start_capture(s: &mut AppState, serial: u32, config: &Config) {
    stop_capture(s);
    let audio_config = AudioConfig {
        sample_rate: config.sample_rate,
        channels: config.channels,
        vorbis_quality: config.vorbis_quality,
    };
    let (pcm_tx, pcm_rx) = crossbeam_channel::bounded(64);
    s.pcm_rx = Some(pcm_rx);
    let capture_stop = Arc::new(AtomicBool::new(false));
    s.capture_stop = Some(capture_stop.clone());
    s.current_device = config.input_device.clone();
    let levels = s.levels.clone();
    let is_streaming = s.is_streaming_flag.clone();
    let pw_pid = s.pw_pid.clone();
    let log = s.log.clone();
    s.capture_thread = Some(
        thread::Builder::new()
            .name("capture".into())
            .spawn(move || {
                crate::audio::run_capture(
                    serial, audio_config, levels, pcm_tx,
                    is_streaming, capture_stop, pw_pid, log,
                )
            })
            .expect("Failed to spawn capture thread"),
    );
}

pub(crate) fn stop_capture(s: &mut AppState) {
    if let Some(flag) = s.capture_stop.take() { flag.store(true, Ordering::Relaxed); }
    kill_pw(&s.pw_pid);
    s.capture_thread.take();
    s.pcm_rx = None;
    s.current_device.clear();
}

pub(crate) fn start_mic_capture(s: &mut AppState, serial: u32, config: &Config) {
    stop_mic_capture(s);
    let (mic_tx, mic_rx) = crossbeam_channel::bounded(64);
    s.mic_rx = Some(mic_rx);
    let mic_stop = Arc::new(AtomicBool::new(false));
    s.mic_capture_stop = Some(mic_stop.clone());
    s.current_mic_device = config.mic_device.clone();
    let levels = s.mic_levels.clone();
    let is_streaming = s.is_streaming_flag.clone();
    let pw_pid = s.mic_pw_pid.clone();
    let log = s.log.clone();
    let sample_rate = config.sample_rate;
    s.mic_capture_thread = Some(
        thread::Builder::new()
            .name("mic-capture".into())
            .spawn(move || {
                crate::audio::run_mic_capture(
                    serial, sample_rate, levels, mic_tx,
                    is_streaming, mic_stop, pw_pid, log,
                )
            })
            .expect("Failed to spawn mic capture thread"),
    );
}

pub(crate) fn stop_mic_capture(s: &mut AppState) {
    if let Some(flag) = s.mic_capture_stop.take() { flag.store(true, Ordering::Relaxed); }
    kill_pw(&s.mic_pw_pid);
    s.mic_capture_thread.take();
    s.mic_rx = None;
    s.current_mic_device.clear();
}

pub(crate) fn start_ptt(s: &mut AppState, config: &Config) {
    stop_ptt(s);
    let ptt_stop = Arc::new(AtomicBool::new(false));
    s.ptt_stop = Some(ptt_stop.clone());
    s.ptt_thread = ptt::spawn_ptt_listener(
        &config.ptt_key,
        s.is_mic_ptt.clone(),
        ptt_stop,
        s.log.clone(),
    );
}

pub(crate) fn stop_ptt(s: &mut AppState) {
    if let Some(flag) = s.ptt_stop.take() { flag.store(true, Ordering::Relaxed); }
    s.ptt_thread.take();
    s.is_mic_ptt.store(false, Ordering::Relaxed);
}

/// Start streaming. Returns Err(message) if config is invalid.
pub(crate) fn start_streaming(s: &mut AppState) -> Result<(), String> {
    let config = Config::load();

    if config.host.is_empty() || config.mount.is_empty() || config.password.is_empty() {
        return Err("Configure server in preferences".into());
    }

    // Ensure capture is running
    if s.capture_thread.is_none() || config.input_device != s.current_device {
        let devices = crate::pipewire::list_devices().unwrap_or_default();
        match devices.iter().find(|d| d.display_name == config.input_device) {
            Some(dev) => start_capture(s, dev.serial, &config),
            None => return Err("Select input device in preferences".into()),
        }
    }

    let pcm_rx = match s.pcm_rx.as_ref() {
        Some(rx) => rx.clone(),
        None => return Err("Capture not ready".into()),
    };
    let mic_rx = s.mic_rx.clone();

    let audio_config = AudioConfig {
        sample_rate: config.sample_rate,
        channels: config.channels,
        vorbis_quality: config.vorbis_quality,
    };
    let duck_config = DuckConfig {
        threshold: config.duck_threshold,
        duck_level: config.duck_level,
        attack_ms: config.duck_attack_ms,
        release_ms: config.duck_release_ms,
        hold_ms: config.duck_hold_ms,
    };
    let icecast_config = IcecastConfig {
        host: config.host.clone(),
        port: config.port,
        mount: config.mount.clone(),
        password: config.password,
    };

    log_msg(&s.log, &format!("Connecting to {}:{}{}", config.host, config.port, config.mount));

    let stream_stop = Arc::new(AtomicBool::new(false));
    s.stream_stop = Some(stream_stop.clone());
    s.is_streaming_flag.store(true, Ordering::Relaxed);

    let is_mic_toggled = s.is_mic_toggled.clone();
    let is_mic_ptt = s.is_mic_ptt.clone();
    let metadata = s.metadata.clone();
    let error_slot = s.error.clone();
    let log = s.log.clone();

    s.stream_thread = Some(
        thread::Builder::new()
            .name("stream".into())
            .spawn(move || {
                let result = crate::audio::run_stream(
                    audio_config, icecast_config, pcm_rx, mic_rx,
                    is_mic_toggled, is_mic_ptt, duck_config, metadata,
                    stream_stop, log.clone(), error_slot.clone(),
                );
                if let Err(ref e) = result {
                    log_msg(&log, &format!("Error: {e:#}"));
                    if let Ok(mut err) = error_slot.lock() {
                        *err = Some(format!("{e:#}"));
                    }
                }
                result
            })
            .expect("Failed to spawn stream thread"),
    );

    s.is_streaming = true;
    s.timer_seconds = 0;
    Ok(())
}

pub(crate) fn stop_streaming(s: &mut AppState) {
    s.is_streaming_flag.store(false, Ordering::Relaxed);
    if let Some(flag) = s.stream_stop.take() {
        flag.store(true, Ordering::Relaxed);
    }
    s.stream_thread.take();
    log_msg(&s.log, "Stream stopped");
    s.is_streaming = false;
    s.timer_seconds = 0;
}

pub(crate) fn shutdown(s: &mut AppState) {
    s.is_streaming_flag.store(false, Ordering::Relaxed);
    if let Some(f) = s.stream_stop.take() { f.store(true, Ordering::Relaxed); }
    if let Some(f) = s.capture_stop.take() { f.store(true, Ordering::Relaxed); }
    kill_pw(&s.pw_pid);
    if let Some(f) = s.mic_capture_stop.take() { f.store(true, Ordering::Relaxed); }
    kill_pw(&s.mic_pw_pid);
    if let Some(f) = s.ptt_stop.take() { f.store(true, Ordering::Relaxed); }
    s.metadata_stop.store(true, Ordering::Relaxed);
}
