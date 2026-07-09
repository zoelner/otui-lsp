//! The pure OTUI/OTML language engine.
//!
//! `otui-core` holds all language semantics — parsing, diagnostics, symbols, completion and
//! formatting — with **no I/O and no dependency on `lsp-types`**. Everything it returns is
//! expressed in byte offsets via the [`lang-api`] contract, so the same engine can back the LSP
//! server or be embedded directly in an editor.
//!
//! Behavior is a faithful mirror of the real OTClient engine, per the spec vendored at
//! `docs/otui-language-service-spec.md`. Milestones fill in the submodules
//! (`syntax`, `schema`, `index`, `diagnostics`, `completion`, `symbols`, `format`); this slice
//! wires the [`LanguageService`] entry point to the parse-level [`diagnostics`] pass over the
//! tree-sitter [`syntax`] substrate.

pub mod diagnostics;
pub mod navigation;
pub mod semantic;
pub mod style_index;
pub mod symbols;
pub mod syntax;

use lang_api::{Diagnostic, DocumentSymbol, LanguageService, SemanticToken};
use navigation::{BaseRef, StyleHeaderRef};
use style_index::StyleDef;
use syntax::SyntaxTree;

/// The OTUI language backend. Constructed once per workspace/session.
#[derive(Debug, Default)]
pub struct OtuiService {
    _private: (),
}

impl OtuiService {
    pub fn new() -> Self {
        Self::default()
    }

    /// Extract the top-level `Name < Base` style definitions declared in a single document
    /// (spec §5.2). This is the per-document half of the workspace style index: the server calls
    /// it on change and feeds the result into a [`style_index::StyleIndex`] keyed by URI. Returns
    /// an empty vector if the source cannot be parsed.
    ///
    /// Kept as an inherent method (not on the [`LanguageService`] trait): the multi-document index
    /// is owned by the server, and the protocol-agnostic trait stays minimal.
    #[must_use]
    pub fn style_defs(&self, source: &str) -> Vec<StyleDef> {
        SyntaxTree::parse(source)
            .map(|tree| style_index::extract_style_defs(&tree))
            .unwrap_or_default()
    }

    /// Locate the top-level `Name < Base` base reference under `offset`, if any (spec §5.3).
    ///
    /// Returns the base token's name + span when the cursor sits on the `Base` of a top-level
    /// inheritance header; `None` otherwise (including when the cursor is on the declared name, a
    /// property, or a nested widget). Resolving the returned name against the workspace
    /// [`style_index::StyleIndex`] — and dropping native `UI*` bases — is the server's job.
    ///
    /// Inherent (not on the [`LanguageService`] trait) for the same reason as
    /// [`style_defs`](Self::style_defs): navigation is driven by server-owned state.
    #[must_use]
    pub fn base_reference_at(&self, source: &str, offset: usize) -> Option<BaseRef> {
        navigation::base_reference_at(source, offset)
    }

    /// Locate the top-level `Name < Base` header under `offset`, if the cursor sits on the declared
    /// name token or the base token (spec §5.5 hover). Returns the whole header descriptor so the
    /// server can tell which part was hovered by comparing `offset` to the returned spans; `None`
    /// for nested widgets, property values, or non-header positions.
    ///
    /// Inherent (not on the [`LanguageService`] trait) for the same reason as
    /// [`base_reference_at`](Self::base_reference_at): rendering the hover consumes server-owned
    /// state (the workspace [`style_index::StyleIndex`]).
    #[must_use]
    pub fn style_header_at(&self, source: &str, offset: usize) -> Option<StyleHeaderRef> {
        navigation::style_header_at(source, offset)
    }
}

impl LanguageService for OtuiService {
    fn language_id(&self) -> &'static str {
        "otui"
    }

    fn diagnostics(&self, source: &str) -> Vec<Diagnostic> {
        // Parse-level category of spec §4: indentation faults plus structural parse errors.
        diagnostics::analyze(source)
    }

    fn semantic_tokens(&self, source: &str) -> Vec<SemanticToken> {
        // Leaf-level highlight over the CST (spec §3 token taxonomy).
        semantic::tokens(source)
    }

    fn document_symbols(&self, source: &str) -> Vec<DocumentSymbol> {
        // Widget-hierarchy outline over the CST (spec §5.1).
        symbols::document_symbols(source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_reports_its_language_id() {
        let svc = OtuiService::new();
        assert_eq!(svc.language_id(), "otui");
    }

    #[test]
    fn clean_source_produces_no_diagnostics() {
        let svc = OtuiService::new();
        assert!(svc
            .diagnostics("MainWindow < UIWindow\n  id: main\n")
            .is_empty());
    }

    #[test]
    fn service_surfaces_parse_level_diagnostics() {
        let svc = OtuiService::new();
        let diags = svc.diagnostics("Panel\n\tid: main\n");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "tab-indentation");
    }
}
