//! Code lenses over top-level style declarations (`textDocument/codeLens`): a small "N widgets
//! inherit this style" annotation on a style's declared name, so the reader can see at a glance
//! whether a style is a leaf or a widely-reused base — without opening
//! `textDocument/implementation` to find out.
//!
//! Deliberately modest: no navigation payload, no per-derivation breakdown, and — per
//! [`StyleIndex::subtypes`] — **only direct** derivations (`X < Name`) are counted, not the whole
//! subtree. A style with zero direct subtypes gets no lens at all (a lens reading "0 widgets" would
//! be noise on every leaf style in the corpus).
//!
//! Pure: byte offsets only, no I/O, no `lsp-types`. The server turns each [`StyleLens`]'s
//! `name_span` into an LSP `Range` and formats the count into a `Command` title.

use crate::style_index::{StyleIndex, extract_style_defs};
use crate::syntax::SyntaxTree;
use lang_api::ByteSpan;

/// One code lens anchored on a top-level style declaration's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyleLens {
    /// The byte span of the declared name identifier (the `Name` in `Name < Base`) — the lens
    /// anchor, mirroring [`StyleDef::name_span`](crate::style_index::StyleDef::name_span).
    pub name_span: ByteSpan,
    /// How many styles directly derive from this one (`X < Name`), workspace-wide.
    pub derived_count: usize,
}

/// Compute the code lenses for every top-level style declaration in `source` that has at least one
/// direct derivation, resolved against the workspace-wide `index` (spec §5.2's `Name < Base`
/// namespace is global, so a style's derivations can live in other documents).
///
/// A style with zero direct subtypes is skipped entirely — only styles with `derived_count >= 1`
/// are emitted. Returns an empty vec when `source` fails to parse.
#[must_use]
pub fn style_lenses(source: &str, index: &StyleIndex) -> Vec<StyleLens> {
    let Some(tree) = SyntaxTree::parse(source) else {
        return Vec::new();
    };
    extract_style_defs(&tree)
        .into_iter()
        .filter_map(|def| {
            let derived_count = index.subtypes(&def.name).len();
            (derived_count >= 1).then_some(StyleLens {
                name_span: def.name_span,
                derived_count,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style_index::DocId;

    /// Build a [`StyleIndex`] from `(doc, otui_source)` pairs.
    fn index(docs: &[(&str, &str)]) -> StyleIndex {
        let mut idx = StyleIndex::new();
        for (doc, src) in docs {
            let tree = SyntaxTree::parse(src).expect("parse otui");
            idx.set_document(DocId::new(*doc), extract_style_defs(&tree));
        }
        idx
    }

    #[test]
    fn a_style_with_subtypes_gets_a_lens_with_the_exact_count() {
        // Button has two direct derivations, one in another document.
        let idx = index(&[
            ("a.otui", "Button < UIButton\nOkButton < Button\n"),
            ("b.otui", "CancelButton < Button\n"),
        ]);
        let src = "Button < UIButton\n";
        let lenses = style_lenses(src, &idx);
        assert_eq!(lenses.len(), 1);
        assert_eq!(lenses[0].derived_count, 2);
        assert_eq!(
            &src[lenses[0].name_span.start..lenses[0].name_span.end],
            "Button"
        );
    }

    #[test]
    fn a_style_with_no_subtypes_gets_no_lens() {
        let idx = index(&[("a.otui", "Leaf < UIWidget\n")]);
        let lenses = style_lenses("Leaf < UIWidget\n", &idx);
        assert!(lenses.is_empty());
    }

    #[test]
    fn only_direct_subtypes_are_counted_not_the_whole_subtree() {
        // Grandchild derives from Child, not from Base directly, so Base's count is 1, not 2.
        let idx = index(&[(
            "a.otui",
            "Base < UIWidget\nChild < Base\nGrandchild < Child\n",
        )]);
        let src = "Base < UIWidget\nChild < Base\nGrandchild < Child\n";
        let lenses = style_lenses(src, &idx);
        let base_lens = lenses
            .iter()
            .find(|l| &src[l.name_span.start..l.name_span.end] == "Base")
            .expect("Base has a lens");
        assert_eq!(base_lens.derived_count, 1);
        let child_lens = lenses
            .iter()
            .find(|l| &src[l.name_span.start..l.name_span.end] == "Child")
            .expect("Child has a lens");
        assert_eq!(child_lens.derived_count, 1);
    }

    #[test]
    fn multiple_top_level_styles_each_get_their_own_lens() {
        let idx = index(&[("a.otui", "A < UIWidget\nB < A\nC < A\nD < UIWidget\n")]);
        let src = "A < UIWidget\nB < A\nC < A\nD < UIWidget\n";
        let lenses = style_lenses(src, &idx);
        // Only A has subtypes (B, C); D has none, B and C have none either.
        assert_eq!(lenses.len(), 1);
        assert_eq!(
            &src[lenses[0].name_span.start..lenses[0].name_span.end],
            "A"
        );
        assert_eq!(lenses[0].derived_count, 2);
    }

    #[test]
    fn unparseable_source_yields_no_lenses() {
        assert!(style_lenses("\t\t< <\n", &StyleIndex::new()).is_empty());
    }
}
