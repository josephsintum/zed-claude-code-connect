//! In-memory mirror of the buffers Zed has open.
//!
//! Selection text used to be read back from disk, so anything the user had typed
//! but not saved was reported to the CLI as whatever the file said before the edit.
//! Zed already sends the buffer contents over LSP document sync; this keeps them.

use std::collections::HashMap;
use std::sync::RwLock;

use tower_lsp::lsp_types::{Position, Range, TextDocumentContentChangeEvent, Url};
use tracing::{debug, warn};

/// The extension registers "Plain Text", so this server attaches to every text
/// buffer -- including a multi-hundred-megabyte log someone drags in. Past this
/// size the document is not mirrored and callers fall back to reading from disk.
const MAX_CACHED_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug)]
struct Document {
    text: String,
    /// Byte offset of the first byte of each line; `line_starts[0]` is always 0.
    line_starts: Vec<usize>,
}

impl Document {
    fn new(text: String) -> Self {
        let line_starts = compute_line_starts(&text);
        Self { text, line_starts }
    }

    fn set_text(&mut self, text: String) {
        self.text = text;
        self.reindex();
    }

    fn reindex(&mut self) {
        self.line_starts = compute_line_starts(&self.text);
    }

    /// Byte offset of an LSP position. LSP counts characters in UTF-16 code units.
    ///
    /// Clamps rather than failing: clients legitimately send a `character` past the
    /// end of a line, and a line number past the end of the document.
    fn offset_of(&self, p: Position) -> usize {
        let line_idx = p.line as usize;
        let Some(&start) = self.line_starts.get(line_idx) else {
            return self.text.len();
        };

        let mut end = self
            .line_starts
            .get(line_idx + 1)
            .copied()
            .unwrap_or(self.text.len());

        // Exclude the terminator so an over-long `character` clamps to end-of-line
        // instead of running into the next line.
        let bytes = self.text.as_bytes();
        if end > start && bytes[end - 1] == b'\n' {
            end -= 1;
        }
        if end > start && bytes[end - 1] == b'\r' {
            end -= 1;
        }

        let line = &self.text[start..end];
        start + char_pos_to_byte_pos(line, p.character as usize).unwrap_or(line.len())
    }

    fn text_in_range(&self, range: Range) -> String {
        let start = self.offset_of(range.start);
        let end = self.offset_of(range.end).max(start);
        self.text.get(start..end).unwrap_or_default().to_string()
    }
}

/// Byte offset within `line` of the given UTF-16 code-unit position. LSP counts
/// characters in UTF-16, so a character outside the BMP is two units.
///
/// A position inside a surrogate pair lands on the start of that character; a
/// position past the end of the line yields None and callers clamp.
fn char_pos_to_byte_pos(line: &str, utf16_pos: usize) -> Option<usize> {
    let mut current = 0;
    for (byte_pos, ch) in line.char_indices() {
        if current == utf16_pos {
            return Some(byte_pos);
        }
        let width = ch.len_utf16();
        if utf16_pos < current + width {
            return Some(byte_pos);
        }
        current += width;
    }
    if current == utf16_pos {
        return Some(line.len());
    }
    None
}

fn compute_line_starts(text: &str) -> Vec<usize> {
    let mut v = Vec::with_capacity(text.len() / 32 + 1);
    v.push(0);
    v.extend(text.match_indices('\n').map(|(i, _)| i + 1));
    v
}

#[derive(Debug, Default)]
pub struct DocumentStore {
    docs: RwLock<HashMap<Url, Document>>,
}

impl DocumentStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(&self, uri: Url, text: String) {
        if text.len() > MAX_CACHED_BYTES {
            warn!(
                "Not mirroring {} ({} bytes, over the {} byte cap); selections there read from disk",
                uri,
                text.len(),
                MAX_CACHED_BYTES
            );
            self.docs.write().unwrap().remove(&uri);
            return;
        }
        self.docs.write().unwrap().insert(uri, Document::new(text));
    }

    pub fn close(&self, uri: &Url) {
        self.docs.write().unwrap().remove(uri);
    }

    /// Apply one `didChange` batch.
    ///
    /// Changes within a batch apply in order, and each range is expressed against
    /// the text produced by the previous one -- so the line index is rebuilt after
    /// every change, not once at the end.
    pub fn apply(&self, uri: &Url, changes: Vec<TextDocumentContentChangeEvent>) {
        let mut guard = self.docs.write().unwrap();
        let Some(doc) = guard.get_mut(uri) else {
            // Never opened, or too large to mirror.
            debug!("didChange for an unmirrored document: {}", uri);
            return;
        };

        for change in changes {
            match change.range {
                None => doc.set_text(change.text),
                Some(range) => {
                    let start = doc.offset_of(range.start);
                    let end = doc.offset_of(range.end).max(start);
                    doc.text.replace_range(start..end, &change.text);
                    doc.reindex();
                }
            }
        }

        if doc.text.len() > MAX_CACHED_BYTES {
            warn!("{} grew past the mirror cap; dropping it", uri);
            guard.remove(uri);
        }
    }

    /// Text for a range, or None when the document is not mirrored.
    pub fn text_in_range(&self, uri: &Url, range: Range) -> Option<String> {
        self.docs
            .read()
            .unwrap()
            .get(uri)
            .map(|d| d.text_in_range(range))
    }

    /// Text for a range read from the file on disk, for documents that are not
    /// mirrored: never opened through document sync, or over the size cap.
    ///
    /// Same `Document` code as the mirror, so positions clamp the same way and
    /// line endings come back as the file has them. The previous implementation
    /// returned "" for a single-line range past end-of-line and joined lines
    /// with "\n" regardless of the file. Note this reads the whole file on
    /// every cursor move in such a document; acceptable for the rare case.
    pub fn text_from_disk(path: &std::path::Path, range: Range) -> String {
        match std::fs::read_to_string(path) {
            Ok(text) => Document::new(text).text_in_range(range),
            Err(e) => {
                warn!("Failed to read {}: {}", path.display(), e);
                String::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri() -> Url {
        Url::parse("file:///tmp/a.rs").unwrap()
    }

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    fn edit(range: Option<Range>, text: &str) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range,
            range_length: None,
            text: text.to_string(),
        }
    }

    #[test]
    fn reads_a_range_spanning_lines() {
        let s = DocumentStore::new();
        s.open(uri(), "alpha\nbravo\ncharlie\n".to_string());
        let got = s
            .text_in_range(
                &uri(),
                Range {
                    start: pos(0, 2),
                    end: pos(2, 3),
                },
            )
            .unwrap();
        assert_eq!(got, "pha\nbravo\ncha");
    }

    #[test]
    fn an_unsaved_edit_is_visible_immediately() {
        let s = DocumentStore::new();
        s.open(uri(), "let x = 1;\n".to_string());
        s.apply(
            &uri(),
            vec![edit(
                Some(Range {
                    start: pos(0, 8),
                    end: pos(0, 9),
                }),
                "42",
            )],
        );
        let got = s
            .text_in_range(
                &uri(),
                Range {
                    start: pos(0, 0),
                    end: pos(0, 11),
                },
            )
            .unwrap();
        assert_eq!(got, "let x = 42;", "the mirror, not the file on disk");
    }

    #[test]
    fn changes_in_one_batch_apply_in_sequence() {
        // The second range is expressed against the text the first one produced.
        let s = DocumentStore::new();
        s.open(uri(), "aaa\nbbb\n".to_string());
        s.apply(
            &uri(),
            vec![
                edit(
                    Some(Range {
                        start: pos(0, 0),
                        end: pos(0, 3),
                    }),
                    "XY",
                ),
                edit(
                    Some(Range {
                        start: pos(1, 0),
                        end: pos(1, 3),
                    }),
                    "Z",
                ),
            ],
        );
        let all = s
            .text_in_range(
                &uri(),
                Range {
                    start: pos(0, 0),
                    end: pos(1, 10),
                },
            )
            .unwrap();
        assert_eq!(all, "XY\nZ");
    }

    #[test]
    fn a_full_replacement_resets_the_document() {
        let s = DocumentStore::new();
        s.open(uri(), "old\n".to_string());
        s.apply(&uri(), vec![edit(None, "brand new\ncontent\n")]);
        let got = s
            .text_in_range(
                &uri(),
                Range {
                    start: pos(1, 0),
                    end: pos(1, 7),
                },
            )
            .unwrap();
        assert_eq!(got, "content");
    }

    #[test]
    fn utf16_positions_land_on_character_boundaries() {
        // 🎉 is one char, 4 UTF-8 bytes, but 2 UTF-16 code units -- so the 'b'
        // that follows it sits at character 3, not character 2.
        let s = DocumentStore::new();
        s.open(uri(), "a🎉b\n".to_string());
        let got = s
            .text_in_range(
                &uri(),
                Range {
                    start: pos(0, 3),
                    end: pos(0, 4),
                },
            )
            .unwrap();
        assert_eq!(got, "b");
    }

    #[test]
    fn multibyte_text_survives_an_edit() {
        let s = DocumentStore::new();
        s.open(uri(), "café ☕\n".to_string());
        s.apply(
            &uri(),
            vec![edit(
                Some(Range {
                    start: pos(0, 5),
                    end: pos(0, 6),
                }),
                "tea",
            )],
        );
        let got = s
            .text_in_range(
                &uri(),
                Range {
                    start: pos(0, 0),
                    end: pos(0, 8),
                },
            )
            .unwrap();
        assert_eq!(got, "café tea");
    }

    #[test]
    fn crlf_line_endings_do_not_bleed_into_the_next_line() {
        let s = DocumentStore::new();
        s.open(uri(), "one\r\ntwo\r\n".to_string());
        // character 99 is past the end of line 0 and must clamp to end-of-line.
        let got = s
            .text_in_range(
                &uri(),
                Range {
                    start: pos(0, 0),
                    end: pos(0, 99),
                },
            )
            .unwrap();
        assert_eq!(got, "one");
    }

    #[test]
    fn a_position_past_the_end_of_the_document_clamps() {
        let s = DocumentStore::new();
        s.open(uri(), "short\n".to_string());
        let got = s
            .text_in_range(
                &uri(),
                Range {
                    start: pos(0, 0),
                    end: pos(99, 99),
                },
            )
            .unwrap();
        assert_eq!(got, "short\n");
    }

    #[test]
    fn an_inverted_range_yields_empty_rather_than_panicking() {
        let s = DocumentStore::new();
        s.open(uri(), "abcdef\n".to_string());
        let got = s
            .text_in_range(
                &uri(),
                Range {
                    start: pos(0, 4),
                    end: pos(0, 1),
                },
            )
            .unwrap();
        assert_eq!(got, "");
    }

    #[test]
    fn an_oversized_document_is_not_mirrored() {
        let s = DocumentStore::new();
        s.open(uri(), "x".repeat(MAX_CACHED_BYTES + 1));
        assert!(
            s.text_in_range(
                &uri(),
                Range {
                    start: pos(0, 0),
                    end: pos(0, 1)
                }
            )
            .is_none(),
            "callers must fall back to disk"
        );
    }

    #[test]
    fn a_change_to_an_unopened_document_is_ignored() {
        let s = DocumentStore::new();
        s.apply(&uri(), vec![edit(None, "hello")]);
        assert!(s
            .text_in_range(
                &uri(),
                Range {
                    start: pos(0, 0),
                    end: pos(0, 5)
                }
            )
            .is_none());
    }

    #[test]
    fn closing_drops_the_mirror() {
        let s = DocumentStore::new();
        s.open(uri(), "gone soon\n".to_string());
        s.close(&uri());
        assert!(s
            .text_in_range(
                &uri(),
                Range {
                    start: pos(0, 0),
                    end: pos(0, 4)
                }
            )
            .is_none());
    }
}
