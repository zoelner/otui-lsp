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

pub mod catalog;
pub mod completion;
pub mod diagnostics;
pub mod fixes;
pub mod folding;
pub mod format;
pub mod hover;
pub mod navigation;
pub mod references;
pub mod schema;
pub mod semantic;
pub mod style_index;
pub mod symbols;
pub mod syntax;

use fixes::Fix;
use hover::StyleHover;
use lang_api::{
    ByteSpan, CompletionItem, Diagnostic, DocumentSymbol, LanguageService, SemanticToken,
};
use navigation::{BaseRef, IdRef, StyleHeaderRef};
use references::{IdOccurrences, StyleNameOccurrences};
use style_index::{StyleDef, StyleIndex};
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

    /// Locate the `id:` value or anchor-target id under `offset`, if any (spec §5.4 references).
    ///
    /// Returns the id text + span when the cursor sits on an `id:` value (a declaration) or on the
    /// `id` prefix of an `<id>.edge` anchor target (a reference); `None` otherwise. Collecting the
    /// id's occurrences (document-local) is the server's job via [`id_occurrences`](Self::id_occurrences).
    ///
    /// Inherent (not on the [`LanguageService`] trait) for the same reason as
    /// [`base_reference_at`](Self::base_reference_at): navigation is driven by server-owned state.
    #[must_use]
    pub fn id_at(&self, source: &str, offset: usize) -> Option<IdRef> {
        navigation::id_at(source, offset)
    }

    /// Find every occurrence of the style name `name` in one document (spec §5.4): the top-level
    /// `name < …` declarations and the `X < name` base references. The server calls this per open
    /// document (the style namespace is global) and maps the spans to `Location`s, honoring the
    /// request's `context.include_declaration` for the declaration spans.
    ///
    /// Inherent (not on the [`LanguageService`] trait), mirroring [`style_defs`](Self::style_defs):
    /// the multi-document fan-out is the server's concern.
    #[must_use]
    pub fn style_name_occurrences(&self, source: &str, name: &str) -> StyleNameOccurrences {
        references::style_name_occurrences(source, name)
    }

    /// Find every occurrence of the id `id` in one document (spec §5.4): the `id:` declaration and the
    /// `<id>.edge` anchor references. Ids are per-widget-tree identities that can repeat across files,
    /// so this is deliberately document-local — the server calls it on the current document only.
    ///
    /// Inherent (not on the [`LanguageService`] trait), mirroring
    /// [`style_name_occurrences`](Self::style_name_occurrences).
    #[must_use]
    pub fn id_occurrences(&self, source: &str, id: &str) -> IdOccurrences {
        references::id_occurrences(source, id)
    }

    /// Describe the hover for the style token under `offset`, resolved against the workspace `index`
    /// (spec §5.5). Returns a structured [`StyleHover`] — native vs. user base, workspace-resolution,
    /// definition count and inheritance are all decided here in the engine — or `None` when the cursor
    /// is not on a top-level style header's name or base token. The server only formats the result
    /// into an LSP hover.
    ///
    /// Inherent (not on the [`LanguageService`] trait) because it consumes server-owned state (the
    /// workspace [`StyleIndex`]).
    #[must_use]
    pub fn style_hover_at(
        &self,
        source: &str,
        offset: usize,
        index: &StyleIndex,
    ) -> Option<StyleHover> {
        hover::style_hover_at(source, offset, index)
    }

    /// Compute completion candidates for the cursor at byte `offset` (spec §6). Returns the OTML
    /// **closed set** that applies — `$state` names, `anchors.<edge>` edges, magic anchor targets,
    /// or `@event` names — or an empty vec when the cursor is not in one of those contexts. Property
    /// names and colors are deliberately not offered (that catalog is a later node); see
    /// [`completion`].
    ///
    /// Inherent (not on the [`LanguageService`] trait) so the protocol-agnostic trait stays minimal
    /// and mirrors [`base_reference_at`](Self::base_reference_at) / [`style_header_at`](Self::style_header_at).
    #[must_use]
    pub fn complete_at(&self, source: &str, offset: usize) -> Vec<CompletionItem> {
        completion::complete_at(source, offset)
    }

    /// Compute the quick-fixes offered for the byte `range` in `source` (spec §7). Recomputes the
    /// parse-level diagnostics internally and derives a conservative correction for each fixable
    /// finding that overlaps `range` — tabs→spaces and indentation rounding for the indentation
    /// codes, and "did you mean" suggestions (bounded edit distance) for the unknown
    /// property/state/anchor-edge and invalid `display`/`layout` value codes. Returns an empty vec
    /// when nothing in `range` is fixable.
    ///
    /// Inherent (not on the [`LanguageService`] trait) so the protocol-agnostic trait stays minimal,
    /// mirroring [`complete_at`](Self::complete_at); the server maps each [`Fix`] onto an
    /// `lsp_types::CodeAction`.
    #[must_use]
    pub fn quick_fixes(&self, source: &str, range: ByteSpan) -> Vec<Fix> {
        fixes::quick_fixes(source, range)
    }

    /// Format the whole document (spec §8): return the canonical, whitespace-normalized text, or
    /// [`None`] when the source does not parse cleanly (parse failure, or any `ERROR` / `MISSING`
    /// node) — in which case the server returns no edits and the document is left untouched.
    ///
    /// The formatter is conservative and byte-oriented: it re-indents structural lines to
    /// `2 * depth` spaces (depth from the parse tree), collapses `key: value` spacing to a single
    /// space after the first colon, strips trailing whitespace, ensures one final newline, and
    /// leaves block-scalar bodies and multi-line value continuations verbatim. See [`format`] for
    /// the full contract.
    ///
    /// Inherent (not on the [`LanguageService`] trait) so the protocol-agnostic trait stays minimal,
    /// mirroring [`style_defs`](Self::style_defs).
    #[must_use]
    pub fn format(&self, source: &str) -> Option<String> {
        format::format(source)
    }

    /// Compute the folding ranges for `source` (spec §2): one collapsible region per multi-line
    /// widget block (`container` / `style_header`) and per multi-line block-scalar body, plus one
    /// per run of consecutive full-line comments. Line numbers are 0-based; a single-line construct
    /// yields no fold. Returns an empty vec when the source cannot be parsed.
    ///
    /// Inherent (not on the [`LanguageService`] trait) so the protocol-agnostic trait stays minimal,
    /// mirroring [`format`](Self::format); the server maps each [`FoldRange`](folding::FoldRange)
    /// onto an `lsp_types::FoldingRange`.
    #[must_use]
    pub fn folding_ranges(&self, source: &str) -> Vec<folding::FoldRange> {
        folding::folding_ranges(source)
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
