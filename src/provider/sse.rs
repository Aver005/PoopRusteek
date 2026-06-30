//! Minimal Server-Sent-Events line framing.
//!
//! Both the DeepSeek provider and the MCP HTTP transports receive SSE as a
//! byte stream and must split it into newline-delimited lines, buffering any
//! trailing partial line until more bytes arrive. This is that shared
//! primitive — decoding the JSON inside each `data:` line stays the caller's
//! job, since the event shapes are provider-specific.

/// Accumulates raw bytes and yields complete lines as they become available.
#[derive(Default)]
pub struct SseLineBuffer {
    buffer: String,
}

impl SseLineBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of bytes (decoded lossily as UTF-8) and return every
    /// complete line it produces. A trailing partial line — one not yet
    /// terminated by `\n` — is retained until the next call.
    pub fn push_bytes(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.push_str(&String::from_utf8_lossy(chunk));
        self.drain_lines()
    }

    fn drain_lines(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        while let Some(line_end) = self.buffer.find('\n') {
            let line = self.buffer[..line_end].to_string();
            self.buffer = self.buffer[line_end + 1..].to_string();
            lines.push(line);
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_complete_lines_in_one_chunk() {
        let mut buf = SseLineBuffer::new();
        let lines = buf.push_bytes(b"data: a\ndata: b\n");
        assert_eq!(lines, vec!["data: a".to_string(), "data: b".to_string()]);
    }

    #[test]
    fn retains_trailing_partial_line_across_chunks() {
        let mut buf = SseLineBuffer::new();
        assert_eq!(buf.push_bytes(b"data: hel"), Vec::<String>::new());
        assert_eq!(buf.push_bytes(b"lo\n"), vec!["data: hello".to_string()]);
    }

    #[test]
    fn line_split_across_three_chunks() {
        let mut buf = SseLineBuffer::new();
        assert!(buf.push_bytes(b"da").is_empty());
        assert!(buf.push_bytes(b"ta: x").is_empty());
        assert_eq!(buf.push_bytes(b"\nrest"), vec!["data: x".to_string()]);
        // "rest" stays buffered until terminated.
        assert_eq!(buf.push_bytes(b"\n"), vec!["rest".to_string()]);
    }

    #[test]
    fn empty_lines_are_preserved() {
        // SSE uses blank lines as event separators; callers may rely on them.
        let mut buf = SseLineBuffer::new();
        let lines = buf.push_bytes(b"data: a\n\ndata: b\n");
        assert_eq!(lines, vec!["data: a".to_string(), String::new(), "data: b".to_string()]);
    }

    #[test]
    fn no_newline_yields_nothing() {
        let mut buf = SseLineBuffer::new();
        assert!(buf.push_bytes(b"data: incomplete").is_empty());
    }
}
