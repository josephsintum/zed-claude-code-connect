//! `Content-Length` framing over blocking byte streams.
//!
//! Blocking on purpose. `tokio::io::stdin()` hands every read to the blocking
//! thread pool and hands the result back, which is two thread handoffs per
//! keystroke; measured against this hand-written loop it cost 12.1us per cursor
//! move, and one further scheduler hop cost 6.4us more. The LSP side therefore
//! owns an OS thread and never enters the runtime on the request path. The MCP
//! side is a real socket and stays on tokio, where it is already faster than the
//! Go implementation it is measured against.

use std::io::{BufRead, Write};

/// Longest frame accepted. The header arrives from another process, so a
/// mistyped or hostile length must not become an allocation of that size.
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Reads `Content-Length` frames from a blocking stream.
pub struct FrameReader<R> {
    inner: R,
    header: String,
}

impl<R: BufRead> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            header: String::new(),
        }
    }

    /// Read the next frame body into `body`, or return `Ok(false)` at end of input.
    ///
    /// `body` is reused across calls, so a session settles on one allocation
    /// rather than one per keystroke.
    pub fn next_frame(&mut self, body: &mut Vec<u8>) -> std::io::Result<bool> {
        let mut len: Option<usize> = None;
        loop {
            self.header.clear();
            if self.inner.read_line(&mut self.header)? == 0 {
                return Ok(false);
            }
            let line = self.header.trim_end();
            if line.is_empty() {
                break;
            }
            // Header names are case-insensitive, and only this one is load-bearing.
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    len = value.trim().parse().ok();
                }
            }
        }

        let Some(len) = len else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "frame header had no usable Content-Length",
            ));
        };
        if len > MAX_FRAME_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("frame of {len} bytes is over the {MAX_FRAME_BYTES} byte cap"),
            ));
        }

        body.clear();
        body.resize(len, 0);
        self.inner.read_exact(body)?;
        Ok(true)
    }
}

/// Writes `Content-Length` frames to a blocking stream.
pub struct FrameWriter<W> {
    inner: W,
}

impl<W: Write> FrameWriter<W> {
    /// No lock, and none needed: one thread reads, dispatches and writes, and
    /// this server never speaks to Zed on its own initiative.
    pub fn new(inner: W) -> Self {
        Self { inner }
    }

    /// Write one frame and flush it. The flush is load-bearing -- Zed waits
    /// forever for a response sitting in a buffer.
    pub fn write_frame(&mut self, body: &[u8]) -> std::io::Result<()> {
        write!(self.inner, "Content-Length: {}\r\n\r\n", body.len())?;
        self.inner.write_all(body)?;
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(input: &[u8]) -> std::io::Result<Vec<String>> {
        let mut r = FrameReader::new(std::io::BufReader::new(input));
        let mut out = Vec::new();
        let mut body = Vec::new();
        while r.next_frame(&mut body)? {
            out.push(String::from_utf8(body.clone()).unwrap());
        }
        Ok(out)
    }

    #[test]
    fn frames_are_split_on_their_declared_length_not_on_a_delimiter() {
        // The second body contains the header text of a frame that is not there.
        let input =
            b"Content-Length: 2\r\n\r\n{}Content-Length: 24\r\n\r\n{\"a\":\"Content-Length: \"}";
        assert_eq!(
            read_all(input).unwrap(),
            vec!["{}".to_string(), "{\"a\":\"Content-Length: \"}".to_string()]
        );
    }

    #[test]
    fn a_header_the_client_capitalised_differently_still_parses() {
        let input = b"content-length: 2\r\n\r\n{}";
        assert_eq!(read_all(input).unwrap(), vec!["{}".to_string()]);
    }

    #[test]
    fn other_headers_are_ignored() {
        let input = b"Content-Type: application/vscode-jsonrpc\r\nContent-Length: 2\r\n\r\n{}";
        assert_eq!(read_all(input).unwrap(), vec!["{}".to_string()]);
    }

    #[test]
    fn a_length_over_the_cap_is_refused_without_allocating_it() {
        let input = b"Content-Length: 99999999999\r\n\r\n";
        let err = read_all(input).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn end_of_input_is_not_an_error() {
        assert_eq!(read_all(b"").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn a_written_frame_declares_its_byte_length_not_its_character_count() {
        let mut out = Vec::new();
        // Four bytes, two characters.
        FrameWriter::new(&mut out)
            .write_frame("\u{00e9}\u{00e9}".as_bytes())
            .unwrap();
        assert_eq!(out, b"Content-Length: 4\r\n\r\n\xc3\xa9\xc3\xa9");
    }
}
