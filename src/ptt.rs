use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use evdev::{Device, EventType, KeyCode};

use crate::log::{SharedLog, log_msg};

fn parse_single_key(name: &str) -> Option<KeyCode> {
    match name.to_uppercase().as_str() {
        "SPACE" => Some(KeyCode::KEY_SPACE),
        "F1" => Some(KeyCode::KEY_F1),
        "F2" => Some(KeyCode::KEY_F2),
        "F3" => Some(KeyCode::KEY_F3),
        "F4" => Some(KeyCode::KEY_F4),
        "F5" => Some(KeyCode::KEY_F5),
        "F6" => Some(KeyCode::KEY_F6),
        "F7" => Some(KeyCode::KEY_F7),
        "F8" => Some(KeyCode::KEY_F8),
        "F9" => Some(KeyCode::KEY_F9),
        "F10" => Some(KeyCode::KEY_F10),
        "F11" => Some(KeyCode::KEY_F11),
        "F12" => Some(KeyCode::KEY_F12),
        "CAPSLOCK" => Some(KeyCode::KEY_CAPSLOCK),
        "SCROLLLOCK" => Some(KeyCode::KEY_SCROLLLOCK),
        "PAUSE" => Some(KeyCode::KEY_PAUSE),
        "INSERT" => Some(KeyCode::KEY_INSERT),
        "HOME" => Some(KeyCode::KEY_HOME),
        "END" => Some(KeyCode::KEY_END),
        "PAGEUP" => Some(KeyCode::KEY_PAGEUP),
        "PAGEDOWN" => Some(KeyCode::KEY_PAGEDOWN),
        _ => None,
    }
}

/// Parse a modifier name to its left+right evdev KeyCode pair.
fn parse_modifier(name: &str) -> Option<(KeyCode, KeyCode)> {
    match name.to_uppercase().as_str() {
        "ALT" => Some((KeyCode::KEY_LEFTALT, KeyCode::KEY_RIGHTALT)),
        "CTRL" | "CONTROL" => Some((KeyCode::KEY_LEFTCTRL, KeyCode::KEY_RIGHTCTRL)),
        "SHIFT" => Some((KeyCode::KEY_LEFTSHIFT, KeyCode::KEY_RIGHTSHIFT)),
        "SUPER" | "META" => Some((KeyCode::KEY_LEFTMETA, KeyCode::KEY_RIGHTMETA)),
        _ => None,
    }
}

/// A parsed PTT key combo (optional modifier + key).
struct PttCombo {
    modifier: Option<(KeyCode, KeyCode)>, // (left, right) variants
    key: KeyCode,
}

/// Parse a combo string like "Alt+Space", "Ctrl+F1", or just "F5".
fn parse_combo(combo: &str) -> Option<PttCombo> {
    let parts: Vec<&str> = combo.split('+').collect();
    match parts.len() {
        1 => {
            let key = parse_single_key(parts[0])?;
            Some(PttCombo { modifier: None, key })
        }
        2 => {
            let modifier = parse_modifier(parts[0]);
            let key = parse_single_key(parts[1])?;
            Some(PttCombo { modifier, key })
        }
        _ => None,
    }
}

pub const PTT_MODIFIER_OPTIONS: &[&str] = &["None", "Alt", "Ctrl", "Shift", "Super"];

pub const PTT_KEY_OPTIONS: &[&str] = &[
    "Space", "F1", "F2", "F3", "F4", "F5", "F6",
    "F7", "F8", "F9", "F10", "F11", "F12",
    "CapsLock", "ScrollLock", "Pause", "Insert",
    "Home", "End", "PageUp", "PageDown",
];

/// Spawn a thread that listens for a key combo on all keyboard devices.
/// `combo_str` can be "Space", "Alt+Space", "Ctrl+F1", etc.
pub fn spawn_ptt_listener(
    combo_str: &str,
    mic_ptt: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    log: SharedLog,
) -> Option<JoinHandle<()>> {
    let combo = match parse_combo(combo_str) {
        Some(c) => c,
        None => {
            log_msg(&log, &format!("Unknown PTT combo: {combo_str}"));
            return None;
        }
    };

    let target_code = combo.key.0;
    let modifier_codes = combo.modifier.map(|(l, r)| (l.0, r.0));
    let combo_display = combo_str.to_string();

    let handle = thread::Builder::new()
        .name("ptt-listener".into())
        .spawn(move || {
            let mut keyboards: Vec<Device> = Vec::new();

            for (_path, device) in evdev::enumerate() {
                if device
                    .supported_keys()
                    .is_some_and(|keys| keys.contains(combo.key))
                {
                    if let Some(name) = device.name() {
                        log_msg(&log, &format!("PTT: monitoring {name} for {combo_display}"));
                    }
                    keyboards.push(device);
                }
            }

            if keyboards.is_empty() {
                log_msg(&log, "PTT: no keyboard devices found");
                return;
            }

            for kb in &mut keyboards {
                let _ = kb.set_nonblocking(true);
            }

            let mut modifier_held = false;
            let mut key_held = false;

            while !stop.load(Ordering::Relaxed) {
                let mut any_event = false;

                for kb in &mut keyboards {
                    if let Ok(events) = kb.fetch_events() {
                        for event in events {
                            if event.event_type() != EventType::KEY {
                                continue;
                            }

                            let code = event.code();
                            let pressed = event.value() == 1;
                            let released = event.value() == 0;

                            // Track modifier state
                            if let Some((l, r)) = modifier_codes {
                                if code == l || code == r {
                                    if pressed {
                                        modifier_held = true;
                                    } else if released {
                                        modifier_held = false;
                                    }
                                    any_event = true;
                                }
                            }

                            // Track key state
                            if code == target_code {
                                if pressed {
                                    key_held = true;
                                } else if released {
                                    key_held = false;
                                }
                                any_event = true;
                            }

                            // Update mic state: modifier must be held (if configured) + key held
                            let active = key_held
                                && (modifier_codes.is_none() || modifier_held);
                            mic_ptt.store(active, Ordering::Relaxed);
                        }
                    }
                }

                if !any_event {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        })
        .expect("Failed to spawn PTT listener thread");

    Some(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_single_key() {
        assert_eq!(parse_single_key("Space"), Some(KeyCode::KEY_SPACE));
        assert_eq!(parse_single_key("F1"), Some(KeyCode::KEY_F1));
        assert_eq!(parse_single_key("f12"), Some(KeyCode::KEY_F12));
        assert_eq!(parse_single_key("CapsLock"), Some(KeyCode::KEY_CAPSLOCK));
        assert_eq!(parse_single_key("nope"), None);
    }

    #[test]
    fn test_parse_modifier() {
        let (l, r) = parse_modifier("Alt").unwrap();
        assert_eq!(l, KeyCode::KEY_LEFTALT);
        assert_eq!(r, KeyCode::KEY_RIGHTALT);
        assert!(parse_modifier("ctrl").is_some());
        assert!(parse_modifier("shift").is_some());
        assert!(parse_modifier("super").is_some());
        assert!(parse_modifier("nope").is_none());
    }

    #[test]
    fn test_parse_combo_single() {
        let c = parse_combo("F5").unwrap();
        assert_eq!(c.key, KeyCode::KEY_F5);
        assert!(c.modifier.is_none());
    }

    #[test]
    fn test_parse_combo_with_modifier() {
        let c = parse_combo("Alt+Space").unwrap();
        assert_eq!(c.key, KeyCode::KEY_SPACE);
        let (l, _) = c.modifier.unwrap();
        assert_eq!(l, KeyCode::KEY_LEFTALT);
    }

    #[test]
    fn test_parse_combo_invalid() {
        assert!(parse_combo("").is_none());
        assert!(parse_combo("Alt+Nope").is_none());
        assert!(parse_combo("A+B+C").is_none());
    }
}
