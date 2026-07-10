//! Mapping `lang_api` diagnostics into `lsp_types` diagnostics.
//!
//! This is the only place the byte-offset engine output meets the LSP wire format. Severity,
//! code and span are translated; `source` is stamped as `"otui"` so clients can group findings.

use lang_api::{
    ByteSpan, CompletionItem as CoreCompletionItem, CompletionKind as CoreCompletionKind,
    Diagnostic as CoreDiagnostic, DocumentSymbol as CoreSymbol, Severity,
    SymbolKind as CoreSymbolKind,
};
use otui_core::folding::{FoldKind as CoreFoldKind, FoldRange as CoreFoldRange};
use otui_core::schema::Rgba;
use tower_lsp::lsp_types::{
    Color, ColorInformation, CompletionItem as LspCompletionItem,
    CompletionItemKind as LspCompletionItemKind, Diagnostic as LspDiagnostic, DiagnosticSeverity,
    DocumentSymbol as LspSymbol, FoldingRange, FoldingRangeKind, Location, NumberOrString,
    Position, Range, SymbolInformation, SymbolKind as LspSymbolKind, TextEdit, Url,
};

use crate::position::{LineIndex, PositionEncoding};

/// The `source` string stamped on every diagnostic this server publishes.
pub const DIAGNOSTIC_SOURCE: &str = "otui";

fn severity_to_lsp(severity: Severity) -> DiagnosticSeverity {
    match severity {
        Severity::Error => DiagnosticSeverity::ERROR,
        Severity::Warning => DiagnosticSeverity::WARNING,
        Severity::Hint => DiagnosticSeverity::HINT,
    }
}

/// Convert a single core diagnostic into an LSP diagnostic, resolving its byte span against
/// `index` under `encoding`.
pub fn to_lsp(
    diag: &CoreDiagnostic,
    index: &LineIndex<'_>,
    encoding: PositionEncoding,
) -> LspDiagnostic {
    let range: Range = index.range(diag.span.start, diag.span.end, encoding);
    LspDiagnostic {
        range,
        severity: Some(severity_to_lsp(diag.severity)),
        code: Some(NumberOrString::String(diag.code.to_owned())),
        code_description: None,
        source: Some(DIAGNOSTIC_SOURCE.to_owned()),
        message: diag.message.clone(),
        related_information: None,
        tags: None,
        data: None,
    }
}

/// Convert every diagnostic for `text`, building one shared [`LineIndex`] for the batch.
pub fn all_to_lsp(
    text: &str,
    diags: &[CoreDiagnostic],
    encoding: PositionEncoding,
) -> Vec<LspDiagnostic> {
    let index = LineIndex::new(text);
    diags.iter().map(|d| to_lsp(d, &index, encoding)).collect()
}

/// Build an LSP [`Location`] for a byte `span` inside `text`, under `encoding`.
///
/// Used by go-to-definition (spec §5.3): a resolved target's `name_span` is a byte span into **its
/// own** document's text, so this must be called with that target document's text (not the request
/// document's), building a fresh [`LineIndex`] over it.
pub fn location_of(uri: Url, text: &str, span: ByteSpan, encoding: PositionEncoding) -> Location {
    let index = LineIndex::new(text);
    Location {
        uri,
        range: index.range(span.start, span.end, encoding),
    }
}

/// Map a protocol-agnostic [`CoreSymbolKind`] onto its LSP `SymbolKind`.
///
/// A widget named by its type is an `OBJECT`; a widget named by an `id:` reads as a `FIELD` of
/// its parent in the outline.
fn symbol_kind_to_lsp(kind: CoreSymbolKind) -> LspSymbolKind {
    match kind {
        CoreSymbolKind::Object => LspSymbolKind::OBJECT,
        CoreSymbolKind::Field => LspSymbolKind::FIELD,
    }
}

/// Recursively convert a core [`DocumentSymbol`](CoreSymbol) into an `lsp_types::DocumentSymbol`,
/// resolving both byte spans against `index` under `encoding`.
#[allow(deprecated)] // `DocumentSymbol.deprecated` is deprecated but a mandatory struct field.
pub fn symbol_to_lsp(
    sym: &CoreSymbol,
    index: &LineIndex<'_>,
    encoding: PositionEncoding,
) -> LspSymbol {
    LspSymbol {
        name: sym.name.clone(),
        detail: sym.detail.clone(),
        kind: symbol_kind_to_lsp(sym.kind),
        tags: None,
        deprecated: None,
        range: index.range(sym.span.start, sym.span.end, encoding),
        selection_range: index.range(sym.selection_span.start, sym.selection_span.end, encoding),
        children: symbols_to_lsp_with(&sym.children, index, encoding),
    }
}

/// Convert a forest of core symbols against an existing `index` (used for recursion into children).
fn symbols_to_lsp_with(
    syms: &[CoreSymbol],
    index: &LineIndex<'_>,
    encoding: PositionEncoding,
) -> Option<Vec<LspSymbol>> {
    if syms.is_empty() {
        return None;
    }
    Some(
        syms.iter()
            .map(|s| symbol_to_lsp(s, index, encoding))
            .collect(),
    )
}

/// Convert every top-level document symbol for `text`, building one shared [`LineIndex`].
pub fn symbols_to_lsp(
    text: &str,
    syms: &[CoreSymbol],
    encoding: PositionEncoding,
) -> Vec<LspSymbol> {
    let index = LineIndex::new(text);
    syms.iter()
        .map(|s| symbol_to_lsp(s, &index, encoding))
        .collect()
}

/// Flatten the symbol forest into LSP [`SymbolInformation`] for clients that did **not** negotiate
/// `hierarchicalDocumentSymbolSupport` (LSP 3.17 §textDocument/documentSymbol): such a client can
/// only consume the flat `SymbolInformation[]` shape. The nesting that [`symbols_to_lsp`] carries
/// in `children` is preserved here only via `container_name` (the enclosing widget's name); each
/// symbol's `location` uses its full span, and every symbol at every depth is emitted (depth-first,
/// source order).
#[allow(deprecated)] // `SymbolInformation` (and its `deprecated` field) are deprecated but are the
                     // only shape a non-hierarchical client accepts.
pub fn symbols_to_flat(
    uri: &Url,
    text: &str,
    syms: &[CoreSymbol],
    encoding: PositionEncoding,
) -> Vec<SymbolInformation> {
    let index = LineIndex::new(text);
    let mut out = Vec::new();
    for sym in syms {
        flatten_symbol(uri, sym, None, &index, encoding, &mut out);
    }
    out
}

/// Push `sym` and, recursively, all its descendants into `out` as flat [`SymbolInformation`],
/// tagging each with its parent's name as `container_name`.
#[allow(deprecated)] // See `symbols_to_flat`.
fn flatten_symbol(
    uri: &Url,
    sym: &CoreSymbol,
    container: Option<&str>,
    index: &LineIndex<'_>,
    encoding: PositionEncoding,
    out: &mut Vec<SymbolInformation>,
) {
    out.push(SymbolInformation {
        name: sym.name.clone(),
        kind: symbol_kind_to_lsp(sym.kind),
        tags: None,
        deprecated: None,
        location: Location {
            uri: uri.clone(),
            range: index.range(sym.span.start, sym.span.end, encoding),
        },
        container_name: container.map(str::to_owned),
    });
    for child in &sym.children {
        flatten_symbol(uri, child, Some(&sym.name), index, encoding, out);
    }
}

/// Convert a single core quick-fix edit — a `(byte span, replacement)` pair — into an LSP
/// [`TextEdit`], resolving the span against `index` under `encoding`. This is the byte-offset →
/// protocol seam for [`code_action`](crate::Backend); the replacement text is carried verbatim.
pub fn text_edit_of(
    span: ByteSpan,
    new_text: &str,
    index: &LineIndex<'_>,
    encoding: PositionEncoding,
) -> TextEdit {
    TextEdit {
        range: index.range(span.start, span.end, encoding),
        new_text: new_text.to_owned(),
    }
}

/// Map a protocol-agnostic [`CoreCompletionKind`] onto its LSP `CompletionItemKind`.
///
/// The enum-member kinds (`$state` names, anchor edges) surface as `ENUM_MEMBER`; a magic anchor
/// target keyword as `VALUE`; an `@event` name as `EVENT`; the deferred property-name seam as
/// `KEYWORD`.
fn completion_kind_to_lsp(kind: CoreCompletionKind) -> LspCompletionItemKind {
    match kind {
        CoreCompletionKind::EnumMember => LspCompletionItemKind::ENUM_MEMBER,
        CoreCompletionKind::Value => LspCompletionItemKind::VALUE,
        CoreCompletionKind::Event => LspCompletionItemKind::EVENT,
        CoreCompletionKind::Keyword => LspCompletionItemKind::KEYWORD,
    }
}

/// Convert a single core [`CompletionItem`](CoreCompletionItem) into an `lsp_types::CompletionItem`,
/// carrying its label, kind and detail. Completion labels are already the value to insert (no span
/// remapping needed — the client applies them at the cursor).
pub fn completion_item_to_lsp(item: &CoreCompletionItem) -> LspCompletionItem {
    LspCompletionItem {
        label: item.label.clone(),
        kind: Some(completion_kind_to_lsp(item.kind)),
        detail: item.detail.clone(),
        ..LspCompletionItem::default()
    }
}

/// Convert every core completion item into its LSP form, preserving order.
pub fn completions_to_lsp(items: &[CoreCompletionItem]) -> Vec<LspCompletionItem> {
    items.iter().map(completion_item_to_lsp).collect()
}

/// Build a single whole-document [`TextEdit`] replacing all of `text` (from the start to its end
/// [`Position`] under `encoding`) with `new_text` — the shape `textDocument/formatting` returns
/// (spec §8). A full-document replace is the simplest safe edit; a minimal-diff edit set is out of
/// scope.
pub fn full_document_edit(text: &str, new_text: String, encoding: PositionEncoding) -> TextEdit {
    let index = LineIndex::new(text);
    let end = index.position(text.len(), encoding);
    TextEdit {
        range: Range {
            start: Position::new(0, 0),
            end,
        },
        new_text,
    }
}

/// Map an engine [`Rgba`] (channels in `[0, 1]`) onto an LSP [`Color`] (also `[0, 1]` f32 channels)
/// — a straight field copy, since both use the same normalized representation.
pub fn color_to_lsp(rgba: Rgba) -> Color {
    Color {
        red: rgba.r,
        green: rgba.g,
        blue: rgba.b,
        alpha: rgba.a,
    }
}

/// Convert one engine color occurrence — a `(byte span, [`Rgba`])` pair from
/// [`document_colors`](otui_core::OtuiService::document_colors) — into an LSP [`ColorInformation`],
/// resolving the span against `index` under `encoding`. This is the byte-offset → protocol seam for
/// `textDocument/documentColor`.
pub fn color_information_of(
    span: ByteSpan,
    rgba: Rgba,
    index: &LineIndex<'_>,
    encoding: PositionEncoding,
) -> ColorInformation {
    ColorInformation {
        range: index.range(span.start, span.end, encoding),
        color: color_to_lsp(rgba),
    }
}

/// Convert every engine color occurrence for `text` into an LSP [`ColorInformation`], building one
/// shared [`LineIndex`] for the batch.
pub fn colors_to_lsp(
    text: &str,
    colors: &[(ByteSpan, Rgba)],
    encoding: PositionEncoding,
) -> Vec<ColorInformation> {
    let index = LineIndex::new(text);
    colors
        .iter()
        .map(|(span, rgba)| color_information_of(*span, *rgba, &index, encoding))
        .collect()
}

/// Map a protocol-agnostic [`CoreFoldKind`] onto its LSP `FoldingRangeKind`. A widget block or
/// block-scalar body is a `Region`; a run of comments is a `Comment`.
fn fold_kind_to_lsp(kind: CoreFoldKind) -> FoldingRangeKind {
    match kind {
        CoreFoldKind::Region => FoldingRangeKind::Region,
        CoreFoldKind::Comment => FoldingRangeKind::Comment,
    }
}

/// Convert a single core [`FoldRange`](CoreFoldRange) into an `lsp_types::FoldingRange`. The line
/// numbers are already 0-based and carried verbatim (no character offsets — this server folds whole
/// lines); the kind is mapped through [`fold_kind_to_lsp`].
pub fn fold_to_lsp(fold: &CoreFoldRange) -> FoldingRange {
    FoldingRange {
        start_line: fold.start_line,
        end_line: fold.end_line,
        kind: Some(fold_kind_to_lsp(fold.kind)),
        ..FoldingRange::default()
    }
}

/// Convert every core fold range into its LSP form, preserving order.
pub fn folds_to_lsp(folds: &[CoreFoldRange]) -> Vec<FoldingRange> {
    folds.iter().map(fold_to_lsp).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lang_api::{ByteSpan, CompletionKind as CoreCompletionKind, LanguageService};
    use otui_core::OtuiService;
    use tower_lsp::lsp_types::Position;

    #[test]
    fn maps_tab_indentation_diagnostic_from_the_engine() {
        // A tab-indented document: the engine flags the tab as a parse-level error.
        let text = "Panel\n\tid: main\n";
        let diags = OtuiService::new().diagnostics(text);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "tab-indentation");

        let lsp = all_to_lsp(text, &diags, PositionEncoding::Utf16);
        assert_eq!(lsp.len(), 1);
        let d = &lsp[0];
        // The tab is the first char of line 1, spanning one column.
        assert_eq!(d.range.start, Position::new(1, 0));
        assert_eq!(d.range.end, Position::new(1, 1));
        assert_eq!(d.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            d.code,
            Some(NumberOrString::String("tab-indentation".to_owned()))
        );
        assert_eq!(d.source.as_deref(), Some("otui"));
    }

    #[test]
    fn maps_severity_and_code_directly() {
        let text = "abc";
        let diag = CoreDiagnostic {
            severity: Severity::Warning,
            code: "some-code",
            message: "a warning".to_owned(),
            span: ByteSpan::new(0, 3),
        };
        let index = LineIndex::new(text);
        let lsp = to_lsp(&diag, &index, PositionEncoding::Utf16);
        assert_eq!(lsp.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(
            lsp.code,
            Some(NumberOrString::String("some-code".to_owned()))
        );
        assert_eq!(lsp.message, "a warning");
    }

    #[test]
    fn maps_symbol_kinds_to_lsp() {
        assert_eq!(
            symbol_kind_to_lsp(CoreSymbolKind::Object),
            LspSymbolKind::OBJECT
        );
        assert_eq!(
            symbol_kind_to_lsp(CoreSymbolKind::Field),
            LspSymbolKind::FIELD
        );
    }

    #[test]
    fn converts_nested_symbol_tree_with_ranges_and_kinds() {
        use tower_lsp::lsp_types::Position;

        // Text laid out so both spans are exercised:
        //   line 0: "Panel"
        //   line 1: "  id: root"
        //   line 2: "  Label"
        let text = "Panel\n  id: root\n  Label\n";
        let root_span = ByteSpan::new(0, text.len());
        // "root" occupies bytes 12..16 on line 1.
        let root_selection = ByteSpan::new(12, 16);
        // "  Label" child: the "Label" tag is bytes 19..24 on line 2.
        let label_span = ByteSpan::new(17, text.len());
        let label_selection = ByteSpan::new(19, 24);

        let tree = CoreSymbol {
            name: "root".to_owned(),
            detail: Some("Panel".to_owned()),
            kind: CoreSymbolKind::Field,
            span: root_span,
            selection_span: root_selection,
            children: vec![CoreSymbol {
                name: "Label".to_owned(),
                detail: Some("Label".to_owned()),
                kind: CoreSymbolKind::Object,
                span: label_span,
                selection_span: label_selection,
                children: Vec::new(),
            }],
        };

        let lsp = symbols_to_lsp(text, std::slice::from_ref(&tree), PositionEncoding::Utf16);
        assert_eq!(lsp.len(), 1);
        let root = &lsp[0];
        assert_eq!(root.name, "root");
        assert_eq!(root.detail.as_deref(), Some("Panel"));
        assert_eq!(root.kind, LspSymbolKind::FIELD);
        assert_eq!(root.range.start, Position::new(0, 0));
        assert_eq!(root.selection_range.start, Position::new(1, 6));
        assert_eq!(root.selection_range.end, Position::new(1, 10));

        // The nested child is carried across recursively with its own ranges/kind.
        let children = root.children.as_ref().expect("root has children");
        assert_eq!(children.len(), 1);
        let label = &children[0];
        assert_eq!(label.name, "Label");
        assert_eq!(label.kind, LspSymbolKind::OBJECT);
        assert_eq!(label.selection_range.start, Position::new(2, 2));
        assert_eq!(label.selection_range.end, Position::new(2, 7));
        // A leaf's children collapse to `None`, not an empty vec.
        assert!(label.children.is_none());
    }

    #[test]
    fn maps_completion_kinds_to_lsp() {
        assert_eq!(
            completion_kind_to_lsp(CoreCompletionKind::EnumMember),
            LspCompletionItemKind::ENUM_MEMBER
        );
        assert_eq!(
            completion_kind_to_lsp(CoreCompletionKind::Value),
            LspCompletionItemKind::VALUE
        );
        assert_eq!(
            completion_kind_to_lsp(CoreCompletionKind::Event),
            LspCompletionItemKind::EVENT
        );
        assert_eq!(
            completion_kind_to_lsp(CoreCompletionKind::Keyword),
            LspCompletionItemKind::KEYWORD
        );
    }

    #[test]
    fn completion_item_carries_label_kind_and_detail() {
        let core = CoreCompletionItem {
            label: "hover".to_owned(),
            kind: CoreCompletionKind::EnumMember,
            detail: Some("state".to_owned()),
        };
        let lsp = completion_item_to_lsp(&core);
        assert_eq!(lsp.label, "hover");
        assert_eq!(lsp.kind, Some(LspCompletionItemKind::ENUM_MEMBER));
        assert_eq!(lsp.detail.as_deref(), Some("state"));
    }

    #[test]
    fn maps_a_byte_span_edit_to_an_lsp_text_edit() {
        // A tab on line 1 (bytes 6..7) replaced with two spaces.
        let text = "Panel\n\tid: main\n";
        let index = LineIndex::new(text);
        let edit = text_edit_of(ByteSpan::new(6, 7), "  ", &index, PositionEncoding::Utf16);
        assert_eq!(edit.range.start, Position::new(1, 0));
        assert_eq!(edit.range.end, Position::new(1, 1));
        assert_eq!(edit.new_text, "  ");
    }

    #[test]
    fn text_edit_span_counts_utf16_units() {
        // "café" — 'é' is one UTF-16 unit; a span ending after it maps to column 4, not byte 5.
        let text = "café";
        let index = LineIndex::new(text);
        let edit = text_edit_of(ByteSpan::new(0, 5), "x", &index, PositionEncoding::Utf16);
        assert_eq!(edit.range.start, Position::new(0, 0));
        assert_eq!(edit.range.end, Position::new(0, 4));
        assert_eq!(edit.new_text, "x");
    }

    #[test]
    fn end_to_end_from_cursor_position_to_completion_items() {
        // Position → byte offset → engine completion → LSP items, the same path the handler drives.
        let text = "Button\n  $\n";
        // Cursor just past the `$` on line 1 (column 3).
        let position = Position::new(1, 3);
        let offset = LineIndex::new(text).offset_at(position, PositionEncoding::Utf16);
        let core = OtuiService::new().complete_at(text, offset);
        let lsp = completions_to_lsp(&core);
        // Every state name comes back, as ENUM_MEMBER, in schema order.
        let labels: Vec<&str> = lsp.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, otui_core::schema::STATES);
        assert!(lsp
            .iter()
            .all(|i| i.kind == Some(LspCompletionItemKind::ENUM_MEMBER)));
    }

    #[test]
    fn full_document_edit_replaces_the_whole_range_with_formatted_text() {
        // A formattable document yields one edit spanning [start .. document end] carrying the
        // formatted text. The over-indented `id` line is normalized by the engine's formatter.
        let text = "Panel\n    id: main\n";
        let formatted = OtuiService::new().format(text).expect("formats cleanly");
        let edit = full_document_edit(text, formatted, PositionEncoding::Utf16);
        assert_eq!(edit.range.start, Position::new(0, 0));
        // The document has two lines terminated by `\n`, so its end is the start of line 2.
        assert_eq!(edit.range.end, Position::new(2, 0));
        assert_eq!(edit.new_text, "Panel\n  id: main\n");
    }

    #[test]
    fn unparsable_document_produces_no_format_edit() {
        // The `formatting` handler's safety gate: a document with a parse error (here an
        // unterminated inline array → `ERROR` node) yields `None` from the engine, so the server
        // returns no edits rather than a spurious whole-document replace.
        assert!(OtuiService::new().format("x: [a, b\n").is_none());
    }

    #[test]
    fn maps_rgba_to_lsp_color_channelwise() {
        let rgba = Rgba {
            r: 1.0,
            g: 0.5,
            b: 0.0,
            a: 0.25,
        };
        let color = color_to_lsp(rgba);
        assert!((color.red - 1.0).abs() < f32::EPSILON);
        assert!((color.green - 0.5).abs() < f32::EPSILON);
        assert!((color.blue - 0.0).abs() < f32::EPSILON);
        assert!((color.alpha - 0.25).abs() < f32::EPSILON);
    }

    #[test]
    fn maps_a_color_occurrence_span_to_a_range() {
        // `#ff0000` sits on line 1, bytes 9..16 of "Panel\n  color: #ff0000".
        let text = "Panel\n  color: #ff0000\n";
        let start = text.find('#').unwrap();
        let span = ByteSpan::new(start, start + "#ff0000".len());
        let rgba = Rgba {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        };
        let index = LineIndex::new(text);
        let info = color_information_of(span, rgba, &index, PositionEncoding::Utf16);
        assert_eq!(info.range.start, Position::new(1, 9));
        assert_eq!(info.range.end, Position::new(1, 16));
        assert!((info.color.red - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn document_color_end_to_end_over_a_small_doc() {
        // Position → engine document_colors → LSP ColorInformation, the path the handler drives.
        // Only context-free `color` literals (hex + functional) are scanned; a bare named color is
        // deliberately not swatched (see otui_core::colors), so this doc uses literals.
        let text = "Panel\n  color: #00ff00\n  background-color: rgb(255, 0, 0)\n";
        let core = OtuiService::new().document_colors(text);
        let lsp = colors_to_lsp(text, &core, PositionEncoding::Utf16);
        assert_eq!(lsp.len(), 2);
        // The hex green on line 1.
        assert_eq!(lsp[0].range.start, Position::new(1, 9));
        assert!((lsp[0].color.green - 1.0).abs() < f32::EPSILON);
        // The functional red on line 2.
        assert!((lsp[1].color.red - 1.0).abs() < f32::EPSILON);
        assert!((lsp[1].color.green - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn maps_fold_range_kinds_and_lines_to_lsp() {
        // A structural region maps to `Region`, carrying its 0-based lines verbatim.
        let region = CoreFoldRange {
            start_line: 0,
            end_line: 3,
            kind: CoreFoldKind::Region,
        };
        let lsp = fold_to_lsp(&region);
        assert_eq!(lsp.start_line, 0);
        assert_eq!(lsp.end_line, 3);
        assert_eq!(lsp.kind, Some(FoldingRangeKind::Region));
        // Whole-line folds carry no character offsets.
        assert!(lsp.start_character.is_none());
        assert!(lsp.end_character.is_none());

        // A comment run maps to `Comment`.
        let comment = CoreFoldRange {
            start_line: 5,
            end_line: 7,
            kind: CoreFoldKind::Comment,
        };
        let lsp = fold_to_lsp(&comment);
        assert_eq!(lsp.start_line, 5);
        assert_eq!(lsp.end_line, 7);
        assert_eq!(lsp.kind, Some(FoldingRangeKind::Comment));
    }

    #[test]
    fn converts_a_batch_of_fold_ranges_preserving_order() {
        let folds = [
            CoreFoldRange {
                start_line: 0,
                end_line: 4,
                kind: CoreFoldKind::Region,
            },
            CoreFoldRange {
                start_line: 2,
                end_line: 4,
                kind: CoreFoldKind::Region,
            },
        ];
        let lsp = folds_to_lsp(&folds);
        assert_eq!(lsp.len(), 2);
        assert_eq!(lsp[0].start_line, 0);
        assert_eq!(lsp[1].start_line, 2);
    }

    #[test]
    #[allow(deprecated)] // reading `SymbolInformation`'s fields in assertions
    fn flattens_symbol_tree_with_container_names() {
        let text = "Panel\n  id: root\n  Label\n";
        let root_span = ByteSpan::new(0, text.len());
        let root_selection = ByteSpan::new(12, 16);
        let label_span = ByteSpan::new(17, text.len());
        let label_selection = ByteSpan::new(19, 24);

        let tree = CoreSymbol {
            name: "root".to_owned(),
            detail: Some("Panel".to_owned()),
            kind: CoreSymbolKind::Field,
            span: root_span,
            selection_span: root_selection,
            children: vec![CoreSymbol {
                name: "Label".to_owned(),
                detail: Some("Label".to_owned()),
                kind: CoreSymbolKind::Object,
                span: label_span,
                selection_span: label_selection,
                children: Vec::new(),
            }],
        };

        let uri = Url::parse("file:///x.otui").unwrap();
        let flat = symbols_to_flat(
            &uri,
            text,
            std::slice::from_ref(&tree),
            PositionEncoding::Utf16,
        );

        // Both the parent and its nested child are emitted, depth-first.
        assert_eq!(flat.len(), 2);
        let root = &flat[0];
        assert_eq!(root.name, "root");
        assert_eq!(root.kind, LspSymbolKind::FIELD);
        assert_eq!(root.location.uri, uri);
        // The flat location uses the symbol's full span, starting at line 0.
        assert_eq!(root.location.range.start, Position::new(0, 0));
        // A top-level symbol has no container.
        assert!(root.container_name.is_none());

        let label = &flat[1];
        assert_eq!(label.name, "Label");
        assert_eq!(label.kind, LspSymbolKind::OBJECT);
        // The child carries its parent's name as the container.
        assert_eq!(label.container_name.as_deref(), Some("root"));
        // The flat location uses the full span, which begins at the widget's indentation (col 0).
        assert_eq!(label.location.range.start, Position::new(2, 0));
    }
}
