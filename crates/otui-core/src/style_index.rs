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
//! * **Duplicate style names are allowed, and the engine is last-wins.** `importStyleFromOTML` does
//!   `m_styles[name] = style` (`uimanager.cpp:508`), which fully **replaces** any earlier definition
//!   of the same name — not a union of the two — with one exception: an existing style already
//!   flagged `__unique` (a `#Name < Base` freeze) is never overwritten (`uimanager.cpp:500`). Which
//!   definition wins is therefore a property of the engine's *module load order*, something a static
//!   index has no way to know. So the index does not try: it stores and returns **every** matching
//!   def (never dedupes/overwrites across docs) and leaves any "which one wins" decision to later
//!   nodes — see [`crate::ids`]'s module docs for the concrete consequence (an over-approximation
//!   that is safe for "might this id exist" and unsafe for "this id does not exist").
//! * **Only top-level declarations are styles.** A `Name < Base` nested inside a widget's block is
//!   a widget *instance*, not a style definition, so only the direct children of the document root
//!   are indexed — nested nodes are never walked as styles. If the engine ever tried to instantiate
//!   one, it would call `getStyle("Name < Base")`, which is undefined and **throws**
//!   (`uimanager.cpp:708-710`: `if (!originalStyleNode) throw Exception(...)`) — so this is not
//!   merely an indexing simplification, it mirrors a real engine failure mode. Styles are imported
//!   only from a document's **root** nodes (`uimanager.cpp:438-444`).
//!
//! Everything here is byte-offset based. No I/O, no `lsp-types`.

use crate::otml_reparent::is_reparented_onto_a_unique_sibling;
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
    /// The widget class named by a `__class:` property in the style's body, if it carries one.
    ///
    /// `__class` re-roots the style onto a different runtime widget class than its `< Base` chain
    /// implies: the engine reads it as `widgetType = styleNode->valueAt("__class")` and instantiates
    /// *that* class, styled from the base. `SpinBox < TextEdit` with `__class: UISpinBox` is a
    /// `UISpinBox` (a Lua widget declaring `minimum` / `maximum` / `step`) wearing a `TextEdit`'s
    /// look — so the style chain alone would miss every property `UISpinBox` adds.
    pub lua_class: Option<String>,
    /// Every `id:` declared within the style's **body** that the engine actually creates a widget
    /// for — at any depth the engine descends into, which is *not* the same as "at any depth in the
    /// source text" (spec §2.3). See `body_ids_of` for why this walks the subtree rather than
    /// just the header's direct children, for the two ways a nested `id:` is deliberately excluded
    /// (a unique ancestor line, or a line reparented onto one), and for why a widget block that
    /// writes `id:` more than once contributes only the *last* one (the engine's own per-widget
    /// merge is last-wins; see `collect_body_ids`). [`crate::ids`] is the module that turns this
    /// into "ids visible from a document that merely instantiates this style".
    pub body_ids: Vec<StyleBodyId>,
    /// The span of the declared name identifier — the go-to-definition **target** for later nodes.
    pub name_span: ByteSpan,
    /// The span of the whole `style_header` node. For a bare declaration this is just the
    /// `Name < Base` line; when the style carries an indented body, the node — and so this span —
    /// extends over that block too (mirroring the document-symbol span semantics).
    pub header_span: ByteSpan,
}

/// A single `id:` declared somewhere in a [`StyleDef`]'s body — the ids a document inherits merely
/// by instantiating that style (spec §2.3), the substrate for resolving a Lua `getChildById`
/// reference into an inherited style file rather than the document's own tree (see [`crate::ids`]).
///
/// `span` is a byte span into the **declaring** document (the one the [`StyleDef`] itself came
/// from) — a caller can turn it directly into a go-to-definition `Location` in that file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyleBodyId {
    /// The declared id text.
    pub id: String,
    /// The byte span of the `id:` value token, in the declaring document.
    pub span: ByteSpan,
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
        lua_class: lua_class_of(node, source),
        body_ids: body_ids_of(node, source),
        name_span,
        header_span: SyntaxTree::span_of(node),
    }
}

/// Every `id:` declared anywhere within `node`'s subtree that the engine actually turns into a
/// widget — **not necessarily every `id:` written in the source text**: see below for the two ways
/// this walk stops short of that.
///
/// Unlike [`lua_class_of`], which reads only a leaf `__class:` property directly on `node`, an
/// `id:` typically sits several levels deep inside the style's block: `MiniWindow < UIMiniWindow`
/// declares `id: contentsPanel` on a nested `MiniWindowContents` child, not on the header itself.
/// So this walks descendants, not just `node`'s direct children — but **only** through `container`
/// and `style_header` nodes, the two grammar kinds a bare-tag or `Name < Base` line without a `:`
/// parses to. Every other kind's line carries a `:` (`state_selector`'s `$state:`, `property`'s
/// `key:`, `event_property`'s `@tag:`, …), which `OTMLParser::parseNode` marks **unique**
/// (`otmlparser.cpp:435`: `node->setUnique(... || dotsPos != std::string::npos)`), and
/// `UIManager::createWidgetFromOTML`'s child loop skips a unique node outright
/// (`uimanager.cpp:735`: `if (!childNode->isUnique()) createWidgetFromOTML(childNode, widget);`).
/// So a widget nested under `$pressed:`, under `layout:`, or under `visible: false` is never
/// created — walking into it would harvest an `id:` for a widget that does not exist at runtime.
/// This mirrors the `check_property_after_child` diagnostic's allowlist
/// (`crate::diagnostics`), which faced the identical problem for the identical reason.
/// `styleNode->get("id")` (`uiwidget.cpp:1918`) is also a direct-child lookup
/// (`otmlnode.cpp:53-59`), so the id this returns for a given `container`/`style_header` is always
/// the one declared directly inside it, never one found by searching past a unique descendant —
/// and, when that widget's block writes `id:` more than once, the *last* one: see
/// [`collect_body_ids`] for why the engine's own merge (`OTMLNode::addChild`) makes that the only
/// one that can ever survive.
///
/// A second, narrower case: `id_property`, `anchor_property` and `list_item` cannot carry an
/// indented block **in the grammar** (see [`crate::otml_reparent`]), so a line written
/// over-indented under one of them parses as a plain sibling in the enclosing block rather than a
/// genuine child — even though the real engine parents it onto that preceding line
/// (`otmlparser.cpp:314`) and, if that line is unique, never creates it either. Each child is
/// checked against [`is_reparented_onto_a_unique_sibling`] before being walked, so that case is
/// excluded too — see that module for why the check is based on the preceding line's raw text
/// (does it contain a `:`) rather than another hand-picked node-kind list.
fn body_ids_of(node: Node<'_>, source: &str) -> Vec<StyleBodyId> {
    let mut out = Vec::new();
    collect_body_ids(node, source, &mut out);
    out
}

/// Among `node`'s **own** direct children, only the *last* `id_property` is real — the engine's
/// per-widget merge is last-wins, not "collect every value". Every OTML line becomes a node via
/// `OTMLNode::create`, and `id:`'s line always carries a `:`, so it is always marked **unique**
/// (`otmlparser.cpp:435`). Lines are added to their parent one at a time, in source order, via
/// `currentParent->addChild(node)` (`otmlparser.cpp:454-456`), and `OTMLNode::addChild` **replaces**
/// (not appends) an existing same-tag child whenever either side is unique
/// (`otmlnode.cpp:86-116`: `if (node->tag() == newChild->tag() && (node->isUnique() ||
/// newChild->isUnique())) { ... replaceChild(node, newChild); ... }`). So a widget block with two
/// `id:` lines never ends up with two `id` children at runtime — the second `addChild` call
/// replaces the first — and `styleNode->get("id")` (`uiwidget.cpp:1918`), a direct-child lookup
/// that returns the first (and, after replacement, *only*) match (`otmlnode.cpp:53-59`), can only
/// ever read the last one written. Emitting the earlier duplicate as a body id would offer
/// navigation to a value the runtime never sets.
///
/// This last-wins rule is **per parent**, not global: a nested `container`/`style_header`
/// (recursed into separately below) is a different OTML node with its own children, so its own
/// `id:` does not compete with — and is not replaced by — one written directly on `node`.
///
/// A *cross-style* version of this question — does a derived style's own child widget suppress an
/// inherited one with the same tag from its base? — does not arise here: a bare widget tag like
/// `Button` carries no `:`, so `OTMLParser::parseNode` never marks it unique
/// (`otmlparser.cpp:435`), and `OTMLNode::addChild`'s replace path only fires when *some* side is
/// unique. Two `Button` children — one merged in from a base style, one written locally — are
/// therefore not the same merge identity to the engine at all: `merge` (`otmlnode.cpp:152-157`)
/// appends both as siblings via `addChild`, and `UIManager::createWidgetFromOTML`'s child loop
/// (`uimanager.cpp:734-738`) instantiates every non-unique child — both `Button`s become real
/// widgets, both contributing their own ids. Suppressing one would drop an id the engine actually
/// creates, the one direction of error this index must never introduce (see the module docs on the
/// deliberate ids-visibility over-approximation). So only the within-widget scalar case above is
/// implemented; cross-style widget-identity merging is not, because the engine does not do it
/// either.
fn collect_body_ids(node: Node<'_>, source: &str, out: &mut Vec<StyleBodyId>) {
    let mut cursor = node.walk();
    let mut last_own_id = None;
    for child in node.children(&mut cursor) {
        if is_reparented_onto_a_unique_sibling(child, source) {
            continue;
        }
        if child.kind() == "id_property"
            && let Some(value) = child.child_by_field_name("value")
        {
            last_own_id = Some(StyleBodyId {
                id: slice(source, value).to_owned(),
                span: SyntaxTree::span_of(value),
            });
        }
    }
    out.extend(last_own_id);

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if is_reparented_onto_a_unique_sibling(child, source) {
            continue;
        }
        if matches!(child.kind(), "container" | "style_header") {
            collect_body_ids(child, source, out);
        }
    }
}

/// The value of a `__class:` property in the style's body, if present.
///
/// The grammar's `_block` is a hidden rule, so a style's body statements are direct children of the
/// `style_header` node. Only a leaf `__class: <value>` counts; an empty or block form yields `None`.
fn lua_class_of(node: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = node.walk();
    let found = node
        .named_children(&mut cursor)
        .filter(|c| c.kind() == "property")
        .find(|c| {
            c.child_by_field_name("key")
                .is_some_and(|k| slice(source, k) == "__class")
        })?;
    let value = found.child_by_field_name("value")?;
    let text = slice(source, value).trim();
    (!text.is_empty()).then(|| text.to_owned())
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

    /// Look up every style whose **base** is `name` across **all** documents — the styles that
    /// directly derive from `name` (`X < name`).
    ///
    /// Returns each such def paired with the document that declares it. Base matching is exact and
    /// case-sensitive, mirroring [`lookup`](Self::lookup) and [`extract_style_defs`]. The order across
    /// documents is unspecified (the backing map is unordered). This is the substrate for
    /// `textDocument/implementation` (the derivations of the style under the cursor).
    #[must_use]
    pub fn subtypes(&self, name: &str) -> Vec<(&DocId, &StyleDef)> {
        self.iter()
            .filter(|(_, def)| def.base.as_deref() == Some(name))
            .collect()
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
    fn body_ids_include_a_direct_id_property_on_the_header() {
        let src = "MainWindow < UIWindow\n  id: main\n";
        let defs = defs_of(src);
        assert_eq!(defs[0].body_ids.len(), 1);
        let body_id = &defs[0].body_ids[0];
        assert_eq!(body_id.id, "main");
        assert_eq!(&src[body_id.span.start..body_id.span.end], "main");
    }

    #[test]
    fn body_ids_are_found_at_any_depth_inside_nested_widgets() {
        // The real shape: MiniWindow's own `id:` values sit on nested child widgets, not on the
        // style header itself.
        let src = "MiniWindow < UIMiniWindow\n  MiniWindowContents\n    id: contentsPanel\n  \
                    Button\n    id: closeButton\n";
        let defs = defs_of(src);
        assert_eq!(defs.len(), 1);
        let ids: Vec<&str> = defs[0].body_ids.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, ["contentsPanel", "closeButton"]);
    }

    #[test]
    fn body_ids_are_empty_for_a_bare_declaration_with_no_block() {
        let defs = defs_of("MainWindow < UIWindow\n");
        assert!(defs[0].body_ids.is_empty());
    }

    #[test]
    fn two_id_properties_directly_on_the_header_collapse_to_the_last_one() {
        // `id:` is always unique (its line has a `:`, `otmlparser.cpp:435`), and
        // `OTMLNode::addChild` replaces (not appends) an existing same-tag child whenever either
        // side is unique (`otmlnode.cpp:86-116`) -- so parsing "id: first" then "id: second" on the
        // same widget leaves only "second" as the `id` child. Emitting "first" too would be a
        // phantom: an id the runtime never sets.
        let src = "MainWindow < UIWindow\n  id: first\n  id: second\n";
        let defs = defs_of(src);
        assert_eq!(defs.len(), 1);
        let ids: Vec<&str> = defs[0].body_ids.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(
            ids,
            ["second"],
            "only the last id: on the widget must survive: {:?}",
            defs[0].body_ids
        );
    }

    #[test]
    fn two_id_properties_on_a_nested_widget_collapse_to_the_last_one() {
        // The same last-wins rule applies at any depth, not just the header: it is a property of
        // the widget the `id:` lines belong to, not of being the top-level style_header.
        let src = "MiniWindow < UIMiniWindow\n  Button\n    id: first\n    id: second\n";
        let defs = defs_of(src);
        assert_eq!(defs.len(), 1);
        let ids: Vec<&str> = defs[0].body_ids.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, ["second"]);
    }

    #[test]
    fn a_duplicate_id_does_not_suppress_an_unrelated_sibling_widgets_id() {
        // Last-wins is scoped to the widget the duplicate `id:` lines are written directly on --
        // it must not reach across to a different, unrelated widget's own single `id:`.
        let src = "MiniWindow < UIMiniWindow\n  Header\n    id: first\n    id: second\n  Footer\n    \
                    id: footerId\n";
        let defs = defs_of(src);
        let ids: Vec<&str> = defs[0].body_ids.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, ["second", "footerId"]);
    }

    #[test]
    fn two_widgets_sharing_a_tag_each_contribute_their_own_id_not_a_last_wins_override() {
        // A bare widget tag like `Button` carries no `:`, so `OTMLParser::parseNode` never marks it
        // unique (`otmlparser.cpp:435`), and `OTMLNode::addChild`'s replace-on-same-tag path
        // (`otmlnode.cpp:86-116`) only fires when *some* side is unique. Two `Button` children are
        // therefore never the same merge identity to the engine -- both are appended
        // (`otmlnode.cpp:152-157`) and both are instantiated
        // (`uimanager.cpp:734-738`: `if (!childNode->isUnique()) createWidgetFromOTML(...)`).
        // Collapsing same-tag widgets the way scalar `id:` properties collapse would drop an id the
        // engine actually creates -- the opposite of what this index must guarantee.
        let src =
            "Outer < UIWidget\n  Button\n    id: firstButton\n  Button\n    id: secondButton\n";
        let defs = defs_of(src);
        let ids: Vec<&str> = defs[0].body_ids.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(
            ids,
            ["firstButton", "secondButton"],
            "both same-tag widgets must contribute their own id, not just the last: {:?}",
            defs[0].body_ids
        );
    }

    #[test]
    fn an_id_nested_inside_a_state_block_is_not_a_body_id() {
        // `$pressed:` (and any `$state:`) is a `state_selector` node — its line carries a `:`, so
        // the engine marks it unique and its child loop (`uimanager.cpp:735`) never turns a unique
        // node's children into widgets. A `VerticalScrollBar` written under `$pressed:` therefore
        // never exists at runtime, and its `id:` must not be reported as a body id.
        let src = "MiniWindow < UIMiniWindow\n  $pressed:\n    VerticalScrollBar\n      \
                    id: phantom\n";
        let defs = defs_of(src);
        assert_eq!(defs.len(), 1);
        assert!(
            defs[0].body_ids.is_empty(),
            "an id under $state must not surface as a body id: {:?}",
            defs[0].body_ids
        );
    }

    #[test]
    fn an_id_nested_under_a_plain_property_block_is_not_a_body_id() {
        // The real-world corpus bug (character.otui:1860 in the OTClient engine): a widget
        // over-indented under a plain `key:` property (e.g. `visible: false`) parents to a
        // `property` node, which is unique for the same reason as a `$state:` block — its line has
        // a `:` — so the engine never creates it.
        let src = "CharacterTitles < UIWidget\n  visible: false\n    VerticalScrollBar\n      \
                    id: ListScrollbar\n";
        let defs = defs_of(src);
        assert_eq!(defs.len(), 1);
        assert!(
            defs[0].body_ids.is_empty(),
            "an id nested under a property block must not surface as a body id: {:?}",
            defs[0].body_ids
        );
    }

    #[test]
    fn a_widget_over_indented_under_a_plain_id_is_reparented_not_nested() {
        // `id:` is one of the three block-less grammar rules (`id_property`, `anchor_property`,
        // `list_item` — see `crate::otml_reparent`): it cannot carry an indented child in the
        // grammar, so a `Button` written deeper-indented under `id: a` does not nest inside the
        // `id_property` the way it would under a `property`/`state_selector` — tree-sitter instead
        // attaches it as a plain sibling of `id: a` in the enclosing block. The engine parents it
        // onto the *preceding line* instead (`otmlparser.cpp:314`), and `id: a`'s line is unique, so
        // `Button` (and its own nested `id: phantomUnderId`) is never created
        // (`uimanager.cpp:735`).
        let src = "MainWindow < UIWindow\n  id: a\n    Button\n      id: phantomUnderId\n";
        let defs = defs_of(src);
        assert_eq!(defs.len(), 1);
        let ids: Vec<&str> = defs[0].body_ids.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(
            ids,
            ["a"],
            "the over-indented Button must not contribute a body id: {:?}",
            defs[0].body_ids
        );
    }

    #[test]
    fn a_widget_over_indented_under_an_anchor_property_is_reparented_not_nested() {
        // Same gap as above, reached through `anchor_property` (the other block-less rule besides
        // `id_property`/`list_item`) instead of `id_property`.
        let src = "MainWindow < UIWindow\n  anchors.left: parent.left\n    Button\n      \
                    id: phantomUnderAnchor\n";
        let defs = defs_of(src);
        assert_eq!(defs.len(), 1);
        assert!(
            defs[0].body_ids.is_empty(),
            "the over-indented Button must not contribute a body id: {:?}",
            defs[0].body_ids
        );
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
    fn subtypes_finds_only_styles_whose_base_matches_across_docs() {
        // `A` is derived from in two different documents; unrelated styles are not returned.
        let mut index = StyleIndex::new();
        index.set_document("a.otui", defs_of("A < UIWidget\nB < A\n"));
        index.set_document("b.otui", defs_of("C < A\nD < UIWidget\n"));

        let mut names: Vec<&str> = index
            .subtypes("A")
            .iter()
            .map(|(_, d)| d.name.as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["B", "C"], "only styles whose base == A");
    }

    #[test]
    fn subtypes_match_is_exact_and_case_sensitive() {
        let mut index = StyleIndex::new();
        index.set_document("a.otui", defs_of("Real < Base\nOther < base\n"));
        // `base` (lowercase) is a different type than `Base`.
        let names: Vec<&str> = index
            .subtypes("Base")
            .iter()
            .map(|(_, d)| d.name.as_str())
            .collect();
        assert_eq!(names, ["Real"]);
    }

    #[test]
    fn subtypes_is_empty_when_nothing_derives() {
        let mut index = StyleIndex::new();
        index.set_document("a.otui", defs_of("Leaf < UIWidget\n"));
        // Nothing derives from `Leaf`, and a native base with no user derivation is empty too.
        assert!(index.subtypes("Leaf").is_empty());
        assert!(index.subtypes("Missing").is_empty());
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
