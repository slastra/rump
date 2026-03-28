use std::process::Command;

use anyhow::{Context, Result};
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct PwDevice {
    pub serial: u32,
    pub display_name: String,
    pub is_monitor: bool,
}

/// Enumerate available PipeWire audio devices (sources and sink monitors).
pub fn list_devices() -> Result<Vec<PwDevice>> {
    let output = Command::new("pw-dump")
        .output()
        .context("Failed to run pw-dump. Is PipeWire installed?")?;

    if !output.status.success() {
        anyhow::bail!("pw-dump failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    let json: Value =
        serde_json::from_slice(&output.stdout).context("Failed to parse pw-dump output")?;

    let nodes = json.as_array().context("Expected JSON array from pw-dump")?;
    let mut devices = Vec::new();

    for node in nodes {
        if node.get("type").and_then(|t| t.as_str()) != Some("PipeWire:Interface:Node") {
            continue;
        }

        let Some(props) = node.pointer("/info/props") else {
            continue;
        };
        let Some(media_class) = props.get("media.class").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(serial) = props
            .get("object.serial")
            .and_then(|v| v.as_u64())
            .map(|s| s as u32)
        else {
            continue;
        };

        let description = props
            .get("node.description")
            .or_else(|| props.get("node.name"))
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown Device");

        let device = match media_class {
            "Audio/Source" => PwDevice {
                serial,
                display_name: format!("{description} (mic)"),
                is_monitor: false,
            },
            "Audio/Sink" => PwDevice {
                serial,
                display_name: format!("Monitor of {description}"),
                is_monitor: true,
            },
            _ => continue,
        };
        devices.push(device);
    }

    // Monitors first (most common for streaming), then sources
    devices.sort_by(|a, b| {
        b.is_monitor
            .cmp(&a.is_monitor)
            .then(a.display_name.cmp(&b.display_name))
    });

    Ok(devices)
}

/// List only Audio/Source devices (microphones).
pub fn list_sources() -> Result<Vec<PwDevice>> {
    Ok(list_devices()?
        .into_iter()
        .filter(|d| !d.is_monitor)
        .collect())
}
