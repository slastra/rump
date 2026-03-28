use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

#[derive(Clone)]
pub struct IcecastConfig {
    pub host: String,
    pub port: u16,
    pub mount: String,
    pub password: String,
}

#[derive(Clone, Default)]
pub struct TrackMetadata {
    pub artist: String,
    pub title: String,
    pub changed: bool,
}

impl TrackMetadata {
    /// Format as "Artist - Title", or just the title if artist is empty.
    pub fn display_string(&self) -> String {
        if self.artist.is_empty() {
            self.title.clone()
        } else {
            format!("{} - {}", self.artist, self.title)
        }
    }
}

pub type SharedMetadata = Arc<Mutex<TrackMetadata>>;

/// An active connection to an Icecast server via the HTTP SOURCE protocol.
pub struct IcecastConnection {
    stream: TcpStream,
    config: IcecastConfig,
}

impl IcecastConnection {
    pub fn connect(config: IcecastConfig) -> Result<Self> {
        let addr = format!("{}:{}", config.host, config.port);
        let stream = TcpStream::connect(&addr)
            .with_context(|| format!("Failed to connect to {addr}"))?;

        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;

        let auth = BASE64.encode(format!("source:{}", config.password));
        let request = format!(
            "SOURCE {} HTTP/1.0\r\n\
             Host: {}:{}\r\n\
             Authorization: Basic {}\r\n\
             User-Agent: RUMP/0.1\r\n\
             Content-Type: application/ogg\r\n\
             ice-name: RUMP Stream\r\n\
             ice-public: 0\r\n\
             \r\n",
            config.mount, config.host, config.port, auth
        );

        let mut conn = Self { stream, config };
        conn.stream.write_all(request.as_bytes())?;
        conn.stream.flush()?;

        // Read and validate HTTP response
        let mut reader = BufReader::new(&conn.stream);
        let mut status_line = String::new();
        reader.read_line(&mut status_line)?;

        if !status_line.contains("200") {
            bail!("Icecast rejected connection: {}", status_line.trim());
        }

        // Consume remaining headers
        let mut header = String::new();
        while reader.read_line(&mut header).unwrap_or(0) > 0 {
            if header.trim().is_empty() {
                break;
            }
            header.clear();
        }

        Ok(conn)
    }

    pub fn send(&mut self, data: &[u8]) -> Result<()> {
        self.stream
            .write_all(data)
            .context("Failed to send audio data to Icecast")
    }

    pub fn update_metadata(&self, meta: &TrackMetadata) -> Result<()> {
        let song = meta.display_string();
        if song.is_empty() {
            return Ok(());
        }

        let auth = BASE64.encode(format!("source:{}", self.config.password));
        let request = format!(
            "GET /admin/metadata?mount={}&mode=updinfo&song={} HTTP/1.0\r\n\
             Host: {}:{}\r\n\
             Authorization: Basic {}\r\n\
             User-Agent: RUMP/0.1\r\n\
             \r\n",
            self.config.mount,
            url_encode(&song),
            self.config.host,
            self.config.port,
            auth
        );

        let addr = format!("{}:{}", self.config.host, self.config.port);
        let mut meta_stream = TcpStream::connect(&addr)?;
        meta_stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        meta_stream.write_all(request.as_bytes())?;
        meta_stream.flush()?;

        Ok(())
    }
}

fn url_encode(s: &str) -> String {
    let mut result = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => result.push(c),
            ' ' => result.push('+'),
            _ => {
                for byte in c.to_string().as_bytes() {
                    result.push_str(&format!("%{byte:02X}"));
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_url_encode_plain() {
        assert_eq!(url_encode("hello"), "hello");
    }

    #[test]
    fn test_url_encode_spaces() {
        assert_eq!(url_encode("hello world"), "hello+world");
    }

    #[test]
    fn test_url_encode_special() {
        assert_eq!(url_encode("a&b=c"), "a%26b%3Dc");
    }

    #[test]
    fn test_display_string_both() {
        let m = TrackMetadata { artist: "Daft Punk".into(), title: "Voyager".into(), changed: false };
        assert_eq!(m.display_string(), "Daft Punk - Voyager");
    }

    #[test]
    fn test_display_string_no_artist() {
        let m = TrackMetadata { artist: String::new(), title: "Voyager".into(), changed: false };
        assert_eq!(m.display_string(), "Voyager");
    }

    #[test]
    fn test_display_string_empty() {
        let m = TrackMetadata::default();
        assert_eq!(m.display_string(), "");
    }
}
