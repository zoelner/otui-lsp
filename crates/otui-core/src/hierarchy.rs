//! The style-inheritance **cursor locator** (spec §5.2/§5.3): the pure core behind
//! `textDocument/typeDefinition` and `textDocument/implementation`, and the classifier a later
//! `textDocument/typeHierarchy` node will reuse.
//!
//! OTML expresses inheritance as `Name < Base` (a top-level `style_header`). This module answers a
//! single question — *which style name does the symbol under the cursor resolve to?* — via
//! [`style_type_at`]: the tag of a **widget instance** (a `container`, at any depth), or the
//! declared-name / base token of a **top-level** `style_header`. The server then resolves that name
//! against the workspace [`StyleIndex`](crate::style_index::StyleIndex): to its declaration(s) for
//! typeDefinition ([`lookup`](crate::style_index::StyleIndex::lookup)), or to the styles deriving from
//! it for implementation ([`subtypes`](crate::style_index::StyleIndex::subtypes)). Those cross-document
//! answers come from the cached index — this module never reparses documents; it only locates the
//! token under a byte offset.
//!
//! ## Fidelity notes (mirroring [`references`](crate::references) / [`style_index`](crate::style_index))
//!
//! * **Only top-level `Name < Base` headers are style declarations.** A `Name < Base` nested in a
//!   widget block is an instance, not an inheritance declaration; only the document root's direct
//!   children are considered `style_header`s here (matching
//!   [`extract_style_defs`](crate::style_index::extract_style_defs)).
//! * **Widget instances nest at any depth.** A `container` tag is an instance wherever it appears, so
//!   [`style_type_at`] searches containers recursively (unlike the top-level-only `style_header` case).
//! * **Native `UI*` names are still returned.** [`style_type_at`] returns a native `UI*` tag/base as a
//!   plain name (it is a real style-ish token); whether it has a *user* declaration is the server's
//!   decision — a native name simply has no declaration in the index, so it resolves to nothing,
//!   exactly as a native base does in go-to-definition. (See
//!   [`is_native_base`](crate::style_index::is_native_base) — used by the server, not here: this
//!   module never classifies, it only locates.)
//!
//! Everything here is byte-offset based. No I/O, no `lsp-types`.

use crate::syntax::SyntaxTree;
use lang_api::ByteSpan;
use tree_sitter::Node;

/// The style name the cursor resolved to, plus the byte span of the token it came from.
///
/// [`name`](Self::name) is the style name text; [`span`](Self::span) is the byte span of the token the
/// cursor was on — the container tag, or the header name / base token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyleRef {
    /// The style name.
    pub name: String,
    /// The byte span of the token this name came from.
    pub span: ByteSpan,
}

/// The style name the symbol under `offset` resolves to for type navigation, or `None`.
///
/// Two token shapes carry a navigable type:
/// * a **widget instance** — the `tag` of a `container`, at **any** depth (a nested `Panel`/`Button`
///   or a top-level bare container). Its tag text is the instance's type.
/// * a **top-level** `style_header`'s declared-name **or** base token. Whichever the cursor sits on is
///   returned (the name for its own declaration, the base for the type it inherits) — this is the
///   symbol the server then resolves to a declaration (typeDefinition) or lists subtypes of
///   (implementation).
///
/// A nested `Name < Base` header is an instance, not a declaration, so its name/base tokens are *not*
/// returned here (only the top-level ones are — the widget-instance path above covers the nested
/// container case). A hit anywhere else (a property, the `<`, whitespace) yields `None`. Native `UI*`
/// tags/bases are returned as names; the server decides they have no user declaration.
///
/// Half-open `[start, end)` boundary convention (matching
/// [`base_reference_at`](crate::navigation::base_reference_at)): an offset exactly at a token's end is
/// not inside it.
#[must_use]
pub fn style_type_at(source: &str, offset: usize) -> Option<StyleRef> {
    let tree = SyntaxTree::parse(source)?;
    let root = tree.root();

    // 1. A top-level `style_header`'s declared-name or base token (top-level only: a nested header is
    //    a widget instance, handled by the container search below via its tag).
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "style_header" {
            continue;
        }
        for field in ["name", "base"] {
            if let Some(node) = child.child_by_field_name(field)
                && let Some(hit) = ref_if_inside(node, source, offset)
            {
                return Some(hit);
            }
        }
    }

    // 2. A `container` tag anywhere in the tree (widget instances nest at any depth).
    container_tag_at(root, source, offset)
}

/// Recursively find the `container` tag under `offset` (widget instances nest at any depth).
fn container_tag_at(node: Node<'_>, source: &str, offset: usize) -> Option<StyleRef> {
    if node.kind() == "container"
        && let Some(tag) = node.child_by_field_name("tag")
        && let Some(hit) = ref_if_inside(tag, source, offset)
    {
        return Some(hit);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(hit) = container_tag_at(child, source, offset) {
            return Some(hit);
        }
    }
    None
}

/// Build a [`StyleRef`] from `node` when `offset` falls inside its span (half-open), else `None`.
fn ref_if_inside(node: Node<'_>, source: &str, offset: usize) -> Option<StyleRef> {
    let span = SyntaxTree::span_of(node);
    if span.start <= offset && offset < span.end {
        Some(StyleRef {
            name: source[span.start..span.end].to_owned(),
            span,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte offset of the first occurrence of `needle` in `src` (panics if absent).
    fn at(src: &str, needle: &str) -> usize {
        src.find(needle).expect("needle present")
    }

    #[test]
    fn type_at_widget_instance_tag_returns_the_tag_name() {
        // A nested `Button` container is a widget instance; its tag is its type.
        let src = "MainWindow < UIWindow\n  Button\n    id: ok\n";
        let got = style_type_at(src, at(src, "Button")).expect("hit");
        assert_eq!(got.name, "Button");
        assert_eq!(&src[got.span.start..got.span.end], "Button");
        // A cursor in the middle of the tag is the same hit.
        assert_eq!(
            style_type_at(src, at(src, "Button") + 2).as_ref(),
            Some(&got)
        );
    }

    #[test]
    fn type_at_top_level_bare_container_tag_returns_it() {
        let src = "Panel\n  id: root\n";
        let got = style_type_at(src, at(src, "Panel")).expect("hit");
        assert_eq!(got.name, "Panel");
    }

    #[test]
    fn type_at_style_header_name_returns_the_name() {
        let src = "MainWindow < UIWindow\n";
        let got = style_type_at(src, at(src, "MainWindow")).expect("hit");
        assert_eq!(got.name, "MainWindow");
        assert_eq!(&src[got.span.start..got.span.end], "MainWindow");
    }

    #[test]
    fn type_at_style_header_base_returns_the_base() {
        let src = "MainWindow < UIWindow\n";
        let got = style_type_at(src, at(src, "UIWindow")).expect("hit");
        // The base token is returned as a name — native classification is the server's concern.
        assert_eq!(got.name, "UIWindow");
    }

    #[test]
    fn type_at_off_symbol_is_none() {
        let src = "MainWindow < UIWindow\n  id: main\n";
        // On the `id:` value, not a type token.
        assert!(style_type_at(src, at(src, "main")).is_none());
        assert!(style_type_at("", 0).is_none());
        // Just past the base token (half-open) is not a hit.
        let end = at(src, "UIWindow") + "UIWindow".len();
        assert!(style_type_at(src, end).is_none());
    }

    #[test]
    fn type_at_unparseable_source_is_none() {
        // Never panic on a tab-indented (scanner-rejected) document; return `None`.
        assert!(style_type_at("\t\t< <\n", 1).is_none());
    }
}
