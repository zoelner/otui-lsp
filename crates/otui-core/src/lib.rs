//! The pure OTUI/OTML language engine.
//!
//! `otui-core` holds all language semantics — parsing, diagnostics, symbols, completion and
//! formatting — with **no I/O and no dependency on `lsp-types`**. Everything it returns is
//! expressed in byte offsets via the [`lang-api`] contract, so the same engine can back the LSP
//! server or be embedded directly in an editor.
//!
//! Behavior is a faithful mirror of the real OTClient engine (opentibiabr), per the spec vendored at
//! `docs/otui-language-service-spec.md`. The [`syntax`] tree-sitter substrate underpins every
//! feature module: [`diagnostics`], [`completion`], [`hover`], [`property_hover`], [`id_hover`],
//! [`symbols`], [`navigation`], [`references`], [`hierarchy`], [`format`], [`indent`], [`folding`], [`semantic`],
//! [`colors`], [`links`], [`fixes`], [`lenses`], [`inlay`], plus the workspace-index building blocks
//! ([`style_index`], [`lua_widgets`], [`lua_refs`], [`lua_ui_loads`], [`otmod`], [`widget_resolve`],
//! [`ids`]), the module-manifest-flavored diagnostics pass ([`manifest`]), and the engine data
//! ([`schema`], [`catalog`]).
//! The [`LanguageService`] trait
//! and the inherent [`OtuiService`] methods below are the entry points the server drives.

pub mod catalog;
pub mod colors;
pub mod completion;
pub mod diagnostics;
pub mod fixes;
pub mod folding;
pub mod format;
pub mod hierarchy;
pub mod hover;
pub mod id_hover;
pub mod ids;
pub mod indent;
pub mod inlay;
pub mod lenses;
pub mod links;
pub mod lua_refs;
pub mod lua_ui_loads;
pub mod lua_widgets;
pub mod manifest;
pub mod navigation;
mod otml_reparent;
pub mod otmod;
pub mod property_hover;
pub mod references;
pub mod schema;
pub mod semantic;
pub mod style_index;
pub mod symbols;
pub mod syntax;
pub mod widget_resolve;

use fixes::Fix;
use hierarchy::StyleRef;
use hover::StyleHover;
use lang_api::{
    ByteSpan, CompletionItem, Diagnostic, DocumentSymbol, LanguageService, SemanticToken,
};
use lua_refs::{LuaIdDef, LuaIdRef};
use lua_widgets::{LuaWidgetDef, LuaWidgetIndex};
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

    /// Extract the widgets a single **Lua** module declares — their custom style properties and
    /// `extends` parent (see [`lua_widgets`]). The per-file half of the workspace Lua widget index:
    /// the server calls it on each `*.lua` it scans and feeds the result into a
    /// [`lua_widgets::LuaWidgetIndex`] keyed by URI, exactly as [`style_defs`](Self::style_defs)
    /// feeds the [`StyleIndex`].
    ///
    /// Inherent (not on the [`LanguageService`] trait): the multi-document index is server-owned.
    #[must_use]
    pub fn lua_widgets(&self, source: &str) -> Vec<LuaWidgetDef> {
        lua_widgets::scan_widgets(source)
    }

    /// Find every place a single **Lua** module refers to a widget `id:` (spec §2.3): the two
    /// `getChildById`/`recursiveGetChildById` call forms and the best-effort `.ui.<id>` dot-chain
    /// form (see [`lua_refs`]). The per-file half of the workspace Lua-refs index: the server calls
    /// it on each `*.lua` it scans and feeds the result into a [`lua_refs::LuaRefIndex`] keyed by
    /// URI, exactly as [`lua_widgets`](Self::lua_widgets) feeds the [`LuaWidgetIndex`].
    ///
    /// Inherent (not on the [`LanguageService`] trait): the multi-document index is server-owned.
    #[must_use]
    pub fn lua_id_refs(&self, source: &str) -> Vec<LuaIdRef> {
        lua_refs::scan_id_refs(source)
    }

    /// Find every place a single **Lua** module defines a widget id at runtime via
    /// `setId("literal")` (spec §2.3) — a widget that may never appear in any `.otui` file, so Lua
    /// is its only definition site. Pure text scan; unlike [`lua_id_refs`](Self::lua_id_refs) there
    /// is (yet) no workspace index for defs — the server can call this directly per file.
    #[must_use]
    pub fn lua_id_defs(&self, source: &str) -> Vec<LuaIdDef> {
        lua_refs::scan_id_defs(source)
    }

    /// Compute parse-level diagnostics for `source`, **widget-aware**: a property unknown to the C++
    /// catalog is not flagged when the enclosing widget's resolved ancestry (across the workspace
    /// `styles` and `lua` indexes) declares it as a Lua-added style property (see
    /// [`diagnostics::analyze_with_widgets`]). With empty indexes this is identical to the
    /// [`LanguageService::diagnostics`] catalog-only pass.
    ///
    /// Inherent (not on the [`LanguageService`] trait) because it consumes server-owned workspace
    /// state, mirroring [`style_hover_at`](Self::style_hover_at).
    #[must_use]
    pub fn diagnostics_with_widgets(
        &self,
        source: &str,
        styles: &StyleIndex,
        lua: &LuaWidgetIndex,
    ) -> Vec<Diagnostic> {
        diagnostics::analyze_with_widgets(source, &diagnostics::WidgetContext { styles, lua })
    }

    /// Like [`diagnostics_with_widgets`](Self::diagnostics_with_widgets), but also returns the
    /// document's asset-path links ([`document_links`](Self::document_links)'s data), parsing
    /// `source` exactly **once** and sharing the resulting tree between both passes.
    ///
    /// Exists for a caller (the server's missing-asset diagnostic) that would otherwise need both
    /// results for the same document on every keystroke: calling
    /// [`diagnostics_with_widgets`](Self::diagnostics_with_widgets) and
    /// [`document_links`](Self::document_links) separately parses `source` twice for one request.
    ///
    /// Inherent (not on the [`LanguageService`] trait) for the same reason as
    /// [`diagnostics_with_widgets`](Self::diagnostics_with_widgets): it consumes server-owned
    /// workspace state.
    #[must_use]
    pub fn diagnostics_with_widgets_and_links(
        &self,
        source: &str,
        styles: &StyleIndex,
        lua: &LuaWidgetIndex,
    ) -> (Vec<Diagnostic>, Vec<links::PathRef>) {
        let tree = SyntaxTree::parse(source);
        let diags = diagnostics::analyze_with_widgets_from_tree(
            source,
            tree.as_ref(),
            &diagnostics::WidgetContext { styles, lua },
        );
        let asset_links = tree.as_ref().map_or_else(Vec::new, |tree| {
            links::document_links_from_tree(source, tree)
        });
        (diags, asset_links)
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
    /// definition count and the **full** resolved inheritance chain (down to the native class it
    /// reaches, if any — see [`hover::Inheritance`]) are all decided here in the engine — or `None`
    /// when the cursor is not on a top-level style header's name or base token. The server only
    /// formats the result into an LSP hover.
    ///
    /// Inherent (not on the [`LanguageService`] trait) because it consumes server-owned state (the
    /// workspace [`StyleIndex`] and [`LuaWidgetIndex`]).
    #[must_use]
    pub fn style_hover_at(
        &self,
        source: &str,
        offset: usize,
        index: &StyleIndex,
        lua: &LuaWidgetIndex,
    ) -> Option<StyleHover> {
        hover::style_hover_at(source, offset, index, lua)
    }

    /// Describe the **property key** under `offset` for hover (spec §5.5): what value the property
    /// expects (color / asset path / a fixed value set / border shorthand / a plain known property),
    /// derived from the catalog + schema metadata; or, when `name` is not a global catalog property,
    /// whether the enclosing widget's resolved ancestry (across the workspace `styles`/`lua` indexes)
    /// declares it as a per-widget property (mirroring [`completion`]'s widget-aware completion).
    /// `None` when the cursor is not on a property key that resolves either way. The server renders
    /// the returned [`property_hover::PropertyHover`] into Markdown; see [`property_hover`].
    ///
    /// Inherent (not on the [`LanguageService`] trait) because the widget-aware branch consumes
    /// server-owned state (the workspace [`StyleIndex`] and [`LuaWidgetIndex`]), mirroring
    /// [`style_hover_at`](Self::style_hover_at).
    #[must_use]
    pub fn property_hover_at(
        &self,
        source: &str,
        offset: usize,
        styles: &StyleIndex,
        lua: &LuaWidgetIndex,
    ) -> Option<property_hover::PropertyHover> {
        property_hover::property_hover_at(source, offset, styles, lua)
    }

    /// Locate the style name the symbol under `offset` resolves to for type navigation
    /// (`textDocument/typeDefinition` / `textDocument/implementation`): the tag of a widget instance
    /// (a `container`, at any depth) or the declared-name / base token of a top-level `Name < Base`
    /// header. Returns the name + its token span, or `None` off any such token. Native `UI*` names are
    /// returned as-is; the server decides they have no user declaration.
    ///
    /// This is only the cursor **locator**; resolving the name to declarations or derivations is the
    /// server's job, answered from the cached workspace [`style_index::StyleIndex`]
    /// ([`lookup`](style_index::StyleIndex::lookup) / [`subtypes`](style_index::StyleIndex::subtypes)),
    /// not by reparsing documents. Inherent (not on the [`LanguageService`] trait) like
    /// [`base_reference_at`](Self::base_reference_at).
    #[must_use]
    pub fn style_type_at(&self, source: &str, offset: usize) -> Option<StyleRef> {
        hierarchy::style_type_at(source, offset)
    }

    /// Compute completion candidates for the cursor at byte `offset` (spec §6). Returns the OTML
    /// **closed set** that applies — `$state` names, `anchors.<edge>` edges, magic anchor targets,
    /// `@event` names, or the global property-name catalog on an ordinary `key:` — or an empty vec
    /// when the cursor is not in one of those contexts. Property **values** and colors are
    /// deliberately not offered (that needs per-property type metadata); see [`completion`].
    ///
    /// This is the workspace-unaware form; [`complete_with_widgets`](Self::complete_with_widgets)
    /// also offers a widget's Lua-added properties. Inherent (not on the [`LanguageService`] trait)
    /// so the protocol-agnostic trait stays minimal.
    #[must_use]
    pub fn complete_at(&self, source: &str, offset: usize) -> Vec<CompletionItem> {
        completion::complete_at(source, offset)
    }

    /// Like [`complete_at`](Self::complete_at), but **widget-aware**: on an ordinary property key it
    /// also offers the custom style properties the enclosing widget adds in Lua (e.g. `column-style`
    /// under a `UITable`), resolved cross-file from the workspace `styles` + `lua` indexes. With empty
    /// indexes it is identical to [`complete_at`](Self::complete_at).
    ///
    /// Inherent (not on the [`LanguageService`] trait) because it consumes server-owned workspace
    /// state, mirroring [`diagnostics_with_widgets`](Self::diagnostics_with_widgets).
    #[must_use]
    pub fn complete_with_widgets(
        &self,
        source: &str,
        offset: usize,
        styles: &StyleIndex,
        lua: &LuaWidgetIndex,
    ) -> Vec<CompletionItem> {
        completion::complete_at_with_widgets(source, offset, styles, lua)
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

    /// Like [`quick_fixes`](Self::quick_fixes), but for a `.otmod`/`.otfont` manifest: restricted
    /// to the schema-agnostic structural fixes (tabs→spaces, odd-indentation rounding) instead of
    /// the widget-aware "did you mean" suggestions, which can misfire on a manifest's own keys —
    /// see [`fixes::structural_quick_fixes`]'s doc comment for why.
    #[must_use]
    pub fn quick_fixes_structural(&self, source: &str, range: ByteSpan) -> Vec<Fix> {
        fixes::structural_quick_fixes(source, range)
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

    /// Format `source` as a whole, then return only the [`LineEdit`](format::LineEdit)s for the
    /// lines in `[start_line, end_line]` (inclusive, 0-based) that actually changed — the primitive
    /// behind `textDocument/rangeFormatting`. Returns [`None`] on the same safety gate as
    /// [`format`](Self::format) (a document that does not parse cleanly is left untouched). See
    /// [`format::format_line_edits`] for how the whole-document format is scoped to the range.
    ///
    /// Inherent (not on the [`LanguageService`] trait), mirroring [`format`](Self::format).
    #[must_use]
    pub fn format_line_edits(
        &self,
        source: &str,
        start_line: u32,
        end_line: u32,
    ) -> Option<Vec<format::LineEdit>> {
        format::format_line_edits(source, start_line, end_line)
    }

    /// The number of leading spaces line `line` (0-based) should have, for
    /// `textDocument/onTypeFormatting` fired the instant Enter is pressed (spec §8). Unlike
    /// [`format`](Self::format) / [`format_line_edits`](Self::format_line_edits), this carries **no**
    /// parse-cleanliness gate: it is computed purely lexically from the preceding lines, so it keeps
    /// answering on exactly the mid-edit, broken document a fresh Enter always produces. Returns
    /// [`None`] when reindenting would be wrong or destructive — inside a block-scalar body, or when
    /// the line (or the line it would compute from) is tab-indented — in which case the server makes
    /// no edit. See [`indent::indent_for_line`] for the full rule.
    ///
    /// Inherent (not on the [`LanguageService`] trait) so the protocol-agnostic trait stays minimal,
    /// mirroring [`format_line_edits`](Self::format_line_edits).
    #[must_use]
    pub fn indent_for_line(&self, source: &str, line: u32) -> Option<usize> {
        indent::indent_for_line(source, line)
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

    /// Find every color value in `source` with its byte span and resolved [`Rgba`]
    /// (`textDocument/documentColor`, spec §2.9). Walks the CST for hex / functional / named color
    /// tokens; the server maps each `(span, rgba)` onto an `lsp_types::ColorInformation` (byte span →
    /// range, [`Rgba`] → `Color`). Returns an empty vec when the source cannot be parsed.
    ///
    /// Inherent (not on the [`LanguageService`] trait) so the protocol-agnostic trait stays minimal,
    /// mirroring [`folding_ranges`](Self::folding_ranges).
    #[must_use]
    pub fn document_colors(&self, source: &str) -> Vec<(ByteSpan, schema::Rgba)> {
        colors::document_colors(source)
    }

    /// Find every file-path-valued property value in `source` (`textDocument/documentLink`): for
    /// each `property` whose key is in [`schema::PATH_PROPERTIES`] (primarily `image-source`), the
    /// value token's byte span and the raw path string. The server resolves each path against the
    /// filesystem (relative to the current file's directory or the workspace root) and emits a
    /// [`DocumentLink`](links::PathRef) only when the target file exists. Returns an empty vec when
    /// the source cannot be parsed.
    ///
    /// Inherent (not on the [`LanguageService`] trait) so the protocol-agnostic trait stays minimal,
    /// mirroring [`document_colors`](Self::document_colors). Kept **pure** — the path→file
    /// resolution and the existence check are the server's I/O, deliberately not done here.
    #[must_use]
    pub fn document_links(&self, source: &str) -> Vec<links::PathRef> {
        links::document_links(source)
    }

    /// Point-locate the file-path-valued property value under `offset` (hover's sprite-preview
    /// case): `None` unless the cursor sits inside the trimmed path text of a `property` whose key
    /// is in [`schema::PATH_PROPERTIES`]. Complements [`document_links`](Self::document_links)'s
    /// bulk sweep with a single-cursor query.
    ///
    /// Inherent (not on the [`LanguageService`] trait) so the protocol-agnostic trait stays minimal,
    /// mirroring [`document_links`](Self::document_links). Kept **pure** — resolving the path against
    /// the filesystem is the server's job.
    #[must_use]
    pub fn asset_ref_at(&self, source: &str, offset: usize) -> Option<links::PathRef> {
        links::asset_ref_at(source, offset)
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
        assert!(
            svc.diagnostics("MainWindow < UIWindow\n  id: main\n")
                .is_empty()
        );
    }

    #[test]
    fn service_surfaces_parse_level_diagnostics() {
        let svc = OtuiService::new();
        let diags = svc.diagnostics("Panel\n\tid: main\n");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "tab-indentation");
    }
}
