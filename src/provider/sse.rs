//! Minimal Server-Sent-Events line framing.
//!
//! Both the DeepSeek provider and the MCP HTTP transports receive SSE as a
//! byte stream and must split it into newline-delimited lines, buffering any
//! trailing partial line until more bytes arrive. This is that shared
//! primitive — decoding the JSON inside each `data:` line stays the caller's
//! job, since the event shapes are provider-specific.

/// Accumulates raw bytes and yields complete lines as they become available.
///
/// Bytes are buffered raw and decoded per *line*, not per chunk: a multibyte
/// UTF-8 char split across two network chunks must not turn into `�`, which is
/// exactly what per-chunk `from_utf8_lossy` would do. `\n` is a single byte,
/// so splitting on it can never cut a character in half.
#[derive(Default)]
pub struct SseLineBuffer {
    buffer: Vec<u8>,
}

impl SseLineBuffer {
    /// A line with no `\n` must not grow memory without bound (runaway or
    /// malicious stream). Past this cap the partial line is force-flushed.
    const MAX_BUFFERED_LINE: usize = 4 * 1024 * 1024;

    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of bytes and return every complete line it produces.
    /// A trailing partial line — one not yet terminated by `\n` — is retained
    /// until the next call.
    pub fn push_bytes(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(chunk);

        let mut lines = Vec::new();
        let mut start = 0;
        while let Some(offset) = self.buffer[start..].iter().position(|&b| b == b'\n') {
            let line_end = start + offset;
            lines.push(String::from_utf8_lossy(&self.buffer[start..line_end]).into_owned());
            start = line_end + 1;
        }
        if start > 0 {
            self.buffer.drain(..start);
        }

        if self.buffer.len() > Self::MAX_BUFFERED_LINE {
            lines.push(String::from_utf8_lossy(&self.buffer).into_owned());
            self.buffer.clear();
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

    #[test]
    fn multibyte_char_split_across_chunks_decodes_cleanly() {
        // "привет" — each Cyrillic char is 2 bytes; split one mid-char.
        let bytes = "data: привет\n".as_bytes();
        let (a, b) = bytes.split_at(9); // cuts inside "р"
        let mut buf = SseLineBuffer::new();
        assert!(buf.push_bytes(a).is_empty());
        assert_eq!(buf.push_bytes(b), vec!["data: привет".to_string()]);
    }

    #[test]
    fn oversized_partial_line_is_force_flushed() {
        let mut buf = SseLineBuffer::new();
        let big = vec![b'x'; SseLineBuffer::MAX_BUFFERED_LINE + 1];
        let lines = buf.push_bytes(&big);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].len(), SseLineBuffer::MAX_BUFFERED_LINE + 1);
        // Buffer is empty again afterwards.
        assert!(buf.push_bytes(b"tail\n") == vec!["tail".to_string()]);
    }
}
