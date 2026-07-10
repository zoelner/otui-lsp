//! Go-to-definition locators (spec §5.3): pure byte-offset cursor queries over the CST.
//!
//! This module answers "what is the cursor sitting on?" for the navigation features. It does **not**
//! resolve anything — resolution against the workspace [`StyleIndex`](crate::style_index::StyleIndex)
//! is the server's job. It only classifies the token under a byte offset and reports its text + span.
//!
//! ## Scope (deliberately narrow)
//!
//! The only reference kind this node locates is the **base** of a top-level `Name < Base` header
//! (spec §5.3, first row): the inheritance target that resolves against the global style namespace.
//! `id:`/anchor navigation and Lua cross-references are later nodes and are not handled here.
//!
//! Everything here is byte-offset based. No I/O, no `lsp-types`.

use crate::syntax::SyntaxTree;
use lang_api::ByteSpan;
use tree_sitter::Node;

/// A cursor hit on the `Base` token of a top-level `Name < Base` inheritance declaration.
///
/// [`name`](Self::name) is the base identifier text (e.g. `UIWindow`, `MyPanel`); [`span`](Self::span)
/// is the byte span of that token, kept so the server can echo it back for client-side highlighting.
/// Whether the name resolves to a defining `.otui` file or is a native `UI*` built-in is left to the
/// index — see [`is_native_base`](crate::style_index::is_native_base).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseRef {
    /// The base identifier the cursor is on (the `Base` in `Name < Base`).
    pub name: String,
    /// The byte span of the base token in the source.
    pub span: ByteSpan,
}

/// If `offset` falls within the `base` token of a **top-level** `Name < Base` declaration, return
/// that base's name and span; otherwise `None`.
///
/// Only the document root's direct children are considered inheritance headers: a `Name < Base`
/// nested inside a widget block is an *instance*, not an inheritance declaration, so its base token
/// is never reported (mirroring [`extract_style_defs`](crate::style_index::extract_style_defs)).
///
/// Native `UI*` bases are still located (a `BaseRef` is returned); classifying them as
/// non-resolvable is the index's concern, not the locator's.
///
/// ## Boundary convention
///
/// Spans are half-open `[start, end)` throughout this codebase, so the hit test is
/// `start <= offset < end`: an offset exactly at the end of the token is **not** inside it (it is the
/// boundary just past the last byte). See the `offset_just_past_base_is_not_a_hit` test.
#[must_use]
pub fn base_reference_at(source: &str, offset: usize) -> Option<BaseRef> {
    let tree = SyntaxTree::parse(source)?;
    let root = tree.root();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "style_header" {
            continue;
        }
        let Some(base) = child.child_by_field_name("base") else {
            continue;
        };
        let span = SyntaxTree::span_of(base);
        if span.start <= offset && offset < span.end {
            return Some(BaseRef {
                name: source[span.start..span.end].to_owned(),
                span,
            });
        }
    }
    None
}

/// A cursor hit anywhere inside a **top-level** `Name < Base` header, describing the whole header.
///
/// Returned by [`style_header_at`] when the cursor sits on the declared name token **or** the base
/// token of a top-level inheritance declaration. The server compares the query offset against
/// [`name_span`](Self::name_span) / [`base_span`](Self::base_span) to decide which part was hovered
/// (describe the base vs. describe this style) — the locator itself does not resolve anything.
///
/// A bare `Name` header with no `< Base` yields [`base`](Self::base) `None` and
/// [`base_span`](Self::base_span) `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyleHeaderRef {
    /// The declared style name (the `Name` in `Name < Base`).
    pub name: String,
    /// The base this style inherits from (the `Base` in `Name < Base`), if the header carries one.
    pub base: Option<String>,
    /// The byte span of the declared name token.
    pub name_span: ByteSpan,
    /// The byte span of the base token, if present.
    pub base_span: Option<ByteSpan>,
}

/// If `offset` falls within the **declared-name** token OR the **base** token of a **top-level**
/// style header, return the whole header descriptor; otherwise `None`.
///
/// Two grammar shapes count as a top-level style header, matching the document-symbol outline:
/// * a `Name < Base` `style_header` (its `name` and `base` fields), and
/// * a bare `Name` `container` (a top-level widget tag with no `< Base`) — reported with
///   [`base`](StyleHeaderRef::base) `None` and [`base_span`](StyleHeaderRef::base_span) `None`, its
///   tag token standing in for the declared name.
///
/// Only the document root's direct children are considered: a header nested inside a widget block is
/// an *instance*, not a style declaration, and is never reported (mirroring [`base_reference_at`] and
/// [`extract_style_defs`](crate::style_index::extract_style_defs)). A hit anywhere else in the header
/// (e.g. on the `<`) yields `None`.
///
/// The caller decides which part was hovered by testing the query offset against the returned
/// [`name_span`](StyleHeaderRef::name_span) / [`base_span`](StyleHeaderRef::base_span). The same
/// half-open `[start, end)` boundary convention as [`base_reference_at`] applies.
#[must_use]
pub fn style_header_at(source: &str, offset: usize) -> Option<StyleHeaderRef> {
    let tree = SyntaxTree::parse(source)?;
    let root = tree.root();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        // The declared-name token differs by shape: a `style_header` carries a `name` field, a bare
        // `container` carries the whole tag. Anything else is not a top-level style header.
        let name_node = match child.kind() {
            "style_header" => child.child_by_field_name("name"),
            "container" => child.child_by_field_name("tag"),
            _ => continue,
        };
        let Some(name_node) = name_node else {
            continue;
        };
        let name_span = SyntaxTree::span_of(name_node);
        // Only a `style_header` has a base; a bare container never does.
        let base_span = child
            .child_by_field_name("base")
            .map(SyntaxTree::span_of)
            .filter(|_| child.kind() == "style_header");

        let in_name = name_span.start <= offset && offset < name_span.end;
        let in_base = base_span.is_some_and(|span| span.start <= offset && offset < span.end);
        if in_name || in_base {
            return Some(StyleHeaderRef {
                name: source[name_span.start..name_span.end].to_owned(),
                base: base_span.map(|span| source[span.start..span.end].to_owned()),
                name_span,
                base_span,
            });
        }
    }
    None
}

/// A cursor hit on an `id:` value or on the `id` portion of an anchor target `<id>.edge` (spec §5.4).
///
/// [`id`](Self::id) is the id text; [`span`](Self::span) is its byte span — the `id:` value token
/// when the cursor is on a declaration, or just the `id` prefix (not the `.edge` suffix) when it is on
/// an anchor reference. Ids are per-document identities, so resolving occurrences is document-local
/// (the server's job — see [`references`](crate::references)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdRef {
    /// The id the cursor is on.
    pub id: String,
    /// The byte span of the id token (the `id:` value, or the `id` prefix of an `<id>.edge` target).
    pub span: ByteSpan,
}

/// If `offset` falls on an `id:` value token, or on the `id` portion of an anchor target
/// `<id>.edge`, return that id and its span; otherwise `None`.
///
/// Two grammar shapes carry an id the cursor can sit on:
/// * the `value` of an `id_property` (`id: <value>`) — the declaration; and
/// * the `id` prefix of a dotted `anchor_target` `<id>.edge` (`anchors.top: header.bottom`) — a
///   reference. A hit on the `.edge` suffix, or on a bare (dot-less) magic target like `parent`,
///   yields `None`: those are not id tokens.
///
/// The same half-open `[start, end)` boundary convention as [`base_reference_at`] applies.
#[must_use]
pub fn id_at(source: &str, offset: usize) -> Option<IdRef> {
    let tree = SyntaxTree::parse(source)?;
    find_id_at(tree.root(), source, offset)
}

/// Depth-first search for the id token under `offset` (ids nest at any depth).
fn find_id_at(node: Node<'_>, source: &str, offset: usize) -> Option<IdRef> {
    match node.kind() {
        "id_property" => {
            if let Some(value) = node.child_by_field_name("value") {
                let span = SyntaxTree::span_of(value);
                if span.start <= offset && offset < span.end {
                    return Some(IdRef {
                        id: source[span.start..span.end].to_owned(),
                        span,
                    });
                }
            }
        }
        "anchor_target" => {
            if let Some(hit) = anchor_id_ref(node, source, offset) {
                return Some(hit);
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(hit) = find_id_at(child, source, offset) {
            return Some(hit);
        }
    }
    None
}

/// If `offset` falls on the `id` prefix of a dotted `anchor_target` `<id>.edge`, return that id ref;
/// otherwise `None` (a hit on the `.edge` suffix, or a dot-less magic target, is not an id).
fn anchor_id_ref(anchor_target: Node<'_>, source: &str, offset: usize) -> Option<IdRef> {
    let target = anchor_target.child_by_field_name("target")?;
    let span = SyntaxTree::span_of(target);
    let text = &source[span.start..span.end];
    let dot = text.find('.')?; // no dot → magic keyword (parent/prev/next/…), not an id
    let prefix = &text[..dot];
    // A dotted target can still be magic: `parent.bottom` / `next.top` / `prev.left` reference the
    // magic widget, not a user id, so their prefix is never an id reference.
    if crate::schema::is_magic_anchor_target(prefix) {
        return None;
    }
    let prefix_end = span.start + dot;
    if span.start <= offset && offset < prefix_end {
        Some(IdRef {
            id: prefix.to_owned(),
            span: ByteSpan::new(span.start, prefix_end),
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte offset of the first occurrence of `needle` in `src` (panics if absent) — a readable way
    /// to place the cursor in a test.
    fn at(src: &str, needle: &str) -> usize {
        src.find(needle).expect("needle present")
    }

    #[test]
    fn cursor_on_base_returns_it() {
        let src = "MainWindow < UIWindow\n";
        let start = at(src, "UIWindow");
        let got = base_reference_at(src, start).expect("hit");
        assert_eq!(got.name, "UIWindow");
        assert_eq!(&src[got.span.start..got.span.end], "UIWindow");
        // A cursor in the middle of the token is the same hit.
        assert_eq!(base_reference_at(src, start + 2).as_ref(), Some(&got));
    }

    #[test]
    fn cursor_on_declared_name_is_not_a_base_ref() {
        // The `Name` left of `<` is the declaration target, not a base reference.
        let src = "MainWindow < UIWindow\n";
        assert!(base_reference_at(src, at(src, "MainWindow")).is_none());
    }

    #[test]
    fn cursor_on_a_property_value_is_not_a_base_ref() {
        let src = "MainWindow < UIWindow\n  id: main\n";
        assert!(base_reference_at(src, at(src, "main")).is_none());
    }

    #[test]
    fn native_base_is_still_located() {
        // Classification (native vs. user) is the index's job; the locator returns the ref anyway.
        let src = "Button < UIButton\n";
        let got = base_reference_at(src, at(src, "UIButton")).expect("hit");
        assert_eq!(got.name, "UIButton");
    }

    #[test]
    fn user_base_is_located() {
        let src = "Base < UIWidget\nChild < Base\n";
        // The `Base` in `Child < Base` (a user base) is located.
        let base_in_child = src.rfind("Base").expect("present");
        let got = base_reference_at(src, base_in_child).expect("hit");
        assert_eq!(got.name, "Base");
    }

    #[test]
    fn offset_just_past_base_is_not_a_hit() {
        // Half-open spans: the offset at `end` is the boundary just past the token, not inside it.
        let src = "MainWindow < UIWindow\n";
        let end = at(src, "UIWindow") + "UIWindow".len();
        assert!(base_reference_at(src, end).is_none());
        // ...but the last byte of the token (end - 1) is inside.
        assert!(base_reference_at(src, end - 1).is_some());
    }

    #[test]
    fn nested_widget_name_is_not_a_base_ref() {
        // The nested `Inner < UIWidget` is an instance, not an inheritance decl: its base must not
        // be reported. Only the top-level `Outer < UIWidget` base is a hit.
        let src = "Outer < UIWidget\n  Inner < UIButton\n    id: x\n";
        // The nested base `UIButton` is not located.
        assert!(base_reference_at(src, at(src, "UIButton")).is_none());
        // The top-level base `UIWidget` is.
        assert_eq!(
            base_reference_at(src, at(src, "UIWidget")).map(|r| r.name),
            Some("UIWidget".to_owned())
        );
    }

    #[test]
    fn empty_or_offscreen_offset_yields_nothing() {
        assert!(base_reference_at("", 0).is_none());
        let src = "MainWindow < UIWindow\n";
        assert!(base_reference_at(src, src.len() + 100).is_none());
    }

    #[test]
    fn header_at_cursor_on_declared_name_returns_the_header() {
        let src = "MainWindow < UIWindow\n";
        let got = style_header_at(src, at(src, "MainWindow")).expect("hit");
        assert_eq!(got.name, "MainWindow");
        assert_eq!(got.base.as_deref(), Some("UIWindow"));
        assert_eq!(&src[got.name_span.start..got.name_span.end], "MainWindow");
        let base_span = got.base_span.expect("base present");
        assert_eq!(&src[base_span.start..base_span.end], "UIWindow");
        // A cursor in the middle of the name is the same hit.
        assert_eq!(
            style_header_at(src, at(src, "MainWindow") + 3).as_ref(),
            Some(&got)
        );
    }

    #[test]
    fn header_at_cursor_on_base_returns_the_header() {
        let src = "MainWindow < UIWindow\n";
        let got = style_header_at(src, at(src, "UIWindow")).expect("hit");
        assert_eq!(got.name, "MainWindow");
        assert_eq!(got.base.as_deref(), Some("UIWindow"));
    }

    #[test]
    fn header_at_bare_header_has_no_base() {
        let src = "Standalone\n  id: x\n";
        let got = style_header_at(src, at(src, "Standalone")).expect("hit");
        assert_eq!(got.name, "Standalone");
        assert_eq!(got.base, None);
        assert_eq!(got.base_span, None);
    }

    #[test]
    fn header_at_nested_widget_name_is_none() {
        // The nested `Inner` name is an instance, not a style header.
        let src = "Outer < UIWidget\n  Inner < UIButton\n    id: x\n";
        assert!(style_header_at(src, at(src, "Inner")).is_none());
        // ...but the top-level name is a hit.
        assert_eq!(
            style_header_at(src, at(src, "Outer")).map(|h| h.name),
            Some("Outer".to_owned())
        );
    }

    #[test]
    fn header_at_property_value_is_none() {
        let src = "MainWindow < UIWindow\n  id: main\n";
        assert!(style_header_at(src, at(src, "main")).is_none());
    }

    #[test]
    fn id_at_cursor_on_id_value_returns_it() {
        let src = "Panel\n  id: header\n";
        let got = id_at(src, at(src, "header")).expect("hit");
        assert_eq!(got.id, "header");
        assert_eq!(&src[got.span.start..got.span.end], "header");
        // A cursor in the middle of the value is the same hit.
        assert_eq!(id_at(src, at(src, "header") + 2).as_ref(), Some(&got));
    }

    #[test]
    fn id_at_cursor_on_anchor_target_id_prefix_returns_it() {
        // The `header` prefix of `header.bottom` is the id reference; its span excludes `.bottom`.
        let src = "Other\n  anchors.top: header.bottom\n";
        let got = id_at(src, at(src, "header.bottom")).expect("hit");
        assert_eq!(got.id, "header");
        assert_eq!(&src[got.span.start..got.span.end], "header");
    }

    #[test]
    fn id_at_cursor_on_anchor_edge_suffix_is_none() {
        // A hit on the `.bottom` edge (after the dot) is not an id token.
        let src = "Other\n  anchors.top: header.bottom\n";
        assert!(id_at(src, at(src, "bottom")).is_none());
    }

    #[test]
    fn id_at_cursor_on_bare_magic_target_is_none() {
        // `parent` is a magic keyword (no dot), not an id.
        let src = "Other\n  anchors.fill: parent\n";
        assert!(id_at(src, at(src, "parent")).is_none());
    }

    #[test]
    fn id_at_cursor_on_dotted_magic_target_prefix_is_none() {
        // `parent.bottom` references the magic parent widget, not a user id — the `parent` prefix
        // must not be classified as an id even though it is dotted.
        let src = "Other\n  anchors.top: parent.bottom\n";
        assert!(id_at(src, at(src, "parent")).is_none());
        // ...while a real id prefix in the same shape IS a hit.
        let src2 = "Other\n  anchors.top: header.bottom\n";
        assert_eq!(
            id_at(src2, at(src2, "header")).map(|r| r.id),
            Some("header".to_owned())
        );
    }

    #[test]
    fn id_at_cursor_off_any_id_is_none() {
        let src = "MainWindow < UIWindow\n";
        assert!(id_at(src, at(src, "UIWindow")).is_none());
        assert!(id_at("", 0).is_none());
    }
}
