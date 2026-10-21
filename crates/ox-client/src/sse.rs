//! Minimal SSE parser. ox's stream is `data: {json}\n\n` plus `: heartbeat`
//! comments; we deliberately avoid an eventsource dependency (~40 lines).

#[derive(Default)]
pub struct FrameParser {
    buf: Vec<u8>,
}

impl FrameParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed raw bytes; returns every complete `data:` payload decoded in order.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        // Normalize CRLF so frame detection only has to look for `\n\n`.
        for byte in chunk {
            match *byte {
                b'\r' => {}
                b => self.buf.push(b),
            }
        }

        let mut out = Vec::new();
        while let Some(pos) = find(&self.buf, b"\n\n") {
            let frame = String::from_utf8_lossy(&self.buf[..pos]).to_string();
            self.buf.drain(..pos + 2);

            let mut data_lines = Vec::new();
            for line in frame.lines() {
                if let Some(rest) = line.strip_prefix("data:") {
                    data_lines.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
                }
                // `:` comments (heartbeats), `event:`, `id:`, `retry:` are ignored:
                // the envelope JSON already carries id/type.
            }
            if !data_lines.is_empty() {
                out.push(data_lines.join("\n"));
            }
        }
        out
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::FrameParser;

    #[test]
    fn parses_one_frame_across_split_chunks() {
        let mut p = FrameParser::new();
        assert!(p.feed(b"data: {\"a\":").is_empty());
        let frames = p.feed(b"1}\n\n");
        assert_eq!(frames, vec![r#"{"a":1}"#.to_string()]);
    }

    #[test]
    fn ignores_heartbeat_comments_and_joins_multiline_data() {
        let mut p = FrameParser::new();
        let frames = p.feed(b": heartbeat\n\ndata: line1\ndata: line2\n\n");
        assert_eq!(frames, vec!["line1\nline2".to_string()]);
    }

    #[test]
    fn handles_crlf_and_multiple_frames_in_one_chunk() {
        let mut p = FrameParser::new();
        let frames = p.feed(b"data: a\r\n\r\ndata: b\r\n\r\n");
        assert_eq!(frames, vec!["a".to_string(), "b".to_string()]);
    }
}
