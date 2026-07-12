//! Resolving the full set of `id:` values **visible** from a document (spec §2.3), including ids
//! inherited from the bodies of the styles it instantiates — not just the ids the document declares
//! itself.
//!
//! ## The problem this closes
//!
//! An OTUI module file typically instantiates a style declared elsewhere (`MainWindow < MiniWindow`)
//! and pairs with Lua code that does `getChildById('closeButton')` — but `closeButton` is not
//! declared anywhere in the module's own `.otui`; it lives in the **body** of `MiniWindow`, in
//! `data/styles/30-miniwindow.otui`. Measured on the real engine corpus, a quarter of all Lua→OTUI id
//! references resolve into an inherited style rather than the paired file. Resolving ids only within
//! one document therefore fails 1 in 4 navigations. [`visible_ids`] is the fix: it walks every
//! widget the document instantiates up its `< Base` chain (via
//! [`resolve_ancestry`](crate::widget_resolve::resolve_ancestry), reused rather than reimplemented —
//! see that module for the cross-file walk and its cycle guard) and unions in each ancestor style's
//! declared [`StyleBodyId`](crate::style_index::StyleBodyId)s.
//!
//! ## Shadowing: a local declaration wins
//!
//! If the same id name is declared both directly in the document **and** in the body of an inherited
//! style, only the **local** [`VisibleId`] (origin [`IdOrigin::Document`]) is returned; the inherited
//! one is dropped. Reasoning: the document being edited is the more specific, more certain
//! declaration site — a caller resolving a reference wants the widget actually in front of them, not
//! a shared base style several files away, and a document that re-declares an id an inherited style
//! already uses is almost always doing so on purpose (overriding/duplicating that widget locally).
//! Jumping to the local declaration is never a *wrong* answer; jumping to the inherited one when a
//! closer, local candidate exists could be.
//!
//! ## Purity
//!
//! Pure and protocol-agnostic like every other module in this crate: byte offsets, no I/O, no
//! `lsp-types`. [`IdOrigin::InheritedStyle`] carries a [`DocId`] rather than a URI so the server can
//! turn it into a `Location` in the declaring file without this crate knowing what a URI is.

use crate::lua_widgets::LuaWidgetIndex;
use crate::style_index::{DocId, StyleIndex};
use crate::syntax::SyntaxTree;
use crate::widget_resolve::resolve_ancestry;
use lang_api::ByteSpan;
use std::collections::HashSet;
use tree_sitter::Node;

/// Where a [`VisibleId`] was declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdOrigin {
    /// Declared directly in the document [`visible_ids`] was asked about.
    Document,
    /// Inherited: declared in the body of style `style`, defined in document `doc` — a
    /// [`crate::style_index::StyleBodyId`] pulled in because the document instantiates `style` (or
    /// something that derives from it).
    InheritedStyle {
        /// The name of the style whose body declares the id (the nearest link in the `< Base` chain
        /// that actually declares it — not necessarily the widget's own immediate type).
        style: String,
        /// The document that defines `style` — the go-to-definition target file.
        doc: DocId,
    },
}

/// One `id:` value visible from a document, with where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleId {
    /// The id text.
    pub id: String,
    /// The byte span of the `id:` value token, **in the declaring document** — for
    /// [`IdOrigin::Document`] that is the document [`visible_ids`] was called on; for
    /// [`IdOrigin::InheritedStyle`] it is the document named by
    /// [`InheritedStyle::doc`](IdOrigin::InheritedStyle).
    pub span: ByteSpan,
    /// Where this id was declared.
    pub origin: IdOrigin,
}

/// Compute every id visible from `source`: the ids it declares directly, plus — for every widget it
/// instantiates, at any depth — the ids declared in the bodies of the styles that widget's `< Base`
/// chain reaches (see the module docs for the shadowing rule and why [`resolve_ancestry`] is reused
/// rather than a second ancestry walker).
///
/// A widget is "instantiated" by any `style_header` (its `base`) or bare `container` (its `tag`) node
/// in the tree, at any depth — not just the top-level entry: a nested widget can itself be an
/// instance of a style declared elsewhere, and its inherited ids are visible too. Each distinct
/// instantiated type is resolved only once, even if the document instantiates it repeatedly.
///
/// Returns an empty vec when `source` cannot be parsed.
#[must_use]
pub fn visible_ids(source: &str, styles: &StyleIndex) -> Vec<VisibleId> {
    let Some(tree) = SyntaxTree::parse(source) else {
        return Vec::new();
    };
    let root = tree.root();

    let mut local = Vec::new();
    let mut local_names = HashSet::new();
    collect_local_ids(root, source, &mut local, &mut local_names);

    // Ids don't come from Lua-declared widgets (only a style's `.otui` body ever carries an
    // `id_property`), so an empty `LuaWidgetIndex` is enough to drive `resolve_ancestry`'s cross-file
    // `.otui` walk without pulling in a Lua parent chain that could never contribute an id anyway.
    let lua = LuaWidgetIndex::new();
    let mut seen_types = HashSet::new();
    let mut inherited = Vec::new();
    collect_instantiated_ids(root, source, styles, &lua, &mut seen_types, &mut inherited);

    // Shadowing (see module docs): a local declaration wins over an inherited id of the same name.
    inherited.retain(|v| !local_names.contains(&v.id));

    let mut out = local;
    out.extend(inherited);
    out
}

/// Depth-first collection of every `id:` declared directly in `node`'s subtree (ids nest at any
/// depth), recording each id's name into `names` as it goes for the shadowing check.
fn collect_local_ids(
    node: Node<'_>,
    source: &str,
    out: &mut Vec<VisibleId>,
    names: &mut HashSet<String>,
) {
    if node.kind() == "id_property" {
        if let Some(value) = node.child_by_field_name("value") {
            let span = SyntaxTree::span_of(value);
            let id = source[span.start..span.end].to_owned();
            names.insert(id.clone());
            out.push(VisibleId {
                id,
                span,
                origin: IdOrigin::Document,
            });
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_local_ids(child, source, out, names);
    }
}

/// Depth-first walk collecting the inherited ids of every widget `node`'s subtree instantiates.
///
/// A `style_header`'s `base` field or a bare `container`'s `tag` field names the type being
/// instantiated at that point; [`resolve_ancestry`] walks that type's full `< Base` chain
/// (cycle-safe by construction), and every [`StyleBodyId`](crate::style_index::StyleBodyId) declared
/// by any style along that chain is visible here. `seen_types` skips a type already resolved
/// elsewhere in the same document, so instantiating the same style twice does not duplicate work or
/// output.
fn collect_instantiated_ids(
    node: Node<'_>,
    source: &str,
    styles: &StyleIndex,
    lua: &LuaWidgetIndex,
    seen_types: &mut HashSet<String>,
    out: &mut Vec<VisibleId>,
) {
    let instantiated = match node.kind() {
        "style_header" => node
            .child_by_field_name("base")
            .map(|n| slice(source, n).to_owned()),
        "container" => node
            .child_by_field_name("tag")
            .map(|n| slice(source, n).to_owned()),
        _ => None,
    };
    if let Some(type_name) = instantiated {
        if seen_types.insert(type_name.clone()) {
            let ancestry = resolve_ancestry(&type_name, styles, lua);
            for ancestor in &ancestry.chain {
                for (doc, def) in styles.lookup(ancestor) {
                    for body_id in &def.body_ids {
                        out.push(VisibleId {
                            id: body_id.id.clone(),
                            span: body_id.span,
                            origin: IdOrigin::InheritedStyle {
                                style: ancestor.clone(),
                                doc: doc.clone(),
                            },
                        });
                    }
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_instantiated_ids(child, source, styles, lua, seen_types, out);
    }
}

/// Slice `source` by `node`'s byte span.
fn slice<'a>(source: &'a str, node: Node<'_>) -> &'a str {
    &source[node.start_byte()..node.end_byte()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style_index::extract_style_defs;

    /// Build a [`StyleIndex`] from `(doc, otui_source)` pairs.
    fn styles(docs: &[(&str, &str)]) -> StyleIndex {
        let mut index = StyleIndex::new();
        for (doc, src) in docs {
            let tree = SyntaxTree::parse(src).expect("parse otui");
            index.set_document(*doc, extract_style_defs(&tree));
        }
        index
    }

    fn ids_of(visible: &[VisibleId]) -> Vec<&str> {
        let mut v: Vec<&str> = visible.iter().map(|x| x.id.as_str()).collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn inherits_ids_declared_in_an_instantiated_styles_body() {
        // The measured real-world case: a module merely instantiates MiniWindow, and
        // `closeButton`/`contentsPanel` live only in MiniWindow's own body, in another file.
        let styles = styles(&[(
            "data/styles/30-miniwindow.otui",
            "MiniWindow < UIMiniWindow\n  MiniWindowContents\n    id: contentsPanel\n  \
             Button\n    id: closeButton\n",
        )]);
        let doc = "MainWindow < MiniWindow\n  Label\n    id: title\n";

        let visible = visible_ids(doc, &styles);
        assert_eq!(ids_of(&visible), ["closeButton", "contentsPanel", "title"]);

        let close = visible
            .iter()
            .find(|v| v.id == "closeButton")
            .expect("present");
        match &close.origin {
            IdOrigin::InheritedStyle { style, doc } => {
                assert_eq!(style, "MiniWindow");
                assert_eq!(doc.as_str(), "data/styles/30-miniwindow.otui");
            }
            IdOrigin::Document => panic!("closeButton must be inherited, not local"),
        }
        // Its span lands on the id token *in the declaring document*, not the asking one.
        let src = "MiniWindow < UIMiniWindow\n  MiniWindowContents\n    id: contentsPanel\n  \
                    Button\n    id: closeButton\n";
        assert_eq!(&src[close.span.start..close.span.end], "closeButton");

        let title = visible.iter().find(|v| v.id == "title").expect("present");
        assert_eq!(title.origin, IdOrigin::Document);
    }

    #[test]
    fn a_locally_declared_id_has_document_origin() {
        let visible = visible_ids("Panel\n  id: header\n", &StyleIndex::new());
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, "header");
        assert_eq!(visible[0].origin, IdOrigin::Document);
    }

    #[test]
    fn a_style_chain_two_levels_deep_is_walked() {
        // A < B < C, and the id lives in C's body; a document instantiating A must still see it.
        let styles = styles(&[("a.otui", "A < B\nB < C\nC < UIWidget\n  id: deep\n")]);
        let doc = "Instance < A\n";
        let visible = visible_ids(doc, &styles);
        assert_eq!(ids_of(&visible), ["deep"]);
        match &visible[0].origin {
            IdOrigin::InheritedStyle { style, .. } => assert_eq!(style, "C"),
            IdOrigin::Document => panic!("must be inherited"),
        }
    }

    #[test]
    fn a_cyclic_base_chain_terminates_instead_of_hanging() {
        // A < B and B < A: resolve_ancestry's cycle guard must still make this return promptly.
        let styles = styles(&[("a.otui", "A < B\n  id: onA\nB < A\n  id: onB\n")]);
        let doc = "Instance < A\n";
        let visible = visible_ids(doc, &styles);
        assert_eq!(ids_of(&visible), ["onA", "onB"]);
    }

    #[test]
    fn a_local_declaration_shadows_an_inherited_id_of_the_same_name() {
        // MiniWindow's body declares `closeButton`; the document redeclares an id with the same
        // name locally. Only the local (Document-origin) entry must be returned.
        let styles = styles(&[(
            "styles.otui",
            "MiniWindow < UIMiniWindow\n  Button\n    id: closeButton\n",
        )]);
        let doc = "MainWindow < MiniWindow\n  Button\n    id: closeButton\n";

        let visible = visible_ids(doc, &styles);
        let matches: Vec<&VisibleId> = visible.iter().filter(|v| v.id == "closeButton").collect();
        assert_eq!(
            matches.len(),
            1,
            "the inherited duplicate must be dropped: {visible:?}"
        );
        assert_eq!(matches[0].origin, IdOrigin::Document);
    }

    #[test]
    fn nested_widget_instantiations_contribute_their_own_inherited_ids() {
        // A widget nested (not just top-level) can itself instantiate another style.
        let styles = styles(&[("styles.otui", "InnerPanel < UIWidget\n  id: innerId\n")]);
        let doc = "Outer < UIWidget\n  InnerPanel\n    id: outerOverride\n";
        let visible = visible_ids(doc, &styles);
        assert_eq!(ids_of(&visible), ["innerId", "outerOverride"]);
    }

    #[test]
    fn no_styles_and_no_local_ids_yields_nothing() {
        assert!(visible_ids("Panel < UIWidget\n", &StyleIndex::new()).is_empty());
    }

    #[test]
    fn unparseable_source_yields_nothing() {
        assert!(visible_ids("", &StyleIndex::new()).is_empty());
    }

    #[test]
    fn repeated_instantiations_of_the_same_style_do_not_duplicate_ids() {
        let styles = styles(&[("styles.otui", "Item < UIWidget\n  id: itemId\n")]);
        let doc = "Outer < UIWidget\n  Item\n  Item\n  Item\n";
        let visible = visible_ids(doc, &styles);
        let matches: Vec<&VisibleId> = visible.iter().filter(|v| v.id == "itemId").collect();
        assert_eq!(matches.len(), 1, "must not duplicate: {visible:?}");
    }
}
