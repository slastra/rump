use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
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

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Connect with a timeout. A bare `TcpStream::connect` can block for minutes
/// on an unreachable host, and Icecast drops a source that goes quiet for its
/// `source-timeout` (10 s by default).
fn connect(host: &str, port: u16) -> Result<TcpStream> {
    let addrs = (host, port).to_socket_addrs()
        .with_context(|| format!("Failed to resolve {host}"))?;
    let mut last_err = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(s) => return Ok(s),
            Err(e) => last_err = Some(e),
        }
    }
    match last_err {
        Some(e) => Err(e).with_context(|| format!("Failed to connect to {host}:{port}")),
        None => bail!("{host} resolved to no addresses"),
    }
}

/// Read an HTTP status line and return its code.
fn read_status(reader: &mut impl BufRead) -> Result<(u16, String)> {
    let mut line = String::new();
    reader.read_line(&mut line).context("No response from Icecast")?;
    let line = line.trim().to_string();
    let code = line.split_whitespace().nth(1).and_then(|c| c.parse().ok())
        .with_context(|| format!("Malformed response: {line:?}"))?;
    Ok((code, line))
}

/// An active connection to an Icecast server via the HTTP SOURCE protocol.
pub struct IcecastConnection {
    stream: TcpStream,
}

impl IcecastConnection {
    pub fn connect(config: &IcecastConfig) -> Result<Self> {
        let stream = connect(&config.host, config.port)?;

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

        let mut conn = Self { stream };
        conn.stream.write_all(request.as_bytes())?;
        conn.stream.flush()?;

        // Read and validate HTTP response
        let mut reader = BufReader::new(&conn.stream);
        let (code, status_line) = read_status(&mut reader)?;
        if code != 200 {
            bail!("Icecast rejected connection: {status_line}");
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
}

/// Push a title to Icecast's admin interface (`updinfo`). Icecast only
/// carries this into Vorbis/MP3 listeners' metadata; Ogg Opus titles travel
/// in-band as OpusTags instead (see audio.rs). Blocking: call it off the
/// audio thread.
pub fn send_admin_metadata(config: &IcecastConfig, song: &str) -> Result<()> {
    let auth = BASE64.encode(format!("source:{}", config.password));
    let request = format!(
        "GET /admin/metadata?mount={}&mode=updinfo&song={} HTTP/1.0\r\n\
         Host: {}:{}\r\n\
         Authorization: Basic {}\r\n\
         User-Agent: RUMP/0.1\r\n\
         \r\n",
        url_encode(&config.mount),
        url_encode(song),
        config.host,
        config.port,
        auth
    );

    let mut stream = connect(&config.host, config.port)?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(request.as_bytes())?;
    stream.flush()?;
    let (code, line) = read_status(&mut BufReader::new(&stream))?;
    if code != 200 {
        bail!("Icecast refused metadata update: {line}");
    }
    Ok(())
}

fn url_encode(s: &str) -> String {
    let mut result = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' | '/' => result.push(c),
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
    fn test_url_encode_keeps_mount_slash() {
        assert_eq!(url_encode("/stream"), "/stream");
    }

    #[test]
    fn test_read_status() {
        let mut r = std::io::Cursor::new(b"HTTP/1.0 401 Authentication Required\r\n".to_vec());
        assert_eq!(read_status(&mut r).unwrap().0, 401);
        let mut r = std::io::Cursor::new(b"garbage\r\n".to_vec());
        assert!(read_status(&mut r).is_err());
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
