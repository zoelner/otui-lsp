//! Mapping `lang_api` semantic tokens into the LSP wire format.
//!
//! Two responsibilities live here, both purely functional and unit-tested without I/O:
//!
//! 1. The **legend** ([`legend`]): the ordered list of `SemanticTokenType`s the server advertises.
//!    It is built 1:1 from [`SemanticTokenKind::ALL`], so a token's `token_type` is exactly its
//!    [`SemanticTokenKind::index`].
//! 2. The **delta encoding** ([`encode`]): LSP transmits semantic tokens as a flat `[deltaLine,
//!    deltaStart, length, tokenType, tokenModifiers]` stream, each field relative to the previous
//!    token. Positions and lengths are in the negotiated encoding's code units, resolved via
//!    [`LineIndex`].

use lang_api::{SemanticToken as CoreToken, SemanticTokenKind};
use tower_lsp::lsp_types::{SemanticToken as LspToken, SemanticTokenType, SemanticTokensLegend};

use crate::position::{LineIndex, PositionEncoding};

/// The `SemanticTokenType` a core [`SemanticTokenKind`] is advertised and encoded as.
///
/// LSP has no dedicated boolean type, so `Boolean` is surfaced as `KEYWORD` (booleans read as
/// keyword-like literals); every other kind maps to its eponymous standard type.
fn type_of(kind: SemanticTokenKind) -> SemanticTokenType {
    match kind {
        SemanticTokenKind::Comment => SemanticTokenType::COMMENT,
        SemanticTokenKind::Type => SemanticTokenType::TYPE,
        SemanticTokenKind::Property => SemanticTokenType::PROPERTY,
        SemanticTokenKind::String => SemanticTokenType::STRING,
        SemanticTokenKind::Number => SemanticTokenType::NUMBER,
        SemanticTokenKind::Boolean => SemanticTokenType::KEYWORD,
        SemanticTokenKind::EnumMember => SemanticTokenType::ENUM_MEMBER,
        SemanticTokenKind::Variable => SemanticTokenType::VARIABLE,
        SemanticTokenKind::Operator => SemanticTokenType::OPERATOR,
        SemanticTokenKind::Keyword => SemanticTokenType::KEYWORD,
    }
}

/// The legend advertised in `initialize`: token types in [`SemanticTokenKind::ALL`] order, with no
/// modifiers. Index `i` in this list is the `token_type` emitted for `SemanticTokenKind::ALL[i]`.
pub fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: SemanticTokenKind::ALL.iter().map(|&k| type_of(k)).collect(),
        token_modifiers: Vec::new(),
    }
}

/// Delta-encode core tokens for `text` into the LSP flat token stream under `encoding`.
///
/// Tokens are sorted by span start (the engine already returns them sorted, but we do not rely on
/// it). Each token becomes `{ delta_line, delta_start, length, token_type, token_modifiers_bitset }`
/// where line/character deltas and `length` are in `encoding` code units.
pub fn encode(text: &str, tokens: &[CoreToken], encoding: PositionEncoding) -> Vec<LspToken> {
    let index = LineIndex::new(text);

    let mut sorted: Vec<&CoreToken> = tokens.iter().collect();
    sorted.sort_by_key(|t| (t.span.start, t.span.end));

    let mut data = Vec::with_capacity(sorted.len());
    let mut prev_line = 0u32;
    let mut prev_start = 0u32;
    for tok in sorted {
        let pos = index.position(tok.span.start, encoding);
        let length = index.encoded_len(tok.span.start, tok.span.end, encoding);
        let delta_line = pos.line - prev_line;
        // `delta_start` is relative to the previous token only when on the same line; otherwise it
        // is the absolute character column.
        let delta_start = if delta_line == 0 {
            pos.character - prev_start
        } else {
            pos.character
        };
        data.push(LspToken {
            delta_line,
            delta_start,
            length,
            token_type: tok.kind.index(),
            token_modifiers_bitset: 0,
        });
        prev_line = pos.line;
        prev_start = pos.character;
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;
    use lang_api::ByteSpan;

    fn tok(start: usize, end: usize, kind: SemanticTokenKind) -> CoreToken {
        CoreToken {
            span: ByteSpan::new(start, end),
            kind,
        }
    }

    #[test]
    fn legend_maps_kinds_one_to_one_in_all_order() {
        let legend = legend();
        assert!(legend.token_modifiers.is_empty());
        assert_eq!(legend.token_types.len(), SemanticTokenKind::ALL.len());
        // The index a kind encodes to must select its own type in the legend.
        assert_eq!(
            legend.token_types[SemanticTokenKind::Comment.index() as usize],
            SemanticTokenType::COMMENT
        );
        assert_eq!(
            legend.token_types[SemanticTokenKind::Variable.index() as usize],
            SemanticTokenType::VARIABLE
        );
        assert_eq!(
            legend.token_types[SemanticTokenKind::EnumMember.index() as usize],
            SemanticTokenType::ENUM_MEMBER
        );
        // Boolean has no LSP type of its own and is surfaced as a keyword.
        assert_eq!(
            legend.token_types[SemanticTokenKind::Boolean.index() as usize],
            SemanticTokenType::KEYWORD
        );
    }

    #[test]
    fn encodes_deltas_across_and_within_lines() {
        // Line 0: "// c"  -> comment (0..4)
        // Line 1: "Ab Cd" -> Type (7..9), Property (10..12)
        let text = "// c\nAb Cd\n";
        let tokens = [
            tok(0, 4, SemanticTokenKind::Comment),
            tok(5, 7, SemanticTokenKind::Type),
            tok(8, 10, SemanticTokenKind::Property),
        ];
        let data = encode(text, &tokens, PositionEncoding::Utf16);

        assert_eq!(
            data,
            vec![
                // comment at line 0, col 0, len 4, type 0 (Comment)
                LspToken {
                    delta_line: 0,
                    delta_start: 0,
                    length: 4,
                    token_type: SemanticTokenKind::Comment.index(),
                    token_modifiers_bitset: 0,
                },
                // "Ab" one line down, absolute col 0, len 2, type 1 (Type)
                LspToken {
                    delta_line: 1,
                    delta_start: 0,
                    length: 2,
                    token_type: SemanticTokenKind::Type.index(),
                    token_modifiers_bitset: 0,
                },
                // "Cd" same line, 3 cols after "Ab" start, len 2, type 2 (Property)
                LspToken {
                    delta_line: 0,
                    delta_start: 3,
                    length: 2,
                    token_type: SemanticTokenKind::Property.index(),
                    token_modifiers_bitset: 0,
                },
            ]
        );
    }

    #[test]
    fn multibyte_token_length_is_utf16_units_not_bytes() {
        // "café" occupies bytes 0..5 ('é' is 2 UTF-8 bytes) but is 4 UTF-16 code units. The
        // following "x" starts at byte 6 — UTF-16 column 5, one after the 4-unit string.
        let text = "café x";
        let tokens = [
            tok(0, 5, SemanticTokenKind::String),
            tok(6, 7, SemanticTokenKind::Variable),
        ];

        let utf16 = encode(text, &tokens, PositionEncoding::Utf16);
        assert_eq!(utf16[0].length, 4, "UTF-16 length counts code units");
        assert_eq!(utf16[0].delta_start, 0);
        // "x" is delta 5 columns from the string start under UTF-16 (c,a,f,é = 4, then space).
        assert_eq!(utf16[1].delta_line, 0);
        assert_eq!(utf16[1].delta_start, 5);
        assert_eq!(utf16[1].length, 1);

        let utf8 = encode(text, &tokens, PositionEncoding::Utf8);
        assert_eq!(utf8[0].length, 5, "UTF-8 length counts bytes");
        // Under UTF-8 the "x" is 6 byte-columns from the start.
        assert_eq!(utf8[1].delta_start, 6);
    }

    #[test]
    fn unsorted_input_is_sorted_before_encoding() {
        let text = "ab\ncd\n";
        // Deliberately reversed order.
        let tokens = [
            tok(3, 5, SemanticTokenKind::Number),
            tok(0, 2, SemanticTokenKind::Type),
        ];
        let data = encode(text, &tokens, PositionEncoding::Utf16);
        assert_eq!(data[0].token_type, SemanticTokenKind::Type.index());
        assert_eq!(data[0].delta_line, 0);
        assert_eq!(data[1].token_type, SemanticTokenKind::Number.index());
        assert_eq!(data[1].delta_line, 1);
    }
}
