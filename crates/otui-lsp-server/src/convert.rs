//! Mapping `lang_api` diagnostics into `lsp_types` diagnostics.
//!
//! This is the only place the byte-offset engine output meets the LSP wire format. Severity,
//! code and span are translated; `source` is stamped as `"otui"` so clients can group findings.

use lang_api::{
    Diagnostic as CoreDiagnostic, DocumentSymbol as CoreSymbol, Severity,
    SymbolKind as CoreSymbolKind,
};
use tower_lsp::lsp_types::{
    Diagnostic as LspDiagnostic, DiagnosticSeverity, DocumentSymbol as LspSymbol, NumberOrString,
    Range, SymbolKind as LspSymbolKind,
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

#[cfg(test)]
mod tests {
    use super::*;
    use lang_api::{ByteSpan, LanguageService};
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
}
