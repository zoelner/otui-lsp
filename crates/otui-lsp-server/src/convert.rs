//! Mapping `lang_api` diagnostics into `lsp_types` diagnostics.
//!
//! This is the only place the byte-offset engine output meets the LSP wire format. Severity,
//! code and span are translated; `source` is stamped as `"otui"` so clients can group findings.

use lang_api::{Diagnostic as CoreDiagnostic, Severity};
use tower_lsp::lsp_types::{
    Diagnostic as LspDiagnostic, DiagnosticSeverity, NumberOrString, Range,
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
}
