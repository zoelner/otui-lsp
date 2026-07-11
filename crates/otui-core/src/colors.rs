//! Locating color values in a document for the LSP `documentColor` feature (spec §2.9).
//!
//! Walks the CST for color values and resolves each to its actual [`Rgba`] via
//! [`schema::color_value`], returning `(span, rgba)` pairs the server maps to LSP `ColorInformation`.
//! Everything here is pure — byte spans only, no `lsp-types` — so it is unit-testable without a live
//! server.
//!
//! ## What counts as a color (and how false positives are avoided)
//!
//! Two kinds of occurrence are swatched:
//!
//! * **Color literals** — the **hex** (`#rgb`/`#rrggbb`/…) and **functional** (`rgb(..)`, `hsl(..)`,
//!   …) forms the grammar tags as a dedicated `color` node. These are **context-free**: a `#ff0000`
//!   or `rgb(1,2,3)` is unambiguously a color regardless of which property it sits on, so they are
//!   always swatched.
//! * **Named colors** (`red`, `blue`, …) — a bare word is lexically indistinguishable from an `id:`
//!   value or any other identifier, so it is swatched **only in a color-typed property value
//!   position**: the value (or an inline-array item of the value) of a `property` whose key is in
//!   [`crate::catalog::COLOR_PROPERTIES`] (the engine's `value<Color>` dispatch sites — `color`,
//!   `background`, `border-color*`, `icon-color`, `image-color`, `ttf-stroke-color`). So
//!   `color: red` / `border-color: blue` swatch, but `id: red` / `text: blue` do NOT — an
//!   `id_property` or a non-color property never triggers a named swatch.

use lang_api::ByteSpan;
use tree_sitter::Node;

use crate::schema::{self, Rgba};
use crate::syntax::SyntaxTree;

/// Find every color value in `source` with the byte span of the exact token and its resolved
/// [`Rgba`] (spec §2.9). Returns an empty vector when the source cannot be parsed. Context-free color
/// literals (hex + functional) are always found; named colors are found only in a color-typed
/// property value position (see the module docs), so `id: red` and identifiers merely spelled like a
/// color yield nothing.
#[must_use]
pub fn document_colors(source: &str) -> Vec<(ByteSpan, Rgba)> {
    let Some(tree) = SyntaxTree::parse(source) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect(tree.root(), source, &mut out);
    out
}

/// Pre-order walk emitting `(span, rgba)` for every color occurrence under `node`.
///
/// A `color` literal node is always emitted (context-free). A `property` whose key is a color-typed
/// tag additionally contributes its **named** color value(s) — a bare `plain_value`, or the
/// `identifier` items of an `inline_array` value. Color-literal children (`color` nodes) are left to
/// the context-free rule as the recursion reaches them, so nothing is double-counted.
fn collect(node: Node<'_>, source: &str, out: &mut Vec<(ByteSpan, Rgba)>) {
    if node.kind() == "color" {
        push_color(node, source, out);
    } else if node.kind() == "property" {
        if let Some(value) = color_typed_value(node, source) {
            collect_named_in_value(value, source, out);
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, source, out);
    }
}

/// The `value` node of `property` when its key is a color-typed tag
/// ([`crate::catalog::COLOR_PROPERTIES`]); `None` otherwise. The key compare is exact (case
/// sensitive), matching the engine's `node->tag() == "..."` dispatch.
fn color_typed_value<'a>(property: Node<'a>, source: &str) -> Option<Node<'a>> {
    let key = property.child_by_field_name("key")?;
    let key_text = &source[key.start_byte()..key.end_byte()];
    if !crate::catalog::COLOR_PROPERTIES.contains(&key_text) {
        return None;
    }
    property.child_by_field_name("value")
}

/// Emit the named-color swatch(es) carried by a color-typed property's `value`: a whole
/// `plain_value` bare name, or the `identifier` items of an `inline_array`. A `color`-literal value
/// is intentionally skipped here — the context-free rule in [`collect`] handles it.
fn collect_named_in_value(value: Node<'_>, source: &str, out: &mut Vec<(ByteSpan, Rgba)>) {
    match value.kind() {
        "plain_value" => push_color(value, source, out),
        "inline_array" => {
            let mut cursor = value.walk();
            for item in value.named_children(&mut cursor) {
                if item.kind() == "identifier" {
                    push_color(item, source, out);
                }
            }
        }
        _ => {}
    }
}

/// Resolve `node`'s text as a color and, if it is one, push its span + [`Rgba`].
fn push_color(node: Node<'_>, source: &str, out: &mut Vec<(ByteSpan, Rgba)>) {
    let text = &source[node.start_byte()..node.end_byte()];
    if let Some(rgba) = schema::color_value(text) {
        out.push((SyntaxTree::span_of(node), rgba));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `(text slice, rgba)` for each color found, for readable assertions.
    fn colors_with_text(source: &str) -> Vec<(&str, Rgba)> {
        document_colors(source)
            .into_iter()
            .map(|(span, rgba)| (&source[span.start..span.end], rgba))
            .collect()
    }

    #[test]
    fn finds_hex_and_functional_colors_with_spans() {
        let source = "Panel\n  color: #ff0000\n  background-color: rgb(0, 255, 0)\n";
        let found = document_colors(source);
        assert_eq!(found.len(), 2);

        // First swatch: the `#ff0000` token, red.
        let (span0, rgba0) = found[0];
        assert_eq!(&source[span0.start..span0.end], "#ff0000");
        assert_eq!(rgba0, Rgba::from_u8(255, 0, 0, 255));

        // Second swatch: the `rgb(0, 255, 0)` token, green.
        let (span1, rgba1) = found[1];
        assert_eq!(&source[span1.start..span1.end], "rgb(0, 255, 0)");
        assert_eq!(rgba1, Rgba::from_u8(0, 255, 0, 255));
    }

    #[test]
    fn named_color_in_a_color_typed_property_is_swatched() {
        // A bare named color in a color-typed property value position is swatched.
        let found = colors_with_text("Label\n  color: red\n");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "red");
        assert_eq!(found[0].1, Rgba::from_u8(255, 0, 0, 255));

        // `border-color` is a color-typed property too; a hex literal there is context-free anyway.
        let found = colors_with_text("Panel\n  border-color: #ffffff\n");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "#ffffff");
        assert_eq!(found[0].1, Rgba::from_u8(255, 255, 255, 255));

        // `transparent` in a color property swatches as fully transparent.
        let found = colors_with_text("Panel\n  background-color: transparent\n");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, Rgba::from_u8(0, 0, 0, 0));
    }

    #[test]
    fn legacy_named_color_in_color_property_swatches_with_alpha() {
        // A legacy engine static (darkPink) resolves via the catalog and swatches in a color
        // property. Engine `green` is the bright 0x00ff00, distinct from CSS green.
        let found = colors_with_text("Panel\n  color: darkPink\n");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, Rgba::from_u8(0x80, 0x00, 0x80, 255));

        let found = colors_with_text("Panel\n  color: green\n");
        assert_eq!(found[0].1, Rgba::from_u8(0, 255, 0, 255));
    }

    #[test]
    fn named_color_in_a_color_property_inline_array_is_swatched() {
        // Both items of a color-typed property's inline array swatch: the named `red` (identifier)
        // and the `#00ff00` literal.
        let found = colors_with_text("Widget\n  color: [red, #00ff00]\n");
        let texts: Vec<&str> = found.iter().map(|(t, _)| *t).collect();
        assert!(texts.contains(&"red"));
        assert!(texts.contains(&"#00ff00"));
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn named_color_in_a_non_color_property_is_not_swatched() {
        // The value of a non-color property is never a named swatch, even inside an array.
        assert!(document_colors("Widget\n  text: [red, blue]\n").is_empty());
    }

    #[test]
    fn identifier_spelled_like_a_color_is_not_a_swatch() {
        // The classic false-positive: an `id:` value spelled exactly like a named color must yield
        // no color (an id_property is not a color-typed property), and neither does a non-color
        // property like `text`.
        assert!(document_colors("Panel\n  id: red\n").is_empty());
        assert!(document_colors("Panel\n  text: blue\n").is_empty());
    }

    #[test]
    fn ignores_non_color_values() {
        // A number, a plain word and an id are not colors — even `width`/`text` are non-color props.
        let source = "Panel\n  width: 100\n  text: Hello World\n  id: main\n";
        assert!(document_colors(source).is_empty());
    }

    #[test]
    fn unparsable_source_yields_no_colors() {
        // An unterminated inline array parses to an ERROR node — no panic, and no colors emitted.
        assert!(document_colors("x: [a, b\n").is_empty());
    }
}
