use std::io::BufRead;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crate::stream::SharedMetadata;
use crate::log::{SharedLog, log_msg};

pub fn spawn_metadata_listener(
    metadata: SharedMetadata,
    stop: Arc<AtomicBool>,
    log: SharedLog,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("metadata-listener".into())
        .spawn(move || {
            let mut child = match Command::new("playerctl")
                .args(["metadata", "--follow", "--format", "{{artist}}\t{{title}}"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => child,
                Err(e) => {
                    log_msg(&log, &format!("playerctl not available: {e}"));
                    return;
                }
            };

            let Some(stdout) = child.stdout.take() else {
                log_msg(&log, "Failed to capture playerctl stdout");
                return;
            };

            let reader = std::io::BufReader::new(stdout);
            for line in reader.lines() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }

                let Ok(line) = line else { break };

                let parts: Vec<&str> = line.splitn(2, '\t').collect();
                let (artist, title) = match parts.len() {
                    2 => (parts[0].to_string(), parts[1].to_string()),
                    1 => (String::new(), parts[0].to_string()),
                    _ => continue,
                };

                if let Ok(mut meta) = metadata.lock() {
                    meta.artist = artist;
                    meta.title = title;
                    meta.changed = true;
                }
            }

            let _ = child.kill();
            let _ = child.wait();
        })
        .expect("Failed to spawn metadata listener thread")
}
