//! In-memory mirror of the buffers Zed has open.
//!
//! Selection text used to be read back from disk, so anything the user had typed
//! but not saved was reported to the CLI as whatever the file said before the edit.
//! Zed already sends the buffer contents over LSP document sync; this keeps them.

use std::collections::HashMap;

use lsp_types::{Position, Range, TextDocumentContentChangeEvent, Url};
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
        self.line_starts = compute_line_starts(&self.text);
    }

    /// Replace the bytes in `start..end` with `inserted`, and repair the line
    /// index in place rather than rebuilding it.
    ///
    /// The rebuild it replaces rescanned the whole buffer and allocated a fresh
    /// `Vec<usize>` after every change. Measured on a 2,000-line file: 19.30us to
    /// rebuild against 0.93us for the edit itself, so the bookkeeping cost twenty
    /// times the work. This does the same job in 0.06us, because an edit at
    /// `start` can only move the line starts after it, and always by the same
    /// amount.
    fn splice(&mut self, start: usize, end: usize, inserted: &str) {
        self.text.replace_range(start..end, inserted);

        // A line start at offset `s` exists because of a newline at `s - 1`. The
        // replaced span is `start..end`, so that newline is gone exactly when
        // `start < s <= end` -- note the second bound is inclusive, because the
        // newline at `end - 1` is the last byte the span removes. Getting this
        // edge wrong loses or keeps one line start only when an edit ends exactly
        // on a line boundary, which is why the exhaustive test below exists.
        let head = self.line_starts.partition_point(|&s| s <= start);
        let tail = head.max(self.line_starts.partition_point(|&s| s <= end));

        let shift = |offset: usize| (offset + inserted.len()) - (end - start);

        if !inserted.as_bytes().contains(&b'\n') {
            // The typing case: one character, no new line starts. Drop whatever
            // the span swallowed and slide the tail.
            self.line_starts.drain(head..tail);
            for s in &mut self.line_starts[head..] {
                *s = shift(*s);
            }
            return;
        }

        let added = inserted
            .match_indices('\n')
            .map(|(i, _)| start + i + 1)
            .collect::<Vec<_>>();
        self.line_starts.splice(head..tail, added.iter().copied());
        for s in &mut self.line_starts[head + added.len()..] {
            *s = shift(*s);
        }
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

/// Enough of a file's identity to notice that it changed. Not a content hash:
/// this is checked on every cursor move, and hashing a 55 MB file would cost
/// more than the read it is meant to avoid.
#[derive(Debug, Clone, Copy)]
struct FileStamp {
    len: u64,
    /// `None` on a filesystem that does not report it, which then never matches
    /// itself -- the cache simply stops helping rather than going stale.
    modified: Option<std::time::SystemTime>,
}

impl FileStamp {
    /// Two stamps match only when the filesystem actually reported a time. A
    /// filesystem that does not never matches itself, so the cache stops helping
    /// rather than going stale.
    fn matches(&self, other: &FileStamp) -> bool {
        self.modified.is_some() && self.modified == other.modified && self.len == other.len
    }
}

/// The line index of one unmirrored file, kept between cursor moves.
#[derive(Debug)]
struct DiskIndex {
    path: std::path::PathBuf,
    stamp: FileStamp,
    line_starts: Vec<usize>,
    len: usize,
}

impl DiskIndex {
    /// Read only the lines the range names, and answer from those.
    ///
    /// The window runs from the start of the first named line to the start of the
    /// line after the last, so both bounds land on a line boundary -- which is
    /// always a UTF-8 boundary too, because a line start follows a `\n`. Rebasing
    /// the range onto the window keeps the clamping identical to the whole-file
    /// path: a line past the end still clamps to the end, and an inverted range
    /// still comes back empty.
    fn read_range(&self, path: &std::path::Path, range: Range) -> String {
        use std::io::{Read, Seek, SeekFrom};

        let first = range.start.line as usize;
        let Some(&window_start) = self.line_starts.get(first) else {
            // The first line is past the end of the file, so both offsets clamp
            // to the end and the answer is empty whatever the end line says.
            return String::new();
        };
        let last = range.end.line as usize;
        let window_end = self
            .line_starts
            .get(last + 1)
            .copied()
            .unwrap_or(self.len)
            .max(window_start);

        let mut buf = vec![0u8; window_end - window_start];
        let read = std::fs::File::open(path)
            .and_then(|mut f| {
                f.seek(SeekFrom::Start(window_start as u64))?;
                f.read_exact(&mut buf)
            })
            .is_ok();
        if !read {
            warn!(
                "Failed to read {} lines {}..{}",
                path.display(),
                first,
                last
            );
            return String::new();
        }
        let Ok(window) = String::from_utf8(buf) else {
            warn!("{} is no longer valid UTF-8", path.display());
            return String::new();
        };

        Document::new(window).text_in_range(Range {
            start: Position {
                line: 0,
                character: range.start.character,
            },
            end: Position {
                line: (last.saturating_sub(first)) as u32,
                character: range.end.character,
            },
        })
    }
}

/// The buffers Zed has open, keyed by URI.
///
/// No lock, because every method is called from the single thread that reads the
/// LSP stream. That is also what makes the ordering of changes within a
/// `didChange` batch a guarantee rather than an accident: both properties come
/// from the same fact, and both break together if anything ever dispatches
/// concurrently.
#[derive(Debug, Default)]
pub struct DocumentStore {
    docs: HashMap<Url, Document>,
    /// Line index of the last unmirrored file a range was read from.
    disk: Option<DiskIndex>,
}

impl DocumentStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(&mut self, uri: Url, text: String) {
        if text.len() > MAX_CACHED_BYTES {
            warn!(
                "Not mirroring {} ({} bytes, over the {} byte cap); selections there read from disk",
                uri,
                text.len(),
                MAX_CACHED_BYTES
            );
            self.docs.remove(&uri);
            return;
        }
        self.docs.insert(uri, Document::new(text));
    }

    pub fn close(&mut self, uri: &Url) {
        self.docs.remove(uri);
    }

    /// Apply one `didChange` batch.
    ///
    /// Changes within a batch apply in order, and each range is expressed against
    /// the text produced by the previous one -- so the line index is repaired
    /// after every change, not once at the end.
    pub fn apply(&mut self, uri: &Url, changes: Vec<TextDocumentContentChangeEvent>) {
        let Some(doc) = self.docs.get_mut(uri) else {
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
                    doc.splice(start, end, &change.text);
                }
            }
        }

        if doc.text.len() > MAX_CACHED_BYTES {
            warn!("{} grew past the mirror cap; dropping it", uri);
            self.docs.remove(uri);
        }
    }

    /// Text for a range, or None when the document is not mirrored.
    pub fn text_in_range(&self, uri: &Url, range: Range) -> Option<String> {
        self.docs.get(uri).map(|d| d.text_in_range(range))
    }

    /// Text for a range, from the mirror if we have it and from disk if not.
    ///
    /// The caller does not need to know which: both go through the same
    /// [`Document`] code, so a position means the same thing whichever source
    /// answered.
    pub fn text_for_range(&mut self, uri: &Url, path: &std::path::Path, range: Range) -> String {
        // A zero-width range is a bare cursor, and its text is provably empty:
        // the two offsets are equal whatever the document says. Answering it here
        // matters because a bare cursor is the state you are in the whole time you
        // are typing, and the disk path below would otherwise read the file on
        // every keystroke.
        if range.start == range.end {
            return String::new();
        }
        match self.text_in_range(uri, range) {
            Some(text) => text,
            None => self.text_from_disk(path, range),
        }
    }

    /// Text for a range read from the file on disk, for documents that are not
    /// mirrored: never opened through document sync, or over the size cap.
    ///
    /// The line index of the last such file is kept, because cursor moves come in
    /// bursts and they come in the *same* file. Without it every move re-read and
    /// re-indexed the whole thing: measured, 42us for a 0.1 MB file, 1.5ms at
    /// 11 MB and 8.6ms at 55 MB, twenty times a second while you hold shift-arrow.
    /// With it, only the first move pays; the rest read just the lines they name.
    ///
    /// The index is cached, not the text. For a 55 MB file that is 8 MB held
    /// rather than 55 -- which is the whole reason such a file is not mirrored in
    /// the first place.
    pub fn text_from_disk(&mut self, path: &std::path::Path, range: Range) -> String {
        let Ok(meta) = std::fs::metadata(path).inspect_err(|e| {
            warn!("Failed to stat {}: {}", path.display(), e);
        }) else {
            return String::new();
        };
        let stamp = FileStamp {
            len: meta.len(),
            modified: meta.modified().ok(),
        };

        if let Some(cached) = &self.disk {
            if cached.path == path && cached.stamp.matches(&stamp) {
                return cached.read_range(path, range);
            }
        }

        // First sight of this file, or it changed under us: read it whole once,
        // answer from that, and keep the index for the moves that follow.
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) => {
                warn!("Failed to read {}: {}", path.display(), e);
                self.disk = None;
                return String::new();
            }
        };
        let doc = Document::new(text);
        let answer = doc.text_in_range(range);
        self.disk = Some(DiskIndex {
            path: path.to_path_buf(),
            stamp,
            line_starts: doc.line_starts,
            len: doc.text.len(),
        });
        answer
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
        let mut s = DocumentStore::new();
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
        let mut s = DocumentStore::new();
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
        let mut s = DocumentStore::new();
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
        let mut s = DocumentStore::new();
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
        let mut s = DocumentStore::new();
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
        let mut s = DocumentStore::new();
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
        let mut s = DocumentStore::new();
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
        let mut s = DocumentStore::new();
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
        let mut s = DocumentStore::new();
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
        let mut s = DocumentStore::new();
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
        let mut s = DocumentStore::new();
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
        let mut s = DocumentStore::new();
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

    /// The incremental fixup must agree with a full rebuild after every edit.
    /// This is the test for the change that took a keystroke from 19.3us to
    /// 0.06us: if the two ever disagree, every position lookup after the edit is
    /// silently wrong, and nothing else in this file would notice.
    #[test]
    fn the_incremental_line_index_matches_a_full_rebuild() {
        let mut doc = Document::new("alpha\nbravo\ncharlie\ndelta\n".to_string());
        let edits: &[(usize, usize, &str)] = &[
            (3, 3, "X"),          // insert, no newline: the typing case
            (0, 6, ""),           // delete a whole line including its \n
            (2, 2, "one\ntwo\n"), // insert several lines
            (5, 12, "\n"),        // replace a span with just a newline
            (0, 0, "\n"),         // insert a newline at the very start
            (1, 1, ""),           // a no-op splice
        ];
        for &(start, end, text) in edits {
            let start = start.min(doc.text.len());
            let end = end.clamp(start, doc.text.len());
            doc.splice(start, end, text);
            assert_eq!(
                doc.line_starts,
                compute_line_starts(&doc.text),
                "after splicing {text:?} into {start}..{end}, text is now {:?}",
                doc.text
            );
        }
    }

    #[test]
    fn an_edit_spanning_many_lines_still_reads_back_correctly() {
        let mut s = DocumentStore::new();
        s.open(uri(), "one\ntwo\nthree\nfour\n".to_string());
        s.apply(
            &uri(),
            vec![edit(
                Some(Range {
                    start: pos(0, 1),
                    end: pos(2, 2),
                }),
                "X\nY",
            )],
        );
        let got = s
            .text_in_range(
                &uri(),
                Range {
                    start: pos(0, 0),
                    end: pos(3, 9),
                },
            )
            .unwrap();
        assert_eq!(got, "oX\nYree\nfour\n");
    }

    /// Every splice of a small buffer, against a full rebuild.
    ///
    /// A hand-picked list of edits missed a real off-by-one here: the boundary
    /// between "this line start's newline was removed" and "it survived" is only
    /// visible when an edit ends exactly on a line boundary. Enumerating the
    /// spans finds that case without anyone having to think of it.
    #[test]
    fn every_splice_of_a_small_buffer_matches_a_full_rebuild() {
        let base = "a\nbb\n\nccc\n";
        let insertions = ["", "x", "\n", "x\n", "\nx", "p\nq\nr"];
        for start in 0..=base.len() {
            for end in start..=base.len() {
                for inserted in insertions {
                    let mut doc = Document::new(base.to_string());
                    doc.splice(start, end, inserted);
                    assert_eq!(
                        doc.line_starts,
                        compute_line_starts(&doc.text),
                        "splicing {inserted:?} into {start}..{end} of {base:?}"
                    );
                }
            }
        }
    }

    /// The cached-index path must answer exactly what reading the whole file
    /// answers, for every range. It reads a byte window and rebases the range
    /// onto it, so a clamp that behaves differently at the window edge would
    /// report the wrong text -- silently, and only in files too big to mirror.
    #[test]
    fn the_windowed_read_answers_what_the_whole_file_answers() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("big.txt");
        std::fs::write(&file, "alpha\nbravo\r\n\ncharl\u{00e9}\ndelta").unwrap();

        for sl in 0..6u32 {
            for sc in 0..8u32 {
                for el in 0..6u32 {
                    for ec in 0..8u32 {
                        let range = Range {
                            start: pos(sl, sc),
                            end: pos(el, ec),
                        };
                        // Cold: no index, so the whole file is read.
                        let cold = DocumentStore::new().text_from_disk(&file, range);
                        // Warm: primed by an unrelated range, so this one reads a
                        // window and rebases onto it.
                        let mut warm = DocumentStore::new();
                        warm.text_from_disk(
                            &file,
                            Range {
                                start: pos(0, 0),
                                end: pos(0, 1),
                            },
                        );
                        assert!(warm.disk.is_some(), "the first read must prime the index");
                        assert_eq!(
                            warm.text_from_disk(&file, range),
                            cold,
                            "{sl}:{sc}-{el}:{ec} differs between the windowed and whole-file reads"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_file_that_changed_is_not_answered_from_the_stale_index() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("moving.txt");
        // Line 1, so a stale index sends the read to the wrong byte offset. Asking
        // for line 0 would not prove anything: both versions start at byte 0, so
        // a stale index would land on the right answer by accident.
        let line_one = Range {
            start: pos(1, 0),
            end: pos(1, 40),
        };

        let mut s = DocumentStore::new();
        std::fs::write(&file, "aaaa\nWRONG\n").unwrap();
        assert_eq!(s.text_from_disk(&file, line_one), "WRONG");

        // Different length and a different line layout, so a stale index reads
        // from byte 5 of a file whose second line now starts at byte 2.
        std::fs::write(&file, "b\nRIGHT\nextra padding here\n").unwrap();
        assert_eq!(
            s.text_from_disk(&file, line_one),
            "RIGHT",
            "the cached index outlived the file it described"
        );
    }

    #[test]
    fn a_bare_cursor_never_touches_the_disk() {
        let mut s = DocumentStore::new();
        // A path that does not exist: reading it would warn and answer empty, so
        // the empty answer alone proves nothing. The untouched cache does.
        let missing = std::path::Path::new("/nonexistent/nope.txt");
        let caret = Range {
            start: pos(10, 4),
            end: pos(10, 4),
        };
        assert_eq!(s.text_for_range(&uri(), missing, caret), "");
        assert!(
            s.disk.is_none(),
            "a caret must short-circuit before the file is even stat-ed"
        );
    }

    #[test]
    fn the_mirror_is_preferred_over_the_file_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "on disk\n").unwrap();
        let mut s = DocumentStore::new();
        s.open(uri(), "unsaved\n".to_string());
        let range = Range {
            start: pos(0, 0),
            end: pos(0, 7),
        };
        assert_eq!(s.text_for_range(&uri(), &file, range), "unsaved");
    }
}
