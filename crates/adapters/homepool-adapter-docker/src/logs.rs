//! Bounded suffix buffers. No job output enters diagnostics or Debug values.
use std::collections::VecDeque;

use crate::model::SandboxLogs;

struct Tail {
    bytes: VecDeque<u8>,
    lines: usize,
    tail: usize,
    max_bytes: usize,
    cut: bool,
}

impl Tail {
    fn new(tail: usize, max_bytes: usize) -> Self {
        Self {
            bytes: VecDeque::new(),
            lines: 0,
            tail,
            max_bytes,
            cut: false,
        }
    }

    fn pop(&mut self) -> Option<u8> {
        let byte = self.bytes.pop_front()?;
        if byte == b'\n' {
            self.lines -= 1;
        }
        if self.bytes.is_empty() {
            self.lines = 0;
        }
        self.cut = true;
        Some(byte)
    }

    fn push(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.bytes.back().is_none_or(|&last| last == b'\n') {
                self.lines += 1;
            }
            self.bytes.push_back(byte);
            while self.lines > self.tail {
                while self.pop().is_some_and(|b| b != b'\n') {}
            }
            while self.bytes.len() > self.max_bytes {
                self.pop();
            }
        }
    }

    fn text(&mut self) -> String {
        String::from_utf8_lossy(self.bytes.make_contiguous()).into_owned()
    }
}

pub(crate) struct Capture {
    streams: [Tail; 2],
}
impl Capture {
    pub(crate) fn new(tail: usize, max_bytes: usize) -> Self {
        Self {
            streams: [
                Tail::new(tail, max_bytes / 2),
                Tail::new(tail, max_bytes / 2),
            ],
        }
    }
    pub(crate) fn push(&mut self, stderr: bool, bytes: &[u8]) {
        self.streams[usize::from(stderr)].push(bytes);
    }
    pub(crate) fn finish(mut self) -> SandboxLogs {
        SandboxLogs {
            stdout: self.streams[0].text(),
            stderr: self.streams[1].text(),
            cut: self.streams.iter().any(|stream| stream.cut),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Capture;

    #[test]
    fn tail_tracks_lines_across_chunks_and_keeps_streams_separate() {
        let mut capture = Capture::new(2, 1024);
        capture.push(false, b"old\nfirst\nsec");
        capture.push(true, b"error\n");
        capture.push(false, b"ond");
        let logs = capture.finish();
        assert_eq!(logs.stdout, "first\nsecond");
        assert_eq!(logs.stderr, "error\n");
        assert!(logs.cut);
    }

    #[test]
    fn byte_limit_bounds_both_streams_and_replaces_invalid_utf8() {
        let mut capture = Capture::new(100, 8);
        capture.push(false, b"long\n1234");
        capture.push(true, b"long\n\xff\0\xfe\n");
        assert!(capture.streams.iter().all(|s| s.bytes.len() <= 4));
        let logs = capture.finish();
        assert_eq!(logs.stdout, "1234");
        assert_eq!(logs.stderr, "\u{fffd}\0\u{fffd}\n");
        assert!(logs.cut);
    }

    #[test]
    fn empty_exact_and_zero_limits() {
        let empty = Capture::new(0, 0).finish();
        assert!(!empty.cut);
        let mut exact = Capture::new(2, 8);
        exact.push(false, b"a\nb\n");
        exact.push(true, b"c\nd\n");
        assert!(!exact.finish().cut);
        for (tail, bytes) in [(0, 100), (10, 0)] {
            let mut capture = Capture::new(tail, bytes);
            capture.push(false, b"discard\n");
            capture.push(true, b"also discard");
            let logs = capture.finish();
            assert_eq!((logs.stdout, logs.stderr), (String::new(), String::new()));
            assert!(logs.cut);
        }
    }
}
