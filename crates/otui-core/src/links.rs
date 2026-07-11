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
}
