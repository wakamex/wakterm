use super::*;
use std::io::{self, Write};
use std::sync::mpsc::{channel, Sender};
use std::time::Duration;

#[derive(Debug)]
struct ChecksumConfig {
    enabled: bool,
}

impl TerminalConfiguration for ChecksumConfig {
    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }

    fn enable_checksum_rectangular_area(&self) -> bool {
        self.enabled
    }
}

struct RecordingWriter {
    bytes: Arc<Mutex<Vec<u8>>>,
    done: Sender<()>,
}

impl Write for RecordingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.bytes.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for RecordingWriter {
    fn drop(&mut self) {
        self.done.send(()).ok();
    }
}

fn checksum_response(enabled: bool) -> Vec<u8> {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let (done_tx, done_rx) = channel();
    let writer = RecordingWriter {
        bytes: Arc::clone(&bytes),
        done: done_tx,
    };
    let mut term = Terminal::new(
        TerminalSize {
            rows: 1,
            cols: 1,
            pixel_width: 8,
            pixel_height: 16,
            dpi: 0,
        },
        Arc::new(ChecksumConfig { enabled }),
        "wakterm",
        "test",
        Box::new(writer),
    );

    term.advance_bytes(b"\x1b[7;1;1;1;1;1*y");
    drop(term);

    done_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("terminal writer did not shut down");
    let response = bytes.lock().unwrap().clone();
    response
}

#[test]
fn checksum_rectangular_area_is_disabled_by_default() {
    std::assert_eq!(checksum_response(false), b"");
}

#[test]
fn checksum_rectangular_area_can_be_enabled() {
    std::assert_eq!(checksum_response(true), b"\x1bP7!~0020\x1b\\");
}
