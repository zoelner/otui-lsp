//! Locating color values in a document for the LSP `documentColor` feature (spec §2.9).
//!
//! Walks the CST for every color token and resolves it to its actual [`Rgba`] via
//! [`schema::color_value`], returning `(span, rgba)` pairs the server maps to LSP `ColorInformation`.
//! Everything here is pure — byte spans only, no `lsp-types` — so it is unit-testable without a live
//! server.

use lang_api::ByteSpan;

use crate::schema::{self, Rgba};
use crate::syntax::SyntaxTree;

/// The CST node kinds whose text can hold a color value:
///
/// * `color` — a hex or functional literal (`#rrggbb`, `rgb(..)`, …), tagged by the grammar's
///   dedicated color rule (spec §2.9). Always a color.
/// * `plain_value` — a whole untyped property value (`color: red`), where a **named** color surfaces
///   (named colors are not lexed as `color` nodes).
/// * `identifier` — a bare word inside an inline array (`[red, blue]`), the array-item form of a
///   named color.
///
/// Each candidate's text is run through [`schema::color_value`]; only the ones that actually resolve
/// to a color become swatches, so non-color values (`width: 10`, `text: Hello`) are ignored.
const COLOR_NODE_KINDS: &[&str] = &["color", "plain_value", "identifier"];

/// Find every color value in `source` and its resolved [`Rgba`], each with the byte span of the exact
/// token (spec §2.9). Returns an empty vector when the source cannot be parsed. Values that do not
/// resolve to a color are skipped, so the result contains only real color occurrences.
#[must_use]
pub fn document_colors(source: &str) -> Vec<(ByteSpan, Rgba)> {
    let Some(tree) = SyntaxTree::parse(source) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    tree.walk(|kind, span| {
        if !COLOR_NODE_KINDS.contains(&kind) {
            return;
        }
        let text = &source[span.start..span.end];
        if let Some(rgba) = schema::color_value(text) {
            out.push((span, rgba));
        }
    });
    out
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
    fn finds_named_colors_as_plain_values() {
        let found = colors_with_text("Label\n  color: red\n");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "red");
        assert_eq!(found[0].1, Rgba::from_u8(255, 0, 0, 255));
    }

    #[test]
    fn finds_named_colors_inside_inline_arrays() {
        let found = colors_with_text("Widget\n  colors: [red, #00ff00]\n");
        let texts: Vec<&str> = found.iter().map(|(t, _)| *t).collect();
        assert!(texts.contains(&"red"));
        assert!(texts.contains(&"#00ff00"));
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn ignores_non_color_values() {
        // A number, a plain word and an unknown name are not colors.
        let source = "Panel\n  width: 100\n  text: Hello World\n  id: main\n";
        assert!(document_colors(source).is_empty());
    }

    #[test]
    fn transparent_named_color_is_fully_transparent() {
        let found = colors_with_text("Panel\n  color: transparent\n");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, Rgba::from_u8(0, 0, 0, 0));
    }

    #[test]
    fn unparsable_source_yields_no_colors() {
        // An unterminated inline array parses to an ERROR node but must not panic.
        let _ = document_colors("x: [a, b\n");
    }
}
