//! Byte-offset ↔ protocol-position conversion — the server's one piece of real logic.
//!
//! `otui-core` speaks in **byte offsets** ([`lang_api::ByteSpan`]); LSP speaks in
//! [`Position`]s of `(line, character)` where `character` is counted in code units of the
//! negotiated [position encoding](PositionEncoding). This module bridges the two.
//!
//! A [`LineIndex`] is built once per document text and lets us convert many spans cheaply: it
//! records the byte offset of every line start, so locating the line for an offset is a binary
//! search, and the column is a single scan of the (usually short) line prefix.

use tower_lsp::lsp_types::{Position, PositionEncodingKind, Range};

/// The unit in which LSP `character` columns are counted.
///
/// UTF-16 is the protocol default and the only encoding a client must support; UTF-8 (byte
/// columns) is selected only when the client advertises it during negotiation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PositionEncoding {
    /// `character` = number of UTF-16 code units from the line start. LSP default.
    #[default]
    Utf16,
    /// `character` = number of UTF-8 bytes from the line start.
    Utf8,
}

impl PositionEncoding {
    /// The `lsp_types` kind advertised in `ServerCapabilities.position_encoding`.
    pub fn to_kind(self) -> PositionEncodingKind {
        match self {
            PositionEncoding::Utf16 => PositionEncodingKind::UTF16,
            PositionEncoding::Utf8 => PositionEncodingKind::UTF8,
        }
    }
}

/// A precomputed index of line starts for one document, enabling cheap byte→[`Position`]
/// conversion.
#[derive(Debug, Clone)]
pub struct LineIndex<'a> {
    text: &'a str,
    /// Byte offset at which each line begins. Always starts with `0`.
    line_starts: Vec<usize>,
}

impl<'a> LineIndex<'a> {
    /// Build an index over `text`.
    pub fn new(text: &'a str) -> Self {
        let mut line_starts = vec![0usize];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        Self { text, line_starts }
    }

    /// Convert a byte `offset` into a [`Position`] under `encoding`.
    ///
    /// Offsets past the end of the text clamp to the document end; offsets that land inside a
    /// multi-byte UTF-8 sequence are treated as the start of that character's column.
    pub fn position(&self, offset: usize, encoding: PositionEncoding) -> Position {
        let mut offset = offset.min(self.text.len());
        // Byte-indexing `self.text` below requires a char boundary. An offset landing inside a
        // multi-byte UTF-8 sequence (e.g. a caller mapping a byte span computed against
        // different text) would otherwise panic; clamp down to the start of that character
        // instead, matching the documented "start of that character's column" behavior.
        while !self.text.is_char_boundary(offset) {
            offset -= 1;
        }
        // The line is the last line whose start is <= offset. `partition_point` gives the first
        // index whose start is > offset, so the line is one before that.
        let line = self.line_starts.partition_point(|&start| start <= offset) - 1;
        let line_start = self.line_starts[line];

        // Column: count code units in the prefix `[line_start, offset)`.
        let prefix = &self.text[line_start..offset];
        let character = match encoding {
            PositionEncoding::Utf8 => prefix.len() as u32,
            PositionEncoding::Utf16 => prefix.chars().map(|c| c.len_utf16() as u32).sum(),
        };

        Position {
            line: line as u32,
            character,
        }
    }

    /// The length of the `[start, end)` byte span measured in `encoding` code units — the unit
    /// LSP semantic-token `length` fields are counted in (UTF-16 code units by default, so a
    /// multi-byte character contributes fewer units than its byte length).
    ///
    /// Offsets are clamped to the text and to char boundaries, mirroring [`position`](Self::position).
    pub fn encoded_len(&self, start: usize, end: usize, encoding: PositionEncoding) -> u32 {
        let mut start = start.min(self.text.len());
        let mut end = end.min(self.text.len());
        while !self.text.is_char_boundary(start) {
            start -= 1;
        }
        while !self.text.is_char_boundary(end) {
            end -= 1;
        }
        if start >= end {
            return 0;
        }
        let slice = &self.text[start..end];
        match encoding {
            PositionEncoding::Utf8 => slice.len() as u32,
            PositionEncoding::Utf16 => slice.chars().map(|c| c.len_utf16() as u32).sum(),
        }
    }

    /// Convert an LSP [`Position`] into a byte offset under `encoding` — the inverse of
    /// [`position`](Self::position).
    ///
    /// Clamping rules mirror `position`'s tolerance so the two round-trip:
    /// * a `line` past the last line clamps to the end of the document;
    /// * a `character` past the end of its line's content clamps to the line end (just before the
    ///   trailing newline, or the document end for the last line);
    /// * a `character` that would land inside a multi-byte UTF-8 sequence clamps down to that
    ///   character's start, so the returned offset is always on a char boundary.
    pub fn offset_at(&self, position: Position, encoding: PositionEncoding) -> usize {
        let line = position.line as usize;
        // A line past the end clamps to the document end.
        let Some(&line_start) = self.line_starts.get(line) else {
            return self.text.len();
        };
        // The byte at which the next line begins (or the document end for the last line). The line's
        // own content excludes a trailing `\n`, so the column never crosses onto the next line.
        let next_line_start = self
            .line_starts
            .get(line + 1)
            .copied()
            .unwrap_or(self.text.len());
        let mut content_end = next_line_start;
        if content_end > line_start && self.text.as_bytes()[content_end - 1] == b'\n' {
            content_end -= 1;
        }
        let line_text = &self.text[line_start..content_end];

        let target = position.character as usize;
        let mut units = 0usize;
        for (byte_idx, ch) in line_text.char_indices() {
            if units >= target {
                return line_start + byte_idx;
            }
            let width = match encoding {
                PositionEncoding::Utf8 => ch.len_utf8(),
                PositionEncoding::Utf16 => ch.len_utf16(),
            };
            // The target lands inside this character's code units: clamp to the character start so
            // the offset stays on a char boundary rather than splitting a UTF-8 sequence.
            if units + width > target {
                return line_start + byte_idx;
            }
            units += width;
        }
        // The character ran past the line's content: clamp to the line end.
        content_end
    }

    /// Convert a `[start, end)` byte span into an LSP [`Range`].
    pub fn range(&self, start: usize, end: usize, encoding: PositionEncoding) -> Range {
        Range {
            start: self.position(start, encoding),
            end: self.position(end, encoding),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_offsets_map_to_columns() {
        let idx = LineIndex::new("hello");
        assert_eq!(
            idx.position(0, PositionEncoding::Utf16),
            Position::new(0, 0)
        );
        assert_eq!(
            idx.position(3, PositionEncoding::Utf16),
            Position::new(0, 3)
        );
        // Clamps past end.
        assert_eq!(
            idx.position(99, PositionEncoding::Utf16),
            Position::new(0, 5)
        );
    }

    #[test]
    fn newline_resets_line_and_character() {
        let text = "Panel\n\tid: main\n";
        let idx = LineIndex::new(text);
        // The tab is the first byte of line 1 (byte offset 6).
        assert_eq!(
            idx.position(6, PositionEncoding::Utf16),
            Position::new(1, 0)
        );
        // One past the tab: still line 1, character 1.
        assert_eq!(
            idx.position(7, PositionEncoding::Utf16),
            Position::new(1, 1)
        );
        // Start of a following empty line 2.
        assert_eq!(
            idx.position(text.len(), PositionEncoding::Utf16),
            Position::new(2, 0)
        );
    }

    #[test]
    fn multibyte_utf8_counts_utf16_code_units() {
        // "café: x" — 'é' is two UTF-8 bytes (0xC3 0xA9) but one UTF-16 code unit.
        // Bytes:  c(0) a(1) f(2) é(3,4) :(5) ' '(6) x(7)
        let text = "café: x";
        let idx = LineIndex::new(text);
        // Byte offset 5 is the ':' — three ASCII + one 2-byte char before it.
        // In UTF-16 the column is 4 (c,a,f,é), not 5 bytes.
        assert_eq!(
            idx.position(5, PositionEncoding::Utf16),
            Position::new(0, 4)
        );
        // Under UTF-8 the same offset is byte column 5.
        assert_eq!(idx.position(5, PositionEncoding::Utf8), Position::new(0, 5));
        // The 'x' at byte 7 → UTF-16 column 6.
        assert_eq!(
            idx.position(7, PositionEncoding::Utf16),
            Position::new(0, 6)
        );
    }

    #[test]
    fn offset_inside_multibyte_char_clamps_without_panicking() {
        // "café: x" — 'é' spans bytes 3..5 (0xC3 0xA9). Byte offset 4 lands on its second byte,
        // which is not a char boundary; the conversion must clamp down to offset 3 (the start
        // of 'é') rather than panic on the slice.
        let text = "café: x";
        let idx = LineIndex::new(text);
        assert!(!text.is_char_boundary(4));
        assert_eq!(
            idx.position(4, PositionEncoding::Utf16),
            idx.position(3, PositionEncoding::Utf16),
        );
        assert_eq!(
            idx.position(4, PositionEncoding::Utf16),
            Position::new(0, 3)
        );
    }

    #[test]
    fn encoded_len_counts_utf16_units_not_bytes() {
        // "café" — 'é' is 2 UTF-8 bytes but 1 UTF-16 code unit, so byte span 0..5 is 4 UTF-16
        // units and 5 UTF-8 bytes.
        let idx = LineIndex::new("café");
        assert_eq!(idx.encoded_len(0, 5, PositionEncoding::Utf16), 4);
        assert_eq!(idx.encoded_len(0, 5, PositionEncoding::Utf8), 5);
        // A char landing on a non-boundary end clamps down.
        assert_eq!(idx.encoded_len(0, 4, PositionEncoding::Utf16), 3);
    }

    #[test]
    fn offset_at_inverts_position_on_ascii() {
        let text = "Panel\n  id: main\n";
        let idx = LineIndex::new(text);
        for enc in [PositionEncoding::Utf16, PositionEncoding::Utf8] {
            // Every char-boundary offset round-trips: offset → Position → offset.
            for offset in 0..=text.len() {
                if !text.is_char_boundary(offset) {
                    continue;
                }
                let pos = idx.position(offset, enc);
                assert_eq!(
                    idx.offset_at(pos, enc),
                    offset,
                    "offset {offset} enc {enc:?}"
                );
            }
        }
    }

    #[test]
    fn offset_at_round_trips_a_multibyte_line() {
        // 'é' is 2 UTF-8 bytes / 1 UTF-16 unit; 'ä' likewise. Round-trip every boundary offset.
        let text = "café < ä\n  id: x\n";
        let idx = LineIndex::new(text);
        for enc in [PositionEncoding::Utf16, PositionEncoding::Utf8] {
            for offset in 0..=text.len() {
                if !text.is_char_boundary(offset) {
                    continue;
                }
                let pos = idx.position(offset, enc);
                assert_eq!(
                    idx.offset_at(pos, enc),
                    offset,
                    "offset {offset} enc {enc:?}"
                );
            }
        }
    }

    #[test]
    fn offset_at_clamps_out_of_range_line_to_document_end() {
        let text = "Panel\n";
        let idx = LineIndex::new(text);
        let pos = Position::new(99, 0);
        assert_eq!(idx.offset_at(pos, PositionEncoding::Utf16), text.len());
    }

    #[test]
    fn offset_at_clamps_out_of_range_character_to_line_end() {
        // Character past the line content clamps to just before the newline (line end), never onto
        // the next line.
        let text = "Panel\nHi\n";
        let idx = LineIndex::new(text);
        // Line 0 content is "Panel" (bytes 0..5); a huge column clamps to byte 5, not into line 1.
        assert_eq!(
            idx.offset_at(Position::new(0, 99), PositionEncoding::Utf16),
            5
        );
    }

    #[test]
    fn offset_at_clamps_character_inside_multibyte_to_char_start() {
        // Under UTF-8, 'é' spans byte columns 3..5. A target column of 4 lands inside it and must
        // clamp down to the character start (byte offset 3), staying on a char boundary.
        let text = "café";
        let idx = LineIndex::new(text);
        assert_eq!(
            idx.offset_at(Position::new(0, 4), PositionEncoding::Utf8),
            3
        );
    }

    #[test]
    fn range_spans_two_positions() {
        let idx = LineIndex::new("Panel\n\tid: main\n");
        let r = idx.range(6, 7, PositionEncoding::Utf16);
        assert_eq!(r.start, Position::new(1, 0));
        assert_eq!(r.end, Position::new(1, 1));
    }
}
