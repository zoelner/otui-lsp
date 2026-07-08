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

/// The contract every language backend implements. Kept intentionally minimal for now; symbols,
/// completion and hover are added as the corresponding milestones land.
pub trait LanguageService {
    /// A stable identifier for the language this backend serves (e.g. `"otui"`).
    fn language_id(&self) -> &'static str;

    /// Compute diagnostics for a full source document.
    fn diagnostics(&self, source: &str) -> Vec<Diagnostic>;
}

#[cfg(test)]
mod tests {
    use super::*;

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
