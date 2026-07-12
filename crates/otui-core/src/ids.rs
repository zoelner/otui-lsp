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
/// A widget is "instantiated" by a `style_header` (its `base`) or bare `container` (its `tag`) node,
/// at any depth **the engine actually descends into** — not just the top-level entry: a nested widget
/// can itself be an instance of a style declared elsewhere, and its inherited ids are visible too. See
/// [`collect_instantiated_ids`] for why that depth stops at the first `:`-bearing (engine-"unique")
/// node. Each distinct instantiated type is resolved only once, even if the document instantiates it
/// repeatedly.
///
/// ## Over-approximation: a duplicated style name contributes every match, not just the winner
///
/// The engine's style registry is **last-wins**: `m_styles[name] = style` fully replaces any earlier
/// definition of the same name (`uimanager.cpp:508`), except that an existing style already marked
/// `__unique` is never overwritten (`uimanager.cpp:500`). Import order — and so which definition
/// actually wins at runtime — is a property of the engine's module load sequence, which this static
/// index cannot know. Rather than guess, [`collect_instantiated_ids`] unions in the body ids of
/// **every** [`StyleDef`](crate::style_index::StyleDef) matching a given name (via
/// [`StyleIndex::lookup`]), including ones that would have lost at runtime. This is deliberate: it
/// favours recall for navigation (offering more than one candidate `Location` is a legal, harmless
/// answer to "where might this id be declared"), at the cost of sometimes offering an id that a
/// particular runtime load order would never actually create.
///
/// **This makes [`visible_ids`] sound only for "might this id exist", never for "this id does not
/// exist".** A caller must not use an id's absence from this list to justify an "unknown id" *error*
/// diagnostic — the over-approximation only adds false positives to existence, it does not add false
/// negatives, so a missing id here is not proof of anything. (This repo has already retired one
/// diagnostic for exactly this class of unsoundness; do not reintroduce it here.)
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

    // Two different instantiated types can share an ancestor in their `< Base` chain (e.g. two
    // widgets both ultimately deriving from the same base), so the same (id, span, declaring
    // document) triple can be pushed once per type that reaches it. Collapse those exact repeats —
    // they name the very same declaration site, not distinct candidates.
    dedup_by_declaration_site(&mut inherited);

    let mut out = local;
    out.extend(inherited);
    out
}

/// Drop every entry whose `(id, span, declaring document)` triple duplicates one already kept,
/// preserving the order of first occurrence. The declaring document is `None` for
/// [`IdOrigin::Document`] (there is only ever one such document: the one being asked about) and
/// `Some(doc)` for [`IdOrigin::InheritedStyle`].
fn dedup_by_declaration_site(ids: &mut Vec<VisibleId>) {
    let mut seen = HashSet::new();
    ids.retain(|v| {
        let doc = match &v.origin {
            IdOrigin::Document => None,
            IdOrigin::InheritedStyle { doc, .. } => Some(doc.as_str().to_owned()),
        };
        seen.insert((v.id.clone(), v.span.start, v.span.end, doc))
    });
}

/// Depth-first collection of every `id:` the engine actually creates a widget for, declared anywhere
/// in `node`'s subtree, recording each id's name into `names` as it goes for the shadowing check.
///
/// Only descends through `container` and `style_header` children — see [`collect_instantiated_ids`]'s
/// doc comment (and [`crate::style_index`]'s equivalent allowlist for a style's own body) for why:
/// every other node kind's line carries a `:`, which makes the engine treat it as unique and never
/// turn its children into widgets.
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
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(child.kind(), "id_property" | "container" | "style_header") {
            collect_local_ids(child, source, out, names);
        }
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
///
/// Recursion stops at anything that is not a `container` or `style_header`. Every other node kind's
/// line contains a `:` (a `state_selector`'s `$state:`, a `property`'s `key:`, an `event_property`'s
/// `@tag:`, …), which makes `OTMLParser::parseNode` mark it **unique**
/// (`otmlparser.cpp:435`: `node->setUnique(... || dotsPos != std::string::npos)`), and
/// `UIManager::createWidgetFromOTML`'s child loop never instantiates a unique node's children
/// (`uimanager.cpp:735`: `if (!childNode->isUnique()) createWidgetFromOTML(childNode, widget);`). A
/// `style_header` or `container` written under a `$state:`/`layout:`/etc. block is therefore never
/// created at runtime, so it cannot itself instantiate anything either — walking into it would
/// resolve ancestry for a widget that does not exist.
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
        if matches!(child.kind(), "container" | "style_header") {
            collect_instantiated_ids(child, source, styles, lua, seen_types, out);
        }
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

    #[test]
    fn an_id_nested_inside_a_state_block_is_never_visible() {
        // `$pressed:` is a `state_selector`: its line has a `:`, so the engine's
        // `!childNode->isUnique()` child loop (uimanager.cpp:735) never creates its children as
        // widgets. An id declared inside must be invisible whether it would have been local...
        let local_doc = "Outer < UIWidget\n  $pressed:\n    VerticalScrollBar\n      id: phantom\n";
        assert!(visible_ids(local_doc, &StyleIndex::new()).is_empty());

        // ...or inherited through a style the document instantiates.
        let styles = styles(&[(
            "styles.otui",
            "MiniWindow < UIMiniWindow\n  $pressed:\n    VerticalScrollBar\n      id: phantom\n",
        )]);
        let doc = "MainWindow < MiniWindow\n";
        assert!(visible_ids(doc, &styles).is_empty());
    }

    #[test]
    fn an_id_nested_under_a_plain_property_block_is_never_visible() {
        // The real-world corpus bug (character.otui:1860 in the OTClient engine): an id
        // over-indented under a plain `key:` property (e.g. `visible: false`) parents to a
        // `property` node, unique for the same reason as `$state:` — its line has a `:` — so the
        // engine never creates it and the id must not be offered as a navigation target.
        let local_doc =
            "CharacterTitles < UIWidget\n  visible: false\n    VerticalScrollBar\n      \
             id: ListScrollbar\n";
        assert!(visible_ids(local_doc, &StyleIndex::new()).is_empty());

        let styles = styles(&[(
            "styles.otui",
            "CharacterTitles < UIWidget\n  visible: false\n    VerticalScrollBar\n      \
             id: ListScrollbar\n",
        )]);
        let doc = "Instance < CharacterTitles\n";
        assert!(visible_ids(doc, &styles).is_empty());
    }

    #[test]
    fn a_shared_ancestor_reached_by_two_instantiated_types_contributes_its_id_once() {
        // A and B both derive from Base; Base declares `shared`. Instantiating both A and B walks
        // Base's ancestry twice (once per starting type), so without deduping by declaration site
        // the exact same (id, span, doc) triple would be pushed twice.
        let styles = styles(&[(
            "styles.otui",
            "Base < UIWidget\n  id: shared\nA < Base\nB < Base\n",
        )]);
        let doc = "Outer < UIWidget\n  A\n  B\n";
        let visible = visible_ids(doc, &styles);
        let matches: Vec<&VisibleId> = visible.iter().filter(|v| v.id == "shared").collect();
        assert_eq!(
            matches.len(),
            1,
            "the shared ancestor's id must appear once, not once per instantiating type: \
             {visible:?}"
        );
    }
}
