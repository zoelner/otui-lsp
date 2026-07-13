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
    /// A widget/type name: a style header's `Name` and a bare container tag. A `< Base` is a more
    /// specific [`BuiltinType`](SemanticTokenKind::BuiltinType) or
    /// [`InheritedType`](SemanticTokenKind::InheritedType) instead.
    Type,
    /// A key: a generic `property`, `id`, an `anchors.<edge>` object/edge, or an `&alias` / `!expr`
    /// key name. An `@event` key is a more specific [`Event`](SemanticTokenKind::Event) instead.
    Property,
    /// A string value: a quoted string, a `#`-carve-out `&` alias literal, a `color` literal
    /// (colors are highlighted as strings — see the `semantic` module), a bare word value, and a
    /// bare identifier inside an inline array.
    String,
    /// A numeric literal (including percentages).
    Number,
    /// A `true` / `false` boolean literal.
    Boolean,
    /// A `$state` name inside the engine's **known** set (a recognised widget state). A name outside
    /// that set is an [`UnknownState`](SemanticTokenKind::UnknownState) instead.
    EnumMember,
    /// A variable: a `$variable` reference, an `id:` value (the id being defined), and an
    /// `anchors.<edge>:` target (the referenced widget/edge).
    Variable,
    /// An operator: the `!` negation marker on a `$state`.
    Operator,
    /// A keyword-like literal: the `~` null value, or the magic `parent`/`prev`/`next` relative
    /// anchor reference in an `anchors.<edge>: <target>` target (the engine's
    /// `UIAnchor::getHookedWidget` treats exactly those three strings as the relative hooked
    /// widget; any other target is a concrete widget id, tokenized as `Variable`).
    Keyword,
    /// A `< Base` naming a built-in native widget class — a base beginning with `UI`. Distinguished
    /// from a file-defined parent so the client can render engine widgets as a standard-library type.
    BuiltinType,
    /// A `< Base` naming a file-defined parent style — a base not beginning with `UI`.
    InheritedType,
    /// An `@event` handler key name (`@onClick`, `@onSetup`, …).
    Event,
    /// A `$state` name **outside** the engine's known set. It silently never matches at runtime — a
    /// latent authoring bug — so it is surfaced distinctly from a valid [`EnumMember`] state.
    UnknownState,
}

impl SemanticTokenKind {
    /// The canonical ordering of every kind. A token's legend index is its position here, so the
    /// server builds its `SemanticTokensLegend` by mapping this array 1:1 to LSP token types.
    pub const ALL: [SemanticTokenKind; 14] = [
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
        SemanticTokenKind::BuiltinType,
        SemanticTokenKind::InheritedType,
        SemanticTokenKind::Event,
        SemanticTokenKind::UnknownState,
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
            SemanticTokenKind::BuiltinType => 10,
            SemanticTokenKind::InheritedType => 11,
            SemanticTokenKind::Event => 12,
            SemanticTokenKind::UnknownState => 13,
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

/// The category of a [`DocumentSymbol`] in the outline, protocol-agnostic.
///
/// Each variant is named after a standard LSP `SymbolKind` (the server crate is the only place
/// that maps a variant to `lsp_types::SymbolKind`), but this enum knows nothing about LSP. The
/// set is intentionally minimal — an OTUI outline is a tree of widgets, distinguished only by
/// whether a widget carries an `id:`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    /// A widget with no `id:`: a bare container tag or a `Name < Base` style header, shown in
    /// the outline under its type name.
    Object,
    /// A widget that declares an `id:`: the id names the widget, so it reads as a named field of
    /// its parent in the outline.
    Field,
}

/// One node of the widget-outline tree (spec §5.1): a widget and its nested widgets.
///
/// All spans are **byte offsets** into the source. `span` covers the whole widget node (used as
/// the LSP `range`); `selection_span` covers just the name token — the `id:` value if the widget
/// has one, else the container tag or the style `Name` — and is always contained within `span`
/// (the LSP `selectionRange`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentSymbol {
    /// Display name: the `id:` value if present, else the container tag / style `Name`.
    pub name: String,
    /// The widget's type, shown alongside the name: the container tag, or a style header's
    /// `< Base`. `None` only if the type token is somehow absent.
    pub detail: Option<String>,
    /// Whether the widget is named by an id ([`SymbolKind::Field`]) or by its type
    /// ([`SymbolKind::Object`]).
    pub kind: SymbolKind,
    /// Byte span of the whole widget node.
    pub span: ByteSpan,
    /// Byte span of the name token; always within [`span`](DocumentSymbol::span).
    pub selection_span: ByteSpan,
    /// Nested widgets, in source order.
    pub children: Vec<DocumentSymbol>,
}

/// The category of a [`CompletionItem`], protocol-agnostic.
///
/// Each variant is named after a standard LSP `CompletionItemKind` (the server crate is the only
/// place that maps a variant to `lsp_types::CompletionItemKind`), but this enum knows nothing about
/// LSP. The set is intentionally minimal — just the categories the OTML **closed sets** need:
///
/// * [`EnumMember`](CompletionKind::EnumMember) — a `$state` name or an `anchors.<edge>` edge:
///   members of a fixed enumeration the engine recognizes.
/// * [`Value`](CompletionKind::Value) — a magic anchor target keyword (`parent` / `next` / `prev`):
///   a literal that stands in the value slot of an anchor.
/// * [`Event`](CompletionKind::Event) — an `@event` handler name.
/// * [`Keyword`](CompletionKind::Keyword) — an OTML property **name** from the generated catalog
///   (spec §2.10), offered when the cursor is building an ordinary `key:`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    /// A member of a fixed enumeration: a `$state` name or an anchor edge.
    EnumMember,
    /// A literal value keyword: a magic anchor target (`parent` / `next` / `prev`).
    Value,
    /// An `@event` handler name.
    Event,
    /// An OTML property **name** (from the generated property catalog or a widget's Lua additions).
    Keyword,
    /// A widget/style type usable as a nested child tag (a workspace `Name < Base` style, a Lua
    /// widget class, or a native `UI*` base).
    Class,
}

/// How a [`CompletionItem::insert_text`] should be interpreted by the client, protocol-agnostic.
///
/// Mirrors LSP's `InsertTextFormat` (the server crate is the only place that maps a variant to
/// `lsp_types::InsertTextFormat`), but this enum knows nothing about LSP. [`Plain`](Self::Plain) is
/// the default: `insert_text` (or, when absent, `label`) is inserted verbatim. [`Snippet`](Self::Snippet)
/// marks `insert_text` as LSP/TextMate snippet syntax (`$0`, `$1`, `${1:placeholder}`) — tab-stops the
/// client's editor walks through after insertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InsertFormat {
    /// `insert_text` (or `label`) is plain text, inserted as-is.
    #[default]
    Plain,
    /// `insert_text` is LSP snippet syntax: tab-stops and placeholders the client's editor expands.
    Snippet,
}

/// One completion candidate, protocol-agnostic (spec §6).
///
/// The engine emits these for the OTML **closed sets** it knows exhaustively (`$state` names, anchor
/// edges, magic anchor targets, `@event` names). `label` is the canonical spelling to insert (from
/// the schema consts — camelCase edges, lowercase states), `kind` classifies it for the client's
/// icon, and `detail` is a short one-line hint. The server maps this onto `lsp_types::CompletionItem`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    /// The text to insert, in its canonical schema spelling.
    pub label: String,
    /// The candidate's category, for the client's completion icon.
    pub kind: CompletionKind,
    /// A short human-readable hint (e.g. `"anchor edge"`), or `None`.
    pub detail: Option<String>,
    /// An opaque ordering key overriding the client's alphabetic sort, or `None` to sort by `label`.
    /// Used where several categories share one position (e.g. a widget's own Lua properties ranked
    /// above the global catalog, above child-widget names) so the most relevant float to the top.
    pub sort_text: Option<String>,
    /// The text to insert in place of `label`, or `None` to insert `label` verbatim. Set only where a
    /// structural snippet (a property `key: $0`, a child widget's `id:` skeleton, …) is worth more
    /// than the bare name; `insert_format` says how to interpret it.
    pub insert_text: Option<String>,
    /// How `insert_text` is interpreted. `Plain` unless `insert_text` carries snippet syntax.
    pub insert_format: InsertFormat,
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

    /// Build the document-symbol outline (the widget hierarchy) for a full source document.
    ///
    /// The result is a forest in source order: one [`DocumentSymbol`] per widget, with nested
    /// widgets as children. Scalar properties are not symbols.
    fn document_symbols(&self, source: &str) -> Vec<DocumentSymbol>;
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
            SemanticTokenKind::BuiltinType,
            SemanticTokenKind::InheritedType,
            SemanticTokenKind::Event,
            SemanticTokenKind::UnknownState,
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
                | SemanticTokenKind::Keyword
                | SemanticTokenKind::BuiltinType
                | SemanticTokenKind::InheritedType
                | SemanticTokenKind::Event
                | SemanticTokenKind::UnknownState => assert_at_own_index(kind),
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
