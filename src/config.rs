use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub mount: String,
    pub password: String,
    pub input_device: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub vorbis_quality: f32,
    // Microphone
    pub mic_device: String,
    pub ptt_key: String,
    // Ducking
    pub duck_threshold: f32,
    pub duck_level: f32,
    pub duck_attack_ms: u32,
    pub duck_release_ms: u32,
    pub duck_hold_ms: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: "localhost".into(),
            port: 8001,
            mount: "/angelo".into(),
            password: String::new(),
            input_device: String::new(),
            sample_rate: 44100,
            channels: 2,
            vorbis_quality: 0.4,
            mic_device: String::new(),
            ptt_key: "Alt+Space".into(),
            duck_threshold: 0.02,
            duck_level: 0.2,
            duck_attack_ms: 100,
            duck_release_ms: 800,
            duck_hold_ms: 500,
        }
    }
}

fn config_path() -> Result<PathBuf> {
    let config_dir = dirs::config_dir().context("Could not determine config directory")?;
    Ok(config_dir.join("rump").join("config.toml"))
}

impl Config {
    pub fn load() -> Self {
        match Self::try_load() {
            Ok(config) => config,
            Err(e) => {
                eprintln!("Failed to load config: {e}, using defaults");
                Self::default()
            }
        }
    }

    fn try_load() -> Result<Self> {
        let path = config_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let config: Config =
            toml::from_str(&contents).with_context(|| "Failed to parse config.toml")?;
        Ok(config)
    }

    pub fn save(&self) {
        if let Err(e) = self.try_save() {
            eprintln!("Failed to save config: {e}");
        }
    }

    fn try_save(&self) -> Result<()> {
        let path = config_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        let contents = toml::to_string_pretty(self).context("Failed to serialize config")?;
        fs::write(&path, contents)
            .with_context(|| format!("Failed to write {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_defaults() {
        let c = Config::default();
        assert_eq!(c.sample_rate, 44100);
        assert_eq!(c.channels, 2);
        assert!((c.vorbis_quality - 0.4).abs() < 0.01);
        assert_eq!(c.ptt_key, "Alt+Space");
        assert!((c.duck_level - 0.2).abs() < 0.01);
    }

    #[test]
    fn test_round_trip() {
        let original = Config::default();
        let toml_str = toml::to_string_pretty(&original).unwrap();
        let parsed: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.host, original.host);
        assert_eq!(parsed.port, original.port);
        assert_eq!(parsed.sample_rate, original.sample_rate);
        assert_eq!(parsed.channels, original.channels);
        assert_eq!(parsed.ptt_key, original.ptt_key);
        assert_eq!(parsed.duck_attack_ms, original.duck_attack_ms);
    }

    #[test]
    fn test_missing_fields_use_defaults() {
        let toml_str = r#"
            host = "example.com"
            port = 9000
        "#;
        let parsed: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(parsed.host, "example.com");
        assert_eq!(parsed.port, 9000);
        // Missing fields should get defaults
        assert_eq!(parsed.sample_rate, 44100);
        assert_eq!(parsed.channels, 2);
        assert_eq!(parsed.ptt_key, "Alt+Space");
    }
}
