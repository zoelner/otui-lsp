//! Global style index (spec §5.2): the workspace-wide `Name < Base` inheritance namespace.
//!
//! In OTML, a top-level widget declaration has the form `Name < Base` (a grammar `style_header`,
//! e.g. `MainWindow < UIWindow`). These names live in a **single global namespace** — the engine's
//! `UIManager::m_styles` — so a style declared in any `.otui` file is visible from every other
//! file. This module builds the pure, protocol-agnostic data behind that namespace:
//!
//! * [`extract_style_defs`] walks one parsed [`SyntaxTree`] and returns the [`StyleDef`]s it
//!   declares (per-document extraction, re-run on every change), and
//! * [`StyleIndex`] aggregates those per-document lists keyed by an opaque [`DocId`], so the server
//!   can key by URI later without this crate ever knowing about URIs.
//!
//! ## Scope (deliberately narrow)
//!
//! This node is **pure indexing**. It does not resolve bases, validate that a base exists, or
//! implement go-to-definition / hover / `workspace/symbol` — those are later nodes that consume
//! this index. The one bit of classification it does carry is [`is_native_base`]: distinguishing a
//! `UI*` built-in engine class (no defining `.otui` file) from a user style that should resolve to
//! a [`StyleDef`].
//!
//! ## Fidelity notes
//!
//! * **Duplicate style names are allowed.** The engine registers styles into a flat map where the
//!   last registration wins at runtime; authoring the same `Name` in two files is legal. The index
//!   therefore stores and returns **every** matching def (never dedupes/overwrites across docs) and
//!   leaves any "which one wins" decision to later nodes.
//! * **Only top-level declarations are styles.** A `Name < Base` nested inside a widget's block is
//!   a widget *instance*, not a style definition, so only the direct children of the document root
//!   are indexed — nested nodes are never walked as styles.
//!
//! Everything here is byte-offset based. No I/O, no `lsp-types`.

use crate::syntax::SyntaxTree;
use lang_api::ByteSpan;
use std::collections::HashMap;
use tree_sitter::Node;

/// A single top-level `Name < Base` style definition extracted from one document.
///
/// Spans are byte offsets into the source the [`SyntaxTree`] was parsed from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyleDef {
    /// The declared style name (the `Name` in `Name < Base`).
    pub name: String,
    /// The base this style inherits from (the `Base` in `Name < Base`), if the header carries one.
    /// `None` only for a malformed/incomplete header whose base field is missing; a well-formed
    /// declaration always has a base. Whether the base resolves to a file or a `UI*` built-in is a
    /// later concern (see [`is_native_base`]).
    pub base: Option<String>,
    /// The span of the declared name identifier — the go-to-definition **target** for later nodes.
    pub name_span: ByteSpan,
    /// The span of the whole `style_header` node. For a bare declaration this is just the
    /// `Name < Base` line; when the style carries an indented body, the node — and so this span —
    /// extends over that block too (mirroring the document-symbol span semantics).
    pub header_span: ByteSpan,
}

/// Extract every top-level style definition declared in `tree`.
///
/// Only the document root's direct children are considered: a `Name < Base` nested inside a widget
/// block is an instance, not a style definition, and is not indexed (see the module docs).
#[must_use]
pub fn extract_style_defs(tree: &SyntaxTree) -> Vec<StyleDef> {
    let source = tree.source();
    let root = tree.root();
    let mut out = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() == "style_header" {
            out.push(build_def(child, source));
        }
    }
    out
}

/// Build a [`StyleDef`] from a `style_header` node.
fn build_def(node: Node<'_>, source: &str) -> StyleDef {
    let name_node = node.child_by_field_name("name");
    let base_node = node.child_by_field_name("base");

    // A style_header always has a name in a well-formed parse; fall back to the whole node's span
    // and an empty name rather than panicking on a malformed/incomplete header.
    let name = name_node.map_or_else(String::new, |n| slice(source, n).to_owned());
    let name_span = name_node.map_or_else(|| SyntaxTree::span_of(node), SyntaxTree::span_of);
    let base = base_node.map(|n| slice(source, n).to_owned());

    StyleDef {
        name,
        base,
        name_span,
        header_span: SyntaxTree::span_of(node),
    }
}

/// Slice `source` by `node`'s byte span.
fn slice<'a>(source: &'a str, node: Node<'_>) -> &'a str {
    &source[node.start_byte()..node.end_byte()]
}

/// Classify a base name as a native built-in engine class.
///
/// Returns `true` when `name` starts with `UI` immediately followed by an uppercase letter
/// (`UIWidget`, `UIWindow`, `UIButton`, …) — the engine convention for its native C++ widget
/// classes, which have no defining `.otui` file. Any other name is a user style that should resolve
/// to a [`StyleDef`]. This does not check whether such a def actually exists (that is a later
/// diagnostics concern); it only splits "built-in native" from "expected to be user-defined".
#[must_use]
pub fn is_native_base(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() >= 3 && b[0] == b'U' && b[1] == b'I' && b[2].is_ascii_uppercase()
}

/// An opaque, protocol-agnostic key identifying a document in the [`StyleIndex`].
///
/// It is just a wrapper around a `String`. The server keys it by document URI; this crate never
/// interprets the contents, so it stays free of any URI/path knowledge.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DocId(String);

impl DocId {
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The underlying key string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for DocId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for DocId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

/// The global, multi-document `Name < Base` style index (spec §5.2).
///
/// Aggregates each document's [`StyleDef`]s keyed by [`DocId`]. Re-indexing a document
/// ([`set_document`](StyleIndex::set_document)) replaces all of its previous defs, and
/// [`remove_document`](StyleIndex::remove_document) drops them — the two operations a server runs on
/// change / close. Lookups fan out across **all** documents, since the namespace is global.
///
/// Duplicate names (within or across documents) are all retained; see the module fidelity notes.
#[derive(Debug, Default)]
pub struct StyleIndex {
    by_doc: HashMap<DocId, Vec<StyleDef>>,
}

impl StyleIndex {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace **all** style defs for one document (re-index on change).
    pub fn set_document(&mut self, doc: impl Into<DocId>, defs: Vec<StyleDef>) {
        self.by_doc.insert(doc.into(), defs);
    }

    /// Remove a document and its defs (e.g. on close/delete), returning them if present.
    pub fn remove_document(&mut self, doc: &DocId) -> Option<Vec<StyleDef>> {
        self.by_doc.remove(doc)
    }

    /// The defs a single document currently contributes, if it is indexed.
    #[must_use]
    pub fn document(&self, doc: &DocId) -> Option<&[StyleDef]> {
        self.by_doc.get(doc).map(Vec::as_slice)
    }

    /// Look up every style defined with `name` across **all** documents.
    ///
    /// Returns each match paired with the document that declares it. Because duplicate names are
    /// legal, this may return more than one entry; callers decide precedence. The order across
    /// documents is unspecified (the backing map is unordered).
    #[must_use]
    pub fn lookup(&self, name: &str) -> Vec<(&DocId, &StyleDef)> {
        self.iter().filter(|(_, def)| def.name == name).collect()
    }

    /// Iterate every `(document, def)` pair in the index — the substrate for `workspace/symbol`.
    pub fn iter(&self) -> impl Iterator<Item = (&DocId, &StyleDef)> {
        self.by_doc
            .iter()
            .flat_map(|(doc, defs)| defs.iter().map(move |def| (doc, def)))
    }

    /// The number of documents currently indexed.
    #[must_use]
    pub fn document_count(&self) -> usize {
        self.by_doc.len()
    }

    /// Whether the index holds no documents.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_doc.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defs_of(source: &str) -> Vec<StyleDef> {
        let tree = SyntaxTree::parse(source).expect("parse");
        extract_style_defs(&tree)
    }

    #[test]
    fn extracts_name_base_and_spans_from_a_bare_declaration() {
        let src = "MainWindow < UIWindow\n";
        let defs = defs_of(src);
        assert_eq!(defs.len(), 1);
        let def = &defs[0];
        assert_eq!(def.name, "MainWindow");
        assert_eq!(def.base.as_deref(), Some("UIWindow"));
        // name_span points at the declared name identifier (the go-to-def target).
        assert_eq!(&src[def.name_span.start..def.name_span.end], "MainWindow");
        // With no block, header_span is the declaration line (the node includes its newline).
        assert_eq!(
            src[def.header_span.start..def.header_span.end].trim_end(),
            "MainWindow < UIWindow"
        );
    }

    #[test]
    fn header_span_covers_the_block_when_the_style_has_a_body() {
        let src = "MainWindow < UIWindow\n  id: main\n";
        let defs = defs_of(src);
        assert_eq!(defs.len(), 1);
        let def = &defs[0];
        // name_span is still just the identifier...
        assert_eq!(&src[def.name_span.start..def.name_span.end], "MainWindow");
        // ...but header_span (the whole node) spans the declaration *and* its indented body.
        let header = &src[def.header_span.start..def.header_span.end];
        assert!(header.starts_with("MainWindow < UIWindow"));
        assert!(header.contains("id: main"));
    }

    #[test]
    fn indexes_multiple_top_level_styles() {
        let defs = defs_of("A < UIWidget\nB < A\n");
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["A", "B"]);
        assert_eq!(defs[1].base.as_deref(), Some("A"));
    }

    #[test]
    fn nested_declarations_are_instances_not_styles() {
        // Only the top-level `Outer < UIWidget` is a style; the nested `Inner < UIWidget` is an
        // instance inside the block and must not be indexed.
        let src = "Outer < UIWidget\n  Inner < UIWidget\n    id: x\n";
        let defs = defs_of(src);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "Outer");
    }

    #[test]
    fn native_base_classification() {
        assert!(is_native_base("UIWindow"));
        assert!(is_native_base("UIWidget"));
        assert!(is_native_base("UIButton"));
        // User styles are not native, even when they merely start with the letters.
        assert!(!is_native_base("MyThing"));
        assert!(!is_native_base("UiWindow")); // lowercase 'i' is not the engine convention
        assert!(!is_native_base("UI")); // no class char after the UI prefix
        assert!(!is_native_base("UIwidget")); // lowercase after the prefix
        assert!(!is_native_base(""));
    }

    #[test]
    fn aggregates_across_docs_and_looks_up_by_name() {
        let mut index = StyleIndex::new();
        index.set_document("a.otui", defs_of("MainWindow < UIWindow\n"));
        index.set_document("b.otui", defs_of("Button < UIButton\n"));
        assert_eq!(index.document_count(), 2);

        let hits = index.lookup("Button");
        assert_eq!(hits.len(), 1);
        let (doc, def) = hits[0];
        assert_eq!(doc.as_str(), "b.otui");
        assert_eq!(def.name, "Button");
        assert_eq!(def.base.as_deref(), Some("UIButton"));

        // A name defined in no document resolves to nothing.
        assert!(index.lookup("Missing").is_empty());
        // Every (doc, def) pair is iterable for workspace/symbol.
        assert_eq!(index.iter().count(), 2);
    }

    #[test]
    fn set_document_replaces_a_docs_defs() {
        let mut index = StyleIndex::new();
        index.set_document("a.otui", defs_of("Old < UIWidget\n"));
        assert_eq!(index.lookup("Old").len(), 1);

        // Re-index the same doc: the old def is gone, the new one present.
        index.set_document("a.otui", defs_of("New < UIWidget\n"));
        assert!(index.lookup("Old").is_empty());
        assert_eq!(index.lookup("New").len(), 1);
        assert_eq!(index.document_count(), 1);
    }

    #[test]
    fn remove_document_drops_its_defs() {
        let mut index = StyleIndex::new();
        let doc = DocId::new("a.otui");
        index.set_document(doc.clone(), defs_of("Gone < UIWidget\n"));
        assert_eq!(index.lookup("Gone").len(), 1);

        let removed = index.remove_document(&doc).expect("was present");
        assert_eq!(removed.len(), 1);
        assert!(index.lookup("Gone").is_empty());
        assert!(index.is_empty());
        // Removing an absent doc is a no-op.
        assert!(index.remove_document(&doc).is_none());
    }

    #[test]
    fn duplicate_names_across_docs_are_all_retained() {
        // The engine allows the same style name in multiple files (last-registered wins at
        // runtime); the index must keep every def and not dedupe them away.
        let mut index = StyleIndex::new();
        index.set_document("a.otui", defs_of("Dup < UIWidget\n"));
        index.set_document("b.otui", defs_of("Dup < UIWindow\n"));

        let mut hits = index.lookup("Dup");
        assert_eq!(hits.len(), 2, "both declarations must be retained");
        // Both bases are represented (order across docs is unspecified).
        hits.sort_by_key(|(doc, _)| doc.as_str());
        assert_eq!(hits[0].1.base.as_deref(), Some("UIWidget"));
        assert_eq!(hits[1].1.base.as_deref(), Some("UIWindow"));
    }
}
