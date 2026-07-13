//! Inlay hints revealing the resolved **native** ancestor of a based widget/style
//! (`textDocument/inlayHint`): `Foo < SomeStyle →UIButton` — helping the reader see what `Foo`
//! ultimately *is* without hand-walking the `Name < Base` chain themselves.
//!
//! Deliberately modest: no "effective inherited property values", no speculative resolution — just
//! [`widget_resolve::resolve_ancestry`]'s already-existing native-class answer, surfaced inline. A
//! hint is emitted only when it says something the reader doesn't already see written on the line:
//! a base that dead-ends (no native class reached) gets no hint, and a base that **is** already the
//! resolved native (`Button < UIButton`) is skipped as a no-op echo.
//!
//! Pure: byte offsets only, no I/O, no `lsp-types`. The server converts each [`AncestorHint`]'s
//! `anchor` offset into an LSP `Position` and filters to the requested viewport range.

use crate::lua_widgets::LuaWidgetIndex;
use crate::style_index::StyleIndex;
use crate::syntax::SyntaxTree;
use crate::widget_resolve::resolve_ancestry;

/// One ancestor inlay hint anchored just after a top-level style's `Base` token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AncestorHint {
    /// The byte offset to render the hint at — the end of the base token, so it reads
    /// `Foo < SomeStyle →UIButton`.
    pub anchor: usize,
    /// The resolved native `UI*` ancestor class.
    pub native: String,
}

/// Compute the ancestor hints for every top-level `Name < Base` style declaration in `source`,
/// resolved against the workspace `styles` and `lua` indexes (see
/// [`widget_resolve::resolve_ancestry`]).
///
/// A hint is emitted only when resolution reaches a native class (`ancestry.native.is_some()`) and
/// that native name differs from the literal `Base` token written on the line — a base that
/// already spells out the resolved native (`Button < UIButton`) would otherwise get a redundant
/// `→UIButton` echo. Returns an empty vec when `source` fails to parse.
///
/// Top-level only, deliberately — mirroring [`extract_style_defs`](crate::style_index)'s own scope
/// (see its "Only top-level declarations are styles" fidelity note): a `style_header` nested inside
/// a widget's body is a widget *instance*, not a style declaration, and gets no ancestor hint here
/// either. Documented as a known gap, not a bug: widening this to walk nested `style_header` nodes
/// too is a plausible future extension.
#[must_use]
pub fn ancestor_hints(
    source: &str,
    styles: &StyleIndex,
    lua: &LuaWidgetIndex,
) -> Vec<AncestorHint> {
    let Some(tree) = SyntaxTree::parse(source) else {
        return Vec::new();
    };
    let root = tree.root();
    let mut out = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "style_header" {
            continue;
        }
        let Some(base_node) = child.child_by_field_name("base") else {
            continue;
        };
        let span = SyntaxTree::span_of(base_node);
        let base_text = &source[span.start..span.end];

        // Deliberately resolves from the line's own *written* base, not from the declared name.
        // Starting at the declared name would first have `pick_def` choose a winner among any
        // duplicate-named defs sharing it — and if this declaration is not the one `pick_def`
        // would pick, that lookup could hand back a *different* duplicate's base, silently
        // resolving a native ancestor this line never actually reaches. Starting at `base_text`
        // sidesteps the ambiguity entirely: it is the base this exact line wrote, with nothing to
        // pick between.
        let ancestry = resolve_ancestry(base_text, styles, lua);
        let Some(native) = ancestry.native else {
            continue; // dead end (undefined base, or a cycle) — nothing to report
        };
        if native == base_text {
            continue; // the base already spells out the resolved native — a no-op echo
        }
        out.push(AncestorHint {
            anchor: span.end,
            native,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua_widgets::scan_widgets;
    use crate::style_index::{extract_style_defs, DocId};

    fn styles(docs: &[(&str, &str)]) -> StyleIndex {
        let mut index = StyleIndex::new();
        for (doc, src) in docs {
            let tree = SyntaxTree::parse(src).expect("parse otui");
            index.set_document(DocId::new(*doc), extract_style_defs(&tree));
        }
        index
    }

    #[test]
    fn a_multi_hop_chain_yields_the_native_ancestor() {
        // Foo < Bar < UIButton: Foo's hint should resolve all the way to UIButton, not stop at Bar.
        let src = "Foo < Bar\nBar < UIButton\n";
        let idx = styles(&[("a.otui", src)]);
        let hints = ancestor_hints(src, &idx, &LuaWidgetIndex::new());

        let foo_hint = hints
            .iter()
            .find(|h| h.anchor == src.find("Bar\n").unwrap() + "Bar".len())
            .expect("Foo's hint present");
        assert_eq!(foo_hint.native, "UIButton");
    }

    #[test]
    fn a_dead_end_base_yields_no_hint() {
        let src = "Thing < NoSuchBase\n";
        let idx = styles(&[("a.otui", src)]);
        assert!(ancestor_hints(src, &idx, &LuaWidgetIndex::new()).is_empty());
    }

    #[test]
    fn a_base_that_already_names_the_resolved_native_is_a_no_op_echo() {
        // Button < UIButton: the base *is* the native class already, so no hint (no redundant
        // →UIButton right after UIButton itself).
        let src = "Button < UIButton\n";
        let idx = styles(&[("a.otui", src)]);
        assert!(ancestor_hints(src, &idx, &LuaWidgetIndex::new()).is_empty());
    }

    #[test]
    fn a_base_that_differs_from_the_native_gets_a_hint() {
        // Foo < Button: Button's own native is UIButton, which differs from the literal "Button"
        // written on the line, so a hint is warranted.
        let src = "Foo < Button\nButton < UIButton\n";
        let idx = styles(&[("a.otui", src)]);
        let hints = ancestor_hints(src, &idx, &LuaWidgetIndex::new());
        let foo_hint = hints
            .iter()
            .find(|h| h.anchor == src.find("Button\n").unwrap() + "Button".len())
            .expect("Foo's hint present");
        assert_eq!(foo_hint.native, "UIButton");
    }

    #[test]
    fn the_hint_anchors_at_the_end_of_the_base_token() {
        let src = "Foo < Button\nButton < UIButton\n";
        let idx = styles(&[("a.otui", src)]);
        let hints = ancestor_hints(src, &idx, &LuaWidgetIndex::new());
        let base_end = src.find("Button\n").unwrap() + "Button".len();
        assert!(hints.iter().any(|h| h.anchor == base_end));
    }

    #[test]
    fn the_lua_parent_chain_does_not_change_which_native_is_reported() {
        // Foo < MyTable < UITable, with UITable's own Lua `extends` chain reaching UIWidget: the
        // hint must still say "UITable" (the native class the .otui chain resolves to), not walk
        // on into UITable's own Lua ancestors.
        let src = "Foo < MyTable\nMyTable < UITable\n";
        let idx = styles(&[("a.otui", src)]);
        let lua_idx = {
            let mut l = LuaWidgetIndex::new();
            l.set_document(
                "uitable.lua",
                scan_widgets("UITable = extends(UIWidget, 'UITable')\n"),
            );
            l
        };
        let hints = ancestor_hints(src, &idx, &lua_idx);
        let foo_hint = hints
            .iter()
            .find(|h| h.anchor == src.find("MyTable\n").unwrap() + "MyTable".len())
            .expect("Foo's hint present");
        assert_eq!(foo_hint.native, "UITable");
    }

    #[test]
    fn unparseable_source_yields_no_hints() {
        assert!(ancestor_hints("\t\t< <\n", &StyleIndex::new(), &LuaWidgetIndex::new()).is_empty());
    }
}
