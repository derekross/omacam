//! Log lines to the systemd journal, formatted with their fields.
//!
//! tracing-journald puts an event's fields into journal fields of their
//! own, so `journalctl -t omacam` showed only the bare message ("photo
//! saved", without the path or the size). Here each event is formatted as
//! on a terminal (message and `key=value` fields, no colors, no time: the
//! journal has its own) and sent as the journal entry's MESSAGE, with
//! SYSLOG_IDENTIFIER=omacam and the level as PRIORITY, through the
//! journal's native socket.

use std::io::Write;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracing::{Level, Metadata};
use tracing_subscriber::fmt::MakeWriter;

/// The journal's native protocol socket.
pub const SOCKET: &str = "/run/systemd/journal/socket";

/// syslog priority of a tracing level.
pub fn priority(level: &Level) -> u8 {
    match *level {
        Level::ERROR => 3,
        Level::WARN => 4,
        Level::INFO => 6,
        _ => 7,
    }
}

/// One journal entry in the native protocol: `KEY=value` lines; a value
/// with a newline in it goes as KEY, newline, its length (64-bit
/// little-endian), the bytes, newline.
pub fn entry(identifier: &str, priority: u8, message: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut field = |key: &str, value: &str| {
        if value.contains('\n') {
            out.extend_from_slice(key.as_bytes());
            out.push(b'\n');
            out.extend_from_slice(&(value.len() as u64).to_le_bytes());
            out.extend_from_slice(value.as_bytes());
            out.push(b'\n');
        } else {
            out.extend_from_slice(format!("{key}={value}\n").as_bytes());
        }
    };
    field("SYSLOG_IDENTIFIER", identifier);
    field("PRIORITY", &priority.to_string());
    field("MESSAGE", message.trim_end_matches('\n'));
    out
}

/// Hands formatted events to the journal.
#[derive(Clone)]
pub struct Journal {
    socket: Arc<UnixDatagram>,
    path: PathBuf,
    identifier: &'static str,
}

impl Journal {
    /// Connects to the journal; None when there is none.
    pub fn open(identifier: &'static str) -> Option<Journal> {
        Journal::open_at(Path::new(SOCKET), identifier)
    }

    pub fn open_at(path: &Path, identifier: &'static str) -> Option<Journal> {
        if !path.exists() {
            return None;
        }
        let socket = UnixDatagram::unbound().ok()?;
        Some(Journal {
            socket: Arc::new(socket),
            path: path.to_path_buf(),
            identifier,
        })
    }
}

/// One event's text, sent when the formatter is done with it.
pub struct Entry {
    journal: Journal,
    priority: u8,
    text: Vec<u8>,
}

impl Write for Entry {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.text.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        if self.text.is_empty() {
            return;
        }
        let message = String::from_utf8_lossy(&self.text);
        let data = entry(self.journal.identifier, self.priority, &message);
        // A lost log line is not worth failing over.
        let _ = self.journal.socket.send_to(&data, &self.journal.path);
    }
}

impl<'a> MakeWriter<'a> for Journal {
    type Writer = Entry;

    fn make_writer(&'a self) -> Entry {
        Entry {
            journal: self.clone(),
            priority: 6,
            text: Vec::new(),
        }
    }

    fn make_writer_for(&'a self, meta: &Metadata<'_>) -> Entry {
        Entry {
            journal: self.clone(),
            priority: priority(meta.level()),
            text: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_follow_the_native_protocol() {
        let e = entry("omacam", 6, "photo saved path=/tmp/a.jpg\n");
        assert_eq!(
            String::from_utf8(e).unwrap(),
            "SYSLOG_IDENTIFIER=omacam\nPRIORITY=6\nMESSAGE=photo saved path=/tmp/a.jpg\n"
        );
        let e = entry("omacam", 4, "two\nlines");
        let mut want = b"SYSLOG_IDENTIFIER=omacam\nPRIORITY=4\nMESSAGE\n".to_vec();
        want.extend_from_slice(&9u64.to_le_bytes());
        want.extend_from_slice(b"two\nlines\n");
        assert_eq!(e, want);
        assert_eq!(priority(&Level::ERROR), 3);
        assert_eq!(priority(&Level::DEBUG), 7);
    }

    /// An event through the fmt layer keeps its fields in MESSAGE.
    #[test]
    fn fields_reach_the_message() {
        let dir = std::env::temp_dir().join(format!("omacam-journal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("socket");
        let _ = std::fs::remove_file(&path);
        let server = UnixDatagram::bind(&path).unwrap();
        let journal = Journal::open_at(&path, "omacam").unwrap();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(journal)
            .with_ansi(false)
            .without_time()
            .with_target(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(path = "/tmp/a.jpg", bytes = 1234, "photo saved");
        });
        let mut buf = [0u8; 1024];
        let n = server.recv(&mut buf).unwrap();
        let got = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(got.contains("PRIORITY=6\n"), "{got}");
        assert!(
            got.contains("MESSAGE=") && got.contains("photo saved path=\"/tmp/a.jpg\" bytes=1234"),
            "{got}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
