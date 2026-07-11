//! Locating asset **path** values in a document for the LSP `documentLink` feature.
//!
//! Walks the CST for the value of every `property` whose key is a file-path-valued OTUI tag
//! ([`schema::PATH_PROPERTIES`] — primarily `image-source`) and reports the value token's byte span
//! plus the raw path string. This is a **pure finder**: byte offsets only, no filesystem, no
//! `lsp-types`. Resolving a path to an actual file on disk — and whether the target exists — is I/O
//! and belongs in the server, not here.
//!
//! ## What counts as a link
//!
//! Only real `property` nodes are considered — the generic `key: value` form the grammar tags as
//! `property`. An `id_property`, an anchor/event/alias/expr property, a bare container tag, or a
//! style header never contributes a link, even if it happens to share a spelling. The value must be
//! non-empty after trimming; the reported span is tightened to cover exactly the trimmed path text
//! (so the editor underlines just the path, not any surrounding whitespace the value token spans).

use lang_api::ByteSpan;
use tree_sitter::Node;

use crate::schema;
use crate::syntax::SyntaxTree;

/// A path-valued property occurrence: the byte span of the value token (tightened to the trimmed
/// path text) and the raw path string it carries. The span is a byte span into the document this was
/// scanned from; the server maps it to an LSP range and resolves `path` against the filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathRef {
    /// The byte span of the path value token in the source (trimmed to the path text).
    pub span: ByteSpan,
    /// The raw path string (the value text, trimmed). Not resolved — the server resolves it.
    pub path: String,
}

/// Find every file-path-valued property value in `source` (LSP `documentLink`). For each `property`
/// whose key is in [`schema::PATH_PROPERTIES`], the value token's trimmed span and raw path text are
/// returned. Non-path properties (`id: x`, `text: y`), non-`property` nodes, and properties with an
/// empty value are ignored. Returns an empty vector when the source cannot be parsed.
#[must_use]
pub fn document_links(source: &str) -> Vec<PathRef> {
    let Some(tree) = SyntaxTree::parse(source) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect(tree.root(), source, &mut out);
    out
}

/// Point-locate the file-path-valued property **value** the cursor is on: if `offset` falls inside
/// the trimmed path text of a `property` whose key is in [`schema::PATH_PROPERTIES`], return that
/// property's [`PathRef`]; otherwise `None`.
///
/// This complements [`document_links`]'s bulk sweep with a point query — "what asset, if any, is
/// under the cursor?" (e.g. to drive a sprite-preview hover). A cursor on the **key**
/// (`image-source`) is deliberately not a hit here — that position is
/// [`property_hover_at`](crate::property_hover::property_hover_at)'s job — nor is a cursor on the
/// value of a non-path property, or anywhere outside a property.
///
/// Spans are half-open `[start, end)`, matching the rest of this crate's locators (see
/// `navigation::base_reference_at`): an offset exactly at the end of the path text is not inside it.
#[must_use]
pub fn asset_ref_at(source: &str, offset: usize) -> Option<PathRef> {
    let tree = SyntaxTree::parse(source)?;
    let start = tree.root().descendant_for_byte_range(offset, offset)?;
    let mut node = start;
    let property = loop {
        if node.kind() == "property" {
            break node;
        }
        node = node.parent()?;
    };
    let path_ref = path_ref(property, source)?;
    if path_ref.span.start <= offset && offset < path_ref.span.end {
        Some(path_ref)
    } else {
        None
    }
}

/// Pre-order walk emitting a [`PathRef`] for every path-valued `property` under `node`.
fn collect(node: Node<'_>, source: &str, out: &mut Vec<PathRef>) {
    if node.kind() == "property" {
        if let Some(path_ref) = path_ref(node, source) {
            out.push(path_ref);
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, source, out);
    }
}

/// Build the [`PathRef`] for `property` when its key is a path-valued tag and its value is a
/// non-empty (after trimming) path; `None` otherwise. The key compare is exact (case sensitive),
/// matching the engine's `node->tag() == "..."` dispatch. The returned span is tightened to the
/// trimmed value text so it underlines exactly the path.
fn path_ref(property: Node<'_>, source: &str) -> Option<PathRef> {
    let key = property.child_by_field_name("key")?;
    let key_text = &source[key.start_byte()..key.end_byte()];
    if !schema::PATH_PROPERTIES.contains(&key_text) {
        return None;
    }
    let value = property.child_by_field_name("value")?;
    let span = SyntaxTree::span_of(value);
    let raw = &source[span.start..span.end];
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Tighten the span onto the trimmed text: the value token may span leading/trailing whitespace
    // the engine ignores, and we want the link range to cover just the path.
    let lead = raw.len() - raw.trim_start().len();
    let start = span.start + lead;
    let end = start + trimmed.len();
    Some(PathRef {
        span: ByteSpan::new(start, end),
        path: trimmed.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte offset of the first occurrence of `needle` in `src` (panics if absent) — the house cursor
    /// helper shared across this crate's `*_at` locator tests.
    fn at(src: &str, needle: &str) -> usize {
        src.find(needle).expect("needle present")
    }

    /// The `(text slice, path)` for each link found, for readable assertions.
    fn links_with_text(source: &str) -> Vec<(&str, String)> {
        document_links(source)
            .into_iter()
            .map(|r| (&source[r.span.start..r.span.end], r.path))
            .collect()
    }

    #[test]
    fn finds_image_source_value_with_span() {
        let source = "Panel\n  image-source: /images/ui/window\n";
        let links = document_links(source);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].path, "/images/ui/window");
        // The span covers exactly the path text.
        assert_eq!(
            &source[links[0].span.start..links[0].span.end],
            "/images/ui/window"
        );
        assert_eq!(links[0].span.start, source.find("/images").unwrap());
    }

    #[test]
    fn finds_icon_and_icon_source_paths() {
        let source =
            "Button\n  icon: /images/icons/ok.png\nOther\n  icon-source: things/sprites.png\n";
        let found = links_with_text(source);
        assert_eq!(
            found,
            vec![
                ("/images/icons/ok.png", "/images/icons/ok.png".to_owned()),
                ("things/sprites.png", "things/sprites.png".to_owned()),
            ]
        );
    }

    #[test]
    fn ignores_non_path_properties() {
        // `id:` is an `id_property`, `text:` is a non-path `property`; neither is a link.
        let source = "Panel\n  id: main\n  text: Hello World\n  color: red\n";
        assert!(document_links(source).is_empty());
    }

    #[test]
    fn ignores_non_property_nodes() {
        // A bare container tag and a style header must never be links, even one literally spelled
        // like a path-valued key would be (there is none here — these are structural nodes).
        let source = "MainWindow < UIWindow\n  Panel\n    id: body\n";
        assert!(document_links(source).is_empty());
    }

    #[test]
    fn empty_value_yields_no_link() {
        // `image-source:` with no value is not a link (nothing to open).
        let source = "Panel\n  image-source:\n";
        assert!(document_links(source).is_empty());
    }

    #[test]
    fn span_covers_exactly_the_path_and_leaves_the_key_out() {
        // The reported span underlines the path token only — never the `image-source:` key, the `:`,
        // or the separating space. A path with an interior `.` and `/` is reported verbatim, so the
        // extension and directory separators are inside the link.
        let source = "Panel\n  image-source: assets/ui/bg.9.png\n";
        let r = &document_links(source)[0];
        let text = &source[r.span.start..r.span.end];
        assert_eq!(text, "assets/ui/bg.9.png");
        assert_eq!(text, r.path, "span text and stored path agree");
        // The span sits strictly after the `: ` separator (the key/colon are not underlined).
        let colon = source.find(':').unwrap();
        assert!(
            r.span.start > colon + 1,
            "span starts past the `: ` separator"
        );
    }

    #[test]
    fn multiple_links_across_widgets_in_source_order() {
        let source =
            "A\n  image-source: a.png\nB\n  image-source: b.png\nC\n  image-source: c.png\n";
        let paths: Vec<String> = document_links(source).into_iter().map(|r| r.path).collect();
        assert_eq!(paths, ["a.png", "b.png", "c.png"]);
    }

    #[test]
    fn asset_ref_at_cursor_in_image_source_value_returns_it() {
        let source = "Panel\n  image-source: /images/ui/window\n";
        let offset = at(source, "/images") + 1;
        let got = asset_ref_at(source, offset).expect("hit");
        assert_eq!(got.path, "/images/ui/window");
        assert_eq!(&source[got.span.start..got.span.end], "/images/ui/window");
    }

    #[test]
    fn asset_ref_at_cursor_on_key_is_none() {
        // The key position is `property_hover_at`'s job, not this locator's.
        let source = "Panel\n  image-source: /images/ui/window\n";
        assert!(asset_ref_at(source, at(source, "image-source") + 1).is_none());
    }

    #[test]
    fn asset_ref_at_cursor_in_non_path_property_value_is_none() {
        let source = "Panel\n  text: Hello World\n  color: red\n";
        assert!(asset_ref_at(source, at(source, "Hello") + 1).is_none());
        assert!(asset_ref_at(source, at(source, "red") + 1).is_none());
    }

    #[test]
    fn asset_ref_at_cursor_outside_any_property_is_none() {
        let source = "Panel\n  image-source: /images/ui/window\n";
        // On the widget tag name.
        assert!(asset_ref_at(source, at(source, "Panel")).is_none());
        assert!(asset_ref_at("", 0).is_none());
    }

    #[test]
    fn asset_ref_at_covers_each_path_property() {
        let source =
            "Button\n  icon: a.png\nOther\n  icon-source: b.png\nThird\n  image-source: c.png\n";
        assert_eq!(
            asset_ref_at(source, at(source, "a.png") + 1).map(|r| r.path),
            Some("a.png".to_owned())
        );
        assert_eq!(
            asset_ref_at(source, at(source, "b.png") + 1).map(|r| r.path),
            Some("b.png".to_owned())
        );
        assert_eq!(
            asset_ref_at(source, at(source, "c.png") + 1).map(|r| r.path),
            Some("c.png".to_owned())
        );
    }

    #[test]
    fn asset_ref_at_boundary_matches_half_open_convention() {
        // Half-open `[start, end)`: the first char of the path is a hit, and the last char is a hit,
        // but the offset exactly at `end` (one past the last char) is not — consistent with
        // `navigation::base_reference_at`'s `offset_just_past_base_is_not_a_hit`.
        let source = "Panel\n  image-source: a.png\n";
        let start = at(source, "a.png");
        let end = start + "a.png".len();
        assert!(asset_ref_at(source, start).is_some());
        assert!(asset_ref_at(source, end - 1).is_some());
        assert!(asset_ref_at(source, end).is_none());
    }

    #[test]
    fn asset_ref_at_agrees_with_document_links_no_regression_from_refactor() {
        // The shared `path_ref` helper still produces exactly what `document_links` produced before
        // the point-locator was factored out.
        let source = "A\n  image-source: a.png\nB\n  icon: b.png\nC\n  icon-source: c.png\n";
        let bulk = document_links(source);
        assert_eq!(bulk.len(), 3);
        for r in &bulk {
            let offset = r.span.start;
            assert_eq!(asset_ref_at(source, offset).as_ref(), Some(r));
        }
    }
}
