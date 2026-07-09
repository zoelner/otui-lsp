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
    fn range_spans_two_positions() {
        let idx = LineIndex::new("Panel\n\tid: main\n");
        let r = idx.range(6, 7, PositionEncoding::Utf16);
        assert_eq!(r.start, Position::new(1, 0));
        assert_eq!(r.end, Position::new(1, 1));
    }
}
