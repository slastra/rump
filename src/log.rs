use std::sync::{Arc, Mutex};

use gtk4::glib;

pub type SharedLog = Arc<Mutex<Vec<String>>>;

pub fn log_msg(log: &SharedLog, msg: &str) {
    let now = glib::DateTime::now_local().unwrap();
    let ts = now.format("%H:%M:%S").unwrap();
    if let Ok(mut buf) = log.lock() {
        buf.push(format!("[{ts}] {msg}"));
    }
}
