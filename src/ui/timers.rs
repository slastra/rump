use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;

use super::state::{AppState, is_mic_active, reset_to_idle, rms_to_display};

pub(crate) fn setup_timers(
    state: Rc<RefCell<AppState>>,
    left_meter: gtk4::LevelBar,
    right_meter: gtk4::LevelBar,
    mic_meter: gtk4::LevelBar,
    mic_indicator: gtk4::Button,
    title_label: gtk4::Label,
    artist_label: gtk4::Label,
    start_button: gtk4::Button,
    timer_label: gtk4::Label,
    log_buffer: gtk4::TextBuffer,
    log_view: gtk4::TextView,
    title_widget: adw::WindowTitle,
) {
    // 50ms timer — VU meters, log drain, metadata, error/thread monitoring
    {
        let state = state.clone();
        let timer_label = timer_label.clone();
        glib::timeout_add_local(Duration::from_millis(50), move || {
            let s = state.borrow();

            // Drain log
            if let Ok(mut msgs) = s.log.lock() {
                if !msgs.is_empty() {
                    let mut end = log_buffer.end_iter();
                    for msg in msgs.drain(..) {
                        if log_buffer.char_count() > 0 { log_buffer.insert(&mut end, "\n"); }
                        log_buffer.insert(&mut end, &msg);
                    }
                    let mark = log_buffer.create_mark(None, &end, false);
                    log_view.scroll_mark_onscreen(&mark);
                    log_buffer.delete_mark(&mark);
                }
            }

            // VU meters
            if let Ok(levels) = s.levels.lock() {
                left_meter.set_value(rms_to_display(levels.left));
                right_meter.set_value(rms_to_display(levels.right));
            }
            if let Ok(mic) = s.mic_levels.lock() {
                mic_meter.set_value(rms_to_display(mic.left));
            }

            // Mic indicator
            if is_mic_active(&s) {
                mic_indicator.set_css_classes(&["circular", "destructive-action"]);
                mic_indicator.set_tooltip_text(Some("Mic LIVE"));
            } else {
                mic_indicator.set_css_classes(&["circular", "flat"]);
                mic_indicator.set_tooltip_text(Some("Mic (click or hold PTT)"));
            }

            // Now playing
            if let Ok(meta) = s.metadata.lock() {
                if !meta.title.is_empty() { title_label.set_text(&meta.title); }
                if !meta.artist.is_empty() { artist_label.set_text(&meta.artist); }
            }

            if !s.is_streaming { return glib::ControlFlow::Continue; }

            // Error
            if let Some(msg) = s.error.lock().ok().and_then(|mut e| e.take()) {
                drop(s);
                let mut s = state.borrow_mut();
                s.is_streaming = false;
                s.is_streaming_flag.store(false, Ordering::Relaxed);
                s.stream_stop.take();
                s.timer_seconds = 0;
                reset_to_idle(&start_button, &timer_label, &title_widget, &format!("Error: {msg}"));
                return glib::ControlFlow::Continue;
            }

            // Stream thread died
            if s.stream_thread.as_ref().is_some_and(|h| h.is_finished()) {
                drop(s);
                let mut s = state.borrow_mut();
                let sub = match s.stream_thread.take().map(|h| h.join()) {
                    Some(Ok(Err(e))) => format!("Error: {e:#}"),
                    Some(Err(_)) => "Stream thread panicked".into(),
                    _ => "Icecast Streaming Client".into(),
                };
                s.is_streaming = false;
                s.is_streaming_flag.store(false, Ordering::Relaxed);
                s.stream_stop.take();
                s.timer_seconds = 0;
                reset_to_idle(&start_button, &timer_label, &title_widget, &sub);
            }

            glib::ControlFlow::Continue
        });
    }

    // 1s timer — stream clock
    {
        glib::timeout_add_local(Duration::from_secs(1), move || {
            let mut s = state.borrow_mut();
            if s.is_streaming {
                s.timer_seconds += 1;
                let t = s.timer_seconds;
                timer_label.set_label(&format!("{:02}:{:02}:{:02}", t / 3600, (t % 3600) / 60, t % 60));
            }
            glib::ControlFlow::Continue
        });
    }
}
