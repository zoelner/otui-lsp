//! Go-to-definition locators (spec §5.3): pure byte-offset cursor queries over the CST.
//!
//! This module answers "what is the cursor sitting on?" for the navigation features. Most of it does
//! **not** resolve anything — resolution against the workspace
//! [`StyleIndex`](crate::style_index::StyleIndex) is the server's job, and these functions only
//! classify the token under a byte offset and report its text + span. [`resolve_anchor_target`] is the
//! one exception: an anchor target resolves entirely within the **current document** (direct siblings
//! only — never cross-file), so its full resolution is pure and lives here too.
//!
//! ## Scope
//!
//! * the **base** of a top-level `Name < Base` header (spec §5.3, first row) — [`base_reference_at`]:
//!   the inheritance target that resolves against the global style namespace (still the server's job);
//! * the **id** of an `id:` declaration or a dotted `<id>.edge` anchor reference — [`id_at`];
//! * an **anchor target** (dotted or the bare `fill:`/`centerIn:` shorthand) — [`resolve_anchor_target`]:
//!   resolved, in full, against the owner's direct siblings (spec §5.3/§5.5).
//!
//! Lua cross-references are a later/separate node and are not handled here.
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

/// Which of the two id-token shapes a cursor hit landed on. Spec §5.5 gives these two *different*
/// hovers: "this widget's id" (plus a reference count) for a [`Declaration`](Self::Declaration), vs.
/// the resolved sibling's kind (or "not found") for an [`AnchorTarget`](Self::AnchorTarget) — a
/// separate, later hover this node does not implement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdRefKind {
    /// The `value` of an `id_property` (`id: <value>`) — the id is *declared* here.
    Declaration,
    /// The `id` prefix of a dotted `anchor_target` (`<id>.edge`) — the id is *referenced* here.
    AnchorTarget,
}

/// A cursor hit on an `id:` value or on the `id` portion of an anchor target `<id>.edge` (spec §5.4).
///
/// [`id`](Self::id) is the id text; [`span`](Self::span) is its byte span — the `id:` value token
/// when the cursor is on a declaration, or just the `id` prefix (not the `.edge` suffix) when it is on
/// an anchor reference; [`kind`](Self::kind) tells the two shapes apart. Ids are per-document
/// identities, so resolving occurrences is document-local (the server's job — see
/// [`references`](crate::references)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdRef {
    /// The id the cursor is on.
    pub id: String,
    /// The byte span of the id token (the `id:` value, or the `id` prefix of an `<id>.edge` target).
    pub span: ByteSpan,
    /// Whether this hit is the id's declaration or a reference to it.
    pub kind: IdRefKind,
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
                        kind: IdRefKind::Declaration,
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
            kind: IdRefKind::AnchorTarget,
        })
    } else {
        None
    }
}

/// How an anchor **target** resolves (spec §5.3 go-to-definition / §5.5 hover), from
/// [`resolve_anchor_target`]. The engine rule ( `UIAnchor::getHookedWidget`,
/// `uianchorlayout.cpp:26-42`): the magic keywords `parent`/`next`/`prev` resolve to the parent widget
/// / the next or previous sibling in source order; anything else is looked up by
/// `parentWidget->getChildById(id)`, a single non-recursive lookup (`uiwidget.cpp:1487-1493`) over the
/// owner's parent's **direct children only** — i.e. the owner's direct siblings. An ancestor or any
/// other non-sibling id is not a parse error; the lookup returns null and the anchor silently becomes
/// a runtime no-op (`uianchorlayout.cpp:252-254`), never a diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnchorTargetResolution {
    /// The target id names exactly one of the anchor owner's **direct sibling** widgets (last-wins
    /// when more than one shares the id, mirroring the engine's single-valued `m_childrenById` map:
    /// each `addChild` overwrites any earlier entry for the same id).
    Sibling {
        /// The target id (the sibling's declared `id:` value).
        id: String,
        /// The sibling's widget kind: its `container` tag, or its `style_header` base (spec §5.5 —
        /// the same "what type is this widget" question [`widget_resolve::enclosing_widget_type`]
        /// answers elsewhere).
        kind: String,
        /// The byte span of the sibling's own `id:` value — the go-to-definition jump target.
        id_decl_span: ByteSpan,
        /// The byte span of the target id token the cursor was on.
        target_span: ByteSpan,
    },
    /// The target is one of the magic pseudo-targets (`parent` / `next` / `prev`) — not a user id, so
    /// there is nothing to jump to; hover explains what the keyword means instead.
    Magic {
        /// The magic keyword (`parent`, `next`, or `prev`).
        keyword: String,
        /// The byte span of the target id token the cursor was on.
        target_span: ByteSpan,
    },
    /// The target id is not a magic keyword and does not name any direct sibling — an ancestor's id,
    /// a typo, or an id declared elsewhere entirely. The engine's lookup would return null and the
    /// anchor silently fails to resolve; this is never a diagnostic (spec §4/§5.3), just "not found".
    Unresolved {
        /// The target id text (unresolved).
        id: String,
        /// The byte span of the target id token the cursor was on.
        target_span: ByteSpan,
    },
}

/// Resolve the anchor **target** under `offset` — the id/magic-keyword portion of an
/// `anchors.<edge>: <target>` value, dotted (`<id>.edge`) or bare (the whole `anchors.fill:`/
/// `anchors.centerIn:` shorthand value, which carries no `.edge` suffix of its own) — against the
/// anchor owner's direct siblings (spec §5.3 go-to-definition, §5.5 hover). `None` when `offset` is
/// not on a target token at all (mirrors [`id_at`]'s boundary convention: a hit on the `.edge` suffix
/// is not a target-id hit).
///
/// Deliberately **not** built on [`references::id_occurrences`](crate::references::id_occurrences):
/// that scan is document-wide, first-declaration-wins, and would happily resolve an ancestor's or an
/// unrelated widget's id — exactly the false positive the engine's direct-children-only
/// `getChildById` lookup never produces (see [`AnchorTargetResolution`]'s doc for the citation).
/// Scoping to direct siblings only is delegated to [`completion::anchor_owner_widget`] and
/// [`completion::direct_sibling_widgets`] — the identical CST walk completion's own anchor-target
/// suggestions already use, so the two features can never disagree on what counts as "in scope".
#[must_use]
pub fn resolve_anchor_target(source: &str, offset: usize) -> Option<AnchorTargetResolution> {
    let tree = SyntaxTree::parse(source)?;
    let root = tree.root();
    let (prefix, target_span) = find_anchor_target_prefix_at(root, source, offset)?;

    if crate::schema::is_magic_anchor_target(&prefix) {
        return Some(AnchorTargetResolution::Magic {
            keyword: prefix,
            target_span,
        });
    }

    let owner = crate::completion::anchor_owner_widget(root, offset);
    let mut resolved = None;
    if let Some(owner) = owner {
        // Last sibling wins on a duplicate id, mirroring the engine's single-valued `m_childrenById`
        // map: each later `addChild` overwrites any earlier entry under the same id.
        for sibling in crate::completion::direct_sibling_widgets(owner) {
            if let Some((id, id_decl_span)) = crate::completion::widget_id_ref(sibling, source)
                && id == prefix
            {
                let kind = crate::widget_resolve::enclosing_widget_type(sibling, source, None)
                    .unwrap_or_else(|| "widget".to_owned());
                resolved = Some(AnchorTargetResolution::Sibling {
                    id,
                    kind,
                    id_decl_span,
                    target_span,
                });
            }
        }
    }

    Some(resolved.unwrap_or(AnchorTargetResolution::Unresolved {
        id: prefix,
        target_span,
    }))
}

/// Depth-first search for the `anchor_target` node whose target's **prefix** — the id/magic-keyword
/// portion before any `.edge` suffix, or the whole token when there is no dot (the bare
/// `anchors.fill:`/`anchors.centerIn:` shorthand) — contains `offset`. Returns the prefix text and its
/// own byte span (dot-exclusive); a hit on the `.edge` suffix itself is not a match.
fn find_anchor_target_prefix_at(
    node: Node<'_>,
    source: &str,
    offset: usize,
) -> Option<(String, ByteSpan)> {
    if node.kind() == "anchor_target" {
        let target = node.child_by_field_name("target")?;
        let span = SyntaxTree::span_of(target);
        let text = &source[span.start..span.end];
        let prefix_end = text.find('.').map_or(span.end, |dot| span.start + dot);
        if span.start <= offset && offset < prefix_end {
            let prefix = &text[..prefix_end - span.start];
            return Some((prefix.to_owned(), ByteSpan::new(span.start, prefix_end)));
        }
        return None;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(hit) = find_anchor_target_prefix_at(child, source, offset) {
            return Some(hit);
        }
    }
    None
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
        // A declaration hit is discriminated `Declaration`, not `AnchorTarget`.
        assert_eq!(got.kind, IdRefKind::Declaration);
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
        // An anchor-target hit is discriminated `AnchorTarget`, not `Declaration` — this is the guard
        // the id-value hover (a later node) relies on to never fire on an anchor target.
        assert_eq!(got.kind, IdRefKind::AnchorTarget);
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

    #[test]
    fn resolve_anchor_target_dotted_sibling_resolves_to_its_kind_and_id_span() {
        let src = "Outer < UIWidget\n  Header < UILabel\n    id: header\n  Body < UIWidget\n    anchors.top: header.bottom\n";
        let got = resolve_anchor_target(src, at(src, "header.bottom")).expect("hit");
        let AnchorTargetResolution::Sibling {
            id,
            kind,
            id_decl_span,
            target_span,
        } = got
        else {
            panic!("expected Sibling, got {got:?}");
        };
        assert_eq!(id, "header");
        assert_eq!(kind, "UILabel");
        // `id_decl_span` points at the sibling's own `id: header` value, not the anchor target.
        assert_eq!(&src[id_decl_span.start..id_decl_span.end], "header");
        assert_eq!(
            id_decl_span,
            ByteSpan::new(at(src, "id: header") + 4, at(src, "id: header") + 10)
        );
        assert_eq!(&src[target_span.start..target_span.end], "header");
    }

    #[test]
    fn resolve_anchor_target_bare_fill_shorthand_resolves_the_sibling() {
        // The bare `fill:`/`centerIn:` shorthand target carries no `.edge` suffix at all — `id_at`
        // does not recognize this shape, but the resolver must.
        let src = "Outer < UIWidget\n  Header < UILabel\n    id: header\n  Body < UIWidget\n    anchors.fill: header\n";
        // The target `header` (not the earlier `id: header` declaration) is the last occurrence.
        let got = resolve_anchor_target(src, src.rfind("header").expect("present")).expect("hit");
        match got {
            AnchorTargetResolution::Sibling { id, kind, .. } => {
                assert_eq!(id, "header");
                assert_eq!(kind, "UILabel");
            }
            other => panic!("expected Sibling, got {other:?}"),
        }
    }

    #[test]
    fn resolve_anchor_target_bare_center_in_shorthand_resolves_the_sibling() {
        let src = "Outer < UIWidget\n  Header < UILabel\n    id: header\n  Body < UIWidget\n    anchors.centerIn: header\n";
        let got = resolve_anchor_target(src, src.rfind("header").expect("present")).expect("hit");
        assert!(matches!(got, AnchorTargetResolution::Sibling { .. }));
    }

    #[test]
    fn resolve_anchor_target_bare_magic_keyword_is_magic() {
        let src = "Outer < UIWidget\n  Body < UIWidget\n    anchors.fill: parent\n";
        let got = resolve_anchor_target(src, at(src, "parent")).expect("hit");
        match got {
            AnchorTargetResolution::Magic {
                keyword,
                target_span,
            } => {
                assert_eq!(keyword, "parent");
                assert_eq!(&src[target_span.start..target_span.end], "parent");
            }
            other => panic!("expected Magic, got {other:?}"),
        }
    }

    #[test]
    fn resolve_anchor_target_dotted_magic_keyword_is_magic() {
        // `parent.bottom` — the magic keyword can carry a dotted edge too.
        let src = "Outer < UIWidget\n  Body < UIWidget\n    anchors.top: parent.bottom\n";
        let got = resolve_anchor_target(src, at(src, "parent.bottom")).expect("hit");
        assert!(
            matches!(got, AnchorTargetResolution::Magic { keyword, .. } if keyword == "parent")
        );
    }

    #[test]
    fn resolve_anchor_target_next_and_prev_are_magic_too() {
        let src = "Outer < UIWidget\n  A < UIWidget\n    anchors.top: prev.bottom\n  B < UIWidget\n    anchors.top: next.bottom\n";
        assert!(matches!(
            resolve_anchor_target(src, at(src, "prev.bottom")),
            Some(AnchorTargetResolution::Magic { .. })
        ));
        assert!(matches!(
            resolve_anchor_target(src, at(src, "next.bottom")),
            Some(AnchorTargetResolution::Magic { .. })
        ));
    }

    #[test]
    fn resolve_anchor_target_ancestor_id_is_unresolved_not_a_sibling() {
        // `outer` is the *ancestor*'s own id, not a sibling of `Body` — `getChildById` only ever
        // searches direct children, so this can never resolve at runtime either.
        let src = "Outer < UIWidget\n  id: outer\n  Body < UIWidget\n    anchors.fill: outer\n";
        let got = resolve_anchor_target(src, src.rfind("outer").expect("present")).expect("hit");
        match got {
            AnchorTargetResolution::Unresolved { id, target_span } => {
                assert_eq!(id, "outer");
                assert_eq!(&src[target_span.start..target_span.end], "outer");
            }
            other => panic!("expected Unresolved, got {other:?}"),
        }
    }

    #[test]
    fn resolve_anchor_target_own_id_is_unresolved_not_its_own_sibling() {
        // A widget anchoring to its own id: it is excluded from its own sibling set.
        let src = "Outer < UIWidget\n  Body < UIWidget\n    id: body\n    anchors.fill: body\n";
        let got = resolve_anchor_target(src, src.rfind("body").expect("present")).expect("hit");
        assert!(matches!(got, AnchorTargetResolution::Unresolved { .. }));
    }

    #[test]
    fn resolve_anchor_target_unknown_id_is_unresolved() {
        let src = "Outer < UIWidget\n  Body < UIWidget\n    anchors.fill: nosuchwidget\n";
        let got = resolve_anchor_target(src, at(src, "nosuchwidget")).expect("hit");
        assert!(
            matches!(got, AnchorTargetResolution::Unresolved { id, .. } if id == "nosuchwidget")
        );
    }

    #[test]
    fn resolve_anchor_target_duplicate_sibling_id_last_one_wins() {
        // Mirrors the engine's single-valued `m_childrenById` map: each later `addChild` overwrites
        // the earlier entry under the same id.
        let src = "Outer < UIWidget\n  A < UILabel\n    id: dup\n  B < UIButton\n    id: dup\n  Body < UIWidget\n    anchors.fill: dup\n";
        let got = resolve_anchor_target(src, src.rfind("dup").expect("present")).expect("hit");
        match got {
            AnchorTargetResolution::Sibling { kind, .. } => assert_eq!(kind, "UIButton"),
            other => panic!("expected Sibling, got {other:?}"),
        }
    }

    #[test]
    fn resolve_anchor_target_edge_suffix_hit_is_none() {
        let src = "Outer < UIWidget\n  Header < UILabel\n    id: header\n  Body < UIWidget\n    anchors.top: header.bottom\n";
        assert!(resolve_anchor_target(src, at(src, "bottom")).is_none());
    }

    #[test]
    fn resolve_anchor_target_off_any_target_is_none() {
        let src = "Outer < UIWidget\n  id: main\n";
        assert!(resolve_anchor_target(src, at(src, "main")).is_none());
        assert!(resolve_anchor_target("", 0).is_none());
    }

    #[test]
    fn resolve_anchor_target_container_sibling_kind_is_its_tag() {
        // A bare-tag (`container`) sibling's kind is its own tag, not a `Name < Base` header.
        let src =
            "Outer < UIWidget\n  Button\n    id: btn\n  Body < UIWidget\n    anchors.fill: btn\n";
        let got = resolve_anchor_target(src, src.rfind("btn").expect("present")).expect("hit");
        assert!(matches!(got, AnchorTargetResolution::Sibling { kind, .. } if kind == "Button"));
    }
}
