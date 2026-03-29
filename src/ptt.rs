use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use futures_util::StreamExt;
use gtk4::glib;

use crate::log::{SharedLog, log_msg};

pub const PTT_MODIFIER_OPTIONS: &[&str] = &["None", "Alt", "Ctrl", "Shift", "Super"];

pub const PTT_KEY_OPTIONS: &[&str] = &[
    "Space", "F1", "F2", "F3", "F4", "F5", "F6",
    "F7", "F8", "F9", "F10", "F11", "F12",
    "CapsLock", "ScrollLock", "Pause", "Insert",
    "Home", "End", "PageUp", "PageDown",
];

fn combo_to_xdg_trigger(combo: &str) -> String {
    let parts: Vec<&str> = combo.split('+').collect();
    let (modifier, key) = if parts.len() == 2 {
        (Some(parts[0]), parts[1])
    } else {
        (None, parts[0])
    };
    let key_lower = key.to_lowercase();
    match modifier {
        Some(m) => format!("<{m}>{key_lower}"),
        None => key_lower,
    }
}

/// Start PTT. Tries XDG Portal first, falls back to evdev.
pub fn start_ptt(combo_str: &str, mic_ptt: Arc<AtomicBool>, log: SharedLog) {
    let trigger = combo_to_xdg_trigger(combo_str);
    let combo = combo_str.to_string();
    let mic_ptt_clone = mic_ptt.clone();
    let log_clone = log.clone();

    log_msg(&log, &format!("PTT: trying XDG Portal for {combo}..."));

    // Try portal with a timeout. If it hangs (common on Hyprland), fall back to evdev.
    let portal_ready = Arc::new(AtomicBool::new(false));
    let portal_ready_clone = portal_ready.clone();

    glib::spawn_future_local(async move {
        match try_portal(trigger, mic_ptt.clone(), log.clone(), portal_ready_clone).await {
            Ok(()) => {}
            Err(e) => {
                log_msg(&log, &format!("PTT: portal failed: {e}"));
                log_msg(&log, "PTT: falling back to evdev");
                start_evdev(&combo, mic_ptt, log.clone());
            }
        }
    });

    // Timeout: if portal hasn't signaled ready in 3 seconds, start evdev
    let log_timeout = log_clone.clone();
    let combo_timeout = combo_str.to_string();
    glib::timeout_add_local_once(Duration::from_secs(3), move || {
        if !portal_ready.load(Ordering::Relaxed) {
            log_msg(&log_timeout, "PTT: portal timed out, falling back to evdev");
            start_evdev(&combo_timeout, mic_ptt_clone, log_timeout.clone());
        }
    });
}

// ── XDG Portal ──────────────────────────────────────────────────

async fn try_portal(
    trigger: String,
    mic_ptt: Arc<AtomicBool>,
    log: SharedLog,
    ready: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    use ashpd::desktop::global_shortcuts::{BindShortcutsOptions, GlobalShortcuts, NewShortcut};
    use ashpd::desktop::CreateSessionOptions;

    let shortcuts = GlobalShortcuts::new().await?;
    let session = shortcuts
        .create_session(CreateSessionOptions::default())
        .await?;

    let shortcut = NewShortcut::new("ptt", "Push to Talk")
        .preferred_trigger(trigger.as_str());

    let request = shortcuts
        .bind_shortcuts(&session, &[shortcut], None, BindShortcutsOptions::default())
        .await?;

    let response = request.response()?;
    let bound = response.shortcuts();

    if bound.is_empty() {
        return Err("compositor did not bind any shortcuts".into());
    }

    let desc = bound[0].trigger_description();
    log_msg(&log, &format!("PTT: portal ready ({desc})"));
    ready.store(true, Ordering::Relaxed);

    let mic_press = mic_ptt.clone();
    let mut activated = shortcuts.receive_activated().await?;
    glib::spawn_future_local(async move {
        while let Some(_) = activated.next().await {
            mic_press.store(true, Ordering::Relaxed);
        }
    });

    let mut deactivated = shortcuts.receive_deactivated().await?;
    glib::spawn_future_local(async move {
        while let Some(_) = deactivated.next().await {
            mic_ptt.store(false, Ordering::Relaxed);
        }
    });

    Ok(())
}

// ── evdev Fallback ──────────────────────────────────────────────

fn parse_evdev_key(name: &str) -> Option<evdev::KeyCode> {
    match name.to_uppercase().as_str() {
        "SPACE" => Some(evdev::KeyCode::KEY_SPACE),
        "F1" => Some(evdev::KeyCode::KEY_F1),
        "F2" => Some(evdev::KeyCode::KEY_F2),
        "F3" => Some(evdev::KeyCode::KEY_F3),
        "F4" => Some(evdev::KeyCode::KEY_F4),
        "F5" => Some(evdev::KeyCode::KEY_F5),
        "F6" => Some(evdev::KeyCode::KEY_F6),
        "F7" => Some(evdev::KeyCode::KEY_F7),
        "F8" => Some(evdev::KeyCode::KEY_F8),
        "F9" => Some(evdev::KeyCode::KEY_F9),
        "F10" => Some(evdev::KeyCode::KEY_F10),
        "F11" => Some(evdev::KeyCode::KEY_F11),
        "F12" => Some(evdev::KeyCode::KEY_F12),
        "CAPSLOCK" => Some(evdev::KeyCode::KEY_CAPSLOCK),
        "SCROLLLOCK" => Some(evdev::KeyCode::KEY_SCROLLLOCK),
        "PAUSE" => Some(evdev::KeyCode::KEY_PAUSE),
        "INSERT" => Some(evdev::KeyCode::KEY_INSERT),
        "HOME" => Some(evdev::KeyCode::KEY_HOME),
        "END" => Some(evdev::KeyCode::KEY_END),
        "PAGEUP" => Some(evdev::KeyCode::KEY_PAGEUP),
        "PAGEDOWN" => Some(evdev::KeyCode::KEY_PAGEDOWN),
        _ => None,
    }
}

fn parse_evdev_modifier(name: &str) -> Option<(evdev::KeyCode, evdev::KeyCode)> {
    match name.to_uppercase().as_str() {
        "ALT" => Some((evdev::KeyCode::KEY_LEFTALT, evdev::KeyCode::KEY_RIGHTALT)),
        "CTRL" | "CONTROL" => Some((evdev::KeyCode::KEY_LEFTCTRL, evdev::KeyCode::KEY_RIGHTCTRL)),
        "SHIFT" => Some((evdev::KeyCode::KEY_LEFTSHIFT, evdev::KeyCode::KEY_RIGHTSHIFT)),
        "SUPER" | "META" => Some((evdev::KeyCode::KEY_LEFTMETA, evdev::KeyCode::KEY_RIGHTMETA)),
        _ => None,
    }
}

fn start_evdev(combo_str: &str, mic_ptt: Arc<AtomicBool>, log: SharedLog) {
    let parts: Vec<&str> = combo_str.split('+').collect();
    let (modifier, key_name) = if parts.len() == 2 {
        (parse_evdev_modifier(parts[0]), parts[1])
    } else {
        (None, parts[0])
    };

    let target_key = match parse_evdev_key(key_name) {
        Some(k) => k,
        None => {
            log_msg(&log, &format!("PTT: unknown key: {key_name}"));
            return;
        }
    };

    let target_code = target_key.0;
    let modifier_codes = modifier.map(|(l, r)| (l.0, r.0));
    let combo = combo_str.to_string();

    thread::Builder::new()
        .name("ptt-evdev".into())
        .spawn(move || {
            let mut keyboards: Vec<evdev::Device> = Vec::new();

            for (_path, device) in evdev::enumerate() {
                if device.supported_keys().is_some_and(|keys| keys.contains(target_key)) {
                    if let Some(name) = device.name() {
                        log_msg(&log, &format!("PTT: evdev monitoring {name} for {combo}"));
                    }
                    keyboards.push(device);
                }
            }

            if keyboards.is_empty() {
                log_msg(&log, "PTT: no keyboard devices found (need input group?)");
                return;
            }

            for kb in &mut keyboards {
                let _ = kb.set_nonblocking(true);
            }

            let mut modifier_held = false;
            let mut key_held = false;

            loop {
                let mut any_event = false;

                for kb in &mut keyboards {
                    if let Ok(events) = kb.fetch_events() {
                        for event in events {
                            if event.event_type() != evdev::EventType::KEY {
                                continue;
                            }
                            let code = event.code();
                            let pressed = event.value() == 1;
                            let released = event.value() == 0;

                            if let Some((l, r)) = modifier_codes {
                                if code == l || code == r {
                                    if pressed { modifier_held = true; }
                                    else if released { modifier_held = false; }
                                    any_event = true;
                                }
                            }

                            if code == target_code {
                                if pressed { key_held = true; }
                                else if released { key_held = false; }
                                any_event = true;
                            }

                            let active = key_held && (modifier_codes.is_none() || modifier_held);
                            mic_ptt.store(active, Ordering::Relaxed);
                        }
                    }
                }

                if !any_event {
                    thread::sleep(Duration::from_millis(5));
                }
            }
        })
        .expect("Failed to spawn evdev PTT thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_combo_to_xdg_trigger() {
        assert_eq!(combo_to_xdg_trigger("Alt+Space"), "<Alt>space");
        assert_eq!(combo_to_xdg_trigger("Ctrl+F1"), "<Ctrl>f1");
        assert_eq!(combo_to_xdg_trigger("F5"), "f5");
    }

    #[test]
    fn test_parse_evdev_key() {
        assert!(parse_evdev_key("Space").is_some());
        assert!(parse_evdev_key("F12").is_some());
        assert!(parse_evdev_key("nope").is_none());
    }

    #[test]
    fn test_parse_evdev_modifier() {
        assert!(parse_evdev_modifier("Alt").is_some());
        assert!(parse_evdev_modifier("Ctrl").is_some());
        assert!(parse_evdev_modifier("nope").is_none());
    }
}
