//! Host-agnostic language-service contract for otui-lsp backends.
//!
//! This crate is deliberately tiny and protocol-agnostic: it knows nothing about LSP or
//! `lsp-types`. Backends (today [`otui-core`], tomorrow a possible `htmlcss-core`) implement
//! [`LanguageService`] and return results in **byte offsets** into the source. The LSP server
//! crate is the only place that converts those byte offsets into protocol positions
//! (UTF-16 by default). This seam is what lets a second language be added without rewriting the
//! transport layer.
//!
//! [`otui-core`]: https://github.com/zoelner/otui-lsp

/// A half-open `[start, end)` range of **byte offsets** into a source document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSpan {
    pub start: usize,
    pub end: usize,
}

impl ByteSpan {
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub const fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub const fn is_empty(&self) -> bool {
        self.start >= self.end
    }
}

/// Diagnostic severity, mirroring the engine-fidelity rules of the OTUI spec (see
/// `docs/otui-language-service-spec.md` §4): most authoring mistakes the engine silently
/// tolerates surface as [`Severity::Hint`], not errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Hint,
}

/// A single diagnostic, protocol-agnostic. `code` is a stable identifier
/// (e.g. `"tab-indentation"`, `"unknown-property"`) that the server maps to an LSP diagnostic
/// code; `span` is byte offsets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: &'static str,
    pub message: String,
    pub span: ByteSpan,
}

/// The semantic category of a source token, protocol-agnostic.
///
/// Each variant is aligned to a standard LSP `SemanticTokenType` name (the server crate is the
/// only place that maps a variant to `lsp_types::SemanticTokenType`), but the enum itself knows
/// nothing about LSP. The set is intentionally minimal — just the categories OTUI/OTML source
/// actually distinguishes. The comment on each variant states its OTUI meaning.
///
/// [`ALL`](SemanticTokenKind::ALL) fixes the canonical ordering used to build the client legend:
/// a token's `token_type` index is its position in `ALL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SemanticTokenKind {
    /// A full-line `//` / `#` comment.
    Comment,
    /// A widget/type name: a style header's `Name` and its `< Base`, and a bare container tag.
    Type,
    /// A key: a generic `property`, `id`, an `anchors.<edge>` object/edge, or an `@event` /
    /// `&alias` / `!expr` key name.
    Property,
    /// A string value: a quoted string, a `#`-carve-out `&` alias literal, a `color` literal
    /// (colors are highlighted as strings — see the `semantic` module), a bare word value, and a
    /// bare identifier inside an inline array.
    String,
    /// A numeric literal (including percentages).
    Number,
    /// A `true` / `false` boolean literal.
    Boolean,
    /// A `$state` name inside a state selector (an enumerated widget state).
    EnumMember,
    /// A variable: a `$variable` reference, an `id:` value (the id being defined), and an
    /// `anchors.<edge>:` target (the referenced widget/edge).
    Variable,
    /// An operator: the `!` negation marker on a `$state`.
    Operator,
    /// A keyword-like literal: the `~` null value.
    Keyword,
}

impl SemanticTokenKind {
    /// The canonical ordering of every kind. A token's legend index is its position here, so the
    /// server builds its `SemanticTokensLegend` by mapping this array 1:1 to LSP token types.
    pub const ALL: [SemanticTokenKind; 10] = [
        SemanticTokenKind::Comment,
        SemanticTokenKind::Type,
        SemanticTokenKind::Property,
        SemanticTokenKind::String,
        SemanticTokenKind::Number,
        SemanticTokenKind::Boolean,
        SemanticTokenKind::EnumMember,
        SemanticTokenKind::Variable,
        SemanticTokenKind::Operator,
        SemanticTokenKind::Keyword,
    ];

    /// This kind's index in [`ALL`](SemanticTokenKind::ALL) — the `token_type` value emitted in
    /// the LSP delta encoding, and the position of its type in the client legend.
    pub const fn index(self) -> u32 {
        match self {
            SemanticTokenKind::Comment => 0,
            SemanticTokenKind::Type => 1,
            SemanticTokenKind::Property => 2,
            SemanticTokenKind::String => 3,
            SemanticTokenKind::Number => 4,
            SemanticTokenKind::Boolean => 5,
            SemanticTokenKind::EnumMember => 6,
            SemanticTokenKind::Variable => 7,
            SemanticTokenKind::Operator => 8,
            SemanticTokenKind::Keyword => 9,
        }
    }
}

/// One highlighted region of a source document: a byte [`span`](SemanticToken::span) and its
/// semantic [`kind`](SemanticToken::kind). Spans are leaf-level and never overlap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemanticToken {
    pub span: ByteSpan,
    pub kind: SemanticTokenKind,
}

/// The contract every language backend implements. Kept intentionally minimal for now; symbols,
/// completion and hover are added as the corresponding milestones land.
pub trait LanguageService {
    /// A stable identifier for the language this backend serves (e.g. `"otui"`).
    fn language_id(&self) -> &'static str;

    /// Compute diagnostics for a full source document.
    fn diagnostics(&self, source: &str) -> Vec<Diagnostic>;

    /// Compute semantic-highlighting tokens for a full source document.
    ///
    /// The returned tokens are leaf-level, sorted by span start, and non-overlapping — the
    /// invariants LSP semantic tokens require.
    fn semantic_tokens(&self, source: &str) -> Vec<SemanticToken>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_kind_index_round_trips_through_all() {
        for (i, kind) in SemanticTokenKind::ALL.iter().enumerate() {
            assert_eq!(kind.index() as usize, i, "index must match position in ALL");
        }
    }

    #[test]
    fn all_lists_every_variant_exactly_once_at_its_index() {
        // Runtime check, driven per-variant below: each variant appears in `ALL` exactly once,
        // at the position its own `index()` returns.
        fn assert_at_own_index(kind: SemanticTokenKind) {
            assert_eq!(
                SemanticTokenKind::ALL
                    .iter()
                    .filter(|&&k| k == kind)
                    .count(),
                1,
                "{kind:?} must appear exactly once in ALL"
            );
            assert_eq!(
                SemanticTokenKind::ALL[kind.index() as usize],
                kind,
                "{kind:?} must sit at its own index() position in ALL"
            );
        }

        // Every variant, driven through the exhaustive match below. This list must be kept in
        // sync with the enum, but that sync is compiler-enforced: the match right below has no
        // wildcard arm, so adding a new `SemanticTokenKind` variant without also adding it here
        // (and to `ALL`) is a compile error, not a silently-shrunk legend.
        let kinds = [
            SemanticTokenKind::Comment,
            SemanticTokenKind::Type,
            SemanticTokenKind::Property,
            SemanticTokenKind::String,
            SemanticTokenKind::Number,
            SemanticTokenKind::Boolean,
            SemanticTokenKind::EnumMember,
            SemanticTokenKind::Variable,
            SemanticTokenKind::Operator,
            SemanticTokenKind::Keyword,
        ];
        assert_eq!(
            kinds.len(),
            SemanticTokenKind::ALL.len(),
            "add new variants to this list too"
        );

        for kind in kinds {
            // Exhaustive match: a new variant added to `SemanticTokenKind` without a matching
            // arm here fails to compile, forcing this guard (and `ALL`) to be updated whenever
            // the enum grows.
            match kind {
                SemanticTokenKind::Comment
                | SemanticTokenKind::Type
                | SemanticTokenKind::Property
                | SemanticTokenKind::String
                | SemanticTokenKind::Number
                | SemanticTokenKind::Boolean
                | SemanticTokenKind::EnumMember
                | SemanticTokenKind::Variable
                | SemanticTokenKind::Operator
                | SemanticTokenKind::Keyword => assert_at_own_index(kind),
            }
        }
    }

    #[test]
    fn byte_span_len_and_emptiness() {
        assert_eq!(ByteSpan::new(2, 5).len(), 3);
        assert!(ByteSpan::new(4, 4).is_empty());
        assert!(!ByteSpan::new(4, 5).is_empty());
        // Degenerate (end < start) spans are treated as empty, never underflowing.
        assert_eq!(ByteSpan::new(5, 2).len(), 0);
        assert!(ByteSpan::new(5, 2).is_empty());
    }
}
